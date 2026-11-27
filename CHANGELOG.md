# Changelog

## 0.1.0

First release: an SPI-mode SD card driver (`SpiSdCard`), the `BlockDevice` trait, and a read-only FAT32 and exFAT `Volume`, behind the `fat32` and `exfat` features (both on by default). `Error` and `SpiError` are `#[non_exhaustive]`.
