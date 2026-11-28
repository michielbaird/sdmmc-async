//! Random sequences of operations on a volume, checked against a plain in-memory model of what the tree should hold.
//! After every few operations the volume is unmounted and mounted again and read back in full.

#![cfg(any(feature = "fat32", feature = "exfat"))]

mod support;

use std::collections::BTreeMap;

use sdmmc_async::testing::RamCard;
use sdmmc_async::{Error, Volume};
use support::tools::{blank_image, check, find, temp_dir};
use support::{block_on, Lcg};

type Vol = Volume<RamCard>;

/// The volume's tree as a model: directory paths and file contents, keyed by lower-case path.
#[derive(Default)]
struct Model {
    /// How many of each operation ran: create, append, overwrite, resize, remove, mkdir, rmdir, rename.
    ran: [usize; 8],
    dirs: BTreeMap<String, String>,
    files: BTreeMap<String, (String, Vec<u8>)>,
}

const NAMES: &[&str] = &[
    "a.txt",
    "b.bin",
    "Mixed.Case",
    "log-0001.csv",
    "a-fairly-long-file-name-for-a-log.gcode",
    "with space.dat",
    "ünï-ß.txt",
    "x",
    "dir1",
    "Dir2",
    "another-directory-with-a-long-name",
];

fn key(path: &str) -> String {
    path.to_lowercase()
}

fn pattern(length: usize, seed: u32) -> Vec<u8> {
    (0..length)
        .map(|index| (index as u32).wrapping_mul(31).wrapping_add(seed) as u8)
        .collect()
}

impl Model {
    fn parents(&self) -> Vec<String> {
        let mut out = vec![String::new()];
        out.extend(self.dirs.values().cloned());
        out
    }

