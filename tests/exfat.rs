mod support;

use sdmmc_async::Error;
use support::{
    block_on, exfat_image, list_root, read_all, try_read_all, Bench, ExFileSpec, Faults, Lcg,
};

fn mount(image: Vec<u8>) -> sdmmc_async::Volume<support::SimCard> {
    Bench::new(image).mount().unwrap()
}

#[test]
fn reads_a_short_name_file() {
    let image = exfat_image(&[ExFileSpec {
        name: "HELLO.TXT",
        data: b"Hello, exFAT\n",
        no_fat_chain: true,
    }]);
    let mut volume = mount(image);
    let mut file = block_on(volume.open_file("HELLO.TXT")).unwrap();
    assert_eq!(read_all(&mut volume, &mut file), b"Hello, exFAT\n");
}

#[test]
fn reads_a_name_longer_than_one_entry() {
    let image = exfat_image(&[ExFileSpec {
        name: "a-very-long-exfat-file.gcode",
        data: b"G1 X10 Y20\n",
        no_fat_chain: true,
    }]);
    let mut volume = mount(image);
    let mut file = block_on(volume.open_file("a-very-long-exfat-file.gcode")).unwrap();
    assert_eq!(read_all(&mut volume, &mut file), b"G1 X10 Y20\n");
}

#[test]
fn reads_a_contiguous_file_across_clusters() {
    let data: Vec<u8> = (0..600).map(|index| (index % 251) as u8).collect();
    let image = exfat_image(&[ExFileSpec {
        name: "BIG.BIN",
        data: &data,
        no_fat_chain: true,
    }]);
    let mut volume = mount(image);
    let mut file = block_on(volume.open_file("BIG.BIN")).unwrap();
    assert_eq!(read_all(&mut volume, &mut file), data);
}

#[test]
fn reads_a_fat_chained_file_across_clusters() {
    let data: Vec<u8> = (0..600).map(|index| (index % 241) as u8).collect();
    let image = exfat_image(&[ExFileSpec {
        name: "BIG.BIN",
        data: &data,
        no_fat_chain: false,
    }]);
    let mut volume = mount(image);
    let mut file = block_on(volume.open_file("BIG.BIN")).unwrap();
    assert_eq!(read_all(&mut volume, &mut file), data);
}

#[test]
fn falls_back_to_the_backup_boot_region() {
    let mut image = exfat_image(&[ExFileSpec {
        name: "HELLO.TXT",
        data: b"backup\n",
        no_fat_chain: true,
    }]);
    image[100] ^= 0xff;
    let mut volume = mount(image);
    let mut file = block_on(volume.open_file("HELLO.TXT")).unwrap();
    assert_eq!(read_all(&mut volume, &mut file), b"backup\n");
}

#[test]
fn reports_a_missing_file() {
    let image = exfat_image(&[]);
    let mut volume = mount(image);
    assert!(matches!(
        block_on(volume.open_file("NOPE.TXT")),
        Err(Error::NotFound)
    ));
}

#[test]
fn seeks_to_the_end() {
    let data: Vec<u8> = (0..600).map(|index| index as u8).collect();
    let image = exfat_image(&[ExFileSpec {
        name: "BIG.BIN",
        data: &data,
        no_fat_chain: true,
    }]);
    let mut volume = mount(image);
    let mut file = block_on(volume.open_file("BIG.BIN")).unwrap();
    block_on(volume.seek(&mut file, 600)).unwrap();
    let mut buffer = [0u8; 8];
    assert_eq!(
        block_on(volume.read_file(&mut file, &mut buffer)).unwrap(),
        0
    );
}

#[test]
fn a_read_failure_at_mount_is_a_device_error() {
    let bench = Bench::new(exfat_image(&[]));
    bench.set_faults(Faults {
        fail_after_reads: Some(0),
        ..Default::default()
    });
    assert!(matches!(bench.mount(), Err(Error::Device(()))));
}

#[test]
fn a_corrupt_main_and_backup_boot_region_is_rejected() {
    let bench = Bench::new(exfat_image(&[]));
    bench.patch(11 * 512, &[0u8; 32]);
    bench.patch(23 * 512, &[0u8; 32]);
    assert!(matches!(bench.mount(), Err(Error::BadBootSector)));
}

#[test]
fn a_cyclic_exfat_chain_is_rejected() {
    let data: Vec<u8> = (0..600).map(|index| index as u8).collect();
    let mut image = exfat_image(&[ExFileSpec {
        name: "BIG.BIN",
        data: &data,
        no_fat_chain: false,
    }]);
    let fat = 24 * 512;
    image[fat + 3 * 4..fat + 3 * 4 + 4].copy_from_slice(&3u32.to_le_bytes());
    let root = 32 * 512;
    image[root + 32 + 24..root + 32 + 32].copy_from_slice(&u64::MAX.to_le_bytes());
    let mut checksum = 0u16;
    for (index, &byte) in image[root..root + 96].iter().enumerate() {
        if index == 2 || index == 3 {
            continue;
        }
        checksum = checksum.rotate_right(1).wrapping_add(u16::from(byte));
    }
    image[root + 2..root + 4].copy_from_slice(&checksum.to_le_bytes());

    let bench = Bench::new(image);
    bench.set_faults(Faults {
        max_reads: Some(20_000),
        ..Default::default()
    });
    let mut volume = bench.mount().unwrap();
    let mut file = block_on(volume.open_file("BIG.BIN")).unwrap();
    assert!(matches!(
        try_read_all(&mut volume, &mut file),
        Err(Error::InvalidCluster)
    ));
}

