//! Writing exFAT: the allocation bitmap, FAT chains, entry sets and file data.
//!
//! Writes go straight to the device, data first, then the allocation, then the directory entry set that makes the change
//! visible. An entry set carries a checksum, so a set cut off half way is ignored by every reader rather than half
//! believed.

use super::{Directory, Volume, ATTR_DIRECTORY, ENTRY_FILE, ENTRY_NAME, ENTRY_STREAM};
use crate::name::{is_valid_name, MAX_NAME};
use crate::time::Timestamp;
use crate::{BlockDevice, BlockWrite, Error, File, BLOCK_SIZE};

const EXFAT_END: u32 = 0xffff_ffff;
const ATTR_ARCHIVE: u16 = 0x20;
const ENTRY_BITMAP: u8 = 0x81;
const ENTRY_UPCASE: u8 = 0x82;
/// The stream extension's flags.
const FLAG_ALLOCATION_POSSIBLE: u8 = 0x01;
const FLAG_NO_FAT_CHAIN: u8 = 0x02;
/// In the boot sector's volume flags.
const VOLUME_DIRTY: u16 = 0x0002;
const FLAGS_OFFSET: usize = 106;

const MAX_NAME_ENTRIES: usize = MAX_NAME.div_ceil(15);
/// The file entry, the stream extension and the name entries.
const MAX_SET: usize = 2 + MAX_NAME_ENTRIES;

/// Where an entry sits in a directory.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Pos {
    cluster: u32,
    sector_in_cluster: u32,
    entry_in_sector: u32,
}

impl Pos {
    pub(super) fn of(dir: &Directory) -> Self {
        Self {
            cluster: dir.cluster,
            sector_in_cluster: dir.sector_in_cluster,
            entry_in_sector: dir.entry_in_sector,
        }
    }
}

/// Where an entry set sits: its first entry, how many entries it has, and what is needed to walk on from it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EntryLoc {
    pub(super) pos: Pos,
    pub(super) count: u8,
    pub(super) dir_start: u32,
    pub(super) dir_no_fat_chain: bool,
}

impl EntryLoc {
    /// For entries nobody will update (read-only use).
    pub(super) const NONE: EntryLoc = EntryLoc {
        pos: Pos {
            cluster: 0,
            sector_in_cluster: 0,
            entry_in_sector: 0,
        },
        count: 0,
        dir_start: 0,
        dir_no_fat_chain: false,
    };

    /// A cursor on the first entry of the set that can step on through the directory's clusters.
    fn cursor(&self) -> Directory {
        let mut dir = Directory::new(self.dir_start, u64::MAX, self.dir_no_fat_chain);
        dir.cluster = self.pos.cluster;
        dir.sector_in_cluster = self.pos.sector_in_cluster;
        dir.entry_in_sector = self.pos.entry_in_sector;
        dir
    }
}

/// A file made of clusters that are not in the FAT chain... or of the clusters of a system file (bitmap, upcase table).
#[derive(Clone, Copy)]
struct Region {
    first: u32,
    length: u64,
}

pub(super) struct WriterState {
    /// The volume was marked in use by this mount.
    dirty: bool,
    /// It was already marked in use when this mount began (a previous writer did not finish): `sync` leaves it so.
    was_dirty: bool,
    /// The system files, found in the root directory at the first write.
    bitmap: Option<Region>,
    upcase: Option<Region>,
    /// Where the next search for a free cluster starts.
    next_free: u32,
}

impl WriterState {
    pub(super) fn new() -> Self {
        Self {
            dirty: false,
            was_dirty: false,
            bitmap: None,
            upcase: None,
            next_free: 2,
        }
    }
}

/// The checksum over an entry set, which skips the checksum field itself.
fn set_checksum(entries: &[[u8; 32]]) -> u16 {
    let mut checksum = 0u16;
    for (entry_index, entry) in entries.iter().enumerate() {
        for (index, &byte) in entry.iter().enumerate() {
            if entry_index == 0 && (index == 2 || index == 3) {
                continue;
            }
            checksum = checksum.rotate_right(1).wrapping_add(u16::from(byte));
        }
    }
    checksum
}

/// exFAT's 32-bit timestamp: date in the top half, time (two-second resolution) in the bottom.
fn exfat_timestamp(now: Timestamp) -> [u8; 4] {
    ((u32::from(now.fat_date()) << 16) | u32::from(now.fat_time())).to_le_bytes()
}

fn upcase_ascii(unit: u16) -> u16 {
    if (u16::from(b'a')..=u16::from(b'z')).contains(&unit) {
        unit - 32
    } else {
        unit
    }
}

