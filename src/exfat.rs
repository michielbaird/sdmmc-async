use crate::{BlockDevice, DirEntry, Error, File, Name, BLOCK_SIZE};

const EXFAT_EOC: u32 = 0xffff_fff8;
const EXFAT_BAD: u32 = 0xffff_fff7;
const ATTR_DIRECTORY: u16 = 0x10;
const ENTRY_FILE: u8 = 0x85;
const ENTRY_STREAM: u8 = 0xc0;
const ENTRY_NAME: u8 = 0xc1;

pub struct Volume<D: BlockDevice> {
    device: D,
    partition_start: u32,
    fat_start: u32,
    cluster_heap_start: u32,
    cluster_count: u32,
    root_cluster: u32,
    sectors_per_cluster_shift: u8,
    cache: Cache,
}

struct Cache {
    sector: u32,
    valid: bool,
    data: [u8; BLOCK_SIZE],
}

#[derive(Clone, Copy, Debug)]
pub struct Directory {
    start_cluster: u32,
    cluster: u32,
    sector_in_cluster: u32,
    entry_in_sector: u32,
    clusters_walked: u32,
    remaining: u64,
    no_fat_chain: bool,
    finished: bool,
}

impl Directory {
    fn new(start_cluster: u32, remaining: u64, no_fat_chain: bool) -> Self {
        Self {
            start_cluster,
            cluster: start_cluster,
            sector_in_cluster: 0,
            entry_in_sector: 0,
            clusters_walked: 0,
            remaining,
            no_fat_chain,
            finished: remaining == 0,
        }
    }
}

impl<D: BlockDevice> Volume<D> {
    pub async fn mount_at(mut device: D, partition_start: u32) -> Result<Self, Error<D::Error>> {
        let boot = match verified_boot(&mut device, partition_start, 0).await {
            Ok(boot) => boot,
            Err(Error::BadBootSector) => verified_boot(&mut device, partition_start, 12).await?,
            Err(other) => return Err(other),
        };

        let bytes_per_sector_shift = boot[108];
        let sectors_per_cluster_shift = boot[109];
        let num_fats = boot[110];
        let volume_length = u64::from_le_bytes([
            boot[72], boot[73], boot[74], boot[75], boot[76], boot[77], boot[78], boot[79],
        ]);
        let fat_offset = u32::from_le_bytes([boot[80], boot[81], boot[82], boot[83]]);
        let fat_length = u32::from_le_bytes([boot[84], boot[85], boot[86], boot[87]]);
        let cluster_heap_offset = u32::from_le_bytes([boot[88], boot[89], boot[90], boot[91]]);
        let cluster_count = u32::from_le_bytes([boot[92], boot[93], boot[94], boot[95]]);
        let root_cluster = u32::from_le_bytes([boot[96], boot[97], boot[98], boot[99]]);

        if bytes_per_sector_shift != 9 {
            return Err(Error::Unsupported);
        }
        if sectors_per_cluster_shift > 16 || num_fats == 0 || num_fats > 2 {
            return Err(Error::BadGeometry);
        }
        if fat_offset < 24 || fat_length == 0 || cluster_count == 0 {
            return Err(Error::BadGeometry);
        }
        let fat_end = u64::from(fat_offset) + u64::from(fat_length) * u64::from(num_fats);
        if fat_end > u64::from(cluster_heap_offset) {
            return Err(Error::BadGeometry);
        }
        let heap_end = u64::from(cluster_heap_offset)
            + (u64::from(cluster_count) << sectors_per_cluster_shift);
        if heap_end > volume_length {
            return Err(Error::BadGeometry);
        }
        let blocks = device.num_blocks().await.map_err(Error::Device)?;
        if u64::from(partition_start) + volume_length > u64::from(blocks) {
            return Err(Error::BadGeometry);
        }
        if u64::from(root_cluster) < 2 || u64::from(root_cluster) >= u64::from(cluster_count) + 2 {
            return Err(Error::BadGeometry);
        }
        let fat_bytes = u64::from(fat_length) * BLOCK_SIZE as u64;
        if fat_bytes / 4 < u64::from(cluster_count) + 2 {
            return Err(Error::BadGeometry);
        }

        Ok(Self {
            device,
            partition_start,
            fat_start: fat_offset,
            cluster_heap_start: cluster_heap_offset,
            cluster_count,
            root_cluster,
            sectors_per_cluster_shift,
            cache: Cache {
                sector: 0,
                valid: false,
                data: [0; BLOCK_SIZE],
            },
        })
    }

    pub(crate) fn into_device(self) -> D {
        self.device
    }

    pub fn open_root(&self) -> Directory {
        Directory::new(self.root_cluster, u64::MAX, false)
    }

