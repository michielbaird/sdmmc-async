mod support;

use sdmmc_async::{send_init_clocks, Error, SpiError, SpiSdCard, Volume};
use support::{block_on, fat32_image, read_all, FileSpec, SpiBench, SpiFaults};

type Card = SpiSdCard<support::SimSpi, support::SimDelay>;

fn hello_image() -> Vec<u8> {
    fat32_image(
        false,
        &[FileSpec {
            name: "HELLO.TXT",
            short: "HELLO   TXT",
            long: false,
            data: b"Hello over SPI\n",
        }],
    )
}

fn card(bench: &SpiBench) -> Card {
    block_on(SpiSdCard::new(bench.spi(), bench.delay())).unwrap()
}

#[test]
fn mounts_a_high_capacity_card_and_reads_a_file() {
    let bench = SpiBench::new(hello_image(), true);
    let card = card(&bench);
    assert!(card.is_high_capacity());
    assert_eq!(card.blocks(), 1024);
    let mut volume: Volume<Card> = block_on(Volume::mount(card)).unwrap();
    let mut file = block_on(volume.open_file("HELLO.TXT")).unwrap();
    assert_eq!(read_all(&mut volume, &mut file), b"Hello over SPI\n");
    assert!(bench.reads() > 0);
}

#[test]
fn mounts_a_standard_capacity_card_and_reads_a_file() {
    let bench = SpiBench::new(hello_image(), false);
    let card = card(&bench);
    assert!(!card.is_high_capacity());
    assert_eq!(card.blocks(), 512);
    let mut volume: Volume<Card> = block_on(Volume::mount(card)).unwrap();
    let mut file = block_on(volume.open_file("HELLO.TXT")).unwrap();
    assert_eq!(read_all(&mut volume, &mut file), b"Hello over SPI\n");
    assert!(bench.reads() > 0);
}

#[test]
fn a_silent_card_times_out() {
    let bench = SpiBench::new(hello_image(), true);
    bench.set_faults(SpiFaults {
        silent: true,
        ..Default::default()
    });
    assert!(matches!(
        block_on(SpiSdCard::new(bench.spi(), bench.delay())),
        Err(SpiError::Timeout)
    ));
}

#[test]
fn a_bad_data_crc_is_rejected() {
    let bench = SpiBench::new(hello_image(), true);
    let mut volume: Volume<Card> = block_on(Volume::mount(card(&bench))).unwrap();
    let mut file = block_on(volume.open_file("HELLO.TXT")).unwrap();
    bench.set_faults(SpiFaults {
        bad_crc: true,
        ..Default::default()
    });
    let mut buffer = [0u8; 32];
    assert!(matches!(
        block_on(volume.read_file(&mut file, &mut buffer)),
        Err(Error::Device(SpiError::BadCrc))
    ));
}

#[test]
fn a_corrupt_data_block_is_rejected() {
    let bench = SpiBench::new(hello_image(), true);
    let mut volume: Volume<Card> = block_on(Volume::mount(card(&bench))).unwrap();
    let mut file = block_on(volume.open_file("HELLO.TXT")).unwrap();
    // The file's first data cluster is sector 49 in the synthetic image.
    bench.set_faults(SpiFaults {
        corrupt_data: Some((49, 0, 0xff)),
        ..Default::default()
    });
    let mut buffer = [0u8; 32];
    assert!(matches!(
        block_on(volume.read_file(&mut file, &mut buffer)),
        Err(Error::Device(SpiError::BadCrc))
    ));
}

#[test]
fn card_removal_is_reported() {
    let bench = SpiBench::new(hello_image(), true);
    let mut volume: Volume<Card> = block_on(Volume::mount(card(&bench))).unwrap();
    let mut file = block_on(volume.open_file("HELLO.TXT")).unwrap();
    bench.set_faults(SpiFaults {
        card_present: false,
        ..Default::default()
    });
    let mut buffer = [0u8; 32];
    assert!(matches!(
        block_on(volume.read_file(&mut file, &mut buffer)),
        Err(Error::Device(SpiError::Timeout))
    ));
}

