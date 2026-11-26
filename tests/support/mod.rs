#![allow(dead_code)]

mod images;
mod spi;

#[allow(unused_imports)]
pub use images::{exfat_image, fat32_image, ExFileSpec, FileSpec};
#[allow(unused_imports)]
pub use spi::{FakeCard, SimBus, SimDelay, SimSpi, SpiBench, SpiFaults};

use std::cell::RefCell;
use std::rc::Rc;

use sdmmc_async::{BlockDevice, Error, File, Volume, BLOCK_SIZE};

/// Faults the simulated card can inject. The filesystem must return an error
/// (never panic, hang or read outside the card) when any of these fire.
#[derive(Clone, Copy, Debug, Default)]
pub struct Faults {
    /// The next read fails after this many successful reads.
    pub fail_after_reads: Option<usize>,
    /// Every read of this block fails.
    pub fail_on_block: Option<u32>,
    /// XOR this byte of this block on every read.
    pub corrupt_block: Option<(u32, usize, u8)>,
    /// Fail once this many reads have been served. A budget, not a fault.
    pub max_reads: Option<usize>,
}

pub struct CardState {
    pub image: Vec<u8>,
    pub faults: Faults,
    pub reads: usize,
    pub out_of_range: usize,
    pub last_block: Option<u32>,
}

/// A simulated SD card at the block layer. This is the test bench's stand-in
/// for the SPI/SDMMC transport; the filesystem sees the same `BlockDevice`
/// trait a real card will.
pub struct SimCard {
    state: Rc<RefCell<CardState>>,
}

impl BlockDevice for SimCard {
    type Error = ();

    async fn read_block(&mut self, block: u32, buffer: &mut [u8; BLOCK_SIZE]) -> Result<(), ()> {
        let mut state = self.state.borrow_mut();
        if let Some(remaining) = state.faults.fail_after_reads {
            if remaining == 0 {
                return Err(());
            }
            state.faults.fail_after_reads = Some(remaining - 1);
        }
        if state.faults.fail_on_block == Some(block) {
            return Err(());
        }
        if let Some(max) = state.faults.max_reads {
            if state.reads >= max {
                return Err(());
            }
        }
        let start = block as usize * BLOCK_SIZE;
        if start + BLOCK_SIZE > state.image.len() {
            state.out_of_range += 1;
            return Err(());
        }
        buffer.copy_from_slice(&state.image[start..start + BLOCK_SIZE]);
        if let Some((corrupt_block, offset, mask)) = state.faults.corrupt_block {
            if corrupt_block == block {
                buffer[offset % BLOCK_SIZE] ^= mask;
            }
        }
        state.reads += 1;
        state.last_block = Some(block);
        Ok(())
    }

    async fn num_blocks(&mut self) -> Result<u32, ()> {
        Ok((self.state.borrow().image.len() / BLOCK_SIZE) as u32)
    }
}

/// A bench owns the card state and hands out cards that share it. The volume
/// owns the card, but the test can still inspect reads and change faults.
pub struct Bench {
    state: Rc<RefCell<CardState>>,
}

impl Bench {
    pub fn new(image: Vec<u8>) -> Self {
        Self {
            state: Rc::new(RefCell::new(CardState {
                image,
                faults: Faults::default(),
                reads: 0,
                out_of_range: 0,
                last_block: None,
            })),
        }
    }

    pub fn card(&self) -> SimCard {
        SimCard {
            state: Rc::clone(&self.state),
        }
    }

    pub fn mount(&self) -> Result<Volume<SimCard>, Error<()>> {
        block_on(Volume::mount(self.card()))
    }

    pub fn set_faults(&self, faults: Faults) {
        self.state.borrow_mut().faults = faults;
    }

    pub fn reads(&self) -> usize {
        self.state.borrow().reads
    }

    pub fn out_of_range(&self) -> usize {
        self.state.borrow().out_of_range
    }

    pub fn last_block(&self) -> Option<u32> {
        self.state.borrow().last_block
    }

    pub fn reset_counters(&self) {
        let mut state = self.state.borrow_mut();
        state.reads = 0;
        state.out_of_range = 0;
        state.last_block = None;
    }

    pub fn patch(&self, offset: usize, bytes: &[u8]) {
        let mut state = self.state.borrow_mut();
        state.image[offset..offset + bytes.len()].copy_from_slice(bytes);
    }
}

pub fn block_on<F: core::future::Future>(future: F) -> F::Output {
    let mut context = core::task::Context::from_waker(core::task::Waker::noop());
    let mut future = core::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            core::task::Poll::Ready(value) => return value,
            core::task::Poll::Pending => {}
        }
    }
}

pub fn try_read_all<D: BlockDevice>(
    volume: &mut Volume<D>,
    file: &mut File,
) -> Result<Vec<u8>, Error<D::Error>> {
    let mut out = Vec::new();
    let mut buffer = [0u8; 64];
    loop {
        let count = block_on(volume.read_file(file, &mut buffer))?;
        if count == 0 {
            break;
        }
        out.extend_from_slice(&buffer[..count]);
    }
    Ok(out)
}

pub fn read_all<D: BlockDevice>(volume: &mut Volume<D>, file: &mut File) -> Vec<u8>
where
    D::Error: core::fmt::Debug,
{
    try_read_all(volume, file).unwrap()
}

pub fn list_root<D: BlockDevice>(volume: &mut Volume<D>) -> Result<Vec<String>, Error<D::Error>> {
    let mut dir = volume.open_root();
    let mut names = Vec::new();
    while let Some(entry) = block_on(volume.next_entry(&mut dir))? {
        names.push(entry.name.as_str().to_string());
    }
    Ok(names)
}

/// Deterministic pseudo-random source, the same shape the firmware tests use.
pub struct Lcg(u64);

impl Lcg {
    pub fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1))
    }

    pub fn next_u32(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as u32
    }

    pub fn below(&mut self, limit: u32) -> u32 {
        if limit == 0 {
            0
        } else {
            self.next_u32() % limit
        }
    }
}
