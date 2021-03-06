use super::FileAttribute;
use crate::data_types::chars::NUL_16;
use crate::table::runtime::Time;
use crate::{CStr16, Char16, Guid, Identify};
use core::cmp;
use core::convert::TryInto;
use core::ffi::c_void;
use core::mem;
use core::result;
use core::slice;

/// Common trait for data structures that can be used with
/// File::set_info() or File::set_info().
///
/// The long-winded name is needed because "FileInfo" is already taken by UEFI.
pub trait FileProtocolInfo: Identify + FromUefi {}

/// Trait for going from an UEFI-originated pointer to a Rust reference
///
/// This is trivial for Sized types, but requires some work when operating on
/// dynamic-sized types like NamedFileProtocolInfo, as the second member of the
/// fat pointer must be reconstructed using hidden UEFI-provided metadata.
pub trait FromUefi {
    /// Turn an UEFI-provided pointer-to-base into a (possibly fat) Rust reference
    unsafe fn from_uefi<'a>(ptr: *mut c_void) -> &'a mut Self;
}

/// Dynamically sized FileProtocolInfo with a header and an UCS-2 name
///
/// All struct that can currently be queried via Get/SetInfo can be described as
/// a (possibly empty) header followed by a variable-sized name.
///
/// Since such dynamic-sized types are a bit unpleasant to handle in Rust today,
/// this generic struct was created to deduplicate the relevant code.
///
/// The reason why this struct covers the whole DST, as opposed to the [Char16]
/// part only, is that pointers to DSTs are created in a rather unintuitive way
/// that is best kept centralized in one place.
#[repr(C)]
pub struct NamedFileProtocolInfo<Header> {
    header: Header,
    name: [Char16],
}

impl<Header> NamedFileProtocolInfo<Header> {
    /// Correct the alignment of a storage buffer for this type by discarding the first few bytes
    ///
    /// Return an empty slice if the storage is not large enough to perform this operation
    pub fn realign_storage(mut storage: &mut [u8]) -> &mut [u8] {
        // Compute the degree of storage misalignment. mem::align_of does not
        // support dynamically sized types, so we must help it a bit.
        let storage_address = storage.as_ptr() as usize;
        let info_alignment = cmp::max(mem::align_of::<Header>(), mem::align_of::<Char16>());
        let storage_misalignment = storage_address % info_alignment;
        let realignment_padding = info_alignment - storage_misalignment;

        // Return an empty slice if the storage is too small to be realigned
        if storage.len() < realignment_padding {
            return &mut [];
        }

        // If the storage is large enough, realign it and return
        storage = &mut storage[realignment_padding..];
        debug_assert_eq!((storage.as_ptr() as usize) % info_alignment, 0);
        storage
    }

    /// Create a NamedFileProtocolInfo structure in user-provided storage
    ///
    /// The structure will be created in-place within the provided storage
    /// buffer. The buffer must be large enough to hold the data structure,
    /// including a null-terminated UCS-2 version of the "name" string.
    ///
    /// The buffer should be suitably aligned for the full data structure. If
    /// it is not, some bytes at the beginning of the buffer will not be used,
    /// resulting in a reduction of effective storage capacity.
    #[allow(clippy::cast_ptr_alignment)]
    fn new_impl<'a>(
        mut storage: &'a mut [u8],
        header: Header,
        name: &str,
    ) -> result::Result<&'a mut Self, FileInfoCreationError> {
        // Try to realign the storage in preparation for storing this type
        storage = Self::realign_storage(storage);

        // Make sure that the storage is large enough for our needs
        let name_length_ucs2 = name.chars().count() + 1;
        let name_size = name_length_ucs2 * mem::size_of::<Char16>();
        let info_size = mem::size_of::<Header>() + name_size;
        if storage.len() < info_size {
            return Err(FileInfoCreationError::InsufficientStorage(info_size));
        }

        // Write the header at the beginning of the storage
        let header_ptr = storage.as_mut_ptr() as *mut Header;
        unsafe {
            header_ptr.write(header);
        }

        // At this point, our storage contains a correct header, followed by
        // random rubbish. It is okay to reinterpret the rubbish as Char16s
        // because 1/we are going to overwrite it and 2/Char16 does not have a
        // Drop implementation. Thus, we are now ready to build a correctly
        // sized &mut Self and go back to the realm of safe code.
        debug_assert!(!mem::needs_drop::<Char16>());
        let info_ptr = unsafe {
            slice::from_raw_parts_mut(storage.as_mut_ptr() as *mut Char16, name_length_ucs2)
                as *mut [Char16] as *mut Self
        };
        let info = unsafe { &mut *info_ptr };
        debug_assert_eq!(info.name.len(), name_length_ucs2);

        // Write down the UCS-2 name before returning the storage reference
        for (target, ch) in info.name.iter_mut().zip(name.chars()) {
            *target = ch
                .try_into()
                .map_err(|_| FileInfoCreationError::InvalidChar(ch))?;
        }
        info.name[name_length_ucs2 - 1] = NUL_16;
        Ok(info)
    }
}

