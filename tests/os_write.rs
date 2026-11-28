//! Volumes this crate has written, checked by the tools everyone else uses (`fsck.vfat`, `fsck.exfat`).

mod support;

use sdmmc_async::testing::RamCard;
use sdmmc_async::Volume;
use support::block_on;
use support::tools::{blank_image, check, find, temp_dir};

fn pattern(length: usize, seed: u8) -> Vec<u8> {
    (0..length)
        .map(|index| (index as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn put<D: sdmmc_async::BlockDevice + sdmmc_async::BlockWrite>(
    volume: &mut Volume<D>,
    path: &str,
    data: &[u8],
) where
    D::Error: std::fmt::Debug,
{
    let mut file = block_on(volume.create_file(path)).unwrap();
    for chunk in data.chunks(1000) {
        assert_eq!(
            block_on(volume.write_file(&mut file, chunk)).unwrap(),
            chunk.len()
        );
    }
    block_on(volume.flush_file(&mut file)).unwrap();
}

#[cfg(feature = "fat32")]
#[test]
fn a_fat32_volume_this_crate_wrote_passes_fsck() {
    let (Some(mkfs), Some(fsck)) = (find("mkfs.vfat"), find("fsck.vfat")) else {
        eprintln!("skipped: mkfs.vfat / fsck.vfat not found");
        return;
    };
    let dir = temp_dir("fat32-write");
    let image = blank_image(&dir, "fat32.img", 64, &mkfs, &["-F", "32", "-n", "SDMMC"]);
    let mut volume = block_on(Volume::mount(RamCard::new(image))).unwrap();

    block_on(volume.create_dir("GCODE")).unwrap();
    block_on(volume.create_dir("GCODE/Old Stuff")).unwrap();
    put(&mut volume, "readme.txt", b"hello\n");
    put(&mut volume, "GCODE/benchy.gcode", &pattern(300_000, 1));
    put(
        &mut volume,
        "GCODE/a-rather-long-file-name.gcode",
        &pattern(70_000, 2),
    );
    put(&mut volume, "GCODE/Old Stuff/x.bin", &pattern(5000, 3));
    for index in 0..40 {
        put(
            &mut volume,
            &format!("many/../file{index}.dat").replace("many/../", ""),
            &pattern(100 + index, 4),
        );
    }
    block_on(volume.rename("GCODE/benchy.gcode", "GCODE/Old Stuff/benchy-v1.gcode")).unwrap();
    block_on(volume.remove_file("file3.dat")).unwrap();
    block_on(volume.remove_file("GCODE/a-rather-long-file-name.gcode")).unwrap();
    block_on(volume.rename("GCODE/Old Stuff", "Archive")).unwrap();
    let mut file = block_on(volume.open_file("readme.txt")).unwrap();
    block_on(volume.seek(&mut file, 6)).unwrap();
    block_on(volume.write_file(&mut file, b"appended\n")).unwrap();
    block_on(volume.set_len(&mut file, 10)).unwrap();
    block_on(volume.flush_file(&mut file)).unwrap();
    block_on(volume.sync()).unwrap();

    let image = volume.into_device().into_image();
    check(&dir, &image, &fsck, &["-n", "-v"]);
}