    fn children(&self, dir: &str) -> usize {
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{}/", key(dir))
        };
        let under =
            |path: &String| path.starts_with(&prefix) && !path[prefix.len()..].contains('/');
        self.dirs.keys().filter(|p| under(p)).count()
            + self.files.keys().filter(|p| under(p)).count()
    }

    fn exists(&self, path: &str) -> bool {
        self.dirs.contains_key(&key(path)) || self.files.contains_key(&key(path))
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

fn remount(mut volume: Vol) -> Vol {
    block_on(volume.sync()).unwrap();
    block_on(Volume::mount(RamCard::new(
        volume.into_device().into_image(),
    )))
    .unwrap()
}

fn read_tree(
    volume: &mut Vol,
    dir: &str,
    out: &mut BTreeMap<String, Vec<u8>>,
    dirs: &mut Vec<String>,
) {
    let mut handle = block_on(volume.open_dir(dir)).unwrap();
    let mut entries = Vec::new();
    while let Some(entry) = block_on(volume.next_entry(&mut handle)).unwrap() {
        entries.push((
            entry.name.as_str().to_string(),
            entry.is_directory,
            entry.size,
        ));
    }
    for (name, is_dir, size) in entries {
        let path = join(dir, &name);
        if is_dir {
            dirs.push(path.clone());
            read_tree(volume, &path, out, dirs);
        } else {
            let mut file = block_on(volume.open_file(&path)).unwrap();
            let content = support::read_all(volume, &mut file);
            assert_eq!(content.len() as u64, size, "{path}");
            out.insert(path, content);
        }
    }
}

fn verify(volume: &mut Vol, model: &Model, context: &str) {
    let mut files = BTreeMap::new();
    let mut dirs = Vec::new();
    read_tree(volume, "", &mut files, &mut dirs);
    let mut found: Vec<String> = dirs.iter().map(|d| d.to_string()).collect();
    found.sort();
    let mut expected: Vec<String> = model.dirs.values().cloned().collect();
    expected.sort();
    assert_eq!(found, expected, "{context}: directories");
    let found_files: BTreeMap<String, Vec<u8>> = files;
    let expected_files: BTreeMap<String, Vec<u8>> = model
        .files
        .values()
        .map(|(path, content)| (path.clone(), content.clone()))
        .collect();
    assert_eq!(
        found_files.keys().collect::<Vec<_>>(),
        expected_files.keys().collect::<Vec<_>>(),
        "{context}: files"
    );
    for (path, content) in &expected_files {
        assert!(
            found_files[path] == *content,
            "{context}: contents of {path}"
        );
    }
}

fn run(mut volume: Vol, seed: u64, operations: usize) -> (Vol, [usize; 8]) {
    let mut random = Lcg::new(seed);
    let mut model = Model::default();
    let budget = |model: &Model| -> usize { model.files.values().map(|(_, c)| c.len()).sum() };
    for step in 0..operations {
        let context = format!("seed {seed} step {step}");
        let parents = model.parents();
        let parent = parents[random.below(parents.len() as u32) as usize].clone();
        let name = NAMES[random.below(NAMES.len() as u32) as usize];
        let path = join(&parent, name);
        let files: Vec<String> = model.files.values().map(|(p, _)| p.clone()).collect();
        let dirs: Vec<String> = model.dirs.values().cloned().collect();
        match random.below(10) {
            0..=2 => {
                // Create a file and write some of it.
                if !model.exists(&path) && budget(&model) < 60_000 {
                    let length = random.below(5000) as usize;
                    let data = pattern(length, random.next_u32());
                    let mut file = block_on(volume.create_file(&path))
                        .unwrap_or_else(|e| panic!("{context}: create {path}: {e:?}"));
                    assert_eq!(
                        block_on(volume.write_file(&mut file, &data)).unwrap(),
                        length,
                        "{context}"
                    );
                    block_on(volume.flush_file(&mut file)).unwrap();
                    model.files.insert(key(&path), (path, data));
                    model.ran[0] += 1;
                }
            }
            3 => {
                // Append.
                if let Some(target) = files
                    .get(random.below(files.len().max(1) as u32) as usize)
                    .cloned()
                {
                    if budget(&model) < 60_000 {
                        let more = pattern(random.below(3000) as usize, random.next_u32());
                        let mut file = block_on(volume.open_file(&target)).unwrap();
                        let end = file.size();
                        block_on(volume.seek(&mut file, end)).unwrap();
                        assert_eq!(
                            block_on(volume.write_file(&mut file, &more)).unwrap(),
                            more.len(),
                            "{context}"
                        );
                        block_on(volume.flush_file(&mut file)).unwrap();
                        model.files.get_mut(&key(&target)).unwrap().1.extend(&more);
                        model.ran[1] += 1;
                    }
                }
            }
            4 => {
                // Overwrite inside.
                if let Some(target) = files
                    .get(random.below(files.len().max(1) as u32) as usize)
                    .cloned()
                {
                    let content = &mut model.files.get_mut(&key(&target)).unwrap().1;
                    if !content.is_empty() {
                        let at = random.below(content.len() as u32) as usize;
                        let length = (random.below(2000) as usize).min(content.len() - at);
                        let data = pattern(length, random.next_u32());
                        let mut file = block_on(volume.open_file(&target)).unwrap();
                        block_on(volume.seek(&mut file, at as u64)).unwrap();
                        assert_eq!(
                            block_on(volume.write_file(&mut file, &data)).unwrap(),
                            length,
                            "{context}"
                        );
                        block_on(volume.flush_file(&mut file)).unwrap();
                        content[at..at + length].copy_from_slice(&data);
                        model.ran[2] += 1;
                    }
                }
            }
            5 => {
                // Truncate or extend.
                if let Some(target) = files
                    .get(random.below(files.len().max(1) as u32) as usize)
                    .cloned()
                {
                    let new_length = random.below(6000) as usize;
                    let old_length = model.files[&key(&target)].1.len();
                    if new_length <= old_length
                        || budget_after(&model, &target, new_length) < 60_000
                    {
                        let mut file = block_on(volume.open_file(&target)).unwrap();
                        block_on(volume.set_len(&mut file, new_length as u64))
                            .unwrap_or_else(|e| panic!("{context}: set_len {target}: {e:?}"));
                        block_on(volume.flush_file(&mut file)).unwrap();
                        model
                            .files
                            .get_mut(&key(&target))
                            .unwrap()
                            .1
                            .resize(new_length, 0);
                        model.ran[3] += 1;
                    }
                }
            }
            6 => {
                // Remove a file.
                if let Some(target) = files
                    .get(random.below(files.len().max(1) as u32) as usize)
                    .cloned()
                {
                    block_on(volume.remove_file(&target))
                        .unwrap_or_else(|e| panic!("{context}: remove {target}: {e:?}"));
                    model.files.remove(&key(&target));
                    model.ran[4] += 1;
                }
            }
            7 => {
                // Make a directory.
                if !model.exists(&path) && dirs.len() < 6 && parent.matches('/').count() < 2 {
                    block_on(volume.create_dir(&path))
                        .unwrap_or_else(|e| panic!("{context}: mkdir {path}: {e:?}"));
                    model.dirs.insert(key(&path), path);
                    model.ran[5] += 1;
                }
            }
            8 => {
                // Remove a directory: only when empty.
                if let Some(target) = dirs
                    .get(random.below(dirs.len().max(1) as u32) as usize)
                    .cloned()
                {
                    let result = block_on(volume.remove_dir(&target));
                    if model.children(&target) == 0 {
                        result.unwrap_or_else(|e| panic!("{context}: rmdir {target}: {e:?}"));
                        model.dirs.remove(&key(&target));
                        model.ran[6] += 1;
                    } else {
                        assert!(
                            matches!(result, Err(Error::NotEmpty)),
                            "{context}: rmdir {target}: {result:?}"
                        );
                    }
                }
            }
            _ => {
                // Rename a file to a new name somewhere.
                if let Some(target) = files
                    .get(random.below(files.len().max(1) as u32) as usize)
                    .cloned()
                {
                    if !model.exists(&path) {
                        block_on(volume.rename(&target, &path)).unwrap_or_else(|e| {
                            panic!("{context}: rename {target} -> {path}: {e:?}")
                        });
                        let (_, content) = model.files.remove(&key(&target)).unwrap();
                        model.files.insert(key(&path), (path, content));
                        model.ran[7] += 1;
                    }
                }
            }
        }
        if step % 15 == 14 {
            volume = remount(volume);
            verify(&mut volume, &model, &context);
        }
    }
    volume = remount(volume);
    verify(&mut volume, &model, &format!("seed {seed} end"));
    (volume, model.ran)
}

fn budget_after(model: &Model, target: &str, new_length: usize) -> usize {
    let others: usize = model
        .files
        .iter()
        .filter(|(k, _)| **k != key(target))
        .map(|(_, (_, c))| c.len())
        .sum();
    others + new_length
}

#[cfg(feature = "fat32")]
#[test]
fn random_operations_on_a_small_fat32_volume_match_the_model() {
    let mut ran = [0usize; 8];
    for seed in 0..25 {
        let volume = block_on(Volume::mount(RamCard::new(support::fat32_image(
            false,
            &[],
        ))))
        .unwrap();
        let (_, counts) = run(volume, seed, 200);
        for (total, count) in ran.iter_mut().zip(counts) {
            *total += count;
        }
    }
    assert!(
        ran.iter().all(|count| *count > 30),
        "operations that ran: {ran:?}"
    );
}

#[cfg(feature = "exfat")]
#[test]
fn random_operations_on_a_small_exfat_volume_match_the_model() {
    let mut ran = [0usize; 8];
    for seed in 0..25 {
        let volume = block_on(Volume::mount(RamCard::new(support::exfat_image_writable(
            &[],
        ))))
        .unwrap();
        let (_, counts) = run(volume, seed, 200);
        for (total, count) in ran.iter_mut().zip(counts) {
            *total += count;
        }
    }
    assert!(
        ran.iter().all(|count| *count > 30),
        "operations that ran: {ran:?}"
    );
}

/// The same on volumes formatted by the real tools, and checked by the real checkers afterwards.
#[test]
fn random_operations_leave_volumes_the_checkers_accept() {
    let mut cases: Vec<(&str, std::path::PathBuf, &[&str], std::path::PathBuf)> = Vec::new();
    #[cfg(feature = "fat32")]
    if let (Some(mkfs), Some(fsck)) = (find("mkfs.vfat"), find("fsck.vfat")) {
        cases.push(("fat32", mkfs, &["-F", "32", "-n", "MODEL"], fsck));
    }
    #[cfg(feature = "exfat")]
    if let (Some(mkfs), Some(fsck)) = (find("mkfs.exfat"), find("fsck.exfat")) {
        cases.push(("exfat", mkfs, &[], fsck));
    }
    if cases.is_empty() {
        eprintln!("skipped: mkfs/fsck tools not found");
        return;
    }
    for (name, mkfs, args, fsck) in cases {
        let dir = temp_dir(&format!("model-{name}"));
        let blank = blank_image(&dir, "blank.img", 40, &mkfs, args);
        for seed in 100..104 {
            let volume = block_on(Volume::mount(RamCard::new(blank.clone()))).unwrap();
            let (volume, _) = run(volume, seed, 200);
            check(&dir, &volume.into_device().into_image(), &fsck, &["-n"]);
        }
    }
}
