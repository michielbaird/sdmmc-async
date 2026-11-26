#![no_std]
#![forbid(unsafe_code)]
#![allow(async_fn_in_trait)]
// With neither filesystem built, only the block and card layers are used and the shared helpers are not.
#![cfg_attr(not(any(feature = "fat32", feature = "exfat")), allow(dead_code))]

#[cfg(feature = "test-support")]
extern crate std;

mod block;
mod error;
#[cfg(feature = "exfat")]
mod exfat;
#[cfg(feature = "fat32")]
mod fat32;
mod fs;
mod name;
mod spi;
#[cfg(feature = "test-support")]
pub mod testing;

pub use block::{BlockDevice, BLOCK_SIZE};
pub use error::Error;
pub use fs::{DirEntry, Directory, File, Volume};
pub use name::Name;
pub use spi::{crc16, crc7, send_init_clocks, SpiError, SpiSdCard};