#[test]
fn the_host_capacity_bit_is_sent_only_to_cards_that_accepted_cmd8() {
    let modern = SpiBench::new(hello_image(), true);
    card(&modern);
    assert!(modern.host_capacity_bits().iter().all(|bit| *bit));
    let legacy = SpiBench::new(hello_image(), false);
    legacy.set_legacy_v1(true);
    let _ = card(&legacy);
    assert!(!legacy.host_capacity_bits().is_empty());
    assert!(legacy.host_capacity_bits().iter().all(|bit| !*bit));
}

#[test]
fn the_device_can_be_borrowed_to_change_its_clock_after_init() {
    let bench = SpiBench::new(hello_image(), true);
    let mut card = card(&bench);
    let _device: &mut support::SimSpi = card.spi_mut();
}

#[test]
fn cmd0_is_retried_after_missing_responses() {
    let bench = SpiBench::new(hello_image(), true);
    bench.set_faults(SpiFaults {
        cmd0_failures: 2,
        ..Default::default()
    });
    let card = card(&bench);
    assert_eq!(card.blocks(), 1024);
}

#[test]
fn the_init_clocks_go_out_on_the_raw_bus_with_chip_select_high() {
    let bench = SpiBench::new(hello_image(), true);
    let mut bus = bench.bus();
    block_on(send_init_clocks(&mut bus)).unwrap();
    assert!(
        bench.deselected_clocks() >= 80 / 8,
        "need at least 74 clocks"
    );
    assert!(!bench.selected());
}

#[test]
fn a_data_token_that_follows_the_r1_with_no_gap_is_not_swallowed() {
    let bench = SpiBench::new(hello_image(), true);
    bench.set_faults(SpiFaults {
        token_gap: 0,
        ..Default::default()
    });
    let mut volume: Volume<Card> = block_on(Volume::mount(card(&bench))).unwrap();
    let mut file = block_on(volume.open_file("HELLO.TXT")).unwrap();
    assert_eq!(read_all(&mut volume, &mut file), b"Hello over SPI\n");
}

#[test]
fn a_card_that_echoes_the_wrong_voltage_pattern_is_rejected() {
    let bench = SpiBench::new(hello_image(), true);
    bench.set_faults(SpiFaults {
        bad_cmd8_echo: true,
        ..Default::default()
    });
    assert!(matches!(
        block_on(SpiSdCard::new(bench.spi(), bench.delay())),
        Err(SpiError::BadResponse(0x01))
    ));
}

fn multi_block_image() -> Vec<u8> {
    let data: Vec<u8> = (0..1500).map(|index| (index % 251) as u8).collect();
    fat32_image(
        false,
        &[FileSpec {
            name: "BIG.BIN",
            short: "BIG     BIN",
            long: false,
            data: &data,
        }],
    )
}

fn big_data() -> Vec<u8> {
    (0..1500).map(|index| (index % 251) as u8).collect()
}

/// Reads the next 512 bytes and returns how many card block reads that took and how many chip-select
/// assertions they needed (a read at a cluster boundary also fetches a FAT sector).
fn read_one_block(
    bench: &SpiBench,
    volume: &mut Volume<Card>,
    file: &mut sdmmc_async::File,
) -> (usize, usize) {
    let (assertions, reads) = (bench.assertions(), bench.reads());
    let mut buffer = [0u8; 512];
    block_on(volume.read_file(file, &mut buffer)).unwrap();
    (bench.assertions() - assertions, bench.reads() - reads)
}

#[test]
fn a_block_read_is_one_chip_select_assertion_when_the_token_is_inside_the_window() {
    let bench = SpiBench::new(multi_block_image(), true);
    let mut volume: Volume<Card> = block_on(Volume::mount(card(&bench))).unwrap();
    let mut file = block_on(volume.open_file("BIG.BIN")).unwrap();
    for _ in 0..3 {
        let (assertions, reads) = read_one_block(&bench, &mut volume, &mut file);
        assert!(reads >= 1);
        assert_eq!(assertions, reads);
    }
    assert_eq!(
        bench.bad_mosi_bytes(),
        0,
        "MOSI must idle high while the card talks"
    );
}

