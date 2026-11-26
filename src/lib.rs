#![no_std]
#![forbid(unsafe_code)]
#![allow(async_fn_in_trait)]

#[cfg(feature = "test-support")]
extern crate std;

mod block;
mod error;
mod exfat;
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