#[test]
fn random_faults_never_panic_hang_or_read_outside_the_card() {
    let data: Vec<u8> = (0..600).map(|index| index as u8).collect();
    for seed in 0..300u64 {
        let image = exfat_image(&[ExFileSpec {
            name: "BIG.BIN",
            data: &data,
            no_fat_chain: seed % 2 == 0,
        }]);
        let bench = Bench::new(image);
        let mut rng = Lcg::new(seed);
        let kind = rng.below(4);
        let block = rng.below(600);
        let offset = rng.below(512) as usize;
        bench.set_faults(Faults {
            fail_after_reads: if kind == 0 {
                Some(rng.below(50) as usize)
            } else {
                None
            },
            fail_on_block: if kind == 1 { Some(block) } else { None },
            corrupt_block: if kind == 2 {
                Some((block, offset, (rng.next_u32() | 1) as u8))
            } else {
                None
            },
            max_reads: Some(5_000),
        });

        if let Ok(mut volume) = bench.mount() {
            if let Ok(mut file) = block_on(volume.open_file("BIG.BIN")) {
                let _ = try_read_all(&mut volume, &mut file);
            }
            let _ = list_root(&mut volume);
        }

        assert_eq!(bench.out_of_range(), 0, "seed {seed} read outside the card");
        assert!(
            bench.reads() <= 5_000,
            "seed {seed}: {} reads",
            bench.reads()
        );
    }
}

const EXFAT_ROOT: usize = (24 + 8) * 512;

fn three_files() -> Vec<u8> {
    exfat_image(&[
        ExFileSpec {
            name: "A.TXT",
            data: b"aaa\n",
            no_fat_chain: true,
        },
        ExFileSpec {
            name: "B.TXT",
            data: b"bbb\n",
            no_fat_chain: true,
        },
        ExFileSpec {
            name: "C.TXT",
            data: b"ccc\n",
            no_fat_chain: true,
        },
    ])
}

fn with_leading_entry(mut image: Vec<u8>, first_bytes: &[u8]) -> Vec<u8> {
    let entries: Vec<u8> = image[EXFAT_ROOT..EXFAT_ROOT + 9 * 32].to_vec();
    let mut lead = [0u8; 32];
    lead[..first_bytes.len()].copy_from_slice(first_bytes);
    image[EXFAT_ROOT..EXFAT_ROOT + 32].copy_from_slice(&lead);
    image[EXFAT_ROOT + 32..EXFAT_ROOT + 32 + entries.len()].copy_from_slice(&entries);
    image
}

#[test]
fn a_volume_label_does_not_hide_the_files_after_it() {
    let mut label = vec![0x83, 6];
    for unit in "MYCARD".encode_utf16() {
        label.extend(unit.to_le_bytes());
    }
    let mut volume = mount(with_leading_entry(three_files(), &label));
    assert_eq!(
        list_root(&mut volume).unwrap(),
        vec!["A.TXT", "B.TXT", "C.TXT"]
    );
}

#[test]
fn an_allocation_bitmap_entry_with_flags_set_does_not_hide_the_files_after_it() {
    let mut volume = mount(with_leading_entry(three_files(), &[0x81, 1]));
    assert_eq!(
        list_root(&mut volume).unwrap(),
        vec!["A.TXT", "B.TXT", "C.TXT"]
    );
}

#[test]
fn a_deleted_file_set_is_skipped_and_the_files_after_it_are_listed() {
    let mut image = three_files();
    for index in 0..3 {
        image[EXFAT_ROOT + index * 32] &= 0x7f;
    }
    let mut volume = mount(image);
    assert_eq!(list_root(&mut volume).unwrap(), vec!["B.TXT", "C.TXT"]);
}

fn recompute_set_checksum(image: &mut [u8], at: usize) {
    let count = image[at + 1] as usize + 1;
    let mut checksum = 0u16;
    for (index, &byte) in image[at..at + count * 32].iter().enumerate() {
        if index == 2 || index == 3 {
            continue;
        }
        checksum = checksum.rotate_right(1).wrapping_add(u16::from(byte));
    }
    image[at + 2..at + 4].copy_from_slice(&checksum.to_le_bytes());
}

#[test]
fn a_directory_entry_pointing_outside_the_volume_is_an_error_not_a_panic() {
    for cluster in [0u32, 1, 0xffff_fff0] {
        let mut image = three_files();
        image[EXFAT_ROOT + 4] = 0x10;
        image[EXFAT_ROOT + 32 + 20..EXFAT_ROOT + 32 + 24].copy_from_slice(&cluster.to_le_bytes());
        image[EXFAT_ROOT + 32 + 24..EXFAT_ROOT + 32 + 32].copy_from_slice(&512u64.to_le_bytes());
        recompute_set_checksum(&mut image, EXFAT_ROOT);
        let mut volume = mount(image);
        let mut dir = block_on(volume.open_dir("A.TXT")).unwrap();
        assert!(
            matches!(
                block_on(volume.next_entry(&mut dir)),
                Err(Error::InvalidCluster)
            ),
            "cluster {cluster:#x}"
        );
    }
}

#[test]
fn a_file_cluster_outside_the_volume_is_an_error_when_read() {
    let mut image = three_files();
    image[EXFAT_ROOT + 32 + 20..EXFAT_ROOT + 32 + 24]
        .copy_from_slice(&0x00ff_ffffu32.to_le_bytes());
    recompute_set_checksum(&mut image, EXFAT_ROOT);
    let mut volume = mount(image);
    let mut file = block_on(volume.open_file("A.TXT")).unwrap();
    let mut buffer = [0u8; 8];
    assert!(matches!(
        block_on(volume.read_file(&mut file, &mut buffer)),
        Err(Error::InvalidCluster)
    ));
}
