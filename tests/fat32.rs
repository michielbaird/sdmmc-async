mod support;

use sdmmc_async::Error;
use support::{
    block_on, fat32_image, list_root, read_all, try_read_all, Bench, Faults, FileSpec, Lcg,
};

fn mount(image: Vec<u8>) -> sdmmc_async::Volume<support::SimCard> {
    Bench::new(image).mount().unwrap()
}

#[test]
fn reads_a_short_name_file() {
    let image = fat32_image(
        false,
        &[FileSpec {
            name: "HELLO.TXT",
            short: "HELLO   TXT",
            long: false,
            data: b"Hello, world\n",
        }],
    );
    let mut volume = mount(image);
    let mut file = block_on(volume.open_file("HELLO.TXT")).unwrap();
    assert_eq!(read_all(&mut volume, &mut file), b"Hello, world\n");
}

#[test]
fn mounts_a_volume_inside_an_mbr() {
    let image = fat32_image(
        true,
        &[FileSpec {
            name: "HELLO.TXT",
            short: "HELLO   TXT",
            long: false,
            data: b"mbr\n",
        }],
    );
    let mut volume = mount(image);
    let mut file = block_on(volume.open_file("HELLO.TXT")).unwrap();
    assert_eq!(read_all(&mut volume, &mut file), b"mbr\n");
}

#[test]
fn lists_short_and_long_names() {
    let image = fat32_image(
        false,
        &[
            FileSpec {
                name: "HELLO.TXT",
                short: "HELLO   TXT",
                long: false,
                data: b"hello",
            },
            FileSpec {
                name: "a-very-long-file-name.gcode",
                short: "AVERYL~1GCO",
                long: true,
                data: b"G0 X0\n",
            },
        ],
    );
    let mut volume = mount(image);
    let names = list_root(&mut volume).unwrap();
    assert!(names.contains(&"HELLO.TXT".to_string()), "{names:?}");
    assert!(
        names.contains(&"a-very-long-file-name.gcode".to_string()),
        "{names:?}"
    );
}

#[test]
fn opens_a_long_name() {
    let image = fat32_image(
        false,
        &[FileSpec {
            name: "a-very-long-file-name.gcode",
            short: "AVERYL~1GCO",
            long: true,
            data: b"G1 X10\n",
        }],
    );
    let mut volume = mount(image);
    let mut file = block_on(volume.open_file("a-very-long-file-name.gcode")).unwrap();
    assert_eq!(read_all(&mut volume, &mut file), b"G1 X10\n");
}

#[test]
fn reads_across_a_cluster_boundary_in_small_chunks() {
    let data: Vec<u8> = (0..600).map(|index| (index % 251) as u8).collect();
    let image = fat32_image(
        false,
        &[FileSpec {
            name: "BIG.BIN",
            short: "BIG     BIN",
            long: false,
            data: &data,
        }],
    );
    let mut volume = mount(image);
    let mut file = block_on(volume.open_file("BIG.BIN")).unwrap();
    assert_eq!(read_all(&mut volume, &mut file), data);
}

#[test]
fn seeks_and_reads() {
    let data: Vec<u8> = (0..600).map(|index| (index % 251) as u8).collect();
    let image = fat32_image(
        false,
        &[FileSpec {
            name: "BIG.BIN",
            short: "BIG     BIN",
            long: false,
            data: &data,
        }],
    );
    let mut volume = mount(image);
    let mut file = block_on(volume.open_file("BIG.BIN")).unwrap();
    block_on(volume.seek(&mut file, 100)).unwrap();
    let mut buffer = [0u8; 20];
    let count = block_on(volume.read_file(&mut file, &mut buffer)).unwrap();
    assert_eq!(&buffer[..count], &data[100..120]);
}

