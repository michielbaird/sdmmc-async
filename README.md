# sdmmc-async

An async, `no_std`, no-alloc SD card stack for embedded Rust: an SPI-mode card driver and a read-only FAT32 and exFAT filesystem, over [`embedded-hal-async`](https://docs.rs/embedded-hal-async).

It is not affiliated with the Embassy project. It works with Embassy's HALs because they implement `embedded-hal-async`, and with any other HAL that does.

## What is in it

- `BlockDevice`: an async trait over 512-byte blocks.
- `SpiSdCard`: an SD card in SPI mode over any `embedded_hal_async::spi::SpiDevice` (CMD0, CMD8, ACMD41, CMD58, CMD16, CMD59, CMD9, CMD17; CRC7 on commands and CRC16 on data blocks). A read whose data token arrives late is repeated inside the driver.
- `Volume`: a mounted volume, FAT32 or exFAT detected from the boot sector, with one API for both: open the root or a path, list a directory, read a file, seek.
- With the `test-support` feature (needs `std`): a RAM card and FAT32 and exFAT image builders, for the tests of crates that use this one.

## Features

| Feature | Default | What it builds |
|---|---|---|
| `fat32` | yes | the FAT32 reader |
| `exfat` | yes | the exFAT reader |
| `test-support` | no | a RAM card and image builders (needs `std`) |

Leave a filesystem out to save its code: `default-features = false, features = ["fat32"]`. With neither, `Volume`, `File`, `Directory`, `DirEntry` and `Name` are not built at all and only `BlockDevice` and `SpiSdCard` remain (a card as raw blocks). A card holding a filesystem that is not built in fails to mount with `Error::FilesystemNotEnabled`, so an embedder can tell it from a damaged card.

## Example

```rust,ignore
use sdmmc_async::{SpiSdCard, Volume};

// `spi` is an `embedded_hal_async::spi::SpiDevice<u8>` (a chip select on a shared bus), `delay` a `DelayNs`.
// The card needs at least 74 clock cycles with chip select high first: `send_init_clocks(&mut bus)` does that.
let card = SpiSdCard::new(spi, delay).await?;
let mut volume = Volume::mount(card).await?;
let mut file = volume.open_file("GCODE/BENCHY.GCO").await?;
let mut buffer = [0u8; 512];
let read = volume.read_file(&mut file, &mut buffer).await?;
```

## Examples

[`examples/rp2040`](https://github.com/michielbaird/sdmmc-async/tree/main/examples/rp2040) (Raspberry Pi Pico) and [`examples/rp2350`](https://github.com/michielbaird/sdmmc-async/tree/main/examples/rp2350) (Pico 2) are Embassy programs that bring up a card on SPI0, list its root directory and print a file. Each is its own crate (`cd examples/rp2040 && cargo run --release`, with a probe and `probe-rs`); the wiring is at the top of its `main.rs`. CI only builds them; they have not been run on a board.

## Limits

- Read-only: no writing, no formatting.
- exFAT: no TexFAT, ASCII-only case-insensitive name lookup (non-ASCII names open case-sensitively), 512-byte sectors.
- SDXC cards work if formatted exFAT; SDHC cards work as FAT32 or exFAT. No FAT12 or FAT16.
- SPI mode only, no SDIO.

## Tested

- Host tests against a fake SPI card with injectable faults (a silent card, a bad CRC, corrupt data, card removal), synthetic images, and images written by tools that are not this crate: `mkfs.vfat` and `mtools` (`mcopy`, `mmd`) for FAT32, and `mkfs.exfat` (exfatprogs) and `ferrosys` (a filesystem-writing tool; optional) for exFAT. Those tests skip, with a message, for any tool that is not installed, so `cargo test` passes without them. They look on `PATH`; to use a tool tree elsewhere, set `SDMMC_FS_TOOLS` to a root with `sbin/mkfs.vfat`, `usr/bin/mcopy`, `usr/bin/mmd` and `usr/sbin/mkfs.exfat`, and `SDMMC_FERROSYS` to the `ferrosys` binary.
- On hardware, in throwaway bring-up images: an STM32F407 board and an RP2040 board, SDHC cards formatted FAT32 and exFAT, SPI modes 0 and 3, 1 to 16 MHz. Other cards, brands, hot removal and writes have not been tried.

To check every combination: `for f in "" "--no-default-features" "--no-default-features --features fat32" "--no-default-features --features exfat"; do cargo test --features test-support $f; done`.

## Minimum Rust

1.87.

## Licence

Licensed under either of the Apache License, Version 2.0 ([LICENSE-APACHE](https://github.com/michielbaird/sdmmc-async/blob/main/LICENSE-APACHE)) or the MIT licence ([LICENSE-MIT](https://github.com/michielbaird/sdmmc-async/blob/main/LICENSE-MIT)) at your option. Unless you say otherwise, any contribution you submit for inclusion is dual licensed as above, without additional terms.
