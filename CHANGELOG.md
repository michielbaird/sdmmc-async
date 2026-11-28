# Changelog

## Unreleased

- New `write` feature (off by default): the `BlockWrite` trait; block writes on `SpiSdCard` (CMD24, with the data response, the busy wait and a repeat on a CRC-error response); a writable `RamCard` with `lose_power_after` for tests.
- With `write`, on both FAT32 and exFAT: `Volume::write_file`, `flush_file`, `set_len`, `sync`, `set_clock`, `create_file`, `create_dir`, `remove_file`, `remove_dir`, `rename`; `Timestamp`.
- New `Error` variants: `NoSpace`, `AlreadyExists`, `NotEmpty`, `FileTooLarge`, `InvalidOperation`; new `SpiError::WriteRejected`.
- Examples `write_check` and `write_files` for the RP2040 and RP2350.

## 0.1.0

First release: an SPI-mode SD card driver (`SpiSdCard`), the `BlockDevice` trait, and a read-only FAT32 and exFAT `Volume`, behind the `fat32` and `exfat` features (both on by default). `Error` and `SpiError` are `#[non_exhaustive]`.
