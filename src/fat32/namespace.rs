//! Creating, removing and renaming files and directories on FAT32: directory entries, long names and 8.3 aliases.

use super::write::SHORT_FREE;
use super::{short_checksum, Directory, EntryLoc, Volume, ATTR_DIRECTORY, ATTR_LFN};
use crate::name::{is_valid_name as valid_name, MAX_NAME};
use crate::time::Timestamp;
use crate::{BlockDevice, BlockWrite, Error, File, BLOCK_SIZE};

const ATTR_ARCHIVE: u8 = 0x20;
/// The most long-name entries a name of `MAX_NAME` UTF-16 units can need.
const MAX_LFN_ENTRIES: usize = MAX_NAME.div_ceil(13);
/// Long-name entries, then the short entry.
const MAX_SET: usize = MAX_LFN_ENTRIES + 1;

/// What it takes to store a name: its 8.3 form, the case bits for it, and whether long-name entries are needed.
struct NamePlan {
    /// The 8 + 3 bytes, space padded, upper case, with no numeric tail yet.
    short: [u8; 11],
    /// Bit 3: the base is lower case, bit 4: the extension is.
    case_flags: u8,
    needs_long_name: bool,
    /// The 8.3 form lost something and needs a `~N` tail to be unique.
    lossy: bool,
}

fn valid_short_char(character: char) -> bool {
    character.is_ascii_uppercase()
        || character.is_ascii_lowercase()
        || character.is_ascii_digit()
        || "$%'-_@~`!(){}^#&".contains(character)
}

fn plan_name(name: &str) -> NamePlan {
    let (stem, extension) = match name.rfind('.') {
        Some(0) | None => (name, ""),
        Some(at) => (&name[..at], &name[at + 1..]),
    };
    let dots_in_stem = stem.contains('.');
    let simple = |text: &str| text.chars().all(valid_short_char);
    let fits = !stem.is_empty()
        && stem.len() <= 8
        && extension.len() <= 3
        && !dots_in_stem
        && !name.starts_with('.')
        && simple(stem)
        && simple(extension);
    let has_lower = |text: &str| text.chars().any(|c| c.is_ascii_lowercase());
    let has_upper = |text: &str| text.chars().any(|c| c.is_ascii_uppercase());
    let mixed =
        (has_lower(stem) && has_upper(stem)) || (has_lower(extension) && has_upper(extension));

    let mut short = [b' '; 11];
    let mut lossy = false;
    let mut put = |text: &str, at: usize, room: usize, lossy: &mut bool| {
        let mut count = 0;
        for character in text.chars() {
            if character == ' ' || character == '.' {
                *lossy = true;
                continue;
            }
            let byte = if character.is_ascii() && valid_short_char(character) {
                character.to_ascii_uppercase() as u8
            } else {
                *lossy = true;
                b'_'
            };
            if count == room {
                *lossy = true;
                break;
            }
            short[at + count] = byte;
            count += 1;
        }
    };
    put(stem, 0, 8, &mut lossy);
    put(extension, 8, 3, &mut lossy);
    if name.starts_with('.') {
        lossy = true;
    }

    let case_flags = if fits && !mixed {
        (if has_lower(stem) { 0x08 } else { 0 }) | (if has_lower(extension) { 0x10 } else { 0 })
    } else {
        0
    };
    NamePlan {
        short,
        case_flags,
        needs_long_name: !(fits && !mixed),
        lossy: lossy && !(fits && !mixed) && !(fits && mixed),
    }
}

/// The decimal digits of `number`, to put after a `~`.
fn tail(number: u32, out: &mut [u8; 8]) -> usize {
    let mut digits = [0u8; 8];
    let mut count = 0;
    let mut rest = number;
    while rest > 0 {
        digits[count] = b'0' + (rest % 10) as u8;
        rest /= 10;
        count += 1;
    }
    out[0] = b'~';
    for index in 0..count {
        out[1 + index] = digits[count - 1 - index];
    }
    count + 1
}

/// What a new entry starts with; `template` is the entry it replaces, when renaming, whose times it keeps.
struct Fresh<'a> {
    attributes: u8,
    first_cluster: u32,
    size: u32,
    template: Option<&'a [u8; 32]>,
}

/// What goes into the entries of a new file or directory.
struct NewEntry<'a> {
    name: &'a str,
    short: [u8; 11],
    case_flags: u8,
    long_name: bool,
    attributes: u8,
    first_cluster: u32,
    size: u32,
    now: Timestamp,
}

