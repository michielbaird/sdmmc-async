//! Writing exFAT (not built yet: every operation reports `Error::Unsupported`).

use crate::time::Timestamp;
use crate::{BlockDevice, BlockWrite, Error, File};

use super::Volume;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct EntryLoc;

impl<D: BlockDevice + BlockWrite> Volume<D> {
    pub(crate) async fn write_file(
        &mut self,
        _file: &mut File,
        _data: &[u8],
    ) -> Result<usize, Error<D::Error>> {
        Err(Error::Unsupported)
    }

    pub(crate) async fn flush_file(
        &mut self,
        _file: &mut File,
        _now: Timestamp,
    ) -> Result<(), Error<D::Error>> {
        Err(Error::Unsupported)
    }

    pub(crate) async fn set_len(
        &mut self,
        _file: &mut File,
        _length: u64,
    ) -> Result<(), Error<D::Error>> {
        Err(Error::Unsupported)
    }

    pub(crate) async fn sync(&mut self) -> Result<(), Error<D::Error>> {
        Err(Error::Unsupported)
    }
}