#[test]
fn a_strict_card_that_forgets_its_response_when_chip_select_rises_still_mounts_and_reads() {
    let bench = SpiBench::new(multi_block_image(), true);
    bench.set_faults(SpiFaults {
        drop_response_on_cs_rise: true,
        ..Default::default()
    });
    let mut volume: Volume<Card> = block_on(Volume::mount(card(&bench))).unwrap();
    let mut file = block_on(volume.open_file("BIG.BIN")).unwrap();
    assert_eq!(read_all(&mut volume, &mut file), big_data());
}

#[test]
fn a_slow_token_falls_back_to_polling_and_the_window_then_follows_the_card() {
    let bench = SpiBench::new(multi_block_image(), true);
    let mut volume: Volume<Card> = block_on(Volume::mount(card(&bench))).unwrap();
    let mut file = block_on(volume.open_file("BIG.BIN")).unwrap();
    bench.set_faults(SpiFaults {
        token_gap: 600,
        ..Default::default()
    });
    let (assertions, reads) = read_one_block(&bench, &mut volume, &mut file);
    assert!(
        assertions > reads,
        "the first read should have needed polling: {assertions} assertions for {reads} reads"
    );
    for _ in 0..2 {
        let (assertions, reads) = read_one_block(&bench, &mut volume, &mut file);
        assert_eq!(assertions, reads);
    }
}

#[test]
fn a_slow_token_on_a_strict_card_is_retried_inside_the_read_and_the_next_read_is_one_assertion() {
    let bench = SpiBench::new(multi_block_image(), true);
    let mut volume: Volume<Card> = block_on(Volume::mount(card(&bench))).unwrap();
    let mut file = block_on(volume.open_file("BIG.BIN")).unwrap();
    bench.set_faults(SpiFaults {
        token_gap: 600,
        drop_response_on_cs_rise: true,
        ..Default::default()
    });
    let (assertions, reads) = read_one_block(&bench, &mut volume, &mut file);
    assert!(
        assertions > reads,
        "the first read missed the token and repeated the command"
    );
    let (assertions, reads) = read_one_block(&bench, &mut volume, &mut file);
    assert_eq!(assertions, reads);
}

#[test]
fn the_window_shrinks_again_when_the_card_gets_faster() {
    let bench = SpiBench::new(multi_block_image(), true);
    let mut volume: Volume<Card> = block_on(Volume::mount(card(&bench))).unwrap();
    let mut file = block_on(volume.open_file("BIG.BIN")).unwrap();
    let mut again = block_on(volume.open_file("BIG.BIN")).unwrap();
    bench.set_faults(SpiFaults {
        token_gap: 600,
        ..Default::default()
    });
    read_one_block(&bench, &mut volume, &mut file);
    bench.set_faults(SpiFaults {
        token_gap: 0,
        ..Default::default()
    });
    read_one_block(&bench, &mut volume, &mut file);
    bench.set_faults(SpiFaults {
        token_gap: 100,
        drop_response_on_cs_rise: true,
        ..Default::default()
    });
    let (assertions, reads) = read_one_block(&bench, &mut volume, &mut again);
    assert!(
        assertions > reads,
        "after a fast read the window is small again, so a gap of 100 on a strict card misses once and the read repeats"
    );
}

#[test]
fn a_token_that_arrives_after_the_window_but_inside_the_over_read_still_yields_the_right_data() {
    for gap in [100, 300, 480, 520, 540, 560] {
        let bench = SpiBench::new(multi_block_image(), true);
        let mut volume: Volume<Card> = block_on(Volume::mount(card(&bench))).unwrap();
        let mut file = block_on(volume.open_file("BIG.BIN")).unwrap();
        bench.set_faults(SpiFaults {
            token_gap: gap,
            ..Default::default()
        });
        assert_eq!(read_all(&mut volume, &mut file), big_data(), "gap {gap}");
    }
}