/// The long-name entries for `name`, last one first as they are stored, then the short entry. Returns how many.
fn build_set(new: &NewEntry<'_>, out: &mut [[u8; 32]; MAX_SET]) -> usize {
    let checksum = short_checksum(&new.short);
    let mut count = 0;
    if new.long_name {
        let mut units = [0xffffu16; MAX_LFN_ENTRIES * 13];
        let mut length = 0;
        for unit in new.name.encode_utf16() {
            units[length] = unit;
            length += 1;
        }
        if length < units.len() {
            units[length] = 0;
        }
        let entries = length.div_ceil(13);
        for sequence in (1..=entries).rev() {
            let mut entry = [0u8; 32];
            entry[0] = sequence as u8 | if sequence == entries { 0x40 } else { 0 };
            entry[11] = ATTR_LFN;
            entry[13] = checksum;
            let chunk = &units[(sequence - 1) * 13..sequence * 13];
            let positions = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
            for (unit, at) in chunk.iter().zip(positions) {
                entry[at..at + 2].copy_from_slice(&unit.to_le_bytes());
            }
            out[count] = entry;
            count += 1;
        }
    }
    let mut entry = [0u8; 32];
    entry[..11].copy_from_slice(&new.short);
    entry[11] = new.attributes;
    entry[12] = new.case_flags;
    let date = new.now.fat_date().to_le_bytes();
    let time = new.now.fat_time().to_le_bytes();
    entry[14..16].copy_from_slice(&time);
    entry[16..18].copy_from_slice(&date);
    entry[18..20].copy_from_slice(&date);
    entry[20..22].copy_from_slice(&((new.first_cluster >> 16) as u16).to_le_bytes());
    entry[22..24].copy_from_slice(&time);
    entry[24..26].copy_from_slice(&date);
    entry[26..28].copy_from_slice(&(new.first_cluster as u16).to_le_bytes());
    entry[28..32].copy_from_slice(&new.size.to_le_bytes());
    out[count] = entry;
    count + 1
}

impl<D: BlockDevice + BlockWrite> Volume<D> {
    /// Moves a directory position on by one entry. `Ok(false)` at the end of the directory's clusters (the position is
    /// then unchanged).
    async fn step(&mut self, at: &mut Directory) -> Result<bool, Error<D::Error>> {
        let mut next = *at;
        next.entry_in_sector += 1;
        if next.entry_in_sector == BLOCK_SIZE as u32 / 32 {
            next.entry_in_sector = 0;
            next.sector_in_cluster += 1;
            if next.sector_in_cluster == u32::from(self.sectors_per_cluster) {
                next.sector_in_cluster = 0;
                match self.next_cluster(next.cluster).await? {
                    Some(cluster) => {
                        next.clusters_walked += 1;
                        if next.clusters_walked > self.cluster_count {
                            return Err(Error::InvalidCluster);
                        }
                        next.cluster = cluster;
                    }
                    None => return Ok(false),
                }
            }
        }
        *at = next;
        Ok(true)
    }

    fn entry_sector(&self, at: &Directory) -> Result<u32, Error<D::Error>> {
        Ok(self.cluster_sector(at.cluster)? + at.sector_in_cluster)
    }

    async fn raw_entry(&mut self, at: &Directory) -> Result<[u8; 32], Error<D::Error>> {
        let sector = self.entry_sector(at)?;
        let offset = at.entry_in_sector as usize * 32;
        let data = self.sector(sector).await?;
        let mut raw = [0u8; 32];
        raw.copy_from_slice(&data[offset..offset + 32]);
        Ok(raw)
    }

    /// Fills every sector of a cluster with zeros.
    async fn zero_cluster(&mut self, cluster: u32) -> Result<(), Error<D::Error>> {
        let first = self.cluster_sector(cluster)?;
        let zeros = [0u8; BLOCK_SIZE];
        for sector in 0..u32::from(self.sectors_per_cluster) {
            self.write_sector(first + sector, &zeros).await?;
        }
        Ok(())
    }

    /// Adds a zeroed cluster to the end of a directory and returns a position at its first entry.
    async fn grow_directory(&mut self, last: &Directory) -> Result<Directory, Error<D::Error>> {
        let cluster = self.allocate_cluster(Some(last.cluster)).await?;
        self.zero_cluster(cluster).await?;
        let mut at = *last;
        at.cluster = cluster;
        at.sector_in_cluster = 0;
        at.entry_in_sector = 0;
        at.clusters_walked += 1;
        Ok(at)
    }

