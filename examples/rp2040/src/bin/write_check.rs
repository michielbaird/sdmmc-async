//! Checks block writes on a real card without losing data: writes a block back unchanged, then writes 64 test patterns to a
//! block that looks unused (one repeated byte, found from the end of the card) and puts the original contents back.
//! Wiring is as in `main.rs`. Run with `cargo run --release --bin write_check`.

#![no_std]
#![no_main]

use defmt::{info, warn};
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, DMA_CH1};
use embassy_rp::spi::{self, Spi};
use embassy_rp::{bind_interrupts, dma};
use embassy_time::Delay;
use embedded_hal_bus::spi::ExclusiveDevice;
use panic_probe as _;
use sdmmc_async::{send_init_clocks, BlockDevice, BlockWrite, SpiSdCard, BLOCK_SIZE};

bind_interrupts!(struct Irqs {
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>, dma::InterruptHandler<DMA_CH1>;
});

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    // The card must be initialised at 400 kHz or less, SPI mode 0 (the default).
    let mut config = spi::Config::default();
    config.frequency = 400_000;
    let mut bus = Spi::new(
        p.SPI0, p.PIN_18, p.PIN_19, p.PIN_16, p.DMA_CH0, p.DMA_CH1, Irqs, config,
    );
    let cs = Output::new(p.PIN_17, Level::High);

    // 74+ clocks with chip select high, before the card sees its first command.
    send_init_clocks(&mut bus).await.unwrap();

    // `ExclusiveDevice` turns the bus plus a chip select pin into the `SpiDevice` the driver wants. On a bus shared with
    // other devices, use `embassy_embedded_hal::shared_bus::asynch::spi::SpiDevice` instead.
    let spi = ExclusiveDevice::new(bus, cs, Delay).unwrap();
    let mut card = match SpiSdCard::new(spi, Delay).await {
        Ok(card) => card,
        Err(error) => {
            warn!("no usable card: {}", defmt::Debug2Format(&error));
            return;
        }
    };
    card.spi_mut().bus_mut().set_frequency(16_000_000);
    let blocks = card.blocks();
    info!("card: {} blocks", blocks);

    // 1. Write a block back unchanged: exercises CMD24 without changing anything on the card.
    let block = 2048u32;
    let mut original = [0u8; BLOCK_SIZE];
    card.read_block(block, &mut original).await.unwrap();
    card.write_block(block, &original).await.unwrap();
    let mut again = [0u8; BLOCK_SIZE];
    card.read_block(block, &mut again).await.unwrap();
    info!(
        "identity write of block {}: {}",
        block,
        if again == original { "ok" } else { "MISMATCH" }
    );

    // 2. A pattern in a block that looks unused (one repeated byte, as freshly erased flash is), found from the end of the
    // card; the original contents are put back afterwards.
    let mut saved = [0u8; BLOCK_SIZE];
    let mut last = 0;
    for candidate in (blocks - 4096..blocks).rev() {
        card.read_block(candidate, &mut saved).await.unwrap();
        if saved.iter().all(|byte| *byte == saved[0]) {
            last = candidate;
            break;
        }
    }
    if last == 0 {
        warn!("no unused-looking block near the end: skipping the pattern test");
        return;
    }
    info!("using block {} (filled with {:x})", last, saved[0]);
    let mut failures = 0u32;
    for round in 0..64u32 {
        let mut pattern = [0u8; BLOCK_SIZE];
        for (index, byte) in pattern.iter_mut().enumerate() {
            *byte = (index as u8).wrapping_mul(31).wrapping_add(round as u8);
        }
        card.write_block(last, &pattern).await.unwrap();
        let mut read = [0u8; BLOCK_SIZE];
        card.read_block(last, &mut read).await.unwrap();
        if read != pattern {
            failures += 1;
        }
    }
    card.write_block(last, &saved).await.unwrap();
    card.read_block(last, &mut again).await.unwrap();
    info!(
        "pattern writes to block {}: {} of 64 mismatched; restored: {}",
        last,
        failures,
        if again == saved { "yes" } else { "NO" }
    );
}
