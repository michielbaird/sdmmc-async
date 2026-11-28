//! Writing exFAT (not built yet: every operation reports `Error::Unsupported`).

use crate::time::Timestamp;
use crate::{BlockDevice, BlockWrite, Error, File};

use super::Volume;

#[derive(Clone, Copy, Debug)]
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

    pub(crate) async fn create_file(
        &mut self,
        _path: &str,
        _now: Timestamp,
    ) -> Result<File, Error<D::Error>> {
        Err(Error::Unsupported)
    }

    pub(crate) async fn create_dir(
        &mut self,
        _path: &str,
        _now: Timestamp,
    ) -> Result<(), Error<D::Error>> {
        Err(Error::Unsupported)
    }

    pub(crate) async fn remove_file(&mut self, _path: &str) -> Result<(), Error<D::Error>> {
        Err(Error::Unsupported)
    }

    pub(crate) async fn remove_dir(&mut self, _path: &str) -> Result<(), Error<D::Error>> {
        Err(Error::Unsupported)
    }

    pub(crate) async fn rename(
        &mut self,
        _from: &str,
        _to: &str,
        _now: Timestamp,
    ) -> Result<(), Error<D::Error>> {
        Err(Error::Unsupported)
    }

    pub(crate) async fn sync(&mut self) -> Result<(), Error<D::Error>> {
        Err(Error::Unsupported)
    }
}