impl<Header> FromUefi for NamedFileProtocolInfo<Header> {
    #[allow(clippy::cast_ptr_alignment)]
    unsafe fn from_uefi<'a>(raw_ptr: *mut c_void) -> &'a mut Self {
        let byte_ptr = raw_ptr as *mut u8;
        let name_ptr = byte_ptr.add(mem::size_of::<Header>()) as *mut Char16;
        let name = CStr16::from_ptr(name_ptr);
        let name_len = name.to_u16_slice_with_nul().len();
        let fat_ptr = slice::from_raw_parts_mut(raw_ptr as *mut Char16, name_len);
        let self_ptr = fat_ptr as *mut [Char16] as *mut Self;
        &mut *self_ptr
    }
}

/// Errors that can occur when creating a FileProtocolInfo
pub enum FileInfoCreationError {
    /// The provided buffer was too small to hold the FileInfo. You need at
    /// least the indicated buffer size (in bytes). Please remember that using
    /// a misaligned buffer will cause a decrease of usable storage capacity.
    InsufficientStorage(usize),

    /// The suggested file name contains invalid code points (not in UCS-2)
    InvalidChar(char),
}

/// Generic file information
///
/// The following rules apply when using this struct with set_info():
///
/// - On directories, the file size is determined by the contents of the
///   directory and cannot be changed by setting file_size. On directories,
///   file_size is ignored during a set_info().
/// - The physical_size is determined by the file_size and cannot be changed.
///   This value is ignored during a set_info() request.
/// - The FileAttribute::DIRECTORY bit cannot be changed. It must match the
///   file’s actual type.
/// - A value of zero in create_time, last_access, or modification_time causes
///   the fields to be ignored (and not updated).
/// - It is forbidden to change the name of a file to the name of another
///   existing file in the same directory.
/// - If a file is read-only, the only allowed change is to remove the read-only
///   attribute. Other changes must be carried out in a separate transaction.
pub type FileInfo = NamedFileProtocolInfo<FileInfoHeader>;

/// Header for generic file information
#[repr(C)]
pub struct FileInfoHeader {
    size: u64,
    file_size: u64,
    physical_size: u64,
    create_time: Time,
    last_access_time: Time,
    modification_time: Time,
    attribute: FileAttribute,
}

unsafe impl Identify for FileInfo {
    const GUID: Guid = Guid::from_values(
        0x0957_6e92,
        0x6d3f,
        0x11d2,
        [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
    );
}

impl FileInfo {
    /// Create a FileInfo structure
    ///
    /// The structure will be created in-place within the provided storage
    /// buffer. The buffer must be large enough to hold the data structure,
    /// including a null-terminated UCS-2 version of the "name" string.
    ///
    /// The buffer should be suitably aligned for the full data structure. If
    /// it is not, some bytes at the beginning of the buffer will not be used,
    /// resulting in a reduction of effective storage capacity.
    ///
    #[allow(clippy::too_many_arguments)]
    pub fn new<'a>(
        storage: &'a mut [u8],
        file_size: u64,
        physical_size: u64,
        create_time: Time,
        last_access_time: Time,
        modification_time: Time,
        attribute: FileAttribute,
        file_name: &str,
    ) -> result::Result<&'a mut Self, FileInfoCreationError> {
        let header = FileInfoHeader {
            size: 0,
            file_size,
            physical_size,
            create_time,
            last_access_time,
            modification_time,
            attribute,
        };
        let info = Self::new_impl(storage, header, file_name)?;
        info.header.size = mem::size_of_val(&info) as u64;
        Ok(info)
    }

    /// File size (number of bytes stored in the file)
    pub fn file_size(&self) -> u64 {
        self.header.file_size
    }

    /// Physical space consumed by the file on the file system volume
    pub fn physical_size(&self) -> u64 {
        self.header.physical_size
    }

    /// Time when the file was created
    pub fn create_time(&self) -> &Time {
        &self.header.create_time
    }

    /// Time when the file was last accessed
    pub fn last_access_time(&self) -> &Time {
        &self.header.last_access_time
    }

    /// Time when the file's contents were last modified
    pub fn modification_time(&self) -> &Time {
        &self.header.modification_time
    }

    /// Attribute bits for the file
    pub fn attribute(&self) -> FileAttribute {
        self.header.attribute
    }

    /// Name of the file
    pub fn file_name(&self) -> &CStr16 {
        unsafe { CStr16::from_ptr(&self.name[0]) }
    }
}

