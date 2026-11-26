pub const BLOCK_SIZE: usize = 512;

/// A device that reads 512-byte blocks. The filesystem layer never assumes
/// anything about the device beyond this.
pub trait BlockDevice {
    type Error;

    async fn read_block(
        &mut self,
        block: u32,
        buffer: &mut [u8; BLOCK_SIZE],
    ) -> Result<(), Self::Error>;

    async fn num_blocks(&mut self) -> Result<u32, Self::Error>;
}

/// A device lent to the filesystem for a while: a mount that borrows the card and gives it back.
impl<D: BlockDevice + ?Sized> BlockDevice for &mut D {
    type Error = D::Error;

    async fn read_block(
        &mut self,
        block: u32,
        buffer: &mut [u8; BLOCK_SIZE],
    ) -> Result<(), Self::Error> {
        (**self).read_block(block, buffer).await
    }

    async fn num_blocks(&mut self) -> Result<u32, Self::Error> {
        (**self).num_blocks().await
    }
}
