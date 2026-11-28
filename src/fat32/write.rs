//! Writing FAT32: the FAT, the allocator, file data and the directory entries.
//!
//! Every write goes straight to the device (the one-sector cache is updated in step), so there is nothing to lose
//! between calls except what the caller has not flushed. The order of writes is data first, then the FAT chain, then the
//! directory entry that makes the new size visible, so a cut in power loses the last write, not the volume.

use super::{Geometry, Volume, FAT32_EOC};
use crate::time::Timestamp;
use crate::{BlockDevice, BlockWrite, Error, File, BLOCK_SIZE};

const FAT32_END: u32 = 0x0fff_ffff;
/// The clean-shutdown flag in FAT entry 1.
const CLEAN_SHUTDOWN: u32 = 0x0800_0000;

const ATTR_ARCHIVE: u8 = 0x20;
/// The first byte of a deleted directory entry.
pub(super) const SHORT_FREE: u8 = 0xe5;

/// What the writing half of a volume remembers.
pub(super) struct WriterState {
    num_fats: u8,
    fat_size: u32,
    fat_mirrored: bool,
    active_fat: u8,
    fsinfo_sector: u16,
    /// The volume has been marked as not cleanly unmounted.
    dirty: bool,
    /// Where the next search for a free cluster starts.
    next_free: u32,
    /// The free cluster count from the FSInfo sector, when the sector is valid and the count known.
    free_count: Option<u32>,
    /// FSInfo's signatures checked out, so it is written back by `sync`.
    fsinfo_valid: bool,
}

impl WriterState {
    pub(super) fn new(geometry: &Geometry) -> Self {
        Self {
            num_fats: geometry.num_fats,
            fat_size: geometry.fat_size,
            fat_mirrored: geometry.fat_mirrored,
            active_fat: geometry.active_fat,
            fsinfo_sector: geometry.fsinfo_sector,
            dirty: false,
            next_free: 2,
            free_count: None,
            fsinfo_valid: false,
        }
    }
}

impl<D: BlockDevice + BlockWrite> Volume<D> {
    /// Writes a whole sector (relative to the volume) and keeps the read cache in step.
    pub(super) async fn write_sector(
        &mut self,
        sector: u32,
        data: &[u8; BLOCK_SIZE],
    ) -> Result<(), Error<D::Error>> {
        let absolute = self
            .partition_start
            .checked_add(sector)
            .ok_or(Error::BadGeometry)?;
        self.device
            .write_block(absolute, data)
            .await
            .map_err(Error::Device)?;
        self.cache.sector = sector;
        self.cache.valid = true;
        self.cache.data = *data;
        Ok(())
    }

    /// Reads a sector, lets `change` edit it, and writes it back.
    pub(super) async fn modify_sector<R>(
        &mut self,
        sector: u32,
        change: impl FnOnce(&mut [u8; BLOCK_SIZE]) -> R,
    ) -> Result<R, Error<D::Error>> {
        let mut data = *self.sector(sector).await?;
        let result = change(&mut data);
        self.write_sector(sector, &data).await?;
        Ok(result)
    }

