//! Exercises file writes on a real card and leaves it as it found it: makes a directory `SDTEST` (and stops if one already
//! exists), writes a log file line by line, a large file with a long name, renames, shortens, reads everything back to
//! compare, then removes it all. Prints how long the writes took.
//! Wiring is as in `main.rs`. Run with `cargo run --release --bin write_files`.

#![no_std]
#![no_main]

use defmt::{info, warn};
use defmt_rtt as _;
use embassy_executor::Spawner;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, DMA_CH1};
use embassy_rp::spi::{self, Spi};
use embassy_rp::{bind_interrupts, dma};
use embassy_time::{Delay, Instant};
use embedded_hal_bus::spi::ExclusiveDevice;
use panic_probe as _;
use sdmmc_async::{send_init_clocks, BlockDevice, BlockWrite, Error, SpiSdCard, Volume};

bind_interrupts!(struct Irqs {
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>, dma::InterruptHandler<DMA_CH1>;
});

/// Compares a file with the bytes `pattern` makes, reading it in pieces.
async fn matches<D: BlockDevice + BlockWrite>(
    volume: &mut Volume<D>,
    path: &str,
    length: usize,
    seed: u8,
) -> bool {
    let Ok(mut file) = volume.open_file(path).await else {
        return false;
    };
    if file.size() != length as u64 {
        return false;
    }
    let mut buffer = [0u8; 700];
    let mut at = 0usize;
    loop {
        let read = match volume.read_file(&mut file, &mut buffer).await {
            Ok(read) => read,
            Err(_) => return false,
        };
        if read == 0 {
            return at == length;
        }
        for (index, byte) in buffer[..read].iter().enumerate() {
            if *byte != pattern(at + index, seed) {
                return false;
            }
        }
        at += read;
    }
}

fn pattern(index: usize, seed: u8) -> u8 {
    (index as u8).wrapping_mul(31).wrapping_add(seed)
}

async fn write_pattern<D: BlockDevice + BlockWrite>(
    volume: &mut Volume<D>,
    file: &mut sdmmc_async::File,
    length: usize,
    seed: u8,
) -> Result<(), Error<D::Error>> {
    let mut buffer = [0u8; 1000];
    let mut at = 0usize;
    while at < length {
        let take = (length - at).min(buffer.len());
        for (index, byte) in buffer[..take].iter_mut().enumerate() {
            *byte = pattern(at + index, seed);
        }
        volume.write_file(file, &buffer[..take]).await?;
        at += take;
    }
    Ok(())
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    let mut config = spi::Config::default();
    config.frequency = 400_000;
    let mut bus = Spi::new(
        p.SPI0, p.PIN_18, p.PIN_19, p.PIN_16, p.DMA_CH0, p.DMA_CH1, Irqs, config,
    );
    let cs = Output::new(p.PIN_17, Level::High);
    send_init_clocks(&mut bus).await.unwrap();
    let spi = ExclusiveDevice::new(bus, cs, Delay).unwrap();
    let mut card = match SpiSdCard::new(spi, Delay).await {
        Ok(card) => card,
        Err(error) => {
            warn!("no usable card: {}", defmt::Debug2Format(&error));
            return;
        }
    };
    card.spi_mut().bus_mut().set_frequency(16_000_000);
    let mut volume = match Volume::mount(card).await {
        Ok(volume) => volume,
        Err(error) => {
            warn!("cannot mount: {}", defmt::Debug2Format(&error));
            return;
        }
    };

    if volume.open_dir("SDTEST").await.is_ok() {
        warn!("SDTEST already exists: not touching it");
        return;
    }

    let started = Instant::now();
    volume.create_dir("SDTEST").await.unwrap();

    // A log: many small appends, flushed every so often, as a data logger would.
    let mut log = volume.create_file("SDTEST/log.txt").await.unwrap();
    for line in 0..300u32 {
        let mut text = [b' '; 40];
        text[..5].copy_from_slice(b"line ");
        let mut value = line;
        for position in (5..9).rev() {
            text[position] = b'0' + (value % 10) as u8;
            value /= 10;
        }
        text[39] = b'\n';
        volume.write_file(&mut log, &text).await.unwrap();
        if line % 50 == 49 {
            volume.flush_file(&mut log).await.unwrap();
        }
    }
    volume.flush_file(&mut log).await.unwrap();
    info!(
        "log written: {} bytes, {} ms",
        log.size(),
        started.elapsed().as_millis()
    );

    // A big file with a long name.
    let big_started = Instant::now();
    let mut big = volume
        .create_file("SDTEST/a-rather-long-file-name-for-a-big-file.bin")
        .await
        .unwrap();
    write_pattern(&mut volume, &mut big, 200_000, 7)
        .await
        .unwrap();
    volume.flush_file(&mut big).await.unwrap();
    let millis = big_started.elapsed().as_millis().max(1);
    info!(
        "big file written: {} bytes, {} ms ({} KiB/s)",
        big.size(),
        millis,
        200_000 * 1000 / 1024 / millis
    );

    let mut ok = true;
    ok &= matches(
        &mut volume,
        "SDTEST/a-rather-long-file-name-for-a-big-file.bin",
        200_000,
        7,
    )
    .await;
    info!("big file reads back: {}", ok);

    // Rename it, shorten it, check again.
    volume
        .rename(
            "SDTEST/a-rather-long-file-name-for-a-big-file.bin",
            "SDTEST/renamed.bin",
        )
        .await
        .unwrap();
    let mut renamed = volume.open_file("SDTEST/renamed.bin").await.unwrap();
    volume.set_len(&mut renamed, 70_000).await.unwrap();
    volume.flush_file(&mut renamed).await.unwrap();
    ok &= matches(&mut volume, "SDTEST/renamed.bin", 70_000, 7).await;
    info!("renamed and shortened file reads back: {}", ok);

    // The log is still whole.
    let log_ok = {
        let mut file = volume.open_file("SDTEST/log.txt").await.unwrap();
        let mut buffer = [0u8; 40];
        let mut lines = 0u32;
        while volume.read_file(&mut file, &mut buffer).await.unwrap() == 40 {
            if &buffer[..5] != b"line " {
                break;
            }
            lines += 1;
        }
        lines == 300
    };
    info!("log has 300 whole lines: {}", log_ok);

    // Leave the card as found.
    volume.remove_file("SDTEST/renamed.bin").await.unwrap();
    volume.remove_file("SDTEST/log.txt").await.unwrap();
    volume.remove_dir("SDTEST").await.unwrap();
    volume.sync().await.unwrap();
    let gone = volume.open_dir("SDTEST").await.is_err();
    info!("cleaned up: {}", gone);

    let mut root = volume.open_root();
    while let Ok(Some(entry)) = volume.next_entry(&mut root).await {
        info!(
            "{} {}",
            if entry.is_directory { "dir " } else { "file" },
            entry.name.as_str()
        );
    }
    info!(
        "done in {} ms: {}",
        started.elapsed().as_millis(),
        if ok && log_ok && gone {
            "ALL GOOD"
        } else {
            "PROBLEM"
        }
    );
}
