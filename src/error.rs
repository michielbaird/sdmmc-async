#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error<E> {
    Device(E),
    BadBootSector,
    BadGeometry,
    NoPartition,
    NotFat32,
    NotExFat,
    Unsupported,
    /// The card holds a filesystem this build leaves out (the `fat32` or `exfat` feature is off).
    FilesystemNotEnabled,
    InvalidCluster,
    NotFound,
    NotAFile,
    NotADirectory,
    InvalidName,
    NameTooLong,
    TruncatedFile,
    InvalidPosition,
}
