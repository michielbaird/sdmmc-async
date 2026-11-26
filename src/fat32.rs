use crate::{BlockDevice, DirEntry, Error, File, Name, BLOCK_SIZE};

const FAT32_EOC: u32 = 0x0fff_fff8;
const FAT32_BAD: u32 = 0x0fff_fff7;
const ATTR_LFN: u8 = 0x0f;
const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;

pub struct Volume<D: BlockDevice> {
    device: D,
    partition_start: u32,
    sectors_per_cluster: u8,
    fat_start: u32,
    data_start: u32,
    cluster_count: u32,
    root_cluster: u32,
    cache: Cache,
}

struct Cache {
    sector: u32,
    valid: bool,
    data: [u8; BLOCK_SIZE],
}

#[derive(Clone, Copy, Debug)]
pub struct Directory {
    cluster: u32,
    sector_in_cluster: u32,
    entry_in_sector: u32,
    clusters_walked: u32,
    finished: bool,
}

impl Directory {
    pub(crate) fn new(start_cluster: u32) -> Self {
        Self {
            cluster: start_cluster,
            sector_in_cluster: 0,
            entry_in_sector: 0,
            clusters_walked: 0,
            finished: false,
        }
    }
}

struct Lfn {
    units: [u16; 260],
    len: usize,
    checksum: u8,
    next: u8,
    active: bool,
}

impl Lfn {
    fn new() -> Self {
        Self {
            units: [0; 260],
            len: 0,
            checksum: 0,
            next: 0,
            active: false,
        }
    }

    fn reset(&mut self) {
        self.len = 0;
        self.checksum = 0;
        self.next = 0;
        self.active = false;
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Geometry {
    sectors_per_cluster: u8,
    fat_start: u32,
    data_start: u32,
    cluster_count: u32,
    root_cluster: u32,
}

/// Validates a boot sector without touching the device, so a damaged one can be rejected and the
/// backup tried before the device is committed to a volume.
pub(crate) fn geometry<E>(
    boot: &[u8; BLOCK_SIZE],
    partition_start: u32,
    blocks: u32,
) -> Result<Geometry, Error<E>> {
    let bytes_per_sector = u16::from_le_bytes([boot[11], boot[12]]);
    if bytes_per_sector != BLOCK_SIZE as u16 {
        return Err(Error::BadBootSector);
    }
    let sectors_per_cluster = boot[13];
    if sectors_per_cluster == 0 || !sectors_per_cluster.is_power_of_two() {
        return Err(Error::BadGeometry);
    }
    let reserved = u16::from_le_bytes([boot[14], boot[15]]);
    let num_fats = boot[16];
    let root_entries = u16::from_le_bytes([boot[17], boot[18]]);
    let total16 = u16::from_le_bytes([boot[19], boot[20]]);
    let fat_size16 = u16::from_le_bytes([boot[22], boot[23]]);
    let total32 = u32::from_le_bytes([boot[32], boot[33], boot[34], boot[35]]);
    let fat_size32 = u32::from_le_bytes([boot[36], boot[37], boot[38], boot[39]]);
    let root_cluster = u32::from_le_bytes([boot[44], boot[45], boot[46], boot[47]]);

    if root_entries != 0 || fat_size16 != 0 {
        return Err(Error::NotFat32);
    }
    if fat_size32 == 0 || reserved == 0 || num_fats == 0 {
        return Err(Error::BadGeometry);
    }
    let total = if total16 != 0 {
        u32::from(total16)
    } else {
        total32
    };
    if total == 0 {
        return Err(Error::BadGeometry);
    }
    if u64::from(partition_start) + u64::from(total) > u64::from(blocks) {
        return Err(Error::BadGeometry);
    }
    let data_start =
        u32::try_from(u64::from(reserved) + u64::from(num_fats) * u64::from(fat_size32))
            .map_err(|_| Error::BadGeometry)?;
    if data_start >= total {
        return Err(Error::BadGeometry);
    }
    let cluster_count = (total - data_start) / u32::from(sectors_per_cluster);
    if cluster_count == 0 || cluster_count >= 0x0fff_fff5 {
        return Err(Error::BadGeometry);
    }
    if root_cluster < 2 || root_cluster >= cluster_count + 2 {
        return Err(Error::BadGeometry);
    }
    let fat_bytes = u64::from(fat_size32) * BLOCK_SIZE as u64;
    if fat_bytes / 4 < u64::from(cluster_count) + 2 {
        return Err(Error::BadGeometry);
    }
    Ok(Geometry {
        sectors_per_cluster,
        fat_start: u32::from(reserved),
        data_start,
        cluster_count,
        root_cluster,
    })
}

impl<D: BlockDevice> Volume<D> {
    pub(crate) fn new(device: D, partition_start: u32, geometry: Geometry) -> Self {
        Self {
            device,
            partition_start,
            sectors_per_cluster: geometry.sectors_per_cluster,
            fat_start: geometry.fat_start,
            data_start: geometry.data_start,
            cluster_count: geometry.cluster_count,
            root_cluster: geometry.root_cluster,
            cache: Cache {
                sector: 0,
                valid: false,
                data: [0; BLOCK_SIZE],
            },
        }
    }

    pub(crate) fn into_device(self) -> D {
        self.device
    }

    pub fn open_root(&self) -> Directory {
        Directory::new(self.root_cluster)
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
            no_fat_chain: false,
        })
    }

