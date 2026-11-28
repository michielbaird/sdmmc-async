/// What can go wrong mounting a volume or reading from it. `E` is the block device's own error.
///
/// New variants may be added in a minor release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error<E> {
    /// The block device failed.
    Device(E),
    /// The boot sector is not valid (and neither is its backup).
    BadBootSector,
    /// The boot sector describes a layout that does not fit the device or is inconsistent.
    BadGeometry,
    /// There is no filesystem at block 0 and no usable partition table.
    NoPartition,
    /// The volume was expected to be FAT32 and is not.
    NotFat32,
    /// The volume was expected to be exFAT and is not.
    NotExFat,
    /// A valid filesystem this crate does not support (FAT12 or FAT16, TexFAT, a sector size other than 512).
    Unsupported,
    /// The card holds a filesystem this build leaves out (the `fat32` or `exfat` feature is off).
    FilesystemNotEnabled,
    /// A cluster number in the filesystem points outside the volume: the volume is damaged.
    InvalidCluster,
    /// No file or directory at that path.
    NotFound,
    /// The path names a directory where a file was needed.
    NotAFile,
    /// The path names a file where a directory was needed (or runs through one).
    NotADirectory,
    /// The path or a name in it is not valid.
    InvalidName,
    /// A name is longer than 255 bytes.
    NameTooLong,
    /// The file's cluster chain ends before its recorded size: the volume is damaged.
    TruncatedFile,
    /// A seek past the end of the file.
    InvalidPosition,
    /// The volume has no free cluster left (or no free directory slot, in a directory that cannot grow).
    NoSpace,
    /// The path already names a file or directory.
    AlreadyExists,
    /// A directory that must be empty is not.
    NotEmpty,
    /// The file would be larger than the filesystem allows (4 GiB less one byte on FAT32).
    FileTooLarge,
    /// The operation is not allowed on this entry: the root directory, `.` or `..`, a directory where a file is needed.
    InvalidOperation,
}