    pub async fn open_file(&mut self, path: &str) -> Result<File, Error<D::Error>> {
        let entry = self.find_path(path).await?;
        if entry.is_directory {
            return Err(Error::NotAFile);
        }
        Ok(File {
            first_cluster: entry.first_cluster,
            cluster: entry.first_cluster,
            size: entry.size,
            cursor: 0,
            clusters_walked: 0,
            no_fat_chain: entry.no_fat_chain,
        })
    }

    pub async fn open_dir(&mut self, path: &str) -> Result<Directory, Error<D::Error>> {
        let mut dir = self.open_root();
        for component in path
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
        {
            let entry = self.find_in(&dir, component).await?;
            if !entry.is_directory {
                return Err(Error::NotADirectory);
            }
            dir = Directory::new(entry.first_cluster, entry.size, entry.no_fat_chain);
        }
        Ok(dir)
    }

    pub async fn next_entry(
        &mut self,
        dir: &mut Directory,
    ) -> Result<Option<DirEntry>, Error<D::Error>> {
        loop {
            let Some(first) = self.next_raw(dir).await? else {
                return Ok(None);
            };
            match first[0] {
                0x00 => {
                    dir.finished = true;
                    return Ok(None);
                }
                ENTRY_FILE => {
                    if let Some(entry) = self.parse_file_set(dir, first).await? {
                        return Ok(Some(entry));
                    }
                }
                // Every other entry is one 32-byte record. Byte 1 only means "secondary count" for
                // file sets, which parse_file_set consumes itself: for a volume label it is the
                // character count, so skipping by it would swallow real files.
                _ => {}
            }
        }
    }

    pub async fn read_file(
        &mut self,
        file: &mut File,
        buffer: &mut [u8],
    ) -> Result<usize, Error<D::Error>> {
        if file.cursor >= file.size || buffer.is_empty() {
            return Ok(0);
        }
        if file.first_cluster < 2 {
            return Err(Error::TruncatedFile);
        }
        let cluster_size = (BLOCK_SIZE as u64) << self.sectors_per_cluster_shift;
        let mut written = 0;
        while written < buffer.len() && file.cursor < file.size {
            if file.cluster < 2 {
                file.cluster = file.first_cluster;
            }
            let offset = file.cursor % cluster_size;
            let sector_in_cluster = (offset / BLOCK_SIZE as u64) as u32;
            let offset_in_sector = (offset % BLOCK_SIZE as u64) as usize;
            let sector = self.cluster_sector(file.cluster)? + sector_in_cluster;
            let available = BLOCK_SIZE - offset_in_sector;
            let remaining = (file.size - file.cursor) as usize;
            let take = available.min(buffer.len() - written).min(remaining);
            {
                let data = self.sector(sector).await?;
                buffer[written..written + take]
                    .copy_from_slice(&data[offset_in_sector..offset_in_sector + take]);
            }
            file.cursor += take as u64;
            written += take;
            if file.cursor < file.size && file.cursor.is_multiple_of(cluster_size) {
                let next = if file.no_fat_chain {
                    let next = file.cluster.checked_add(1).ok_or(Error::InvalidCluster)?;
                    if u64::from(next) < u64::from(self.cluster_count) + 2 {
                        Some(next)
                    } else {
                        None
                    }
                } else {
                    self.next_cluster(file.cluster).await?
                };
                match next {
                    Some(next) => {
                        file.clusters_walked += 1;
                        if file.clusters_walked > self.cluster_count {
                            return Err(Error::InvalidCluster);
                        }
                        file.cluster = next;
                    }
                    None => return Err(Error::TruncatedFile),
                }
            }
        }
        Ok(written)
    }

    pub async fn seek(&mut self, file: &mut File, position: u64) -> Result<(), Error<D::Error>> {
        if position > file.size {
            return Err(Error::InvalidPosition);
        }
        if position == file.size {
            file.cursor = position;
            return Ok(());
        }
        let cluster_size = (BLOCK_SIZE as u64) << self.sectors_per_cluster_shift;
        let target = position / cluster_size;
        let mut cluster = file.first_cluster;
        if cluster < 2 && position > 0 {
            return Err(Error::TruncatedFile);
        }
        if file.no_fat_chain {
            let target = u64::from(file.first_cluster) + target;
            if target >= u64::from(self.cluster_count) + 2 {
                return Err(Error::InvalidCluster);
            }
            cluster = target as u32;
        } else {
            let mut walked = 0u32;
            for _ in 0..target {
                match self.next_cluster(cluster).await? {
                    Some(next) => {
                        walked += 1;
                        if walked > self.cluster_count {
                            return Err(Error::InvalidCluster);
                        }
                        cluster = next;
                    }
                    None => return Err(Error::TruncatedFile),
                }
            }
        }
        file.cluster = cluster;
        file.cursor = position;
        file.clusters_walked = target as u32;
        Ok(())
    }

