#[cfg(feature = "exfat")]
use crate::exfat;
#[cfg(feature = "fat32")]
use crate::fat32;
use crate::{BlockDevice, Error, Name, BLOCK_SIZE};

/// An open file. It is plain data: the `Volume` owns the device and performs
/// the reads, so the file handle can be stored anywhere without borrowing the
/// filesystem.
#[derive(Clone, Copy, Debug)]
pub struct File {
    pub(crate) first_cluster: u32,
    pub(crate) cluster: u32,
    pub(crate) size: u64,
    pub(crate) cursor: u64,
    pub(crate) clusters_walked: u32,
    /// Only exFAT has files with no FAT chain.
    #[cfg(feature = "exfat")]
    pub(crate) no_fat_chain: bool,
}

impl File {
    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn position(&self) -> u64 {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.size == 0
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DirEntry {
    pub name: Name,
    /// The 8.3 alias on FAT32. Equal to `name` on exFAT.
    /// Only FAT32 has an 8.3 alias.
    #[cfg(feature = "fat32")]
    pub(crate) short_name: Name,
    pub attributes: u8,
    pub first_cluster: u32,
    pub size: u64,
    pub is_directory: bool,
    #[cfg(feature = "exfat")]
    pub(crate) no_fat_chain: bool,
}

/// An open directory. Opaque: obtain one from `Volume::open_root` or
/// `Volume::open_dir` and pass it back to `Volume::next_entry`.
#[derive(Clone, Copy, Debug)]
pub struct Directory {
    inner: DirectoryInner,
}

#[derive(Clone, Copy, Debug)]
enum DirectoryInner {
    #[cfg(feature = "fat32")]
    Fat32(fat32::Directory),
    #[cfg(feature = "exfat")]
    ExFat(exfat::Directory),
}

/// A mounted volume. The filesystem type is detected from the boot sector, so
/// callers use one API for FAT32 and exFAT cards.
pub struct Volume<D: BlockDevice> {
    inner: VolumeInner<D>,
}

enum VolumeInner<D: BlockDevice> {
    #[cfg(feature = "fat32")]
    Fat32(fat32::Volume<D>),
    #[cfg(feature = "exfat")]
    ExFat(exfat::Volume<D>),
}

/// Runs `$body` on whichever filesystem the volume is, binding it as `$volume`.
macro_rules! on_volume {
    ($inner:expr, $volume:ident => $body:expr) => {
        match $inner {
            #[cfg(feature = "fat32")]
            VolumeInner::Fat32($volume) => $body,
            #[cfg(feature = "exfat")]
            VolumeInner::ExFat($volume) => $body,
        }
    };
}

impl<D: BlockDevice> Volume<D> {
    pub async fn mount(mut device: D) -> Result<Self, Error<D::Error>> {
        let mut sector = [0u8; BLOCK_SIZE];
        device
            .read_block(0, &mut sector)
            .await
            .map_err(Error::Device)?;

        if is_exfat_boot(&sector) {
            return mount_exfat(device, 0).await;
        }
        if is_fat32_boot(&sector) {
            return mount_fat32(device, 0, sector).await;
        }
        if is_fat_boot(&sector) {
            return Err(Error::Unsupported);
        }

        let Some(start) = first_partition(&sector) else {
            // No partition table: a superfloppy FAT32 volume whose boot sector is damaged.
            let backup = read_backup_boot(&mut device, 0).await;
            return match backup {
                Some(backup) => mount_fat32(device, 0, backup).await,
                None => Err(Error::NoPartition),
            };
        };
        device
            .read_block(start, &mut sector)
            .await
            .map_err(Error::Device)?;
        if is_exfat_boot(&sector) {
            return mount_exfat(device, start).await;
        }
        if is_fat32_boot(&sector) {
            return mount_fat32(device, start, sector).await;
        }
        if is_fat_boot(&sector) {
            return Err(Error::Unsupported);
        }
        match read_backup_boot(&mut device, start).await {
            Some(backup) => mount_fat32(device, start, backup).await,
            None => Err(Error::BadBootSector),
        }
    }

    /// The device, when the volume is no longer wanted.
    pub fn into_device(self) -> D {
        on_volume!(self.inner, volume => volume.into_device())
    }

    pub fn open_root(&self) -> Directory {
        Directory {
            inner: match &self.inner {
                #[cfg(feature = "fat32")]
                VolumeInner::Fat32(volume) => DirectoryInner::Fat32(volume.open_root()),
                #[cfg(feature = "exfat")]
                VolumeInner::ExFat(volume) => DirectoryInner::ExFat(volume.open_root()),
            },
        }
    }

    pub async fn open_file(&mut self, path: &str) -> Result<File, Error<D::Error>> {
        on_volume!(&mut self.inner, volume => volume.open_file(path).await)
    }

    pub async fn open_dir(&mut self, path: &str) -> Result<Directory, Error<D::Error>> {
        let inner = match &mut self.inner {
            #[cfg(feature = "fat32")]
            VolumeInner::Fat32(volume) => DirectoryInner::Fat32(volume.open_dir(path).await?),
            #[cfg(feature = "exfat")]
            VolumeInner::ExFat(volume) => DirectoryInner::ExFat(volume.open_dir(path).await?),
        };
        Ok(Directory { inner })
    }

    pub async fn next_entry(
        &mut self,
        dir: &mut Directory,
    ) -> Result<Option<DirEntry>, Error<D::Error>> {
        match (&mut self.inner, &mut dir.inner) {
            #[cfg(feature = "fat32")]
            (VolumeInner::Fat32(volume), DirectoryInner::Fat32(dir)) => {
                volume.next_entry(dir).await
            }
            #[cfg(feature = "exfat")]
            (VolumeInner::ExFat(volume), DirectoryInner::ExFat(dir)) => {
                volume.next_entry(dir).await
            }
            #[cfg(all(feature = "fat32", feature = "exfat"))]
            _ => Err(Error::Unsupported),
        }
    }

    pub async fn read_file(
        &mut self,
        file: &mut File,
        buffer: &mut [u8],
    ) -> Result<usize, Error<D::Error>> {
        on_volume!(&mut self.inner, volume => volume.read_file(file, buffer).await)
    }

    pub async fn seek(&mut self, file: &mut File, position: u64) -> Result<(), Error<D::Error>> {
        on_volume!(&mut self.inner, volume => volume.seek(file, position).await)
    }
}

/// Mounts FAT32 from `boot`; if its geometry is damaged, the backup boot sector (sector 6 of the
/// volume) gets a chance before the error is reported.
#[cfg(feature = "fat32")]
async fn mount_fat32<D: BlockDevice>(
    mut device: D,
    start: u32,
    boot: [u8; BLOCK_SIZE],
) -> Result<Volume<D>, Error<D::Error>> {
    let blocks = device.num_blocks().await.map_err(Error::Device)?;
    let error = match fat32::geometry(&boot, start, blocks) {
        Ok(geometry) => return Ok(fat32_volume(device, start, geometry)),
        Err(error) => error,
    };
    if let Some(backup) = read_backup_boot(&mut device, start).await {
        if let Ok(geometry) = fat32::geometry::<D::Error>(&backup, start, blocks) {
            return Ok(fat32_volume(device, start, geometry));
        }
    }
    Err(error)
}

#[cfg(not(feature = "fat32"))]
async fn mount_fat32<D: BlockDevice>(
    _device: D,
    _start: u32,
    _boot: [u8; BLOCK_SIZE],
) -> Result<Volume<D>, Error<D::Error>> {
    Err(Error::FilesystemNotEnabled)
}

#[cfg(feature = "fat32")]
fn fat32_volume<D: BlockDevice>(device: D, start: u32, geometry: fat32::Geometry) -> Volume<D> {
    Volume {
        inner: VolumeInner::Fat32(fat32::Volume::new(device, start, geometry)),
    }
}

#[cfg(feature = "exfat")]
async fn mount_exfat<D: BlockDevice>(device: D, start: u32) -> Result<Volume<D>, Error<D::Error>> {
    Ok(Volume {
        inner: VolumeInner::ExFat(exfat::Volume::mount_at(device, start).await?),
    })
}

#[cfg(not(feature = "exfat"))]
async fn mount_exfat<D: BlockDevice>(
    _device: D,
    _start: u32,
) -> Result<Volume<D>, Error<D::Error>> {
    Err(Error::FilesystemNotEnabled)
}

async fn read_backup_boot<D: BlockDevice>(device: &mut D, start: u32) -> Option<[u8; BLOCK_SIZE]> {
    let mut backup = [0u8; BLOCK_SIZE];
    device
        .read_block(start.checked_add(6)?, &mut backup)
        .await
        .ok()?;
    is_fat32_boot(&backup).then_some(backup)
}

fn is_exfat_boot(sector: &[u8; BLOCK_SIZE]) -> bool {
    sector[510] == 0x55 && sector[511] == 0xaa && &sector[3..11] == b"EXFAT   "
}

fn is_fat32_boot(sector: &[u8; BLOCK_SIZE]) -> bool {
    // Structural markers only: the full geometry is validated by the FAT32 mount, so a corrupt
    // sectors-per-cluster still reaches that validation and reports BadGeometry rather than
    // falling through to the MBR path.
    let bytes_per_sector = u16::from_le_bytes([sector[11], sector[12]]);
    let root_entries = u16::from_le_bytes([sector[17], sector[18]]);
    let fat_size16 = u16::from_le_bytes([sector[22], sector[23]]);
    let fat_size32 = u32::from_le_bytes([sector[36], sector[37], sector[38], sector[39]]);
    sector[510] == 0x55
        && sector[511] == 0xaa
        && bytes_per_sector == 512
        && root_entries == 0
        && fat_size16 == 0
        && fat_size32 != 0
}

fn is_fat_boot(sector: &[u8; BLOCK_SIZE]) -> bool {
    let bytes_per_sector = u16::from_le_bytes([sector[11], sector[12]]);
    let sectors_per_cluster = sector[13];
    let reserved = u16::from_le_bytes([sector[14], sector[15]]);
    let num_fats = sector[16];
    let fat_size16 = u16::from_le_bytes([sector[22], sector[23]]);
    let fat_size32 = u32::from_le_bytes([sector[36], sector[37], sector[38], sector[39]]);
    sector[510] == 0x55
        && sector[511] == 0xaa
        && bytes_per_sector == 512
        && sectors_per_cluster != 0
        && sectors_per_cluster.is_power_of_two()
        && reserved != 0
        && (1..=2).contains(&num_fats)
        && (fat_size16 != 0 || fat_size32 != 0)
}

fn first_partition(mbr: &[u8; BLOCK_SIZE]) -> Option<u32> {
    if mbr[510] != 0x55 || mbr[511] != 0xaa {
        return None;
    }
    for index in 0..4 {
        let offset = 0x1be + index * 16;
        let kind = mbr[offset + 4];
        let start = u32::from_le_bytes([
            mbr[offset + 8],
            mbr[offset + 9],
            mbr[offset + 10],
            mbr[offset + 11],
        ]);
        if start != 0 && matches!(kind, 0x04 | 0x06 | 0x07 | 0x0b | 0x0c | 0x0e) {
            return Some(start);
        }
    }
    None
}
