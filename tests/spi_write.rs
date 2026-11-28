mod support;

use sdmmc_async::{BlockDevice, BlockWrite, SpiError, SpiSdCard, BLOCK_SIZE};
use support::{block_on, fat32_image, SpiBench, SpiFaults};

type Card = SpiSdCard<support::SimSpi, support::SimDelay>;

fn bench(high_capacity: bool) -> SpiBench {
    SpiBench::new(fat32_image(false, &[]), high_capacity)
}

fn card(bench: &SpiBench) -> Card {
    block_on(SpiSdCard::new(bench.spi(), bench.delay())).unwrap()
}

fn pattern(seed: u8) -> [u8; BLOCK_SIZE] {
    let mut block = [0u8; BLOCK_SIZE];
    for (index, byte) in block.iter_mut().enumerate() {
        *byte = (index as u8).wrapping_mul(7).wrapping_add(seed);
    }
    block
}

#[test]
fn a_written_block_reads_back_on_high_and_standard_capacity_cards() {
    for high_capacity in [true, false] {
        let bench = bench(high_capacity);
        let mut card = card(&bench);
        let block = pattern(3);
        block_on(card.write_block(100, &block)).unwrap();
        let mut read = [0u8; BLOCK_SIZE];
        block_on(card.read_block(100, &mut read)).unwrap();
        assert_eq!(read, block);
        assert_eq!(bench.writes(), 1);
        assert_eq!(&bench.image()[100 * 512..101 * 512], &block);
    }
}

#[test]
fn neighbouring_blocks_are_untouched() {
    let bench = bench(true);
    let before = bench.image();
    let mut card = card(&bench);
    block_on(card.write_block(40, &pattern(9))).unwrap();
    let after = bench.image();
    assert_eq!(before[..40 * 512], after[..40 * 512]);
    assert_eq!(before[41 * 512..], after[41 * 512..]);
}

#[test]
fn the_busy_wait_outlasts_a_slow_card() {
    let bench = bench(true);
    let mut card = card(&bench);
    bench.set_faults(SpiFaults {
        write_busy_clocks: 300,
        ..Default::default()
    });
    block_on(card.write_block(10, &pattern(1))).unwrap();
    assert_eq!(bench.writes(), 1);
}

#[test]
fn a_card_that_never_finishes_programming_times_out() {
    let bench = bench(true);
    let mut card = card(&bench);
    bench.set_faults(SpiFaults {
        write_busy_forever: true,
        ..Default::default()
    });
    assert!(matches!(
        block_on(card.write_block(10, &pattern(1))),
        Err(SpiError::Timeout)
    ));
}

#[test]
fn a_crc_error_response_is_repeated() {
    let bench = bench(true);
    let mut card = card(&bench);
    bench.set_faults(SpiFaults {
        write_crc_errors: 2,
        ..Default::default()
    });
    block_on(card.write_block(10, &pattern(5))).unwrap();
    assert_eq!(&bench.image()[10 * 512..11 * 512], &pattern(5));
}

#[test]
fn a_persistent_crc_error_is_reported() {
    let bench = bench(true);
    let mut card = card(&bench);
    bench.set_faults(SpiFaults {
        write_crc_errors: 10,
        ..Default::default()
    });
    assert!(matches!(
        block_on(card.write_block(10, &pattern(5))),
        Err(SpiError::BadCrc)
    ));
    assert_eq!(bench.writes(), 0);
}

#[test]
fn a_write_error_response_is_reported_and_not_repeated() {
    let bench = bench(true);
    let mut card = card(&bench);
    bench.set_faults(SpiFaults {
        write_response: Some(0x0d),
        ..Default::default()
    });
    assert!(matches!(
        block_on(card.write_block(10, &pattern(5))),
        Err(SpiError::WriteRejected(0x0d))
    ));
}

#[test]
fn a_block_past_the_end_is_out_of_range() {
    let bench = bench(true);
    let mut card = card(&bench);
    let blocks = card.blocks();
    assert!(matches!(
        block_on(card.write_block(blocks, &pattern(1))),
        Err(SpiError::OutOfRange)
    ));
}

#[test]
fn a_removed_card_times_out() {
    let bench = bench(true);
    let mut card = card(&bench);
    bench.set_faults(SpiFaults {
        card_present: false,
        ..Default::default()
    });
    assert!(matches!(
        block_on(card.write_block(10, &pattern(1))),
        Err(SpiError::Timeout)
    ));
}

#[test]
fn chip_select_stays_low_from_the_command_through_the_data_response() {
    let bench = bench(true);
    let mut card = card(&bench);
    let before = bench.assertions();
    block_on(card.write_block(10, &pattern(1))).unwrap();
    // One assertion for the command, block and response, then one per busy poll (the card was busy for 4 clocks).
    let busy_polls = 5;
    assert_eq!(bench.assertions() - before, 1 + busy_polls);
    assert!(!bench.selected());
}
