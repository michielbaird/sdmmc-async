//! A cut in power at any point of a sequence of operations must leave a volume that mounts, still holds every file the
//! sequence did not touch, and passes the filesystem checker (apart from being marked in use).

#![cfg(any(feature = "fat32", feature = "exfat"))]

mod support;

use sdmmc_async::testing::RamCard;
use sdmmc_async::{BlockDevice, BlockWrite, Volume};
use support::block_on;
use support::tools::{blank_image, find, run, temp_dir};

fn pattern(length: usize, seed: u8) -> Vec<u8> {
    (0..length)
        .map(|index| (index as u8).wrapping_mul(37).wrapping_add(seed))
        .collect()
}

type Outcome = Result<(), String>;

fn run_op<T, E: std::fmt::Debug>(result: Result<T, E>) -> Result<T, String> {
    result.map_err(|error| format!("{error:?}"))
}

fn put<D: BlockDevice + BlockWrite>(volume: &mut Volume<D>, path: &str, data: &[u8]) -> Outcome
where
    D::Error: std::fmt::Debug,
{
    let mut file = run_op(block_on(volume.create_file(path)))?;
    for chunk in data.chunks(700) {
        run_op(block_on(volume.write_file(&mut file, chunk)))?;
    }
    run_op(block_on(volume.flush_file(&mut file)))
}

fn get<D: BlockDevice>(volume: &mut Volume<D>, path: &str) -> Option<Vec<u8>>
where
    D::Error: std::fmt::Debug,
{
    let mut file = block_on(volume.open_file(path)).ok()?;
    Some(support::read_all(volume, &mut file))
}

/// The files that exist before the sequence and are never touched by it.
fn populate<D: BlockDevice + BlockWrite>(volume: &mut Volume<D>)
where
    D::Error: std::fmt::Debug,
{
    block_on(volume.create_dir("keep")).unwrap();
    put(volume, "keep/old-and-untouched.bin", &pattern(9000, 1)).unwrap();
    put(volume, "also-kept.txt", &pattern(1200, 2)).unwrap();
    block_on(volume.sync()).unwrap();
}

/// The operations a cut can land in the middle of. After the cut the card has stopped taking writes, so an operation
/// can fail on what it reads back; that ends the sequence, as losing power would.
fn sequence<D: BlockDevice + BlockWrite>(volume: &mut Volume<D>) -> Outcome
where
    D::Error: std::fmt::Debug,
{
    run_op(block_on(volume.create_dir("work")))?;
    put(volume, "work/one.txt", &pattern(3000, 3))?;
    put(
        volume,
        "a-rather-long-name-for-a-file.gcode",
        &pattern(20_000, 4),
    )?;
    let mut file = run_op(block_on(volume.open_file("work/one.txt")))?;
    let end = file.size();
    run_op(block_on(volume.seek(&mut file, end)))?;
    run_op(block_on(volume.write_file(&mut file, &pattern(2500, 5))))?;
    run_op(block_on(volume.flush_file(&mut file)))?;
    run_op(block_on(
        volume.remove_file("a-rather-long-name-for-a-file.gcode"),
    ))?;
    put(volume, "work/moved.gcode", &pattern(20_000, 4))?;
    run_op(block_on(volume.remove_file("work/one.txt")))?;
    run_op(block_on(volume.create_dir("more")))?;
    put(volume, "more/x.bin", &pattern(1000, 6))?;
    let mut file = run_op(block_on(volume.open_file("work/moved.gcode")))?;
    run_op(block_on(volume.set_len(&mut file, 4000)))?;
    run_op(block_on(volume.flush_file(&mut file)))?;
    run_op(block_on(volume.remove_file("more/x.bin")))?;
    run_op(block_on(volume.remove_dir("more")))?;
    run_op(block_on(volume.sync()))
}

struct Case {
    name: &'static str,
    mkfs: std::path::PathBuf,
    mkfs_args: &'static [&'static str],
    fsck: std::path::PathBuf,
}

/// What a checker says about a volume that lost power while being written, none of which is damage: it was marked in use,
/// a cluster was allocated that no entry points to yet, the free-space counts are stale, one FAT copy lags the other.
fn benign(line: &str) -> bool {
    // A shortened file whose clusters were not freed yet: its size is already the new one, the chain is longer.
    if line.contains("cluster chain length is >") || line.contains("Truncating file to") {
        return true;
    }
    // A shortened file whose clusters were not freed yet (exFAT's wording).
    if line.contains("more clusters are allocated") {
        return true;
    }
    // An entry set that was being written across a sector boundary and is incomplete: no reader looks at it, a checker
    // offers to delete it. (A wrongly built set shows up in `os_write`, which allows none of this.)
    if line.contains("unknown entry type") || line.contains("the checksum of a file is wrong") {
        return true;
    }
    // Long-name entries whose short entry was deleted.
    if line.contains("found outside a LFN sequence") {
        return true;
    }
    const ALLOWED: &[&str] = &[
        "fsck.fat",
        "FATs differ but appear to be intact",
        "Using first FAT",
        "Reclaimed",
        "Dirty bit is set",
        "Automatically removing dirty bit",
        "Free cluster summary wrong",
        "Auto-correcting",
        "Leaving filesystem unchanged",
        "exfatprogs",
        "ERROR: the volume is dirty",
    ];
    // The path a message is about; what it says follows on the next lines, which are checked on their own.
    line.trim().is_empty()
        || line.starts_with('/')
        || ALLOWED.iter().any(|allowed| line.contains(allowed))
        || (line.contains(" files, ") && line.contains(" clusters"))
}

