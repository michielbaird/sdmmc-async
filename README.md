# sdmmc-async

An async, `no_std`, no-alloc SD card stack for embedded Rust: an SPI-mode card driver and FAT32 and exFAT filesystems (read-only unless the `write` feature is on), over [`embedded-hal-async`](https://docs.rs/embedded-hal-async).

It is not affiliated with the Embassy project. It works with Embassy's HALs because they implement `embedded-hal-async`, and with any other HAL that does.

## What is in it

- `BlockDevice`: an async trait over 512-byte blocks.
- `SpiSdCard`: an SD card in SPI mode over any `embedded_hal_async::spi::SpiDevice` (CMD0, CMD8, ACMD41, CMD58, CMD16, CMD59, CMD9, CMD17; CRC7 on commands and CRC16 on data blocks). A read whose data token arrives late is repeated inside the driver.
- `Volume`: a mounted volume, FAT32 or exFAT detected from the boot sector, with one API for both: open the root or a path, list a directory, read a file, seek.
- With the `write` feature: `BlockWrite` and card block writes (CMD24), and on `Volume` writing and appending to files, truncating, creating, removing and renaming files and directories (long names included), and `sync`.
- With the `test-support` feature (needs `std`): a RAM card and FAT32 and exFAT image builders, for the tests of crates that use this one.

## Features

| Feature | Default | What it builds |
|---|---|---|
| `fat32` | yes | the FAT32 reader |
| `exfat` | yes | the exFAT reader |
| `write` | no | writing: the `BlockWrite` trait, `SpiSdCard` writes (CMD24), and file and directory changes on FAT32 and exFAT |
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

## Writing

```rust,ignore
// With the `write` feature, and a card that is `BlockWrite` (`SpiSdCard` is).
volume.create_dir("LOGS").await?;
let mut log = volume.create_file("LOGS/run-0001.csv").await?;
volume.write_file(&mut log, b"t,value\n").await?;
volume.write_file(&mut log, b"0,42\n").await?;
volume.flush_file(&mut log).await?;      // the file's size is now on the card
volume.rename("LOGS/run-0001.csv", "LOGS/done.csv").await?;
volume.sync().await?;                    // the volume is marked cleanly unmounted
```

What to know before trusting it with data:

- **Flush and sync.** `write_file` puts the data and the allocation on the card at once but the directory entry (size, first cluster, times) only when you call `flush_file` (or `set_len`, which flushes when it shortens). A PC reading the card sees a file as it was at its last flush. `sync` clears the volume's "in use" flag and updates FAT32's free-space count; until it runs, a PC will offer to check the card. A card that loses power mid-write is not harmed by that alone: see below.
- **No journal, an order instead.** Data goes first, then the allocation (FAT or bitmap), then the directory entry. A cut in power loses the last write, and can leave clusters allocated that nothing refers to (a checker reclaims them), a shortened file whose clusters are not freed yet, or (rename) a file under both its old and new name; it does not damage other files. An exFAT or FAT32 directory entry set that crosses a sector boundary can be left half written, which readers ignore and a checker offers to delete. The test suite cuts power at every write of a sequence of operations and runs `fsck.vfat` and `fsck.exfat` over each result.
- **Open handles.** A `File` is plain data. After `remove_file`, `rename` or `remove_dir` on it, do not use it again. Two handles to one file do not see each other's writes; flush one and reopen the other.
- **Time.** There is no clock here: everything is stamped 1980-01-01 until you give the volume one with `set_clock(fn() -> Timestamp)`.
- **Names.** Up to 255 UTF-16 units; not `\ / : * ? " < > |` or control characters; no trailing dot or space. FAT32 gets 8.3 aliases (`LONG-F~1.TXT`) as Windows would make them. Lookup is case-insensitive for ASCII.
- **Limits.** A FAT32 file is at most 4 GiB less one byte (`Error::FileTooLarge`). No formatting, no FAT12 or FAT16, no TexFAT (on exFAT only the first FAT is written). A file is written a sector at a time (CMD24); multi-block writes are not used yet, so throughput is bounded by the card's per-block write time (about 125 KiB/s for 1000-byte writes at 16 MHz on the card tried).

## Examples

[`examples/rp2040`](https://github.com/michielbaird/sdmmc-async/tree/main/examples/rp2040) (Raspberry Pi Pico) and [`examples/rp2350`](https://github.com/michielbaird/sdmmc-async/tree/main/examples/rp2350) (Pico 2) are Embassy programs that bring up a card on SPI0, list its root directory and print a file. Each also has `write_check` (block writes on a block that looks unused, put back afterwards) and `write_files` (creates, writes, renames, shortens and removes files under `SDTEST/`, leaving the card as found; it stops if `SDTEST` exists). Each is its own crate (`cd examples/rp2040 && cargo run --release`, with a probe and `probe-rs`); the wiring is at the top of its `main.rs`. CI only builds them; both have been run on a Pico and a Pico 2 with an SDHC card formatted exFAT or FAT32.

## Limits

- No formatting; writing needs the `write` feature (see above).
- exFAT: no TexFAT, ASCII-only case-insensitive name lookup (non-ASCII names open case-sensitively), 512-byte sectors.
- SDXC cards work if formatted exFAT; SDHC cards work as FAT32 or exFAT. No FAT12 or FAT16.
- SPI mode only, no SDIO.

## Tested

- Host tests against a fake SPI card with injectable faults (a silent card, a bad CRC, corrupt data, card removal), synthetic images, and images written by tools that are not this crate: `mkfs.vfat` and `mtools` (`mcopy`, `mmd`) for FAT32, and `mkfs.exfat` (exfatprogs) and `ferrosys` (a filesystem-writing tool; optional) for exFAT. Those tests skip, with a message, for any tool that is not installed, so `cargo test` passes without them. They look on `PATH`; to use a tool tree elsewhere, set `SDMMC_FS_TOOLS` to a root with `sbin/mkfs.vfat`, `usr/bin/mcopy`, `usr/bin/mmd` and `usr/sbin/mkfs.exfat`, and `SDMMC_FERROSYS` to the `ferrosys` binary.
- On hardware, in throwaway bring-up images: an STM32F407 board and an RP2040 board, SDHC cards formatted FAT32 and exFAT, SPI modes 0 and 3, 1 to 16 MHz. Other cards, brands and hot removal have not been tried.

- Writing, on the host: the same scenarios on both filesystems (overwrite, append across clusters, truncate and extend, a full volume, create, remove, rename and move, long and Unicode names, directories that grow, files that stop being contiguous), random sequences of operations compared with an in-memory model, `fsck.vfat` and `fsck.exfat` over volumes this crate wrote, and a power cut at every write of a sequence of operations (`SDMMC_POWER_LOSS_STRIDE=1` checks every cut; the default checks every fifth).
- Writing, on hardware: an RP2350 board (Pico 2) with one SDHC card formatted exFAT, at 16 MHz: block writes, and `write_files`. Not tried: FAT32 on hardware, other cards, other boards, power cuts.

To check every combination: `for f in "" "--no-default-features" "--no-default-features --features fat32" "--no-default-features --features exfat"; do cargo test --features test-support,write $f; cargo test --features test-support $f; done`.

## Minimum Rust

1.87.

## Licence

Licensed under either of the Apache License, Version 2.0 ([LICENSE-APACHE](https://github.com/michielbaird/sdmmc-async/blob/main/LICENSE-APACHE)) or the MIT licence ([LICENSE-MIT](https://github.com/michielbaird/sdmmc-async/blob/main/LICENSE-MIT)) at your option. Unless you say otherwise, any contribution you submit for inclusion is dual licensed as above, without additional terms.