/// What a new file or directory starts with.
struct Allocation {
    attributes: u16,
    first_cluster: u32,
    length: u64,
    flags: u8,
}

/// Everything about a new entry set.
struct NewSet<'a> {
    units: &'a [u16],
    name_hash: u16,
    attributes: u16,
    first_cluster: u32,
    length: u64,
    flags: u8,
    now: Timestamp,
}

fn build_set(new: &NewSet<'_>, out: &mut [[u8; 32]; MAX_SET]) -> usize {
    let name_entries = new.units.len().div_ceil(15);
    let count = 2 + name_entries;
    for entry in out.iter_mut().take(count) {
        *entry = [0; 32];
    }
    let stamp = exfat_timestamp(new.now);
    let file = &mut out[0];
    file[0] = ENTRY_FILE;
    file[1] = (1 + name_entries) as u8;
    file[4..6].copy_from_slice(&new.attributes.to_le_bytes());
    file[8..12].copy_from_slice(&stamp);
    file[12..16].copy_from_slice(&stamp);
    file[16..20].copy_from_slice(&stamp);
    let stream = &mut out[1];
    stream[0] = ENTRY_STREAM;
    stream[1] = new.flags;
    stream[3] = new.units.len() as u8;
    stream[4..6].copy_from_slice(&new.name_hash.to_le_bytes());
    stream[8..16].copy_from_slice(&new.length.to_le_bytes());
    stream[20..24].copy_from_slice(&new.first_cluster.to_le_bytes());
    stream[24..32].copy_from_slice(&new.length.to_le_bytes());
    for (index, chunk) in new.units.chunks(15).enumerate() {
        let entry = &mut out[2 + index];
        entry[0] = ENTRY_NAME;
        for (unit_index, unit) in chunk.iter().enumerate() {
            entry[2 + unit_index * 2..4 + unit_index * 2].copy_from_slice(&unit.to_le_bytes());
        }
    }
    let checksum = set_checksum(&out[..count]);
    out[0][2..4].copy_from_slice(&checksum.to_le_bytes());
    count
}

fn encode_name(name: &str, units: &mut [u16; MAX_NAME]) -> usize {
    let mut length = 0;
    for unit in name.encode_utf16() {
        units[length] = unit;
        length += 1;
    }
    length
}

impl<D: BlockDevice + BlockWrite> Volume<D> {
    fn cluster_bytes(&self) -> u64 {
        (BLOCK_SIZE as u64) << self.sectors_per_cluster_shift
    }