#[test]
fn the_host_keeps_mosi_high_for_every_byte_it_reads_in_every_path() {
    for gap in [0, 2, 300, 600, 1500] {
        let bench = SpiBench::new(multi_block_image(), true);
        let mut volume: Volume<Card> = block_on(Volume::mount(card(&bench))).unwrap();
        let mut file = block_on(volume.open_file("BIG.BIN")).unwrap();
        bench.set_faults(SpiFaults {
            token_gap: gap,
            ..Default::default()
        });
        read_all(&mut volume, &mut file);
        assert_eq!(bench.bad_mosi_bytes(), 0, "gap {gap}");
    }
}

#[test]
fn the_window_learned_from_a_polled_gap_covers_the_next_read_on_a_strict_card() {
    let bench = SpiBench::new(multi_block_image(), true);
    let mut volume: Volume<Card> = block_on(Volume::mount(card(&bench))).unwrap();
    let mut file = block_on(volume.open_file("BIG.BIN")).unwrap();
    bench.set_faults(SpiFaults {
        token_gap: 1500,
        ..Default::default()
    });
    let (assertions, reads) = read_one_block(&bench, &mut volume, &mut file);
    assert!(assertions > reads, "the first read has to poll");
    bench.set_faults(SpiFaults {
        token_gap: 1500,
        drop_response_on_cs_rise: true,
        ..Default::default()
    });
    let (assertions, reads) = read_one_block(&bench, &mut volume, &mut file);
    assert_eq!(assertions, reads);
}

#[test]
fn a_strict_card_with_a_slow_token_still_initialises_because_the_first_window_is_wide() {
    let bench = SpiBench::new(hello_image(), true);
    bench.set_faults(SpiFaults {
        token_gap: 100,
        drop_response_on_cs_rise: true,
        ..Default::default()
    });
    assert!(block_on(SpiSdCard::new(bench.spi(), bench.delay())).is_ok());
}

#[test]
fn a_token_inside_the_over_read_band_completes_the_block_in_a_second_transaction() {
    let bench = SpiBench::new(multi_block_image(), true);
    let mut volume: Volume<Card> = block_on(Volume::mount(card(&bench))).unwrap();
    let mut file = block_on(volume.open_file("BIG.BIN")).unwrap();
    bench.set_faults(SpiFaults {
        token_gap: 300,
        ..Default::default()
    });
    let (assertions, reads) = read_one_block(&bench, &mut volume, &mut file);
    assert_eq!(
        assertions,
        reads + 1,
        "the block needed a second transaction"
    );
}

#[test]
fn a_strict_card_fails_that_second_transaction_once_and_the_read_repeats_as_a_single_one() {
    let bench = SpiBench::new(multi_block_image(), true);
    let mut volume: Volume<Card> = block_on(Volume::mount(card(&bench))).unwrap();
    let mut file = block_on(volume.open_file("BIG.BIN")).unwrap();
    bench.set_faults(SpiFaults {
        token_gap: 300,
        drop_response_on_cs_rise: true,
        ..Default::default()
    });
    let (assertions, reads) = read_one_block(&bench, &mut volume, &mut file);
    assert!(assertions > reads, "the read repeated");
    let (assertions, reads) = read_one_block(&bench, &mut volume, &mut file);
    assert_eq!(assertions, reads);
}

#[test]
fn a_data_error_token_is_reported_as_a_bad_response() {
    let bench = SpiBench::new(hello_image(), true);
    let mut volume: Volume<Card> = block_on(Volume::mount(card(&bench))).unwrap();
    let mut file = block_on(volume.open_file("HELLO.TXT")).unwrap();
    bench.set_faults(SpiFaults {
        data_error_token: true,
        ..Default::default()
    });
    let mut buffer = [0u8; 16];
    assert!(matches!(
        block_on(volume.read_file(&mut file, &mut buffer)),
        Err(Error::Device(SpiError::BadResponse(0x01)))
    ));
}