    async fn find_path(&mut self, path: &str) -> Result<DirEntry, Error<D::Error>> {
        let mut components = path
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .peekable();
        let mut dir = self.open_root();
        let mut last = None;
        while let Some(component) = components.next() {
            let entry = self.find_in(&dir, component).await?;
            if components.peek().is_some() {
                if !entry.is_directory {
                    return Err(Error::NotADirectory);
                }
                dir = Directory::new(entry.first_cluster, entry.size, entry.no_fat_chain);
            }
            last = Some(entry);
        }
        last.ok_or(Error::NotFound)
    }

    async fn find_in(&mut self, dir: &Directory, name: &str) -> Result<DirEntry, Error<D::Error>> {
        let mut search = Directory::new(dir.start_cluster, dir.remaining, dir.no_fat_chain);
        while let Some(entry) = self.next_entry(&mut search).await? {
            if entry.name.eq_ignore_ascii_case(name) {
                return Ok(entry);
            }
        }
        Err(Error::NotFound)
    }

    async fn next_raw(&mut self, dir: &mut Directory) -> Result<Option<[u8; 32]>, Error<D::Error>> {
        if dir.finished {
            return Ok(None);
        }
        let sector = self.cluster_sector(dir.cluster)? + dir.sector_in_cluster;
        let offset = dir.entry_in_sector as usize * 32;
        let mut raw = [0u8; 32];
        {
            let data = self.sector(sector).await?;
            raw.copy_from_slice(&data[offset..offset + 32]);
        }
        self.advance(dir).await?;
        Ok(Some(raw))
    }

    async fn parse_file_set(
        &mut self,
        dir: &mut Directory,
        first: [u8; 32],
    ) -> Result<Option<DirEntry>, Error<D::Error>> {
        let secondary_count = first[1] as usize;
        if !(2..=18).contains(&secondary_count) {
            for _ in 0..secondary_count {
                if self.next_raw(dir).await?.is_none() {
                    break;
                }
            }
            return Ok(None);
        }

        let mut checksum = 0u16;
        for (index, &byte) in first.iter().enumerate() {
            if index == 2 || index == 3 {
                continue;
            }
            checksum = checksum.rotate_right(1).wrapping_add(u16::from(byte));
        }

        let mut stream = None;
        let mut units = [0u16; 255];
        let mut unit_count = 0usize;
        let mut name_length = 0usize;
        for _ in 0..secondary_count {
            let Some(raw) = self.next_raw(dir).await? else {
                return Ok(None);
            };
            for &byte in raw.iter() {
                checksum = checksum.rotate_right(1).wrapping_add(u16::from(byte));
            }
            match raw[0] {
                ENTRY_STREAM => {
                    stream = Some(raw);
                    name_length = raw[3] as usize;
                }
                ENTRY_NAME => {
                    for index in 0..15 {
                        if unit_count < units.len() {
                            units[unit_count] =
                                u16::from_le_bytes([raw[2 + index * 2], raw[3 + index * 2]]);
                            unit_count += 1;
                        }
                    }
                }
                _ => {}
            }
        }

        let stored = u16::from_le_bytes([first[2], first[3]]);
        if checksum != stored {
            return Ok(None);
        }
        let Some(stream) = stream else {
            return Ok(None);
        };
        let length = name_length.min(unit_count);
        if length == 0 {
            return Ok(None);
        }
        let mut name = Name::empty();
        for decoded in core::char::decode_utf16(units[..length].iter().copied()) {
            let character = decoded.unwrap_or('\u{fffd}');
            if name.push_char(character).is_err() {
                return Ok(None);
            }
        }

        let attributes = u16::from_le_bytes([first[4], first[5]]);
        Ok(Some(DirEntry {
            name,
            #[cfg(feature = "fat32")]
            short_name: name,
            attributes: attributes as u8,
            first_cluster: u32::from_le_bytes([stream[20], stream[21], stream[22], stream[23]]),
            size: u64::from_le_bytes([
                stream[24], stream[25], stream[26], stream[27], stream[28], stream[29], stream[30],
                stream[31],
            ]),
            is_directory: attributes & ATTR_DIRECTORY != 0,
            no_fat_chain: stream[1] & 0x02 != 0,
        }))
    }