    /// Sets a FAT entry in every FAT that is kept up to date. The top four bits of the entry are reserved and preserved.
    pub(super) async fn set_fat_entry(
        &mut self,
        cluster: u32,
        value: u32,
    ) -> Result<(), Error<D::Error>> {
        let offset = cluster.checked_mul(4).ok_or(Error::InvalidCluster)?;
        let within = offset / BLOCK_SIZE as u32;
        let at = (offset % BLOCK_SIZE as u32) as usize;
        let (first, last) = if self.writer.fat_mirrored {
            (0, self.writer.num_fats)
        } else {
            (self.writer.active_fat, self.writer.active_fat + 1)
        };
        for copy in first..last {
            let sector = self
                .fat_start
                .checked_add(u32::from(copy) * self.writer.fat_size)
                .and_then(|start| start.checked_add(within))
                .ok_or(Error::InvalidCluster)?;
            self.modify_sector(sector, |data| {
                let old = u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]);
                let new = (old & 0xf000_0000) | (value & 0x0fff_ffff);
                data[at..at + 4].copy_from_slice(&new.to_le_bytes());
            })
            .await?;
        }
        Ok(())
    }

    /// Marks the volume as in use (clears the clean-shutdown flag) before the first change, and reads FSInfo.
    pub(super) async fn mark_dirty(&mut self) -> Result<(), Error<D::Error>> {
        if self.writer.dirty {
            return Ok(());
        }
        self.load_fsinfo().await?;
        let entry = self.fat_entry(1).await?;
        if entry & CLEAN_SHUTDOWN != 0 {
            self.set_fat_entry(1, entry & !CLEAN_SHUTDOWN).await?;
        }
        self.writer.dirty = true;
        Ok(())
    }

    async fn load_fsinfo(&mut self) -> Result<(), Error<D::Error>> {
        let sector = self.writer.fsinfo_sector;
        if sector == 0 || sector == 0xffff {
            return Ok(());
        }
        let data = *self.sector(u32::from(sector)).await?;
        let lead = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let structure = u32::from_le_bytes([data[484], data[485], data[486], data[487]]);
        let trail = u32::from_le_bytes([data[508], data[509], data[510], data[511]]);
        if lead != 0x4161_5252 || structure != 0x6141_7272 || trail != 0xaa55_0000 {
            return Ok(());
        }
        self.writer.fsinfo_valid = true;
        let free = u32::from_le_bytes([data[488], data[489], data[490], data[491]]);
        let next = u32::from_le_bytes([data[492], data[493], data[494], data[495]]);
        if free <= self.cluster_count {
            self.writer.free_count = Some(free);
        }
        if (2..self.cluster_count + 2).contains(&next) {
            self.writer.next_free = next;
        }
        Ok(())
    }

    /// Writes FSInfo back and marks the volume cleanly unmounted. Call it when done writing.
    pub(crate) async fn sync(&mut self) -> Result<(), Error<D::Error>> {
        if !self.writer.dirty {
            return Ok(());
        }
        if self.writer.fsinfo_valid {
            let sector = u32::from(self.writer.fsinfo_sector);
            let free = self.writer.free_count.unwrap_or(0xffff_ffff);
            let next = self.writer.next_free;
            self.modify_sector(sector, |data| {
                data[488..492].copy_from_slice(&free.to_le_bytes());
                data[492..496].copy_from_slice(&next.to_le_bytes());
            })
            .await?;
        }
        let entry = self.fat_entry(1).await?;
        self.set_fat_entry(1, entry | CLEAN_SHUTDOWN).await?;
        self.writer.dirty = false;
        Ok(())
    }

    /// Takes a free cluster, marks it end-of-chain and, when `after` is given, links that cluster to it.
    pub(super) async fn allocate_cluster(
        &mut self,
        after: Option<u32>,
    ) -> Result<u32, Error<D::Error>> {
        let total = self.cluster_count;
        let start = self.writer.next_free.clamp(2, total + 1);
        for step in 0..total {
            let cluster = 2 + (start - 2 + step) % total;
            if self.fat_entry(cluster).await? != 0 {
                continue;
            }
            self.set_fat_entry(cluster, FAT32_END).await?;
            if let Some(previous) = after {
                self.set_fat_entry(previous, cluster).await?;
            }
            self.writer.next_free = if cluster + 1 < total + 2 {
                cluster + 1
            } else {
                2
            };
            if let Some(free) = self.writer.free_count.as_mut() {
                *free = free.saturating_sub(1);
            }
            return Ok(cluster);
        }
        Err(Error::NoSpace)
    }

    /// Frees a cluster chain, from `start` to its end.
    pub(super) async fn free_chain(&mut self, start: u32) -> Result<(), Error<D::Error>> {
        let mut cluster = start;
        for _ in 0..=self.cluster_count {
            if cluster < 2 || cluster >= self.cluster_count + 2 {
                return Err(Error::InvalidCluster);
            }
            let next = self.fat_entry(cluster).await?;
            self.set_fat_entry(cluster, 0).await?;
            if let Some(free) = self.writer.free_count.as_mut() {
                *free += 1;
            }
            if cluster < self.writer.next_free {
                self.writer.next_free = cluster;
            }
            if next >= FAT32_EOC || next == 0 {
                return Ok(());
            }
            cluster = next;
        }
        Err(Error::InvalidCluster)
    }

    pub(super) fn cluster_bytes(&self) -> u64 {
        u64::from(self.sectors_per_cluster) * BLOCK_SIZE as u64
    }

    /// Makes `file.cluster` the `index`th cluster of the file (counting from 0), walking from the nearest known point.
    async fn walk_to(&mut self, file: &mut File, index: u64) -> Result<(), Error<D::Error>> {
        if u64::from(file.clusters_walked) > index || file.cluster < 2 {
            file.cluster = file.first_cluster;
            file.clusters_walked = 0;
        }
        if file.cluster < 2 {
            return Err(Error::TruncatedFile);
        }
        while u64::from(file.clusters_walked) < index {
            match self.next_cluster(file.cluster).await? {
                Some(next) => {
                    file.cluster = next;
                    file.clusters_walked += 1;
                }
                None => return Err(Error::TruncatedFile),
            }
        }
        Ok(())
    }

    /// Writes `data` at the file's position, growing the file as needed. Returns how many bytes were written, which is
    /// fewer than `data.len()` only when the volume is full or the file reaches 4 GiB less one byte (an error when
    /// nothing at all could be written). The entry is updated by `flush_file`.
    pub(crate) async fn write_file(
        &mut self,
        file: &mut File,
        data: &[u8],
    ) -> Result<usize, Error<D::Error>> {
        if data.is_empty() {
            return Ok(0);
        }
        self.mark_dirty().await?;
        let cluster_bytes = self.cluster_bytes();
        let mut done = 0;
        while done < data.len() {
            if file.cursor >= u64::from(u32::MAX) {
                if done == 0 {
                    return Err(Error::FileTooLarge);
                }
                break;
            }
            let index = file.cursor / cluster_bytes;
            if file.cursor == file.size && file.cursor.is_multiple_of(cluster_bytes) {
                // The next byte starts a cluster the file does not have yet (or one it has past its size).
                let previous = if index == 0 {
                    None
                } else {
                    self.walk_to(file, index - 1).await?;
                    Some(file.cluster)
                };
                let existing = match previous {
                    Some(cluster) => self.next_cluster(cluster).await?,
                    None if file.first_cluster >= 2 => Some(file.first_cluster),
                    None => None,
                };
                let cluster = match existing {
                    Some(cluster) => cluster,
                    None => match self.allocate_cluster(previous).await {
                        Ok(cluster) => cluster,
                        Err(Error::NoSpace) if done > 0 => break,
                        Err(error) => return Err(error),
                    },
                };
                if index == 0 {
                    file.first_cluster = cluster;
                }
                file.cluster = cluster;
                file.clusters_walked = index as u32;
                file.dirty = true;
            } else {
                self.walk_to(file, index).await?;
            }

            let offset = file.cursor % cluster_bytes;
            let sector = self.cluster_sector(file.cluster)? + (offset / BLOCK_SIZE as u64) as u32;
            let within = (offset % BLOCK_SIZE as u64) as usize;
            let take = (BLOCK_SIZE - within).min(data.len() - done);
            let chunk = &data[done..done + take];
            if take == BLOCK_SIZE {
                let mut block = [0u8; BLOCK_SIZE];
                block.copy_from_slice(chunk);
                self.write_sector(sector, &block).await?;
            } else {
                self.modify_sector(sector, |block| {
                    block[within..within + take].copy_from_slice(chunk)
                })
                .await?;
            }
            file.cursor += take as u64;
            done += take;
            if file.cursor > file.size {
                file.size = file.cursor;
            }
            file.dirty = true;
        }
        Ok(done)
    }

    /// Writes the file's size, first cluster and times into its directory entry.
    pub(crate) async fn flush_file(
        &mut self,
        file: &mut File,
        now: Timestamp,
    ) -> Result<(), Error<D::Error>> {
        if !file.dirty {
            return Ok(());
        }
        let loc = file.loc.fat32().ok_or(Error::InvalidOperation)?;
        let size = u32::try_from(file.size).map_err(|_| Error::FileTooLarge)?;
        let first = file.first_cluster;
        let sector = self.cluster_sector(loc.short.cluster)? + loc.short.sector_in_cluster;
        let at = loc.short.entry_in_sector as usize * 32;
        let date = now.fat_date().to_le_bytes();
        let time = now.fat_time().to_le_bytes();
        self.modify_sector(sector, |block| {
            let entry = &mut block[at..at + 32];
            entry[11] |= ATTR_ARCHIVE;
            entry[18..20].copy_from_slice(&date);
            entry[20..22].copy_from_slice(&((first >> 16) as u16).to_le_bytes());
            entry[22..24].copy_from_slice(&time);
            entry[24..26].copy_from_slice(&date);
            entry[26..28].copy_from_slice(&(first as u16).to_le_bytes());
            entry[28..32].copy_from_slice(&size.to_le_bytes());
        })
        .await?;
        file.dirty = false;
        Ok(())
    }

    /// Sets the file's length: shorter frees clusters, longer fills with zeros. The position moves to the new end if it was
    /// past it.
    pub(crate) async fn set_len(
        &mut self,
        file: &mut File,
        length: u64,
    ) -> Result<(), Error<D::Error>> {
        if length > u64::from(u32::MAX) {
            return Err(Error::FileTooLarge);
        }
        if length == file.size {
            return Ok(());
        }
        self.mark_dirty().await?;
        let position = file.cursor;
        if length > file.size {
            file.cursor = file.size;
            let zeros = [0u8; BLOCK_SIZE];
            while file.size < length {
                let take = (length - file.size).min(BLOCK_SIZE as u64) as usize;
                if self.write_file(file, &zeros[..take]).await? == 0 {
                    return Err(Error::NoSpace);
                }
            }
        } else {
            let cluster_bytes = self.cluster_bytes();
            let keep = length.div_ceil(cluster_bytes);
            if keep == 0 {
                if file.first_cluster >= 2 {
                    self.free_chain(file.first_cluster).await?;
                }
                file.first_cluster = 0;
            } else {
                file.cluster = file.first_cluster;
                file.clusters_walked = 0;
                self.walk_to(file, keep - 1).await?;
                let tail = self.fat_entry(file.cluster).await?;
                self.set_fat_entry(file.cluster, FAT32_END).await?;
                if (2..FAT32_EOC).contains(&tail) {
                    self.free_chain(tail).await?;
                }
            }
            file.size = length;
        }
        file.dirty = true;
        // A cluster the file had may be gone: start the position over from a cluster it certainly has.
        file.cluster = file.first_cluster;
        file.clusters_walked = 0;
        file.cursor = position.min(file.size);
        if file.cursor < file.size {
            let target = file.cursor;
            file.cursor = 0;
            self.seek(file, target).await?;
        }
        Ok(())
    }
}
