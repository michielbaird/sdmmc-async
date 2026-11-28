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
    #[cfg(feature = "write")]
    writes: usize,
    #[cfg(feature = "write")]
    power_loss_after: Option<usize>,
}

impl RamCard {
    /// A card that serves `image`.
    pub fn new(image: Vec<u8>) -> Self {
        Self {
            image,
            #[cfg(feature = "write")]
            writes: 0,
            #[cfg(feature = "write")]
            power_loss_after: None,
        }
    }

    /// The card's contents.
    pub fn image(&self) -> &[u8] {
        &self.image
    }

    /// The card's contents, when the card is no longer wanted.
    pub fn into_image(self) -> Vec<u8> {
        self.image
    }
}

#[cfg(feature = "write")]
impl RamCard {
    /// The number of blocks written so far (writes lost to `lose_power_after` are not counted).
    pub fn writes(&self) -> usize {
        self.writes
    }

    /// Simulates a power cut: after `writes` more block writes land, every later write is silently dropped (and still
    /// reported as done, as the host would not know). For testing that a volume survives an interrupted operation.
    pub fn lose_power_after(&mut self, writes: usize) {
        self.power_loss_after = Some(self.writes + writes);
    }
}

#[cfg(feature = "write")]
impl crate::BlockWrite for RamCard {
    async fn write_block(
        &mut self,
        block: u32,
        buffer: &[u8; BLOCK_SIZE],
    ) -> Result<(), Infallible> {
        if self
            .power_loss_after
            .is_some_and(|limit| self.writes >= limit)
        {
            return Ok(());
        }
        let start = block as usize * BLOCK_SIZE;
        if let Some(slot) = self.image.get_mut(start..start + BLOCK_SIZE) {
            slot.copy_from_slice(buffer);
            self.writes += 1;
        }
        Ok(())
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
