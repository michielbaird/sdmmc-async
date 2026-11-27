//! Test support for the crates that sit on this one: images of FAT32 and exFAT volumes with chosen files, and a card that
//! serves one from memory. Needs `std`; behind the `test-support` feature.

use core::convert::Infallible;
use std::vec::Vec;

use crate::{BlockDevice, BLOCK_SIZE};

#[path = "../tests/support/images.rs"]
mod images;

pub use images::{exfat_image, fat32_image, ExFileSpec, FileSpec};

/// A card in memory: a volume image served block by block.
pub struct RamCard {
    image: Vec<u8>,
}

impl RamCard {
    /// A card that serves `image`.
    pub fn new(image: Vec<u8>) -> Self {
        Self { image }
    }
}

impl BlockDevice for RamCard {
    type Error = Infallible;

    async fn read_block(
        &mut self,
        block: u32,
        buffer: &mut [u8; BLOCK_SIZE],
    ) -> Result<(), Infallible> {
        let start = block as usize * BLOCK_SIZE;
        match self.image.get(start..start + BLOCK_SIZE) {
            Some(data) => buffer.copy_from_slice(data),
            None => buffer.fill(0),
        }
        Ok(())
    }

    async fn num_blocks(&mut self) -> Result<u32, Infallible> {
        Ok((self.image.len() / BLOCK_SIZE) as u32)
    }
}
