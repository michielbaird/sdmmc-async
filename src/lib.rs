#![doc = include_str!("../README.md")]
#![no_std]
#![forbid(unsafe_code)]
#![warn(missing_docs)]
// The futures of `BlockDevice` are not `Send`-bounded: this crate is meant for single-core executors such as Embassy's.
#![allow(async_fn_in_trait)]

#[cfg(feature = "test-support")]
extern crate std;

mod block;
mod error;
#[cfg(feature = "exfat")]
mod exfat;
#[cfg(feature = "fat32")]
mod fat32;
#[cfg(any(feature = "fat32", feature = "exfat"))]
mod fs;
#[cfg(any(feature = "fat32", feature = "exfat"))]
mod name;
mod spi;
#[cfg(feature = "test-support")]
pub mod testing;

pub use block::{BlockDevice, BLOCK_SIZE};
pub use error::Error;
#[cfg(any(feature = "fat32", feature = "exfat"))]
pub use fs::{DirEntry, Directory, File, Volume};
#[cfg(any(feature = "fat32", feature = "exfat"))]
pub use name::Name;
pub use spi::{crc16, crc7, send_init_clocks, SpiError, SpiSdCard};
