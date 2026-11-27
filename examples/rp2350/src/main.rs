//! Lists the root directory of the SD card on a Raspberry Pi Pico 2 (RP2350) and prints the start of a text file.
//!
//! Wiring (SPI0, the default pins of most SD card breakouts on a Pico 2):
//!
//! | SD card | Pico 2 pin |
//! |---|---|
//! | CS   | GP17 |
//! | SCK  | GP18 |
//! | MOSI (DI) | GP19 |
//! | MISO (DO) | GP16 |
//! | 3V3 / GND | 3V3(OUT) / GND |
//!
//! Put a FAT32 or exFAT card in; if it has a file `HELLO.TXT` in the root it is read and printed.
//! Run with `cargo run --release` from this directory (needs a probe and `probe-rs`).

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
use sdmmc_async::{send_init_clocks, SpiSdCard, Volume};

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
    info!(
        "card: {} blocks, high capacity: {}",
        card.blocks(),
        card.is_high_capacity()
    );

    // The card is initialised: raise the clock. 16 MHz was the highest tried on hardware (on the RP2040).
    card.spi_mut().bus_mut().set_frequency(16_000_000);

    let mut volume = match Volume::mount(card).await {
        Ok(volume) => volume,
        Err(error) => {
            warn!("cannot mount: {}", defmt::Debug2Format(&error));
            return;
        }
    };

    let mut root = volume.open_root();
    while let Ok(Some(entry)) = volume.next_entry(&mut root).await {
        info!(
            "{} {} ({} bytes)",
            if entry.is_directory { "dir " } else { "file" },
            entry.name.as_str(),
            entry.size
        );
    }

    match volume.open_file("HELLO.TXT").await {
        Ok(mut file) => {
            let mut buffer = [0u8; 128];
            match volume.read_file(&mut file, &mut buffer).await {
                Ok(read) => info!(
                    "HELLO.TXT: {}",
                    core::str::from_utf8(&buffer[..read]).unwrap_or("(not text)")
                ),
                Err(error) => warn!("read failed: {}", defmt::Debug2Format(&error)),
            }
        }
        Err(error) => warn!("HELLO.TXT: {}", defmt::Debug2Format(&error)),
    }
}
