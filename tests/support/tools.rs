//! The filesystem tools the OS-image tests use. A tool that is not installed makes its test skip, with a message.
//! `SDMMC_FS_TOOLS` can point at a tree with `sbin/`, `usr/bin/` and `usr/sbin/` holding them.

use std::path::{Path, PathBuf};
use std::process::Command;

pub fn find(name: &str) -> Option<PathBuf> {
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

pub fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sdmmc-async-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Runs a tool and returns (success, combined output).
pub fn run(command: &Path, args: &[&str]) -> (bool, String) {
    let output = Command::new(command)
        .args(args)
        .env("MTOOLS_SKIP_CHECK", "1")
        .output()
        .unwrap_or_else(|error| panic!("failed to run {}: {error}", command.display()));
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), text)
}

/// A blank image of `megabytes` formatted by `mkfs`.
pub fn blank_image(dir: &Path, name: &str, megabytes: u64, mkfs: &Path, args: &[&str]) -> Vec<u8> {
    let image = dir.join(name);
    let file = std::fs::File::create(&image).unwrap();
    file.set_len(megabytes * 1024 * 1024).unwrap();
    drop(file);
    let mut all: Vec<&str> = args.to_vec();
    let path = image.to_str().unwrap().to_string();
    all.push(&path);
    let (ok, text) = run(mkfs, &all);
    assert!(ok, "{} failed: {text}", mkfs.display());
    std::fs::read(image).unwrap()
}

/// Writes `image` out and runs a checker over it; the checker passing is the test.
pub fn check(dir: &Path, image: &[u8], fsck: &Path, args: &[&str]) {
    let path = dir.join("check.img");
    std::fs::write(&path, image).unwrap();
    let mut all: Vec<&str> = args.to_vec();
    let text = path.to_str().unwrap().to_string();
    all.push(&text);
    let (ok, output) = run(fsck, &all);
    assert!(ok, "{} reported problems:\n{output}", fsck.display());
}
