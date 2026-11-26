# sdmmc-async

An async, `no_std`, no-alloc SD card stack for embedded Rust: an SPI-mode card driver and a read-only FAT32 and exFAT filesystem, over [`embedded-hal-async`](https://docs.rs/embedded-hal-async).

It is not affiliated with the Embassy project. It works with Embassy's HALs because they implement `embedded-hal-async`, and with any other HAL that does.

## What is in it

- `BlockDevice`: an async trait over 512-byte blocks.
- `SpiSdCard`: an SD card in SPI mode over any `embedded_hal_async::spi::SpiDevice` (CMD0, CMD8, ACMD41, CMD58, CMD16, CMD59, CMD9, CMD17; CRC7 on commands and CRC16 on data blocks). A read whose data token arrives late is repeated inside the driver.
- `Volume`: a mounted volume, FAT32 or exFAT detected from the boot sector, with one API for both: open the root or a path, list a directory, read a file, seek.
- With the `test-support` feature (needs `std`): a RAM card and FAT32 and exFAT image builders, for the tests of crates that use this one.

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

## Limits

- Read-only: no writing, no formatting.
- exFAT: no TexFAT, ASCII-only case-insensitive name lookup (non-ASCII names open case-sensitively), 512-byte sectors.
- SDXC cards work if formatted exFAT; SDHC cards work as FAT32 or exFAT. No FAT12 or FAT16.
- SPI mode only, no SDIO.

## Tested

- Host tests against a fake SPI card with injectable faults (a silent card, a bad CRC, corrupt data, card removal), synthetic images, and images written by tools that are not this crate (`mkfs.vfat` and `mtools` for FAT32, `mkfs.exfat` and `ferrosys` for exFAT). Those tests skip, with a message, when the tools are missing; point `SDMMC_FS_TOOLS` at a tree with `sbin/mkfs.vfat`, `usr/bin/mcopy`, `usr/bin/mmd` and `usr/sbin/mkfs.exfat`, and `SDMMC_FERROSYS` at the `ferrosys` binary.
- On hardware, in throwaway bring-up images: an STM32F407 board and an RP2040 board, SDHC cards formatted FAT32 and exFAT, SPI modes 0 and 3, 1 to 16 MHz. Other cards, brands, hot removal and writes have not been tried.

## Minimum Rust

1.87.

## Licence

Licensed under either of the Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE)) or the MIT licence ([LICENSE-MIT](LICENSE-MIT)) at your option. Unless you say otherwise, any contribution you submit for inclusion is dual licensed as above, without additional terms.
