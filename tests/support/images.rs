#![allow(dead_code)]

use std::vec;
use std::vec::Vec;

/// A file to put in a FAT32 image's root directory.
pub struct FileSpec<'a> {
    /// The long name (written only when `long` is set).
    pub name: &'a str,
    /// The 8.3 name, as stored: 11 bytes, space padded, no dot.
    pub short: &'a str,
    /// Whether to write `name` as a long-name entry in front of the 8.3 entry.
    pub long: bool,
    /// The file's contents.
    pub data: &'a [u8],
}

const SECTOR: usize = 512;
const SECTORS_PER_CLUSTER: u8 = 1;
const RESERVED: u16 = 32;
const NUM_FATS: u8 = 2;
const FAT_SIZE: u32 = 8;
const TOTAL: u32 = 512;
const DATA_START: u32 = RESERVED as u32 + NUM_FATS as u32 * FAT_SIZE;
const ROOT_CLUSTER: u32 = 2;

/// A small FAT32 volume holding `files`, with an MBR in front of it when `with_mbr` is set.
pub fn fat32_image(with_mbr: bool, files: &[FileSpec<'_>]) -> Vec<u8> {
    let partition_start = if with_mbr { 32u32 } else { 0 };
    let mut image = vec![0u8; (partition_start + TOTAL) as usize * SECTOR];
    if with_mbr {
        let mbr = &mut image[..SECTOR];
        mbr[510] = 0x55;
        mbr[511] = 0xaa;
        let entry = 0x1be;
        mbr[entry + 4] = 0x0c;
        mbr[entry + 8..entry + 12].copy_from_slice(&partition_start.to_le_bytes());
        mbr[entry + 12..entry + 16].copy_from_slice(&TOTAL.to_le_bytes());
    }
    let volume = partition_start as usize * SECTOR;
    write_boot(&mut image[volume..volume + SECTOR]);
    let backup = image[volume..volume + SECTOR].to_vec();
    image[volume + 6 * SECTOR..volume + 7 * SECTOR].copy_from_slice(&backup);

    let fat = volume + RESERVED as usize * SECTOR;
    for copy in 0..NUM_FATS as usize {
        let base = fat + copy * FAT_SIZE as usize * SECTOR;
        set_fat(&mut image[base..], 0, 0x0fff_fff8);
        set_fat(&mut image[base..], 1, 0x0fff_ffff);
        set_fat(&mut image[base..], ROOT_CLUSTER, 0x0fff_ffff);
    }

    let mut next_cluster = ROOT_CLUSTER + 1;
    let mut clusters = Vec::new();
    for spec in files {
        let count = spec.data.len().div_ceil(SECTOR);
        let start = if count == 0 { 0 } else { next_cluster };
        let mut chain = Vec::new();
        for _ in 0..count {
            chain.push(next_cluster);
            next_cluster += 1;
        }
        clusters.push((start, chain));
    }

    for (_, chain) in &clusters {
        for (index, &cluster) in chain.iter().enumerate() {
            let value = chain.get(index + 1).copied().unwrap_or(0x0fff_ffff);
            for copy in 0..NUM_FATS as usize {
                let base = fat + copy * FAT_SIZE as usize * SECTOR;
                set_fat(&mut image[base..], cluster, value);
            }
        }
    }

    let root = volume
        + (DATA_START as usize + (ROOT_CLUSTER as usize - 2) * SECTORS_PER_CLUSTER as usize)
            * SECTOR;
    let mut offset = root;
    for (spec, (start, _)) in files.iter().zip(clusters.iter()) {
        offset = write_entry(&mut image, offset, spec, *start);
    }
    image[offset] = 0x00;

    for (spec, (_, chain)) in files.iter().zip(clusters.iter()) {
        let mut remaining = spec.data;
        for &cluster in chain {
            let cluster_offset = volume
                + (DATA_START as usize + (cluster as usize - 2) * SECTORS_PER_CLUSTER as usize)
                    * SECTOR;
            let take = remaining.len().min(SECTOR);
            image[cluster_offset..cluster_offset + take].copy_from_slice(&remaining[..take]);
            remaining = &remaining[take..];
        }
    }
    image
}

fn write_boot(boot: &mut [u8]) {
    boot[0] = 0xeb;
    boot[1] = 0x58;
    boot[2] = 0x90;
    boot[3..11].copy_from_slice(b"SWORDFSH");
    boot[11..13].copy_from_slice(&(SECTOR as u16).to_le_bytes());
    boot[13] = SECTORS_PER_CLUSTER;
    boot[14..16].copy_from_slice(&RESERVED.to_le_bytes());
    boot[16] = NUM_FATS;
    boot[17..19].copy_from_slice(&0u16.to_le_bytes());
    boot[19..21].copy_from_slice(&0u16.to_le_bytes());
    boot[21] = 0xf8;
    boot[22..24].copy_from_slice(&0u16.to_le_bytes());
    boot[24..26].copy_from_slice(&63u16.to_le_bytes());
    boot[26..28].copy_from_slice(&255u16.to_le_bytes());
    boot[28..32].copy_from_slice(&0u32.to_le_bytes());
    boot[32..36].copy_from_slice(&TOTAL.to_le_bytes());
    boot[36..40].copy_from_slice(&FAT_SIZE.to_le_bytes());
    boot[44..48].copy_from_slice(&ROOT_CLUSTER.to_le_bytes());
    boot[48..50].copy_from_slice(&1u16.to_le_bytes());
    boot[50..52].copy_from_slice(&6u16.to_le_bytes());
    boot[64] = 0x80;
    boot[66] = 0x29;
    boot[67..71].copy_from_slice(&0x1234_5678u32.to_le_bytes());
    boot[71..82].copy_from_slice(b"SDMMC      ");
    boot[82..90].copy_from_slice(b"FAT32   ");
    boot[510] = 0x55;
    boot[511] = 0xaa;
}

fn set_fat(fat: &mut [u8], cluster: u32, value: u32) {
    let offset = cluster as usize * 4;
    fat[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn short_bytes(name: &str) -> [u8; 11] {
    assert_eq!(name.len(), 11, "short name must be 11 bytes");
    let mut bytes = [b' '; 11];
    bytes.copy_from_slice(name.as_bytes());
    bytes
}

fn short_checksum(name: &[u8; 11]) -> u8 {
    let mut sum = 0u8;
    for &byte in name {
        sum = ((sum & 1) << 7).wrapping_add(sum >> 1).wrapping_add(byte);
    }
    sum
}

fn write_entry(image: &mut [u8], offset: usize, spec: &FileSpec<'_>, start: u32) -> usize {
    let short = short_bytes(spec.short);
    let mut offset = offset;
    if spec.long {
        let units: Vec<u16> = spec.name.encode_utf16().collect();
        let count = units.len().div_ceil(13);
        let checksum = short_checksum(&short);
        for index in (1..=count).rev() {
            let sequence = index as u8 | if index == count { 0x40 } else { 0 };
            let mut entry = [0u8; 32];
            entry[0] = sequence;
            entry[11] = 0x0f;
            entry[13] = checksum;
            for position in 0..13 {
                let unit_index = (index - 1) * 13 + position;
                let unit = units.get(unit_index).copied().unwrap_or(0xffff);
                let lo = match position {
                    0..=4 => 1 + position * 2,
                    5..=10 => 14 + (position - 5) * 2,
                    _ => 28 + (position - 11) * 2,
                };
                entry[lo..lo + 2].copy_from_slice(&unit.to_le_bytes());
            }
            image[offset..offset + 32].copy_from_slice(&entry);
            offset += 32;
        }
    }
    let mut entry = [0u8; 32];
    entry[0..11].copy_from_slice(&short);
    entry[11] = 0x20;
    entry[20..22].copy_from_slice(&((start >> 16) as u16).to_le_bytes());
    entry[26..28].copy_from_slice(&((start & 0xffff) as u16).to_le_bytes());
    entry[28..32].copy_from_slice(&(spec.data.len() as u32).to_le_bytes());
    image[offset..offset + 32].copy_from_slice(&entry);
    offset + 32
}

/// A file to put in an exFAT image's root directory.
pub struct ExFileSpec<'a> {
    /// The file's name.
    pub name: &'a str,
    /// The file's contents.
    pub data: &'a [u8],
    /// Store the data contiguously with no FAT chain (the exFAT NoFatChain flag).
    pub no_fat_chain: bool,
}

const EX_BOOT_SECTORS: u32 = 12;
const EX_FAT_OFFSET: u32 = 24;
const EX_FAT_LENGTH: u32 = 8;
const EX_HEAP_OFFSET: u32 = EX_FAT_OFFSET + EX_FAT_LENGTH;
const EX_TOTAL: u32 = 512;
const EX_CLUSTER_COUNT: u32 = EX_TOTAL - EX_HEAP_OFFSET;
const EX_ROOT_CLUSTER: u32 = 2;

/// A small exFAT volume holding `files`.
pub fn exfat_image(files: &[ExFileSpec<'_>]) -> Vec<u8> {
    let mut image = vec![0u8; EX_TOTAL as usize * SECTOR];
    write_exfat_boot(&mut image[..SECTOR], EX_TOTAL, EX_FAT_OFFSET, EX_FAT_LENGTH);
    write_exfat_checksum(&mut image, 0);

    let mut backup = [0u8; SECTOR];
    backup.copy_from_slice(&image[..SECTOR]);
    image[EX_BOOT_SECTORS as usize * SECTOR..][..SECTOR].copy_from_slice(&backup);
    write_exfat_checksum(&mut image, EX_BOOT_SECTORS);

    let fat = EX_FAT_OFFSET as usize * SECTOR;
    set_fat(&mut image[fat..], 0, 0xffff_fff8);
    set_fat(&mut image[fat..], 1, 0xffff_ffff);
    set_fat(&mut image[fat..], EX_ROOT_CLUSTER, 0xffff_ffff);

    let mut next_cluster = EX_ROOT_CLUSTER + 1;
    let mut clusters = Vec::new();
    for spec in files {
        let count = spec.data.len().div_ceil(SECTOR);
        let start = if count == 0 { 0 } else { next_cluster };
        let mut chain = Vec::new();
        for _ in 0..count {
            chain.push(next_cluster);
            next_cluster += 1;
        }
        clusters.push((start, chain));
    }
    for (spec, (_, chain)) in files.iter().zip(clusters.iter()) {
        if spec.no_fat_chain {
            continue;
        }
        for (index, &cluster) in chain.iter().enumerate() {
            let value = chain.get(index + 1).copied().unwrap_or(0xffff_ffff);
            set_fat(&mut image[fat..], cluster, value);
        }
    }

    let root = (EX_HEAP_OFFSET as usize + (EX_ROOT_CLUSTER as usize - 2)) * SECTOR;
    let mut offset = root;
    for (spec, (start, _)) in files.iter().zip(clusters.iter()) {
        offset = write_exfat_entry(&mut image, offset, spec, *start);
    }
    image[offset] = 0x00;

    for (spec, (_, chain)) in files.iter().zip(clusters.iter()) {
        let mut remaining = spec.data;
        for &cluster in chain {
            let cluster_offset = (EX_HEAP_OFFSET as usize + (cluster as usize - 2)) * SECTOR;
            let take = remaining.len().min(SECTOR);
            image[cluster_offset..cluster_offset + take].copy_from_slice(&remaining[..take]);
            remaining = &remaining[take..];
        }
    }
    image
}

fn write_exfat_boot(boot: &mut [u8], volume_length: u32, fat_offset: u32, fat_length: u32) {
    boot[0] = 0xeb;
    boot[1] = 0x76;
    boot[2] = 0x90;
    boot[3..11].copy_from_slice(b"EXFAT   ");
    boot[64..72].copy_from_slice(&0u64.to_le_bytes());
    boot[72..80].copy_from_slice(&(volume_length as u64).to_le_bytes());
    boot[80..84].copy_from_slice(&fat_offset.to_le_bytes());
    boot[84..88].copy_from_slice(&fat_length.to_le_bytes());
    boot[88..92].copy_from_slice(&(fat_offset + fat_length).to_le_bytes());
    boot[92..96].copy_from_slice(&EX_CLUSTER_COUNT.to_le_bytes());
    boot[96..100].copy_from_slice(&EX_ROOT_CLUSTER.to_le_bytes());
    boot[100..104].copy_from_slice(&0x1234_5678u32.to_le_bytes());
    boot[104..106].copy_from_slice(&0x0100u16.to_le_bytes());
    boot[106..108].copy_from_slice(&0u16.to_le_bytes());
    boot[108] = 9;
    boot[109] = 0;
    boot[110] = 1;
    boot[111] = 0x80;
    boot[112] = 0;
    boot[510] = 0x55;
    boot[511] = 0xaa;
}

fn write_exfat_checksum(image: &mut [u8], start: u32) {
    let mut checksum = 0u32;
    for index in 0..11u32 {
        let sector_start = (start + index) as usize * SECTOR;
        for offset in 0..SECTOR {
            if index == 0 && (offset == 106 || offset == 107 || offset == 112) {
                continue;
            }
            checksum = checksum
                .rotate_right(1)
                .wrapping_add(u32::from(image[sector_start + offset]));
        }
    }
    let checksum_sector = (start + 11) as usize * SECTOR;
    for slot in 0..SECTOR / 4 {
        image[checksum_sector + slot * 4..checksum_sector + slot * 4 + 4]
            .copy_from_slice(&checksum.to_le_bytes());
    }
}

fn write_exfat_entry(
    image: &mut [u8],
    mut offset: usize,
    spec: &ExFileSpec<'_>,
    start: u32,
) -> usize {
    let units: Vec<u16> = spec.name.encode_utf16().collect();
    let name_entries = units.len().div_ceil(15).max(1);
    let secondary_count = 1 + name_entries;

    let mut file = [0u8; 32];
    file[0] = 0x85;
    file[1] = secondary_count as u8;
    file[4..6].copy_from_slice(&0x20u16.to_le_bytes());

    let mut stream = [0u8; 32];
    stream[0] = 0xc0;
    stream[1] = if spec.no_fat_chain { 0x03 } else { 0x01 };
    stream[3] = units.len() as u8;
    stream[8..16].copy_from_slice(&(spec.data.len() as u64).to_le_bytes());
    stream[20..24].copy_from_slice(&start.to_le_bytes());
    stream[24..32].copy_from_slice(&(spec.data.len() as u64).to_le_bytes());

    let mut name_blocks = vec![[0u8; 32]; name_entries];
    for (block_index, block) in name_blocks.iter_mut().enumerate() {
        block[0] = 0xc1;
        for position in 0..15 {
            let unit_index = block_index * 15 + position;
            let unit = units.get(unit_index).copied().unwrap_or(0);
            block[2 + position * 2..4 + position * 2].copy_from_slice(&unit.to_le_bytes());
        }
    }

    let mut checksum = 0u16;
    for (index, &byte) in file.iter().enumerate() {
        if index == 2 || index == 3 {
            continue;
        }
        checksum = checksum.rotate_right(1).wrapping_add(u16::from(byte));
    }
    for block in core::iter::once(&stream).chain(name_blocks.iter()) {
        for &byte in block.iter() {
            checksum = checksum.rotate_right(1).wrapping_add(u16::from(byte));
        }
    }
    file[2..4].copy_from_slice(&checksum.to_le_bytes());

    image[offset..offset + 32].copy_from_slice(&file);
    offset += 32;
    image[offset..offset + 32].copy_from_slice(&stream);
    offset += 32;
    for block in &name_blocks {
        image[offset..offset + 32].copy_from_slice(block);
        offset += 32;
    }
    offset
}
