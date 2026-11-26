#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error<E> {
    Device(E),
    BadBootSector,
    BadGeometry,
    NoPartition,
    NotFat32,
    NotExFat,
    Unsupported,
    InvalidCluster,
    NotFound,
    NotAFile,
    NotADirectory,
    InvalidName,
    NameTooLong,
    TruncatedFile,
    InvalidPosition,
}