    async fn advance(&mut self, dir: &mut Directory) -> Result<(), Error<D::Error>> {
        if dir.remaining != u64::MAX {
            dir.remaining = dir.remaining.saturating_sub(32);
            if dir.remaining == 0 {
                dir.finished = true;
                return Ok(());
            }
        }
        dir.entry_in_sector += 1;
        if dir.entry_in_sector == BLOCK_SIZE as u32 / 32 {
            dir.entry_in_sector = 0;
            dir.sector_in_cluster += 1;
            if dir.sector_in_cluster == (1u32 << self.sectors_per_cluster_shift) {
                dir.sector_in_cluster = 0;
                let next = if dir.no_fat_chain {
                    let next = dir.cluster.checked_add(1).ok_or(Error::InvalidCluster)?;
                    if u64::from(next) < u64::from(self.cluster_count) + 2 {
                        Some(next)
                    } else {
                        None
                    }
                } else {
                    self.next_cluster(dir.cluster).await?
                };
                match next {
                    Some(next) => {
                        dir.clusters_walked += 1;
                        if dir.clusters_walked > self.cluster_count {
                            return Err(Error::InvalidCluster);
                        }
                        dir.cluster = next;
                    }
                    None => dir.finished = true,
                }
            }
        }
        Ok(())
    }

    async fn sector(&mut self, sector: u32) -> Result<&[u8; BLOCK_SIZE], Error<D::Error>> {
        if !self.cache.valid || self.cache.sector != sector {
            let absolute = self
                .partition_start
                .checked_add(sector)
                .ok_or(Error::BadGeometry)?;
            self.device
                .read_block(absolute, &mut self.cache.data)
                .await
                .map_err(Error::Device)?;
            self.cache.sector = sector;
            self.cache.valid = true;
        }
        Ok(&self.cache.data)
    }

    async fn fat_entry(&mut self, cluster: u32) -> Result<u32, Error<D::Error>> {
        let offset = cluster.checked_mul(4).ok_or(Error::InvalidCluster)?;
        let sector = self
            .fat_start
            .checked_add(offset / BLOCK_SIZE as u32)
            .ok_or(Error::InvalidCluster)?;
        let offset_in_sector = (offset % BLOCK_SIZE as u32) as usize;
        let data = self.sector(sector).await?;
        Ok(u32::from_le_bytes([
            data[offset_in_sector],
            data[offset_in_sector + 1],
            data[offset_in_sector + 2],
            data[offset_in_sector + 3],
        ]))
    }

    async fn next_cluster(&mut self, cluster: u32) -> Result<Option<u32>, Error<D::Error>> {
        let entry = self.fat_entry(cluster).await?;
        if entry >= EXFAT_EOC {
            return Ok(None);
        }
        if entry == EXFAT_BAD || entry < 2 || u64::from(entry) >= u64::from(self.cluster_count) + 2
        {
            return Err(Error::InvalidCluster);
        }
        Ok(Some(entry))
    }

    /// Clusters come from untrusted directory entries and the FAT: outside the volume is an error.
    fn cluster_sector(&self, cluster: u32) -> Result<u32, Error<D::Error>> {
        if cluster < 2 || u64::from(cluster) >= u64::from(self.cluster_count) + 2 {
            return Err(Error::InvalidCluster);
        }
        let offset = u64::from(cluster - 2) << self.sectors_per_cluster_shift;
        u32::try_from(u64::from(self.cluster_heap_start) + offset)
            .map_err(|_| Error::InvalidCluster)
    }
}

async fn verified_boot<D: BlockDevice>(
    device: &mut D,
    partition_start: u32,
    start: u32,
) -> Result<[u8; BLOCK_SIZE], Error<D::Error>> {
    let mut boot = [0u8; BLOCK_SIZE];
    let sector = partition_start
        .checked_add(start)
        .ok_or(Error::BadGeometry)?;
    device
        .read_block(sector, &mut boot)
        .await
        .map_err(Error::Device)?;
    if &boot[3..11] != b"EXFAT   " || boot[510] != 0x55 || boot[511] != 0xaa {
        return Err(Error::BadBootSector);
    }

    let mut checksum = 0u32;
    for index in 0..11u32 {
        let mut sector_data = [0u8; BLOCK_SIZE];
        if index == 0 {
            sector_data.copy_from_slice(&boot);
        } else {
            let sector = partition_start
                .checked_add(start)
                .and_then(|base| base.checked_add(index))
                .ok_or(Error::BadGeometry)?;
            device
                .read_block(sector, &mut sector_data)
                .await
                .map_err(Error::Device)?;
        }
        for (offset, &byte) in sector_data.iter().enumerate() {
            if index == 0 && (offset == 106 || offset == 107 || offset == 112) {
                continue;
            }
            checksum = checksum.rotate_right(1).wrapping_add(u32::from(byte));
        }
    }

    let sector = partition_start
        .checked_add(start)
        .and_then(|base| base.checked_add(11))
        .ok_or(Error::BadGeometry)?;
    let mut stored = [0u8; BLOCK_SIZE];
    device
        .read_block(sector, &mut stored)
        .await
        .map_err(Error::Device)?;
    let stored = u32::from_le_bytes([stored[0], stored[1], stored[2], stored[3]]);
    if checksum != stored {
        return Err(Error::BadBootSector);
    }
    Ok(boot)
}