    pub async fn open_dir(&mut self, path: &str) -> Result<Directory, Error<D::Error>> {
        let mut cluster = self.root_cluster;
        for component in path
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
        {
            let entry = self.find_in(cluster, component).await?;
            if !entry.is_directory {
                return Err(Error::NotADirectory);
            }
            cluster = entry.first_cluster;
        }
        Ok(Directory::new(cluster))
    }

    pub async fn next_entry(
        &mut self,
        dir: &mut Directory,
    ) -> Result<Option<DirEntry>, Error<D::Error>> {
        let mut lfn = Lfn::new();
        loop {
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

            if raw[0] == 0x00 {
                dir.finished = true;
                return Ok(None);
            }
            if raw[0] == 0xe5 {
                lfn.reset();
                continue;
            }
            if raw[11] == ATTR_LFN {
                if !push_lfn(&mut lfn, &raw) {
                    lfn.reset();
                }
                continue;
            }

            let attributes = raw[11];
            if attributes & ATTR_VOLUME_ID != 0 {
                lfn.reset();
                continue;
            }
            let short = short_name(&raw);
            if short.as_str() == "." || short.as_str() == ".." {
                lfn.reset();
                continue;
            }
            let name = if lfn.active && lfn.next == 0 && lfn.checksum == short_checksum(&raw[..11])
            {
                lfn_name(&lfn).unwrap_or(short)
            } else {
                short
            };
            lfn.reset();

            let first_cluster = (u32::from(u16::from_le_bytes([raw[20], raw[21]])) << 16)
                | u32::from(u16::from_le_bytes([raw[26], raw[27]]));
            let size = u32::from_le_bytes([raw[28], raw[29], raw[30], raw[31]]);
            return Ok(Some(DirEntry {
                name,
                short_name: short,
                attributes,
                first_cluster,
                size: u64::from(size),
                is_directory: attributes & ATTR_DIRECTORY != 0,
                no_fat_chain: false,
            }));
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
        let cluster_size = u64::from(self.sectors_per_cluster) * BLOCK_SIZE as u64;
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
                match self.next_cluster(file.cluster).await? {
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
        let cluster_size = u64::from(self.sectors_per_cluster) * BLOCK_SIZE as u64;
        let target = position / cluster_size;
        let mut cluster = file.first_cluster;
        if cluster < 2 && position > 0 {
            return Err(Error::TruncatedFile);
        }
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
        let mut cluster = self.root_cluster;
        let mut last = None;
        while let Some(component) = components.next() {
            let entry = self.find_in(cluster, component).await?;
            if components.peek().is_some() {
                if !entry.is_directory {
                    return Err(Error::NotADirectory);
                }
                cluster = entry.first_cluster;
            }
            last = Some(entry);
        }
        last.ok_or(Error::NotFound)
    }

    async fn find_in(
        &mut self,
        start_cluster: u32,
        name: &str,
    ) -> Result<DirEntry, Error<D::Error>> {
        let mut dir = Directory::new(start_cluster);
        while let Some(entry) = self.next_entry(&mut dir).await? {
            if entry.name.eq_ignore_ascii_case(name) || entry.short_name.eq_ignore_ascii_case(name)
            {
                return Ok(entry);
            }
        }
        Err(Error::NotFound)
    }

    async fn advance(&mut self, dir: &mut Directory) -> Result<(), Error<D::Error>> {
        dir.entry_in_sector += 1;
        if dir.entry_in_sector == BLOCK_SIZE as u32 / 32 {
            dir.entry_in_sector = 0;
            dir.sector_in_cluster += 1;
            if dir.sector_in_cluster == u32::from(self.sectors_per_cluster) {
                dir.sector_in_cluster = 0;
                match self.next_cluster(dir.cluster).await? {
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
        ]) & 0x0fff_ffff)
    }

    async fn next_cluster(&mut self, cluster: u32) -> Result<Option<u32>, Error<D::Error>> {
        let entry = self.fat_entry(cluster).await?;
        if entry >= FAT32_EOC {
            return Ok(None);
        }
        if entry == FAT32_BAD || entry < 2 || entry >= self.cluster_count + 2 {
            return Err(Error::InvalidCluster);
        }
        Ok(Some(entry))
    }

    /// Clusters come from untrusted directory entries and the FAT: outside the volume is an error.
    fn cluster_sector(&self, cluster: u32) -> Result<u32, Error<D::Error>> {
        if cluster < 2 || cluster >= self.cluster_count + 2 {
            return Err(Error::InvalidCluster);
        }
        (cluster - 2)
            .checked_mul(u32::from(self.sectors_per_cluster))
            .and_then(|offset| self.data_start.checked_add(offset))
            .ok_or(Error::InvalidCluster)
    }
}

fn push_lfn(lfn: &mut Lfn, raw: &[u8; 32]) -> bool {
    let sequence = raw[0];
    let index = sequence & 0x1f;
    if !(1..=20).contains(&index) {
        return false;
    }
    let checksum = raw[13];
    if sequence & 0x40 != 0 {
        lfn.active = true;
        lfn.len = index as usize * 13;
        lfn.checksum = checksum;
        lfn.next = index;
    } else if !lfn.active || checksum != lfn.checksum || index != lfn.next {
        return false;
    }

    let start = (index as usize - 1) * 13;
    if start + 13 > lfn.units.len() {
        return false;
    }
    let units = [
        [raw[1], raw[2]],
        [raw[3], raw[4]],
        [raw[5], raw[6]],
        [raw[7], raw[8]],
        [raw[9], raw[10]],
        [raw[14], raw[15]],
        [raw[16], raw[17]],
        [raw[18], raw[19]],
        [raw[20], raw[21]],
        [raw[22], raw[23]],
        [raw[24], raw[25]],
        [raw[28], raw[29]],
        [raw[30], raw[31]],
    ];
    for (position, pair) in units.iter().enumerate() {
        lfn.units[start + position] = u16::from_le_bytes(*pair);
    }
    lfn.next = index - 1;
    true
}

fn lfn_name(lfn: &Lfn) -> Option<Name> {
    let end = lfn.units[..lfn.len]
        .iter()
        .position(|unit| *unit == 0 || *unit == 0xffff)
        .unwrap_or(lfn.len);
    let mut name = Name::empty();
    for decoded in core::char::decode_utf16(lfn.units[..end].iter().copied()) {
        let character = decoded.unwrap_or('\u{fffd}');
        name.push_char(character).ok()?;
    }
    Some(name)
}

fn short_name(raw: &[u8; 32]) -> Name {
    let mut name = Name::empty();
    let lower_base = raw[12] & 0x08 != 0;
    let lower_extension = raw[12] & 0x10 != 0;

    let mut base = [0u8; 8];
    base.copy_from_slice(&raw[0..8]);
    if base[0] == 0x05 {
        base[0] = 0xe5;
    }
    let mut end = base.len();
    while end > 0 && base[end - 1] == b' ' {
        end -= 1;
    }
    for &byte in &base[..end] {
        let byte = if lower_base {
            byte.to_ascii_lowercase()
        } else {
            byte
        };
        let _ = name.push_byte(byte);
    }

    let mut extension = [0u8; 3];
    extension.copy_from_slice(&raw[8..11]);
    let mut end = extension.len();
    while end > 0 && extension[end - 1] == b' ' {
        end -= 1;
    }
    if end > 0 {
        let _ = name.push_byte(b'.');
        for &byte in &extension[..end] {
            let byte = if lower_extension {
                byte.to_ascii_lowercase()
            } else {
                byte
            };
            let _ = name.push_byte(byte);
        }
    }
    name
}

fn short_checksum(name: &[u8]) -> u8 {
    let mut sum = 0u8;
    for &byte in &name[..11] {
        sum = ((sum & 1) << 7).wrapping_add(sum >> 1).wrapping_add(byte);
    }
    sum
}