    /// Finds `count` consecutive free entries in the directory starting at `start`, adding clusters when it is full.
    async fn find_free_run(
        &mut self,
        start: u32,
        count: usize,
    ) -> Result<Directory, Error<D::Error>> {
        let mut at = Directory::new(start);
        let mut run_start = at;
        let mut run = 0usize;
        // After the end-of-directory marker every entry is free, whatever it holds.
        let mut after_end = false;
        loop {
            let raw = self.raw_entry(&at).await?;
            let free = after_end || raw[0] == 0x00 || raw[0] == SHORT_FREE;
            if raw[0] == 0x00 {
                after_end = true;
            }
            if free {
                if run == 0 {
                    run_start = at;
                }
                run += 1;
                if run == count {
                    return Ok(run_start);
                }
            } else {
                run = 0;
            }
            if self.step(&mut at).await? {
                continue;
            }
            // Out of clusters: the rest of the run goes in new ones.
            let per_cluster = self.sectors_per_cluster as usize * BLOCK_SIZE / 32;
            loop {
                let grown = self.grow_directory(&at).await?;
                if run == 0 {
                    run_start = grown;
                }
                run += per_cluster;
                if run >= count {
                    return Ok(run_start);
                }
                at = grown;
            }
        }
    }

    /// Whether a directory has an entry whose 8.3 name is `short`.
    async fn short_name_taken(
        &mut self,
        start: u32,
        short: &[u8; 11],
    ) -> Result<bool, Error<D::Error>> {
        let mut at = Directory::new(start);
        loop {
            let raw = self.raw_entry(&at).await?;
            if raw[0] == 0x00 {
                return Ok(false);
            }
            if raw[0] != SHORT_FREE && raw[11] != ATTR_LFN && raw[..11] == short[..] {
                return Ok(true);
            }
            if !self.step(&mut at).await? {
                return Ok(false);
            }
        }
    }

    /// Writes a set of entries starting at `first`, one write per sector, then makes sure an end-of-directory marker still
    /// follows them if they used up the old one. Returns where the last entry went.
    async fn write_set(
        &mut self,
        first: &Directory,
        entries: &[[u8; 32]],
    ) -> Result<Directory, Error<D::Error>> {
        let mut at = *first;
        let mut last = *first;
        let mut ended = false;
        let mut index = 0;
        while index < entries.len() {
            let sector = self.entry_sector(&at)?;
            let mut block = *self.sector(sector).await?;
            loop {
                let offset = at.entry_in_sector as usize * 32;
                if block[offset] == 0x00 {
                    ended = true;
                }
                block[offset..offset + 32].copy_from_slice(&entries[index]);
                last = at;
                index += 1;
                if index == entries.len() {
                    break;
                }
                let (cluster, sector_in_cluster) = (at.cluster, at.sector_in_cluster);
                if !self.step(&mut at).await? {
                    return Err(Error::InvalidCluster);
                }
                if at.cluster != cluster || at.sector_in_cluster != sector_in_cluster {
                    break;
                }
            }
            self.write_sector(sector, &block).await?;
        }
        if ended {
            let mut next = last;
            if self.step(&mut next).await? {
                let sector = self.entry_sector(&next)?;
                let offset = next.entry_in_sector as usize * 32;
                if self.sector(sector).await?[offset] != 0x00 {
                    self.modify_sector(sector, |block| block[offset] = 0x00)
                        .await?;
                }
            }
        }
        Ok(last)
    }