fn cases() -> Vec<Case> {
    let mut out = Vec::new();
    #[cfg(feature = "fat32")]
    if let (Some(mkfs), Some(fsck)) = (find("mkfs.vfat"), find("fsck.vfat")) {
        out.push(Case {
            name: "fat32",
            mkfs,
            mkfs_args: &["-F", "32", "-n", "CUT"],
            fsck,
        });
    }
    #[cfg(feature = "exfat")]
    if let (Some(mkfs), Some(fsck)) = (find("mkfs.exfat"), find("fsck.exfat")) {
        out.push(Case {
            name: "exfat",
            mkfs,
            mkfs_args: &[],
            fsck,
        });
    }
    out
}

#[test]
fn a_cut_in_power_anywhere_leaves_a_consistent_volume() {
    let cases = cases();
    if cases.is_empty() {
        eprintln!("skipped: mkfs/fsck tools not found");
        return;
    }
    for case in cases {
        let dir = temp_dir(case.name);
        let blank = blank_image(&dir, "blank.img", 40, &case.mkfs, case.mkfs_args);

        // A volume with its untouched files, and how many writes the sequence makes.
        let mut volume = block_on(Volume::mount(RamCard::new(blank))).unwrap();
        populate(&mut volume);
        let prepared = volume.into_device().into_image();
        let mut volume = block_on(Volume::mount(RamCard::new(prepared.clone()))).unwrap();
        sequence(&mut volume).unwrap();
        let total = volume.into_device().writes();
        assert!(total > 20, "{total}");

        let mut unexpected: Vec<String> = Vec::new();
        // Every cut is checked with `SDMMC_POWER_LOSS_STRIDE=1`; the default skips some to keep the run short.
        let stride: usize = std::env::var("SDMMC_POWER_LOSS_STRIDE")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(5)
            .max(1);
        for cut in (0..total).step_by(stride).chain(std::iter::once(total - 1)) {
            let mut card = RamCard::new(prepared.clone());
            card.lose_power_after(cut);
            let mut volume = block_on(Volume::mount(card)).unwrap();
            let _ = sequence(&mut volume);
            let image = volume.into_device().into_image();

            let mut volume =
                block_on(Volume::mount(RamCard::new(image.clone()))).unwrap_or_else(|error| {
                    panic!("{}: cut {cut}: does not mount: {error:?}", case.name)
                });
            assert_eq!(
                get(&mut volume, "keep/old-and-untouched.bin"),
                Some(pattern(9000, 1)),
                "{}: cut {cut}",
                case.name
            );
            assert_eq!(
                get(&mut volume, "also-kept.txt"),
                Some(pattern(1200, 2)),
                "{}: cut {cut}",
                case.name
            );

            let path = dir.join("cut.img");
            std::fs::write(&path, &image).unwrap();
            let (_, output) = run(&case.fsck, &["-n", path.to_str().unwrap()]);
            for line in output.lines().filter(|line| !benign(line)) {
                unexpected.push(format!("{}: cut {cut} of {total}: {line}", case.name));
            }
        }
        assert!(
            unexpected.is_empty(),
            "the checker found more than a volume marked in use, a leaked cluster or stale free-space counts:\n{}",
            unexpected.join("\n")
        );
    }
}

/// A rename writes the new entries before it removes the old ones: a cut can leave the file under both names, never under
/// neither, and the data is intact whichever name it is opened by.
#[test]
fn a_cut_in_power_during_a_rename_never_loses_the_file() {
    let cases = cases();
    if cases.is_empty() {
        eprintln!("skipped: mkfs tools not found");
        return;
    }
    for case in cases {
        let dir = temp_dir(&format!("{}-rename", case.name));
        let blank = blank_image(&dir, "blank.img", 40, &case.mkfs, case.mkfs_args);
        let mut volume = block_on(Volume::mount(RamCard::new(blank))).unwrap();
        block_on(volume.create_dir("target")).unwrap();
        put(
            &mut volume,
            "a-rather-long-name-for-a-file.gcode",
            &pattern(20_000, 4),
        )
        .unwrap();
        put(&mut volume, "short.txt", &pattern(900, 5)).unwrap();
        block_on(volume.sync()).unwrap();
        let prepared = volume.into_device().into_image();

        let renames = [
            (
                "a-rather-long-name-for-a-file.gcode",
                "target/moved-somewhere-else.gcode",
                20_000,
                4,
            ),
            (
                "a-rather-long-name-for-a-file.gcode",
                "renamed-in-place-with-a-long-name.gcode",
                20_000,
                4,
            ),
            ("short.txt", "other.txt", 900, 5),
        ];
        for (from, to, length, seed) in renames {
            let mut volume = block_on(Volume::mount(RamCard::new(prepared.clone()))).unwrap();
            block_on(volume.rename(from, to)).unwrap();
            let total = volume.into_device().writes();
            for cut in 0..=total {
                let mut card = RamCard::new(prepared.clone());
                card.lose_power_after(cut);
                let mut volume = block_on(Volume::mount(card)).unwrap();
                let _ = block_on(volume.rename(from, to));
                let image = volume.into_device().into_image();
                let mut volume = block_on(Volume::mount(RamCard::new(image))).unwrap();
                let found = get(&mut volume, from).or_else(|| get(&mut volume, to));
                assert_eq!(
                    found,
                    Some(pattern(length, seed)),
                    "{}: {from} -> {to}: cut {cut} of {total}",
                    case.name
                );
            }
        }
    }
}