#[test]
fn seeks_to_the_end() {
    let data: Vec<u8> = (0..600).map(|index| index as u8).collect();
    let image = fat32_image(
        false,
        &[FileSpec {
            name: "BIG.BIN",
            short: "BIG     BIN",
            long: false,
            data: &data,
        }],
    );
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
fn reports_a_missing_file() {
    let image = fat32_image(false, &[]);
    let mut volume = mount(image);
    assert!(matches!(
        block_on(volume.open_file("NOPE.TXT")),
        Err(Error::NotFound)
    ));
}

#[test]
fn rejects_fat16() {
    let mut image = fat32_image(false, &[]);
    image[22] = 1;
    image[23] = 0;
    image[17] = 0;
    image[18] = 2;
    image[36..40].copy_from_slice(&0u32.to_le_bytes());
    assert!(matches!(Bench::new(image).mount(), Err(Error::Unsupported)));
}

#[test]
fn a_read_failure_at_mount_is_a_device_error() {
    let bench = Bench::new(fat32_image(false, &[]));
    bench.set_faults(Faults {
        fail_after_reads: Some(0),
        ..Default::default()
    });
    assert!(matches!(bench.mount(), Err(Error::Device(()))));
}

#[test]
fn a_read_failure_during_a_file_read_is_a_device_error() {
    let bench = Bench::new(fat32_image(
        false,
        &[FileSpec {
            name: "HELLO.TXT",
            short: "HELLO   TXT",
            long: false,
            data: b"Hello, world\n",
        }],
    ));
    let mut volume = bench.mount().unwrap();
    let mut file = block_on(volume.open_file("HELLO.TXT")).unwrap();
    // The first file cluster is sector 49 in this image.
    bench.set_faults(Faults {
        fail_on_block: Some(49),
        ..Default::default()
    });
    let mut buffer = [0u8; 16];
    assert!(matches!(
        block_on(volume.read_file(&mut file, &mut buffer)),
        Err(Error::Device(()))
    ));
}

#[test]
fn a_corrupt_geometry_in_both_boot_sectors_is_rejected() {
    let mut image = fat32_image(false, &[]);
    // sectors_per_cluster becomes 254, not a power of two.
    for boot in [0, 6 * 512] {
        image[boot + 13] ^= 0xff;
    }
    assert!(matches!(Bench::new(image).mount(), Err(Error::BadGeometry)));
}

#[test]
fn a_cyclic_fat_chain_is_rejected() {
    let data: Vec<u8> = (0..600).map(|index| index as u8).collect();
    let mut image = fat32_image(
        false,
        &[FileSpec {
            name: "BIG.BIN",
            short: "BIG     BIN",
            long: false,
            data: &data,
        }],
    );
    let fat = 32 * 512;
    image[fat + 3 * 4..fat + 3 * 4 + 4].copy_from_slice(&3u32.to_le_bytes());
    let root = 48 * 512;
    image[root + 28..root + 32].copy_from_slice(&u32::MAX.to_le_bytes());

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
        let image = fat32_image(
            false,
            &[FileSpec {
                name: "BIG.BIN",
                short: "BIG     BIN",
                long: false,
                data: &data,
            }],
        );
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

const FAT32_ROOT: usize = (32 + 2 * 8) * 512;

fn one_file() -> Vec<u8> {
    fat32_image(
        false,
        &[FileSpec {
            name: "HELLO.TXT",
            short: "HELLO   TXT",
            long: false,
            data: b"hi\n",
        }],
    )
}

#[test]
fn a_boot_sector_whose_fat_size_overflows_is_rejected_not_a_panic() {
    let mut image = one_file();
    for boot in [0, 6 * 512] {
        image[boot + 16] = 255;
        image[boot + 36..boot + 40].copy_from_slice(&0x0200_0000u32.to_le_bytes());
    }
    assert!(matches!(Bench::new(image).mount(), Err(Error::BadGeometry)));
}

#[test]
fn a_directory_entry_pointing_at_cluster_zero_or_one_is_an_error_not_a_panic() {
    for cluster in [0u16, 1] {
        let mut image = one_file();
        image[FAT32_ROOT + 11] = 0x10;
        image[FAT32_ROOT + 20..FAT32_ROOT + 22].copy_from_slice(&0u16.to_le_bytes());
        image[FAT32_ROOT + 26..FAT32_ROOT + 28].copy_from_slice(&cluster.to_le_bytes());
        let mut volume = Bench::new(image).mount().unwrap();
        let mut dir = block_on(volume.open_dir("HELLO.TXT")).unwrap();
        assert!(
            matches!(
                block_on(volume.next_entry(&mut dir)),
                Err(Error::InvalidCluster)
            ),
            "cluster {cluster}"
        );
    }
}

#[test]
fn a_file_or_directory_cluster_past_the_end_of_the_volume_is_an_error() {
    for (attribute, high, low) in [
        (0x20u8, 0x0fffu16, 0xfff0u16),
        (0x10, 0x0fff, 0xfff0),
        (0x20, 0, 5000),
    ] {
        let mut image = one_file();
        image[FAT32_ROOT + 11] = attribute;
        image[FAT32_ROOT + 20..FAT32_ROOT + 22].copy_from_slice(&high.to_le_bytes());
        image[FAT32_ROOT + 26..FAT32_ROOT + 28].copy_from_slice(&low.to_le_bytes());
        let mut volume = Bench::new(image).mount().unwrap();
        if attribute == 0x20 {
            let mut file = block_on(volume.open_file("HELLO.TXT")).unwrap();
            let mut buffer = [0u8; 16];
            assert!(matches!(
                block_on(volume.read_file(&mut file, &mut buffer)),
                Err(Error::InvalidCluster)
            ));
        } else {
            let mut dir = block_on(volume.open_dir("HELLO.TXT")).unwrap();
            assert!(matches!(
                block_on(volume.next_entry(&mut dir)),
                Err(Error::InvalidCluster)
            ));
        }
    }
}

#[test]
fn mounts_a_fat32_volume_without_the_type_string() {
    let mut image = fat32_image(
        false,
        &[FileSpec {
            name: "HELLO.TXT",
            short: "HELLO   TXT",
            long: false,
            data: b"no string\n",
        }],
    );
    image[82..90].copy_from_slice(&[0u8; 8]);
    let mut volume = mount(image);
    let mut file = block_on(volume.open_file("HELLO.TXT")).unwrap();
    assert_eq!(read_all(&mut volume, &mut file), b"no string\n");
}

#[test]
fn mounts_from_the_backup_boot_sector_when_the_primary_is_damaged() {
    let mut image = fat32_image(
        false,
        &[FileSpec {
            name: "HELLO.TXT",
            short: "HELLO   TXT",
            long: false,
            data: b"backup\n",
        }],
    );
    image[36..40].copy_from_slice(&0u32.to_le_bytes());
    let mut volume = mount(image);
    let mut file = block_on(volume.open_file("HELLO.TXT")).unwrap();
    assert_eq!(read_all(&mut volume, &mut file), b"backup\n");
}

#[test]
fn opens_a_long_named_file_by_its_8_3_alias() {
    let image = fat32_image(
        false,
        &[FileSpec {
            name: "a-very-long-file-name.gcode",
            short: "AVERYL~1GCO",
            long: true,
            data: b"G1 X10\n",
        }],
    );
    let mut volume = mount(image);
    let mut file = block_on(volume.open_file("AVERYL~1.GCO")).unwrap();
    assert_eq!(read_all(&mut volume, &mut file), b"G1 X10\n");
}

fn damage_primary(image: &mut [u8], boot: usize, field: &str) {
    match field {
        "sectors_per_cluster" => image[boot + 13] = 3,
        "num_fats" => image[boot + 16] = 0,
        "reserved" => image[boot + 14..boot + 16].copy_from_slice(&0u16.to_le_bytes()),
        "fat_size_overflow" => {
            image[boot + 16] = 255;
            image[boot + 36..boot + 40].copy_from_slice(&0x0200_0000u32.to_le_bytes());
        }
        "root_cluster" => image[boot + 44..boot + 48].copy_from_slice(&0u32.to_le_bytes()),
        other => panic!("{other}"),
    }
}

const DAMAGED_FIELDS: [&str; 5] = [
    "sectors_per_cluster",
    "num_fats",
    "reserved",
    "fat_size_overflow",
    "root_cluster",
];

#[test]
fn a_damaged_geometry_field_in_the_primary_is_rescued_by_the_backup_boot_sector() {
    for field in DAMAGED_FIELDS {
        let mut image = one_file();
        damage_primary(&mut image, 0, field);
        let mut volume = Bench::new(image)
            .mount()
            .unwrap_or_else(|e| panic!("{field}: {e:?}"));
        let mut file = block_on(volume.open_file("HELLO.TXT")).unwrap();
        assert_eq!(read_all(&mut volume, &mut file), b"hi\n", "{field}");
    }
}

#[test]
fn the_backup_boot_sector_rescues_a_partition_inside_an_mbr() {
    for field in DAMAGED_FIELDS {
        let mut image = fat32_image(
            true,
            &[FileSpec {
                name: "HELLO.TXT",
                short: "HELLO   TXT",
                long: false,
                data: b"hi\n",
            }],
        );
        damage_primary(&mut image, 32 * 512, field);
        let mut volume = Bench::new(image)
            .mount()
            .unwrap_or_else(|e| panic!("{field}: {e:?}"));
        let mut file = block_on(volume.open_file("HELLO.TXT")).unwrap();
        assert_eq!(read_all(&mut volume, &mut file), b"hi\n", "{field}");
    }
}

#[test]
fn when_both_boot_sectors_are_bad_the_primarys_error_is_reported() {
    let mut image = one_file();
    damage_primary(&mut image, 0, "sectors_per_cluster");
    damage_primary(&mut image, 6 * 512, "num_fats");
    assert!(matches!(Bench::new(image).mount(), Err(Error::BadGeometry)));
}

#[test]
fn an_unreadable_backup_does_not_hide_the_primarys_geometry_error() {
    let mut image = one_file();
    damage_primary(&mut image, 0, "reserved");
    let bench = Bench::new(image);
    bench.set_faults(Faults {
        fail_on_block: Some(6),
        ..Default::default()
    });
    assert!(matches!(bench.mount(), Err(Error::BadGeometry)));
}

#[test]
fn a_backup_that_does_not_fit_the_card_is_not_trusted() {
    let mut image = one_file();
    damage_primary(&mut image, 0, "sectors_per_cluster");
    image[6 * 512 + 32..6 * 512 + 36].copy_from_slice(&0x00ff_ffffu32.to_le_bytes());
    assert!(matches!(Bench::new(image).mount(), Err(Error::BadGeometry)));
}

#[test]
fn a_sector_zero_that_is_neither_a_volume_nor_an_mbr_reports_no_partition() {
    let mut image = fat32_image(false, &[]);
    image[..6 * 512].fill(0);
    image[510] = 0x55;
    image[511] = 0xaa;
    image[6 * 512..7 * 512].fill(0);
    assert!(matches!(Bench::new(image).mount(), Err(Error::NoPartition)));
}

#[test]
fn a_partition_whose_boot_sector_and_backup_are_both_blank_reports_a_bad_boot_sector() {
    let mut image = fat32_image(
        true,
        &[FileSpec {
            name: "HELLO.TXT",
            short: "HELLO   TXT",
            long: false,
            data: b"hi\n",
        }],
    );
    image[32 * 512..(32 + 7) * 512].fill(0);
    assert!(matches!(
        Bench::new(image).mount(),
        Err(Error::BadBootSector)
    ));
}