impl FileProtocolInfo for FileInfo {}

/// System volume information
///
/// May only be obtained on the root directory's file handle.
///
/// Please note that only the system volume's volume label may be set using
/// this information structure. Consider using FileSystemVolumeLabel instead.
pub type FileSystemInfo = NamedFileProtocolInfo<FileSystemInfoHeader>;

/// Header for system volume information
#[repr(C)]
pub struct FileSystemInfoHeader {
    size: u64,
    read_only: bool,
    volume_size: u64,
    free_space: u64,
    block_size: u32,
}

unsafe impl Identify for FileSystemInfo {
    const GUID: Guid = Guid::from_values(
        0x0957_6e93,
        0x6d3f,
        0x11d2,
        [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b],
    );
}

impl FileSystemInfo {
    /// Create a FileSystemInfo structure
    ///
    /// The structure will be created in-place within the provided storage
    /// buffer. The buffer must be large enough to hold the data structure,
    /// including a null-terminated UCS-2 version of the "name" string.
    ///
    /// The buffer should be suitably aligned for the full data structure. If
    /// it is not, some bytes at the beginning of the buffer will not be used,
    /// resulting in a reduction of effective storage capacity.
    #[allow(clippy::too_many_arguments)]
    pub fn new<'a>(
        storage: &'a mut [u8],
        read_only: bool,
        volume_size: u64,
        free_space: u64,
        block_size: u32,
        volume_label: &str,
    ) -> result::Result<&'a mut Self, FileInfoCreationError> {
        let header = FileSystemInfoHeader {
            size: 0,
            read_only,
            volume_size,
            free_space,
            block_size,
        };
        let info = Self::new_impl(storage, header, volume_label)?;
        info.header.size = mem::size_of_val(&info) as u64;
        Ok(info)
    }

    /// Truth that the volume only supports read access
    pub fn read_only(&self) -> bool {
        self.header.read_only
    }

    /// Number of bytes managed by the file system
    pub fn volume_size(&self) -> u64 {
        self.header.volume_size
    }

    /// Number of available bytes for use by the file system
    pub fn free_space(&self) -> u64 {
        self.header.free_space
    }

    /// Nominal block size by which files are typically grown
    pub fn block_size(&self) -> u32 {
        self.header.block_size
    }

    /// Volume label
    pub fn volume_label(&self) -> &CStr16 {
        unsafe { CStr16::from_ptr(&self.name[0]) }
    }
}

impl FileProtocolInfo for FileSystemInfo {}

/// System volume label
///
/// May only be obtained on the root directory's file handle.
pub type FileSystemVolumeLabel = NamedFileProtocolInfo<FileSystemVolumeLabelHeader>;

/// Header for system volume label information
#[repr(C)]
pub struct FileSystemVolumeLabelHeader {}

unsafe impl Identify for FileSystemVolumeLabel {
    const GUID: Guid = Guid::from_values(
        0xdb47_d7d3,
        0xfe81,
        0x11d3,
        [0x9a, 0x35, 0x00, 0x90, 0x27, 0x3f, 0xc1, 0x4d],
    );
}

impl FileSystemVolumeLabel {
    /// Create a FileSystemVolumeLabel structure
    ///
    /// The structure will be created in-place within the provided storage
    /// buffer. The buffer must be large enough to hold the data structure,
    /// including a null-terminated UCS-2 version of the "name" string.
    ///
    /// The buffer should be suitably aligned for the full data structure. If
    /// it is not, some bytes at the beginning of the buffer will not be used,
    /// resulting in a reduction of effective storage capacity.
    pub fn new<'a>(
        storage: &'a mut [u8],
        volume_label: &str,
    ) -> result::Result<&'a mut Self, FileInfoCreationError> {
        let header = FileSystemVolumeLabelHeader {};
        Self::new_impl(storage, header, volume_label)
    }

    /// Volume label
    pub fn volume_label(&self) -> &CStr16 {
        unsafe { CStr16::from_ptr(&self.name[0]) }
    }
}

impl FileProtocolInfo for FileSystemVolumeLabel {}
