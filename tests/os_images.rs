//! Validation against filesystem images written by tools that are not this
//! crate: `mkfs.vfat` + `mtools` for FAT32, `mkfs.exfat` for an OS-formatted
//! exFAT volume, and `ferrosys` for a populated exFAT volume (the container
//! has no root and no FUSE, so it cannot mount exFAT to copy files with the
//! kernel). The tests skip, with a message, when the tools are not installed.
//!
//! Point `SDMMC_FS_TOOLS` at a tree with `sbin/mkfs.vfat`,
//! `usr/bin/mcopy`, `usr/bin/mmd` and `usr/sbin/mkfs.exfat`, and
//! `SDMMC_FERROSYS` at the `ferrosys` binary.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use support::{block_on, read_all, Bench};

struct Tools {
    mkfs_vfat: PathBuf,
    mkfs_exfat: PathBuf,
    mcopy: PathBuf,
    mmd: PathBuf,
    ferrosys: Option<PathBuf>,
}

fn find(name: &str) -> Option<PathBuf> {
    if let Some(root) = std::env::var_os("SDMMC_FS_TOOLS").map(PathBuf::from) {
        for candidate in [
            root.join("sbin").join(name),
            root.join("usr/sbin").join(name),
            root.join("usr/bin").join(name),
        ] {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn tools() -> Option<Tools> {
    let ferrosys = std::env::var_os("SDMMC_FERROSYS")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .or_else(|| find("ferrosys"));
    Some(Tools {
        mkfs_vfat: find("mkfs.vfat")?,
        mkfs_exfat: find("mkfs.exfat")?,
        mcopy: find("mcopy")?,
        mmd: find("mmd")?,
        ferrosys,
    })
}

fn run(command: &Path, args: &[&str]) {
    let output = Command::new(command)
        .args(args)
        .env("MTOOLS_SKIP_CHECK", "1")
        .output()
        .unwrap_or_else(|error| panic!("failed to run {}: {error}", command.display()));
    assert!(
        output.status.success(),
        "{} {:?} failed:\nstdout: {}\nstderr: {}",
        command.display(),
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn seed(dir: &Path) {
    fs::create_dir_all(dir.join("subdir")).unwrap();
    fs::write(dir.join("hello.txt"), b"Hello from the OS\n").unwrap();
    fs::write(dir.join("a-very-long-file-name.gcode"), b"G1 X10 Y20\n").unwrap();
    fs::write(dir.join("subdir/nested.gcode"), b"G28\n").unwrap();
    let binary: Vec<u8> = (0..600).map(|index| (index % 251) as u8).collect();
    fs::write(dir.join("binary.bin"), binary).unwrap();
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sdmmc-async-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn fat32_image(tools: &Tools, dir: &Path) -> Vec<u8> {
    let seed_dir = dir.join("seed");
    seed(&seed_dir);
    let image = dir.join("fat32.img");
    let file = fs::File::create(&image).unwrap();
    file.set_len(64 * 1024 * 1024).unwrap();
    drop(file);

    run(
        &tools.mkfs_vfat,
        &["-F", "32", "-n", "SDMMC", image.to_str().unwrap()],
    );
    let image_arg = image.to_str().unwrap();
    run(&tools.mmd, &["-i", image_arg, "::SUBDIR"]);
    run(
        &tools.mcopy,
        &[
            "-i",
            image_arg,
            seed_dir.join("hello.txt").to_str().unwrap(),
            "::HELLO.TXT",
        ],
    );
    run(
        &tools.mcopy,
        &[
            "-i",
            image_arg,
            seed_dir
                .join("a-very-long-file-name.gcode")
                .to_str()
                .unwrap(),
            "::a-very-long-file-name.gcode",
        ],
    );
    run(
        &tools.mcopy,
        &[
            "-i",
            image_arg,
            seed_dir.join("binary.bin").to_str().unwrap(),
            "::BINARY.BIN",
        ],
    );
    run(
        &tools.mcopy,
        &[
            "-i",
            image_arg,
            seed_dir.join("subdir/nested.gcode").to_str().unwrap(),
            "::SUBDIR/NESTED.GCO",
        ],
    );
    fs::read(image).unwrap()
}

fn exfat_os_image(tools: &Tools, dir: &Path) -> Vec<u8> {
    let image = dir.join("exfat-os.img");
    let file = fs::File::create(&image).unwrap();
    file.set_len(64 * 1024 * 1024).unwrap();
    drop(file);
    run(&tools.mkfs_exfat, &[image.to_str().unwrap()]);
    fs::read(image).unwrap()
}

fn exfat_populated_image(tools: &Tools, dir: &Path) -> Option<Vec<u8>> {
    let ferrosys = tools.ferrosys.as_ref()?;
    let seed_dir = dir.join("seed");
    seed(&seed_dir);
    let image = dir.join("exfat-ferrosys.img");
    run(
        ferrosys,
        &[
            "format",
            "--type",
            "exfat",
            "--volume-serial",
            "1234abcd",
            "--size",
            "auto",
            "--owner",
            "0:0",
            "--accept-loss",
            "all",
            "--from-dir",
            seed_dir.to_str().unwrap(),
            image.to_str().unwrap(),
        ],
    );
    Some(fs::read(image).unwrap())
}

#[test]
fn a_fat32_image_from_the_os_is_read_correctly() {
    let Some(tools) = tools() else {
        eprintln!("skipping: install dosfstools and mtools (or set SDMMC_FS_TOOLS)");
        return;
    };
    let dir = temp_dir("fat32-os");
    let image = fat32_image(&tools, &dir);
    let mut volume = Bench::new(image).mount().unwrap();

    let names = support::list_root(&mut volume).unwrap();
    for expected in [
        "SUBDIR",
        "HELLO.TXT",
        "a-very-long-file-name.gcode",
        "BINARY.BIN",
    ] {
        assert!(names.iter().any(|name| name == expected), "{names:?}");
    }

    let mut file = block_on(volume.open_file("HELLO.TXT")).unwrap();
    assert_eq!(read_all(&mut volume, &mut file), b"Hello from the OS\n");

    let mut file = block_on(volume.open_file("a-very-long-file-name.gcode")).unwrap();
    assert_eq!(read_all(&mut volume, &mut file), b"G1 X10 Y20\n");

    let mut file = block_on(volume.open_file("BINARY.BIN")).unwrap();
    let expected: Vec<u8> = (0..600).map(|index| (index % 251) as u8).collect();
    assert_eq!(read_all(&mut volume, &mut file), expected);

    let mut subdir = block_on(volume.open_dir("SUBDIR")).unwrap();
    let entries = block_on(volume.next_entry(&mut subdir)).unwrap().unwrap();
    assert_eq!(entries.name.as_str(), "NESTED.GCO");

    let mut file = block_on(volume.open_file("SUBDIR/NESTED.GCO")).unwrap();
    assert_eq!(read_all(&mut volume, &mut file), b"G28\n");

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_os_formatted_exfat_volume_is_accepted() {
    let Some(tools) = tools() else {
        eprintln!("skipping: install exfatprogs (or set SDMMC_FS_TOOLS)");
        return;
    };
    let dir = temp_dir("exfat-os");
    let image = exfat_os_image(&tools, &dir);
    let mut volume = Bench::new(image).mount().unwrap();
    let names = support::list_root(&mut volume).unwrap();
    assert!(names.is_empty(), "{names:?}");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_exfat_image_written_by_an_independent_tool_is_read_correctly() {
    let Some(tools) = tools() else {
        eprintln!("skipping: install exfatprogs (or set SDMMC_FS_TOOLS)");
        return;
    };
    let dir = temp_dir("exfat-populated");
    let Some(image) = exfat_populated_image(&tools, &dir) else {
        eprintln!("skipping: install ferrosys (or set SDMMC_FERROSYS)");
        return;
    };
    let mut volume = Bench::new(image).mount().unwrap();

    let names = support::list_root(&mut volume).unwrap();
    for expected in [
        "hello.txt",
        "a-very-long-file-name.gcode",
        "binary.bin",
        "subdir",
    ] {
        assert!(names.iter().any(|name| name == expected), "{names:?}");
    }

    let mut file = block_on(volume.open_file("hello.txt")).unwrap();
    assert_eq!(read_all(&mut volume, &mut file), b"Hello from the OS\n");

    let mut file = block_on(volume.open_file("a-very-long-file-name.gcode")).unwrap();
    assert_eq!(read_all(&mut volume, &mut file), b"G1 X10 Y20\n");

    let mut file = block_on(volume.open_file("binary.bin")).unwrap();
    let expected: Vec<u8> = (0..600).map(|index| (index % 251) as u8).collect();
    assert_eq!(read_all(&mut volume, &mut file), expected);

    let mut file = block_on(volume.open_file("subdir/nested.gcode")).unwrap();
    assert_eq!(read_all(&mut volume, &mut file), b"G28\n");

    fs::remove_dir_all(dir).unwrap();
}