    async fn write_sector(
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

    async fn modify_sector<R>(
        &mut self,
        sector: u32,
        change: impl FnOnce(&mut [u8; BLOCK_SIZE]) -> R,
    ) -> Result<R, Error<D::Error>> {
        let mut data = *self.sector(sector).await?;
        let result = change(&mut data);
        self.write_sector(sector, &data).await?;
        Ok(result)
    }

    async fn set_fat_entry(&mut self, cluster: u32, value: u32) -> Result<(), Error<D::Error>> {
        let offset = cluster.checked_mul(4).ok_or(Error::InvalidCluster)?;
        let sector = self
            .fat_start
            .checked_add(offset / BLOCK_SIZE as u32)
            .ok_or(Error::InvalidCluster)?;
        let at = (offset % BLOCK_SIZE as u32) as usize;
        self.modify_sector(sector, |data| {
            data[at..at + 4].copy_from_slice(&value.to_le_bytes());
        })
        .await
    }

    /// Marks the volume in use before the first change, and finds the allocation bitmap and upcase table.
    async fn mark_dirty(&mut self) -> Result<(), Error<D::Error>> {
        if self.writer.dirty {
            return Ok(());
        }
        self.load_system_files().await?;
        let flags = {
            let data = self.sector(0).await?;
            u16::from_le_bytes([data[FLAGS_OFFSET], data[FLAGS_OFFSET + 1]])
        };
        self.writer.was_dirty = flags & VOLUME_DIRTY != 0;
        if !self.writer.was_dirty {
            self.modify_sector(0, |data| {
                let flags = flags | VOLUME_DIRTY;
                data[FLAGS_OFFSET..FLAGS_OFFSET + 2].copy_from_slice(&flags.to_le_bytes());
            })
            .await?;
        }
        self.writer.dirty = true;
        Ok(())
    }

    async fn load_system_files(&mut self) -> Result<(), Error<D::Error>> {
        let mut root = self.open_root();
        while let Some(raw) = self.next_raw(&mut root).await? {
            match raw[0] {
                0x00 => break,
                ENTRY_BITMAP => {
                    self.writer.bitmap = Some(Region {
                        first: u32::from_le_bytes([raw[20], raw[21], raw[22], raw[23]]),
                        length: u64::from_le_bytes(raw[24..32].try_into().unwrap()),
                    });
                }
                ENTRY_UPCASE => {
                    self.writer.upcase = Some(Region {
                        first: u32::from_le_bytes([raw[20], raw[21], raw[22], raw[23]]),
                        length: u64::from_le_bytes(raw[24..32].try_into().unwrap()),
                    });
                }
                _ => {}
            }
        }
        if self.writer.bitmap.is_none() {
            return Err(Error::BadGeometry);
        }
        Ok(())
    }

    /// Marks the volume cleanly unmounted (unless an earlier writer left it marked in use). Call it when done writing.
    pub(crate) async fn sync(&mut self) -> Result<(), Error<D::Error>> {
        if !self.writer.dirty {
            return Ok(());
        }
        if !self.writer.was_dirty {
            self.modify_sector(0, |data| {
                let flags = u16::from_le_bytes([data[FLAGS_OFFSET], data[FLAGS_OFFSET + 1]]);
                let flags = flags & !VOLUME_DIRTY;
                data[FLAGS_OFFSET..FLAGS_OFFSET + 2].copy_from_slice(&flags.to_le_bytes());
            })
            .await?;
        }
        self.writer.dirty = false;
        Ok(())
    }

    /// The `index`th cluster of a system file: contiguous unless the FAT chains it.
    async fn region_cluster(&mut self, region: Region, index: u64) -> Result<u32, Error<D::Error>> {
        let chained = self.fat_entry(region.first).await? != 0;
        if !chained {
            return u32::try_from(u64::from(region.first) + index)
                .map_err(|_| Error::InvalidCluster);
        }
        let mut cluster = region.first;
        for _ in 0..index {
            cluster = self
                .next_cluster(cluster)
                .await?
                .ok_or(Error::InvalidCluster)?;
        }
        Ok(cluster)
    }

    /// The sector, byte and mask holding the allocation bit of a cluster.
    async fn bitmap_bit(&mut self, cluster: u32) -> Result<(u32, usize, u8), Error<D::Error>> {
        let region = self.writer.bitmap.ok_or(Error::BadGeometry)?;
        let bit = u64::from(cluster.checked_sub(2).ok_or(Error::InvalidCluster)?);
        let byte = bit / 8;
        if byte >= region.length {
            return Err(Error::InvalidCluster);
        }
        let in_region = self.cluster_bytes();
        let holder = self.region_cluster(region, byte / in_region).await?;
        let within = byte % in_region;
        let sector = self.cluster_sector(holder)? + (within / BLOCK_SIZE as u64) as u32;
        Ok((
            sector,
            (within % BLOCK_SIZE as u64) as usize,
            1 << (bit % 8),
        ))
    }

    async fn is_allocated(&mut self, cluster: u32) -> Result<bool, Error<D::Error>> {
        let (sector, at, mask) = self.bitmap_bit(cluster).await?;
        Ok(self.sector(sector).await?[at] & mask != 0)
    }

    async fn set_allocated(
        &mut self,
        cluster: u32,
        allocated: bool,
    ) -> Result<(), Error<D::Error>> {
        let (sector, at, mask) = self.bitmap_bit(cluster).await?;
        self.modify_sector(sector, |data| {
            if allocated {
                data[at] |= mask;
            } else {
                data[at] &= !mask;
            }
        })
        .await
    }

    /// Takes a free cluster in the bitmap.
    async fn allocate(&mut self) -> Result<u32, Error<D::Error>> {
        let total = self.cluster_count;
        let start = self.writer.next_free.clamp(2, total + 1);
        for step in 0..total {
            let cluster = 2 + (start - 2 + step) % total;
            if !self.is_allocated(cluster).await? {
                self.set_allocated(cluster, true).await?;
                self.writer.next_free = if cluster + 1 < total + 2 {
                    cluster + 1
                } else {
                    2
                };
                return Ok(cluster);
            }
        }
        Err(Error::NoSpace)
    }

    async fn release(&mut self, cluster: u32) -> Result<(), Error<D::Error>> {
        self.set_allocated(cluster, false).await?;
        if cluster < self.writer.next_free {
            self.writer.next_free = cluster;
        }
        Ok(())
    }

    /// Adds a cluster after `last` (the file's last, or none for an empty file). Returns the new cluster and whether the
    /// file still has no FAT chain: one stays contiguous when the next cluster is free, otherwise it gets a FAT chain,
    /// written for the clusters it already has (`count`).
    async fn extend_chain(
        &mut self,
        first: u32,
        last: Option<u32>,
        count: u64,
        no_fat_chain: bool,
    ) -> Result<(u32, bool), Error<D::Error>> {
        let Some(last) = last else {
            let cluster = self.allocate().await?;
            self.set_fat_entry(cluster, EXFAT_END).await?;
            return Ok((cluster, false));
        };
        if no_fat_chain {
            let next = last + 1;
            if u64::from(next) < u64::from(self.cluster_count) + 2
                && !self.is_allocated(next).await?
            {
                self.set_allocated(next, true).await?;
                return Ok((next, true));
            }
            // Out of room next door: from here on the file is a chain.
            let cluster = self.allocate().await?;
            self.set_fat_entry(cluster, EXFAT_END).await?;
            for index in 0..count.saturating_sub(1) {
                let this = first + index as u32;
                self.set_fat_entry(this, this + 1).await?;
            }
            self.set_fat_entry(last, cluster).await?;
            return Ok((cluster, false));
        }
        let cluster = self.allocate().await?;
        self.set_fat_entry(cluster, EXFAT_END).await?;
        self.set_fat_entry(last, cluster).await?;
        Ok((cluster, false))
    }

    /// Frees all the clusters of a file or directory of `length` bytes.
    async fn free_chain(
        &mut self,
        first: u32,
        no_fat_chain: bool,
        length: u64,
    ) -> Result<(), Error<D::Error>> {
        if no_fat_chain {
            let count = length.div_ceil(self.cluster_bytes());
            for index in 0..count {
                self.release(first + index as u32).await?;
            }
            return Ok(());
        }
        let mut cluster = first;
        for _ in 0..=self.cluster_count {
            let next = self.next_cluster(cluster).await?;
            self.set_fat_entry(cluster, 0).await?;
            self.release(cluster).await?;
            match next {
                Some(next) => cluster = next,
                None => return Ok(()),
            }
        }
        Err(Error::InvalidCluster)
    }

    async fn walk_to(&mut self, file: &mut File, index: u64) -> Result<(), Error<D::Error>> {
        if file.first_cluster < 2 {
            return Err(Error::TruncatedFile);
        }
        if file.no_fat_chain {
            file.cluster = u32::try_from(u64::from(file.first_cluster) + index)
                .map_err(|_| Error::InvalidCluster)?;
            file.clusters_walked = index as u32;
            return Ok(());
        }
        if u64::from(file.clusters_walked) > index || file.cluster < 2 {
            file.cluster = file.first_cluster;
            file.clusters_walked = 0;
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

    /// Writes `data` at the file's position, growing the file as needed; returns the bytes written. The entry is updated
    /// by `flush_file`.
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
            let index = file.cursor / cluster_bytes;
            if file.cursor == file.size && file.cursor.is_multiple_of(cluster_bytes) {
                let last = if index == 0 {
                    None
                } else {
                    self.walk_to(file, index - 1).await?;
                    Some(file.cluster)
                };
                let (cluster, no_fat_chain) = match self
                    .extend_chain(file.first_cluster, last, index, file.no_fat_chain)
                    .await
                {
                    Ok(extended) => extended,
                    Err(Error::NoSpace) if done > 0 => break,
                    Err(error) => return Err(error),
                };
                if index == 0 {
                    file.first_cluster = cluster;
                }
                file.no_fat_chain = no_fat_chain;
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

    /// Reads an entry set into `out`.
    async fn read_set(
        &mut self,
        loc: &EntryLoc,
        out: &mut [[u8; 32]; MAX_SET],
    ) -> Result<usize, Error<D::Error>> {
        let count = loc.count as usize;
        if !(3..=MAX_SET).contains(&count) {
            return Err(Error::InvalidOperation);
        }
        let mut cursor = loc.cursor();
        for entry in out.iter_mut().take(count) {
            *entry = self
                .next_raw(&mut cursor)
                .await?
                .ok_or(Error::InvalidCluster)?;
        }
        Ok(count)
    }

    /// Writes entries over the ones starting at `cursor`, one write per sector. Returns whether it overwrote the
    /// end-of-directory marker.
    async fn write_run(
        &mut self,
        mut cursor: Directory,
        entries: &[[u8; 32]],
    ) -> Result<bool, Error<D::Error>> {
        let mut ended = false;
        let mut index = 0;
        while index < entries.len() {
            let sector = self.cluster_sector(cursor.cluster)? + cursor.sector_in_cluster;
            let mut block = *self.sector(sector).await?;
            loop {
                let offset = cursor.entry_in_sector as usize * 32;
                if block[offset] == 0x00 {
                    ended = true;
                }
                block[offset..offset + 32].copy_from_slice(&entries[index]);
                index += 1;
                // Step on, to the next entry or the next sector.
                let (cluster, sector_in_cluster) = (cursor.cluster, cursor.sector_in_cluster);
                self.advance(&mut cursor).await?;
                if index == entries.len() {
                    break;
                }
                if cursor.finished {
                    return Err(Error::InvalidCluster);
                }
                if cursor.cluster != cluster || cursor.sector_in_cluster != sector_in_cluster {
                    break;
                }
            }
            self.write_sector(sector, &block).await?;
        }
        if ended && !cursor.finished {
            // The entry after the set must still end the directory.
            let sector = self.cluster_sector(cursor.cluster)? + cursor.sector_in_cluster;
            let offset = cursor.entry_in_sector as usize * 32;
            if self.sector(sector).await?[offset] != 0x00 {
                self.modify_sector(sector, |block| block[offset] = 0x00)
                    .await?;
            }
        }
        Ok(ended)
    }

    /// Changes an entry set in place and rewrites its checksum.
    async fn modify_set(
        &mut self,
        loc: &EntryLoc,
        change: impl FnOnce(&mut [[u8; 32]]),
    ) -> Result<(), Error<D::Error>> {
        let mut set = [[0u8; 32]; MAX_SET];
        let count = self.read_set(loc, &mut set).await?;
        change(&mut set[..count]);
        let checksum = set_checksum(&set[..count]);
        set[0][2..4].copy_from_slice(&checksum.to_le_bytes());
        self.write_run(loc.cursor(), &set[..count]).await?;
        Ok(())
    }

    /// Writes the file's size, first cluster, flags and times into its entry set.
    pub(crate) async fn flush_file(
        &mut self,
        file: &mut File,
        now: Timestamp,
    ) -> Result<(), Error<D::Error>> {
        if !file.dirty {
            return Ok(());
        }
        let loc = file.loc.exfat().ok_or(Error::InvalidOperation)?;
        let stamp = exfat_timestamp(now);
        let (size, first, no_fat_chain) = (file.size, file.first_cluster, file.no_fat_chain);
        self.modify_set(&loc, |set| {
            let attributes = u16::from_le_bytes([set[0][4], set[0][5]]) | ATTR_ARCHIVE;
            set[0][4..6].copy_from_slice(&attributes.to_le_bytes());
            set[0][12..16].copy_from_slice(&stamp);
            set[0][16..20].copy_from_slice(&stamp);
            let stream = &mut set[1];
            stream[1] = FLAG_ALLOCATION_POSSIBLE | if no_fat_chain { FLAG_NO_FAT_CHAIN } else { 0 };
            stream[8..16].copy_from_slice(&size.to_le_bytes());
            stream[20..24].copy_from_slice(&first.to_le_bytes());
            stream[24..32].copy_from_slice(&size.to_le_bytes());
        })
        .await?;
        file.dirty = false;
        Ok(())
    }

    /// Sets the file's length: shorter frees clusters, longer fills with zeros.
    pub(crate) async fn set_len(
        &mut self,
        file: &mut File,
        length: u64,
    ) -> Result<(), Error<D::Error>> {
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
            let have = file.size.div_ceil(cluster_bytes);
            if keep == 0 {
                if file.first_cluster >= 2 {
                    self.free_chain(file.first_cluster, file.no_fat_chain, file.size)
                        .await?;
                }
                file.first_cluster = 0;
                file.no_fat_chain = false;
            } else if file.no_fat_chain {
                for index in keep..have {
                    self.release(file.first_cluster + index as u32).await?;
                }
            } else {
                file.cluster = file.first_cluster;
                file.clusters_walked = 0;
                self.walk_to(file, keep - 1).await?;
                let tail = self.next_cluster(file.cluster).await?;
                self.set_fat_entry(file.cluster, EXFAT_END).await?;
                if let Some(tail) = tail {
                    self.free_chain(tail, false, 0).await?;
                }
            }
            file.size = length;
        }
        file.dirty = true;
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

    // ---- names --------------------------------------------------------------------------------------------------

    /// The upper-case form of a UTF-16 unit, through the volume's table (ASCII needs no lookup).
    async fn upcase(&mut self, unit: u16) -> Result<u16, Error<D::Error>> {
        if unit < 0x80 {
            return Ok(upcase_ascii(unit));
        }
        let Some(table) = self.writer.upcase else {
            return Ok(unit);
        };
        // The table is a list of upper-case units by code, where 0xffff followed by n means n units map to themselves.
        let mut code = 0u32;
        let mut position = 0u64;
        let words = table.length / 2;
        let cluster_bytes = self.cluster_bytes();
        let mut pending_skip = false;
        while position < words {
            let byte = position * 2;
            let holder = self.region_cluster(table, byte / cluster_bytes).await?;
            let within = byte % cluster_bytes;
            let sector = self.cluster_sector(holder)? + (within / BLOCK_SIZE as u64) as u32;
            let at = (within % BLOCK_SIZE as u64) as usize;
            let data = self.sector(sector).await?;
            let value = u16::from_le_bytes([data[at], data[at + 1]]);
            position += 1;
            if pending_skip {
                code += u32::from(value);
                pending_skip = false;
                if code > u32::from(unit) {
                    return Ok(unit);
                }
            } else if value == 0xffff {
                pending_skip = true;
            } else {
                if code == u32::from(unit) {
                    return Ok(value);
                }
                code += 1;
            }
        }
        Ok(unit)
    }

    async fn name_hash(&mut self, units: &[u16]) -> Result<u16, Error<D::Error>> {
        let mut hash = 0u16;
        for &unit in units {
            let upper = self.upcase(unit).await?;
            for byte in upper.to_le_bytes() {
                hash = hash.rotate_right(1).wrapping_add(u16::from(byte));
            }
        }
        Ok(hash)
    }

    // ---- directories --------------------------------------------------------------------------------------------

    async fn zero_cluster(&mut self, cluster: u32) -> Result<(), Error<D::Error>> {
        let first = self.cluster_sector(cluster)?;
        let zeros = [0u8; BLOCK_SIZE];
        for sector in 0..1u32 << self.sectors_per_cluster_shift {
            self.write_sector(first + sector, &zeros).await?;
        }
        Ok(())
    }

    /// Adds a zeroed cluster to the end of a directory (whose cursor is on its last cluster), and records the new length in
    /// the directory's own entry. Returns the new cluster and whether the directory still has no FAT chain.
    async fn grow_directory(&mut self, dir: &Directory) -> Result<(u32, bool), Error<D::Error>> {
        let have = u64::from(dir.clusters_walked) + 1;
        let (cluster, no_fat_chain) = self
            .extend_chain(dir.start_cluster, Some(dir.cluster), have, dir.no_fat_chain)
            .await?;
        self.zero_cluster(cluster).await?;
        if let Some(loc) = dir.entry {
            let length = (have + 1) * self.cluster_bytes();
            self.modify_set(&loc, |set| {
                let stream = &mut set[1];
                stream[1] =
                    FLAG_ALLOCATION_POSSIBLE | if no_fat_chain { FLAG_NO_FAT_CHAIN } else { 0 };
                stream[8..16].copy_from_slice(&length.to_le_bytes());
                stream[24..32].copy_from_slice(&length.to_le_bytes());
            })
            .await?;
        }
        Ok((cluster, no_fat_chain))
    }

    /// Finds `count` consecutive free entries in a directory (given as a cursor at its start), adding clusters when it
    /// is full. Returns a cursor at the first of them, free to step on through the directory.
    async fn find_free_run(
        &mut self,
        directory: Directory,
        count: usize,
    ) -> Result<Directory, Error<D::Error>> {
        let mut at = directory;
        let mut run_start = at;
        let mut run = 0usize;
        let mut after_end = false;
        let per_cluster = (1usize << self.sectors_per_cluster_shift) * BLOCK_SIZE / 32;
        loop {
            let sector = self.cluster_sector(at.cluster)? + at.sector_in_cluster;
            let first_byte = self.sector(sector).await?[at.entry_in_sector as usize * 32];
            // An entry whose in-use bit is clear is free, and everything after the first 0x00 is.
            let free = after_end || first_byte & 0x80 == 0;
            if first_byte == 0x00 {
                after_end = true;
            }
            if free {
                if run == 0 {
                    run_start = at;
                }
                run += 1;
                if run == count {
                    return Ok(Self::unbounded(run_start));
                }
            } else {
                run = 0;
            }
            self.advance(&mut at).await?;
            if !at.finished {
                continue;
            }
            // Out of directory: the rest of the run goes in new clusters.
            loop {
                let (cluster, no_fat_chain) = self.grow_directory(&at).await?;
                if !no_fat_chain && at.no_fat_chain {
                    // The directory got a FAT chain: from here on every cursor on it follows that.
                    run_start.no_fat_chain = false;
                }
                let mut grown = at;
                grown.cluster = cluster;
                grown.sector_in_cluster = 0;
                grown.entry_in_sector = 0;
                grown.clusters_walked += 1;
                grown.no_fat_chain = no_fat_chain;
                grown.finished = false;
                grown.remaining = if at.remaining == u64::MAX {
                    u64::MAX
                } else {
                    per_cluster as u64 * 32
                };
                if run == 0 {
                    run_start = grown;
                }
                run += per_cluster;
                if run >= count {
                    run_start.no_fat_chain = no_fat_chain && run_start.no_fat_chain;
                    return Ok(Self::unbounded(run_start));
                }
                at = grown;
                // `at` now stands on the new cluster; step to its last entry and let the grow loop add another.
                at.sector_in_cluster = (1u32 << self.sectors_per_cluster_shift) - 1;
                at.entry_in_sector = BLOCK_SIZE as u32 / 32 - 1;
            }
        }
    }

    fn unbounded(mut dir: Directory) -> Directory {
        dir.remaining = u64::MAX;
        dir.finished = false;
        dir
    }

    /// Adds an entry set to a directory and returns where it landed.
    async fn add_entry(
        &mut self,
        directory: &Directory,
        name: &str,
        new: Allocation,
        now: Timestamp,
    ) -> Result<EntryLoc, Error<D::Error>> {
        let mut units = [0u16; MAX_NAME];
        let unit_count = encode_name(name, &mut units);
        let name_hash = self.name_hash(&units[..unit_count]).await?;
        let new = NewSet {
            units: &units[..unit_count],
            name_hash,
            attributes: new.attributes,
            first_cluster: new.first_cluster,
            length: new.length,
            flags: new.flags,
            now,
        };
        let mut set = [[0u8; 32]; MAX_SET];
        let count = build_set(&new, &mut set);
        self.place_set(directory, &set[..count]).await
    }

    /// Writes a finished entry set into the first free run of a directory.
    async fn place_set(
        &mut self,
        directory: &Directory,
        set: &[[u8; 32]],
    ) -> Result<EntryLoc, Error<D::Error>> {
        let first = self.find_free_run(*directory, set.len()).await?;
        self.write_run(first, set).await?;
        Ok(EntryLoc {
            pos: Pos::of(&first),
            count: set.len() as u8,
            dir_start: first.start_cluster,
            dir_no_fat_chain: first.no_fat_chain,
        })
    }

    /// Splits a path into the directory that holds it and the last name.
    async fn resolve_parent<'p>(
        &mut self,
        path: &'p str,
        forbidden: Option<u32>,
    ) -> Result<(Directory, &'p str), Error<D::Error>> {
        let mut parts = path
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .peekable();
        let mut directory = self.open_root();
        let mut last = None;
        while let Some(part) = parts.next() {
            if part == ".." {
                return Err(Error::InvalidName);
            }
            if parts.peek().is_some() {
                let entry = self.find_in(&directory, part).await?;
                if !entry.is_directory {
                    return Err(Error::NotADirectory);
                }
                if forbidden == Some(entry.first_cluster) {
                    return Err(Error::InvalidOperation);
                }
                directory = Directory::new(entry.first_cluster, entry.size, entry.no_fat_chain);
                directory.entry = entry.loc.exfat();
            } else {
                last = Some(part);
            }
        }
        Ok((directory, last.ok_or(Error::InvalidName)?))
    }

    async fn require_absent(
        &mut self,
        directory: &Directory,
        name: &str,
    ) -> Result<(), Error<D::Error>> {
        match self.find_in(directory, name).await {
            Ok(_) => Err(Error::AlreadyExists),
            Err(Error::NotFound) => Ok(()),
            Err(other) => Err(other),
        }
    }

    pub(crate) async fn create_file(
        &mut self,
        path: &str,
        now: Timestamp,
    ) -> Result<File, Error<D::Error>> {
        let (directory, name) = self.resolve_parent(path, None).await?;
        if !is_valid_name(name) {
            return Err(Error::InvalidName);
        }
        self.require_absent(&directory, name).await?;
        self.mark_dirty().await?;
        let loc = self
            .add_entry(
                &directory,
                name,
                Allocation {
                    attributes: ATTR_ARCHIVE,
                    first_cluster: 0,
                    length: 0,
                    flags: FLAG_ALLOCATION_POSSIBLE,
                },
                now,
            )
            .await?;
        Ok(File {
            first_cluster: 0,
            cluster: 0,
            size: 0,
            cursor: 0,
            clusters_walked: 0,
            no_fat_chain: false,
            loc: crate::fs::EntryLoc::ExFat(loc),
            dirty: false,
        })
    }

    pub(crate) async fn create_dir(
        &mut self,
        path: &str,
        now: Timestamp,
    ) -> Result<(), Error<D::Error>> {
        let (parent, name) = self.resolve_parent(path, None).await?;
        if !is_valid_name(name) {
            return Err(Error::InvalidName);
        }
        self.require_absent(&parent, name).await?;
        self.mark_dirty().await?;
        let cluster = self.allocate().await?;
        self.set_fat_entry(cluster, EXFAT_END).await?;
        self.zero_cluster(cluster).await?;
        let length = self.cluster_bytes();
        if let Err(error) = self
            .add_entry(
                &parent,
                name,
                Allocation {
                    attributes: ATTR_DIRECTORY,
                    first_cluster: cluster,
                    length,
                    flags: FLAG_ALLOCATION_POSSIBLE,
                },
                now,
            )
            .await
        {
            let _ = self.free_chain(cluster, false, 0).await;
            return Err(error);
        }
        Ok(())
    }

    /// Marks every entry of a set not in use.
    async fn delete_set(&mut self, loc: &EntryLoc) -> Result<(), Error<D::Error>> {
        let mut set = [[0u8; 32]; MAX_SET];
        let count = self.read_set(loc, &mut set).await?;
        for entry in set.iter_mut().take(count) {
            entry[0] &= 0x7f;
        }
        self.write_run(loc.cursor(), &set[..count]).await?;
        Ok(())
    }

    pub(crate) async fn remove_file(&mut self, path: &str) -> Result<(), Error<D::Error>> {
        let entry = self.find_path(path).await?;
        if entry.is_directory {
            return Err(Error::NotAFile);
        }
        let loc = entry.loc.exfat().ok_or(Error::InvalidOperation)?;
        self.mark_dirty().await?;
        self.delete_set(&loc).await?;
        if entry.first_cluster >= 2 {
            self.free_chain(entry.first_cluster, entry.no_fat_chain, entry.size)
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn remove_dir(&mut self, path: &str) -> Result<(), Error<D::Error>> {
        let entry = self.find_path(path).await?;
        if !entry.is_directory {
            return Err(Error::NotADirectory);
        }
        let loc = entry.loc.exfat().ok_or(Error::InvalidOperation)?;
        let mut contents = Directory::new(entry.first_cluster, entry.size, entry.no_fat_chain);
        if self.next_entry(&mut contents).await?.is_some() {
            return Err(Error::NotEmpty);
        }
        self.mark_dirty().await?;
        self.delete_set(&loc).await?;
        if entry.first_cluster >= 2 {
            self.free_chain(entry.first_cluster, entry.no_fat_chain, entry.size)
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn rename(
        &mut self,
        from: &str,
        to: &str,
        now: Timestamp,
    ) -> Result<(), Error<D::Error>> {
        let entry = self.find_path(from).await?;
        let old = entry.loc.exfat().ok_or(Error::InvalidOperation)?;
        let forbidden = entry.is_directory.then_some(entry.first_cluster);
        let (directory, name) = self.resolve_parent(to, forbidden).await?;
        if !is_valid_name(name) {
            return Err(Error::InvalidName);
        }
        match self.find_in(&directory, name).await {
            Ok(existing) => {
                let same = existing.loc.exfat().is_some_and(|other| {
                    other.pos.cluster == old.pos.cluster
                        && other.pos.sector_in_cluster == old.pos.sector_in_cluster
                        && other.pos.entry_in_sector == old.pos.entry_in_sector
                });
                if !same {
                    return Err(Error::AlreadyExists);
                }
            }
            Err(Error::NotFound) => {}
            Err(other) => return Err(other),
        }
        self.mark_dirty().await?;
        let mut set = [[0u8; 32]; MAX_SET];
        self.read_set(&old, &mut set).await?;
        let mut units = [0u16; MAX_NAME];
        let unit_count = encode_name(name, &mut units);
        let name_hash = self.name_hash(&units[..unit_count]).await?;
        // The same file under a new name: keep the attributes, times and the stream's allocation.
        let mut new = [[0u8; 32]; MAX_SET];
        let new_count = build_set(
            &NewSet {
                units: &units[..unit_count],
                name_hash,
                attributes: u16::from_le_bytes([set[0][4], set[0][5]]),
                first_cluster: 0,
                length: 0,
                flags: 0,
                now,
            },
            &mut new,
        );
        new[0][4..28].copy_from_slice(&set[0][4..28]);
        new[1][1] = set[1][1];
        new[1][8..16].copy_from_slice(&set[1][8..16]);
        new[1][20..32].copy_from_slice(&set[1][20..32]);
        let checksum = set_checksum(&new[..new_count]);
        new[0][2..4].copy_from_slice(&checksum.to_le_bytes());
        // A set in the same directory is placed before the old one goes, so a cut in power never leaves the file nameless.
        self.place_set(&directory, &new[..new_count]).await?;
        self.delete_set(&old).await?;
        Ok(())
    }
}