    /// Splits a path into the directory that holds it (its first cluster) and the last name. `forbidden` is a directory
    /// the walk must not pass through.
    async fn resolve_parent<'p>(
        &mut self,
        path: &'p str,
        forbidden: Option<u32>,
    ) -> Result<(u32, &'p str), Error<D::Error>> {
        let mut parts = path
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .peekable();
        let mut cluster = self.root_cluster;
        let mut last = None;
        while let Some(part) = parts.next() {
            if part == ".." {
                return Err(Error::InvalidName);
            }
            if parts.peek().is_some() {
                let entry = self.find_in(cluster, part).await?;
                if !entry.is_directory {
                    return Err(Error::NotADirectory);
                }
                if forbidden == Some(entry.first_cluster) {
                    return Err(Error::InvalidOperation);
                }
                cluster = entry.first_cluster;
            } else {
                last = Some(part);
            }
        }
        Ok((cluster, last.ok_or(Error::InvalidName)?))
    }

    /// Picks the 8.3 name for `name` in the directory: a numeric tail is added when the plain form is not enough.
    async fn pick_short_name(
        &mut self,
        directory: u32,
        plan: &NamePlan,
    ) -> Result<[u8; 11], Error<D::Error>> {
        if !plan.lossy && !self.short_name_taken(directory, &plan.short).await? {
            return Ok(plan.short);
        }
        let base_length = plan.short[..8]
            .iter()
            .rposition(|b| *b != b' ')
            .map_or(1, |i| i + 1);
        for number in 1..1_000_000u32 {
            let mut digits = [0u8; 8];
            let tail_length = tail(number, &mut digits);
            let mut short = plan.short;
            let keep = base_length.min(8 - tail_length);
            short[..8].fill(b' ');
            short[..keep].copy_from_slice(&plan.short[..keep]);
            short[keep..keep + tail_length].copy_from_slice(&digits[..tail_length]);
            if !self.short_name_taken(directory, &short).await? {
                return Ok(short);
            }
        }
        Err(Error::NoSpace)
    }

    /// Adds an entry for `name` to a directory and returns where it landed.
    async fn add_entry(
        &mut self,
        directory: u32,
        name: &str,
        fresh: Fresh<'_>,
        now: Timestamp,
    ) -> Result<EntryLoc, Error<D::Error>> {
        let Fresh {
            attributes,
            first_cluster,
            size,
            template,
        } = fresh;
        let plan = plan_name(name);
        let short = self.pick_short_name(directory, &plan).await?;
        // The case bits only describe a short name that is the name itself.
        let case_flags = if plan.needs_long_name {
            0
        } else {
            plan.case_flags
        };
        let mut set = [[0u8; 32]; MAX_SET];
        let new = NewEntry {
            name,
            short,
            case_flags,
            long_name: plan.needs_long_name,
            attributes,
            first_cluster,
            size,
            now,
        };
        let count = build_set(&new, &mut set);
        if let Some(template) = template {
            // A renamed entry keeps the times of the original.
            set[count - 1][13..20].copy_from_slice(&template[13..20]);
            set[count - 1][22..26].copy_from_slice(&template[22..26]);
        }
        let first = self.find_free_run(directory, count).await?;
        let short_at = self.write_set(&first, &set[..count]).await?;
        Ok(EntryLoc {
            first,
            short: short_at,
            count: count as u8,
            parent: directory,
        })
    }

    /// Fails with `AlreadyExists` when the directory has `name` (ignoring case), as a long name or an 8.3 name.
    async fn require_absent(&mut self, directory: u32, name: &str) -> Result<(), Error<D::Error>> {
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
        if !valid_name(name) {
            return Err(Error::InvalidName);
        }
        self.require_absent(directory, name).await?;
        self.mark_dirty().await?;
        let loc = self
            .add_entry(
                directory,
                name,
                Fresh {
                    attributes: ATTR_ARCHIVE,
                    first_cluster: 0,
                    size: 0,
                    template: None,
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
            #[cfg(feature = "exfat")]
            no_fat_chain: false,
            loc: crate::fs::EntryLoc::Fat32(loc),
            dirty: false,
        })
    }

    pub(crate) async fn create_dir(
        &mut self,
        path: &str,
        now: Timestamp,
    ) -> Result<(), Error<D::Error>> {
        let (parent, name) = self.resolve_parent(path, None).await?;
        if !valid_name(name) {
            return Err(Error::InvalidName);
        }
        self.require_absent(parent, name).await?;
        self.mark_dirty().await?;
        // The new directory is ready (with `.` and `..`) before it is linked into its parent.
        let cluster = self.allocate_cluster(None).await?;
        self.zero_cluster(cluster).await?;
        let up = if parent == self.root_cluster {
            0
        } else {
            parent
        };
        let mut dots = [[0u8; 32]; MAX_SET];
        let dot_entry = |name, short: &[u8; 11], first_cluster| NewEntry {
            name,
            short: *short,
            case_flags: 0,
            long_name: false,
            attributes: ATTR_DIRECTORY,
            first_cluster,
            size: 0,
            now,
        };
        build_set(&dot_entry(".", b".          ", cluster), &mut dots);
        let dot = dots[0];
        build_set(&dot_entry("..", b"..         ", up), &mut dots);
        let dotdot = dots[0];
        let sector = self.cluster_sector(cluster)?;
        let mut block = [0u8; BLOCK_SIZE];
        block[..32].copy_from_slice(&dot);
        block[32..64].copy_from_slice(&dotdot);
        self.write_sector(sector, &block).await?;
        if let Err(error) = self
            .add_entry(
                parent,
                name,
                Fresh {
                    attributes: ATTR_DIRECTORY,
                    first_cluster: cluster,
                    size: 0,
                    template: None,
                },
                now,
            )
            .await
        {
            let _ = self.free_chain(cluster).await;
            return Err(error);
        }
        Ok(())
    }

    /// Marks every entry of a set deleted, one write per sector, so a set within one sector goes in a single write.
    async fn delete_set(&mut self, loc: &EntryLoc) -> Result<(), Error<D::Error>> {
        let mut at = loc.first;
        let mut remaining = loc.count;
        while remaining > 0 {
            let sector = self.entry_sector(&at)?;
            let mut block = *self.sector(sector).await?;
            loop {
                block[at.entry_in_sector as usize * 32] = SHORT_FREE;
                remaining -= 1;
                if remaining == 0 {
                    break;
                }
                let (cluster, sector_in_cluster) = (at.cluster, at.sector_in_cluster);
                if !self.step(&mut at).await? {
                    return Err(Error::InvalidCluster);
                }
                if at.cluster != cluster || at.sector_in_cluster != sector_in_cluster {
                    break;
                }
            }
            self.write_sector(sector, &block).await?;
        }
        Ok(())
    }

    pub(crate) async fn remove_file(&mut self, path: &str) -> Result<(), Error<D::Error>> {
        let entry = self.find_path(path).await?;
        if entry.is_directory {
            return Err(Error::NotAFile);
        }
        let loc = entry.loc.fat32().ok_or(Error::InvalidOperation)?;
        self.mark_dirty().await?;
        self.delete_set(&loc).await?;
        if entry.first_cluster >= 2 {
            self.free_chain(entry.first_cluster).await?;
        }
        Ok(())
    }

    pub(crate) async fn remove_dir(&mut self, path: &str) -> Result<(), Error<D::Error>> {
        let entry = self.find_path(path).await?;
        if !entry.is_directory {
            return Err(Error::NotADirectory);
        }
        let loc = entry.loc.fat32().ok_or(Error::InvalidOperation)?;
        let mut contents = Directory::new(entry.first_cluster);
        if self.next_entry(&mut contents).await?.is_some() {
            return Err(Error::NotEmpty);
        }
        self.mark_dirty().await?;
        self.delete_set(&loc).await?;
        if entry.first_cluster >= 2 {
            self.free_chain(entry.first_cluster).await?;
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
        let old = entry.loc.fat32().ok_or(Error::InvalidOperation)?;
        let forbidden = entry.is_directory.then_some(entry.first_cluster);
        let (directory, name) = self.resolve_parent(to, forbidden).await?;
        if !valid_name(name) {
            return Err(Error::InvalidName);
        }
        match self.find_in(directory, name).await {
            Ok(existing) => {
                // Renaming an entry to the same name in another case is allowed.
                let same = matches!(
                    existing.loc,
                    crate::fs::EntryLoc::Fat32(other)
                        if other.short.cluster == old.short.cluster
                            && other.short.sector_in_cluster == old.short.sector_in_cluster
                            && other.short.entry_in_sector == old.short.entry_in_sector
                );
                if !same {
                    return Err(Error::AlreadyExists);
                }
            }
            Err(Error::NotFound) => {}
            Err(other) => return Err(other),
        }
        self.mark_dirty().await?;
        let raw = self.raw_entry(&old.short).await?;
        let size = u32::from_le_bytes([raw[28], raw[29], raw[30], raw[31]]);

        // The new entries go in before the old ones come out, so a cut in power can leave the file under both names but never
        // under neither.
        let attributes = raw[11];
        self.add_entry(
            directory,
            name,
            Fresh {
                attributes,
                first_cluster: entry.first_cluster,
                size,
                template: Some(&raw),
            },
            now,
        )
        .await?;
        if entry.is_directory && directory != old.parent {
            let up = if directory == self.root_cluster {
                0
            } else {
                directory
            };
            let sector = self.cluster_sector(entry.first_cluster)?;
            self.modify_sector(sector, |block| {
                block[32 + 20..32 + 22].copy_from_slice(&((up >> 16) as u16).to_le_bytes());
                block[32 + 26..32 + 28].copy_from_slice(&(up as u16).to_le_bytes());
            })
            .await?;
        }
        self.delete_set(&old).await?;
        Ok(())
    }
}
