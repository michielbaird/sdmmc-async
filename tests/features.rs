#![cfg(any(feature = "fat32", feature = "exfat"))]
//! Which filesystems a build mounts follows its features: one that is built in mounts, one that is left out fails with
//! `Error::FilesystemNotEnabled`. Runs in every feature combination (see the README).

mod support;

use sdmmc_async::Error;
use support::{exfat_image, fat32_image, Bench};

#[test]
fn a_filesystem_is_mounted_exactly_when_its_feature_is_built() {
    let fat32 = Bench::new(fat32_image(false, &[])).mount();
    let exfat = Bench::new(exfat_image(&[])).mount();
    if cfg!(feature = "fat32") {
        assert!(fat32.is_ok());
    } else {
        assert_eq!(fat32.err(), Some(Error::FilesystemNotEnabled));
    }
    if cfg!(feature = "exfat") {
        assert!(exfat.is_ok());
    } else {
        assert_eq!(exfat.err(), Some(Error::FilesystemNotEnabled));
    }
}

#[test]
fn a_card_with_a_partition_table_follows_the_same_rule() {
    let fat32 = Bench::new(fat32_image(true, &[])).mount();
    if cfg!(feature = "fat32") {
        assert!(fat32.is_ok());
    } else {
        assert_eq!(fat32.err(), Some(Error::FilesystemNotEnabled));
    }
}
