use embedded_hal_async::delay::DelayNs;
use embedded_hal_async::spi::{Operation, SpiBus, SpiDevice};

use crate::{BlockDevice, BLOCK_SIZE};

/// What can go wrong talking to a card over SPI. `E` is the SPI device's own error.
///
/// New variants may be added in a minor release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SpiError<E> {
    /// The SPI device failed.
    Spi(E),
    /// The card did not answer in time (none fitted, or removed).
    Timeout,
    /// The card answered with an unexpected R1 byte or data error token.
    BadResponse(u8),
    /// A command response or data block failed its CRC, even after repeating the read.
    BadCrc,
    /// The card cannot be used: not an SD card, or its capacity is not readable.
    UnsupportedCard,
    /// A block number past the end of the card.
    OutOfRange,
    /// The card refused a written block: its data response token (the low five bits), for example a write error (`0x0d`).
    /// A CRC error is reported as `BadCrc`.
    WriteRejected(u8),
}

/// The SD spec allows a card 100 ms to start a read; the poll loop waits at least that long at any clock.
const READ_TOKEN_POLLS: u32 = 2_000;
const READ_TOKEN_POLL_US: u32 = 50;

/// Repeats of a data read that failed (see `read_with_data`).
const READ_RETRIES: usize = 2;

/// Repeats of a block write the card answered with a CRC error.
#[cfg(feature = "write")]
const WRITE_RETRIES: usize = 2;
/// The SD spec allows a card 250 ms to program a block; the busy wait gives it about 500 ms: 64 short polls, then 1 ms ones.
#[cfg(feature = "write")]
const WRITE_FAST_POLLS: u32 = 64;
#[cfg(feature = "write")]
const WRITE_SLOW_POLLS: u32 = 500;

/// A card answers a command within eight bytes.
const R1_WINDOW: usize = 8;
/// How many idle bytes the single-transaction read allows between the R1 and the data token. It starts
/// at `START_TOKEN_WINDOW` and follows the gap the card actually shows, within these limits.
const MIN_TOKEN_WINDOW: usize = 32;
const START_TOKEN_WINDOW: usize = 256;
const MAX_TOKEN_WINDOW: usize = 2_048;
const SCRATCH: usize = R1_WINDOW + MAX_TOKEN_WINDOW + 1 + BLOCK_SIZE + 2;

/// The card must see at least 74 clock cycles with chip select high before its first command. A
/// `SpiDevice` always asserts chip select, so call this on the raw bus (at 400 kHz or less) before
/// wrapping it into the device handed to `SpiSdCard::new`.
pub async fn send_init_clocks<B: SpiBus<u8>>(bus: &mut B) -> Result<(), B::Error> {
    bus.write(&[0xff; 10]).await?;
    bus.flush().await
}

/// An SD card behind a `SpiDevice`, so the bus can be shared with other devices. Every device on a
/// shared bus keeps its own clock and mode in its own `SpiDevice`; this driver never touches the bus
/// configuration. The card is read-only.
///
/// The SD spec wants chip select low from the command through the data block. A read is therefore
/// one transaction: the command frame, a window for the R1 and the idle bytes before the data token,
/// and the block with its CRC. The window follows the gap the card shows (it clocks the whole window
/// every time, so it stays as small as the card allows). If the token has not arrived when the window
/// ends, the read falls back to polling for it with chip select released between polls, which a card
/// that aborts on chip select rising will fail; the window then grows, so a retry takes the single
/// transaction path.
///
/// The `SpiDevice` must be at a clock of 400 kHz or less for `new`, and can be raised afterwards
/// through `spi_mut`. A card's MISO driver can take a clock edge to let go of the line after chip
/// select rises, so on a bus shared with a device that is selected right afterwards the first bit it
/// sends can be corrupted; give the card its own bus if that matters.
pub struct SpiSdCard<SPI, D> {
    spi: SPI,
    delay: D,
    high_capacity: bool,
    blocks: u32,
    check_crc: bool,
    token_window: usize,
    scratch: [u8; SCRATCH],
}

#[derive(Clone, Copy)]
enum Reply {
    R1,
    R1WithFourBytes,
}

#[derive(Clone, Copy)]
struct Response {
    r1: u8,
    extra: [u8; 4],
}

impl<SPI, D> SpiSdCard<SPI, D>
where
    SPI: SpiDevice<u8>,
    D: DelayNs,
{
    /// Initialises the card: resets it, negotiates SDHC/SDXC, turns on CRCs and reads its size. The `SpiDevice` must be at
    /// 400 kHz or less, and the card must already have had its 74 init clocks (see `send_init_clocks`).
    pub async fn new(spi: SPI, delay: D) -> Result<Self, SpiError<SPI::Error>> {
        Self::init(spi, delay).await.map_err(|(_, error)| error)
    }

    /// `new`, giving the SPI device back when the card does not initialise (none fitted, a bad card), so the caller can
    /// try again.
    #[allow(clippy::type_complexity)]
    pub async fn init(spi: SPI, delay: D) -> Result<Self, (SPI, SpiError<SPI::Error>)> {
        let mut card = Self {
            spi,
            delay,
            high_capacity: false,
            blocks: 0,
            check_crc: false,
            token_window: START_TOKEN_WINDOW,
            scratch: [0xff; SCRATCH],
        };
        match card.initialise().await {
            Ok(()) => Ok(card),
            Err(error) => Err((card.spi, error)),
        }
    }

    async fn initialise(&mut self) -> Result<(), SpiError<SPI::Error>> {
        self.delay.delay_ms(1).await;

        // CMD0 is the one command a marginal card often misses. Retry it before
        // giving up, and keep the final Timeout rather than turning it into a
        // misleading bad response.
        let mut r1 = 0xff;
        let mut timed_out = false;
        for _ in 0..10 {
            match self.command(0, 0, Reply::R1).await {
                Ok(response) if response.r1 == 0x01 => {
                    r1 = 0x01;
                    break;
                }
                Ok(response) => {
                    r1 = response.r1;
                    timed_out = false;
                }
                Err(SpiError::Timeout) => {
                    r1 = 0xff;
                    timed_out = true;
                }
                Err(error) => return Err(error),
            }
            self.delay.delay_ms(1).await;
        }
        if r1 != 0x01 {
            return Err(if timed_out {
                SpiError::Timeout
            } else {
                SpiError::BadResponse(r1)
            });
        }

        let interface = self.command(8, 0x1aa, Reply::R1WithFourBytes).await?;
        let version_2 = interface.r1 & 0x04 == 0;
        if version_2 && (interface.r1 != 0x01 || interface.extra != [0x00, 0x00, 0x01, 0xaa]) {
            return Err(SpiError::BadResponse(interface.r1));
        }

        let host_capacity = if version_2 { 0x4000_0000 } else { 0 };
        let mut ready = false;
        for _ in 0..100 {
            let app = self.command(55, 0, Reply::R1).await?;
            if app.r1 > 0x01 {
                return Err(SpiError::BadResponse(app.r1));
            }
            if self.command(41, host_capacity, Reply::R1).await?.r1 == 0x00 {
                ready = true;
                break;
            }
            self.delay.delay_ms(10).await;
        }
        if !ready {
            return Err(SpiError::Timeout);
        }

        let ocr = self.command(58, 0, Reply::R1WithFourBytes).await?;
        if ocr.r1 != 0x00 {
            return Err(SpiError::BadResponse(ocr.r1));
        }
        self.high_capacity = ocr.extra[0] & 0x40 != 0;

        if !self.high_capacity {
            let length = self.command(16, BLOCK_SIZE as u32, Reply::R1).await?;
            if length.r1 != 0x00 {
                return Err(SpiError::BadResponse(length.r1));
            }
        }

        self.check_crc = self.command(59, 1, Reply::R1).await?.r1 == 0x00;

        let csd = self.read_csd().await?;
        self.blocks = blocks_from_csd(&csd).ok_or(SpiError::UnsupportedCard)?;
        Ok(())
    }

    /// The SPI device, when the card is no longer wanted.
    pub fn into_spi(self) -> SPI {
        self.spi
    }

    /// The card's size in 512-byte blocks.
    pub fn blocks(&self) -> u32 {
        self.blocks
    }

    /// Whether the card is SDHC or SDXC (block addressed) rather than standard capacity.
    pub fn is_high_capacity(&self) -> bool {
        self.high_capacity
    }

    /// The device, for raising its clock once the card is initialised.
    pub fn spi_mut(&mut self) -> &mut SPI {
        &mut self.spi
    }

    async fn read_block_inner(
        &mut self,
        block: u32,
        buffer: &mut [u8; BLOCK_SIZE],
    ) -> Result<(), SpiError<SPI::Error>> {
        let address = if self.high_capacity {
            block
        } else {
            block
                .checked_mul(BLOCK_SIZE as u32)
                .ok_or(SpiError::OutOfRange)?
        };
        self.read_with_data(17, address, buffer).await
    }

    /// Writes one block with CMD24. Chip select stays low from the command through the card's data response, so the
    /// card never sees it rise between the command and the block; the card's programming time is waited out
    /// afterwards, polling with chip select released between polls.
    #[cfg(feature = "write")]
    async fn write_block_inner(
        &mut self,
        block: u32,
        buffer: &[u8; BLOCK_SIZE],
    ) -> Result<(), SpiError<SPI::Error>> {
        if block >= self.blocks {
            return Err(SpiError::OutOfRange);
        }
        let address = if self.high_capacity {
            block
        } else {
            block
                .checked_mul(BLOCK_SIZE as u32)
                .ok_or(SpiError::OutOfRange)?
        };
        let mut result = self.write_block_once(address, buffer).await;
        for _ in 0..WRITE_RETRIES {
            if !matches!(result, Err(SpiError::BadCrc)) {
                break;
            }
            result = self.write_block_once(address, buffer).await;
        }
        result
    }

    #[cfg(feature = "write")]
    async fn write_block_once(
        &mut self,
        address: u32,
        buffer: &[u8; BLOCK_SIZE],
    ) -> Result<(), SpiError<SPI::Error>> {
        let frame = command_frame(24, address);
        let crc = crc16(buffer).to_be_bytes();
        // The card answers with an R1 within eight bytes and takes the data token any time after it, so the token goes
        // after a fixed window of idle bytes. If the R1 is not zero the card ignores the block and the error is
        // reported below.
        let mut r1_window = [0xffu8; R1_WINDOW];
        let mut response = [0xffu8; 1];
        self.spi
            .transaction(&mut [
                Operation::Write(&frame),
                Operation::TransferInPlace(&mut r1_window),
                Operation::Write(&[0xfe]),
                Operation::Write(buffer),
                Operation::Write(&crc),
                Operation::TransferInPlace(&mut response),
            ])
            .await
            .map_err(SpiError::Spi)?;

        let r1 = r1_window
            .iter()
            .copied()
            .find(|byte| *byte != 0xff)
            .ok_or(SpiError::Timeout)?;
        if r1 != 0x00 {
            return Err(SpiError::BadResponse(r1));
        }
        // The data response is `xxx0sss1`; `sss` is 010 accepted, 101 CRC error, 110 write error.
        let status = match response[0] & 0x1f {
            0x05 => Ok(()),
            0x0b => Err(SpiError::BadCrc),
            other => Err(SpiError::WriteRejected(other)),
        };
        // A card can hold the line low after a rejected block too, and would take the next command's bytes as part of
        // that: wait it out either way.
        self.wait_not_busy().await?;
        status
    }

    /// Waits for the card to finish programming: it holds the line low until it is done.
    #[cfg(feature = "write")]
    async fn wait_not_busy(&mut self) -> Result<(), SpiError<SPI::Error>> {
        for poll in 0..WRITE_FAST_POLLS + WRITE_SLOW_POLLS {
            let mut byte = [0xffu8];
            self.spi
                .transaction(&mut [Operation::TransferInPlace(&mut byte)])
                .await
                .map_err(SpiError::Spi)?;
            if byte[0] == 0xff {
                return Ok(());
            }
            if poll < WRITE_FAST_POLLS {
                self.delay.delay_us(READ_TOKEN_POLL_US).await;
            } else {
                self.delay.delay_ms(1).await;
            }
        }
        Err(SpiError::Timeout)
    }

    async fn read_csd(&mut self) -> Result<[u8; 16], SpiError<SPI::Error>> {
        let mut csd = [0u8; 16];
        self.read_with_data(9, 0, &mut csd).await?;
        Ok(csd)
    }

    /// A command that answers with a data block. A read whose token came later than the window allowed can fail on a
    /// card that cannot resume a block after chip select rises (hardware, 2026-11-25: about 1 % of scattered reads on
    /// an SDHC card, the same blocks at every clock). That failure widened the window, so the repeat is one
    /// transaction; the caller never sees the first failure.
    async fn read_with_data(
        &mut self,
        index: u8,
        argument: u32,
        buffer: &mut [u8],
    ) -> Result<(), SpiError<SPI::Error>> {
        let mut result = self.read_with_data_once(index, argument, buffer).await;
        for _ in 0..READ_RETRIES {
            if !matches!(
                result,
                Err(SpiError::BadResponse(_) | SpiError::Timeout | SpiError::BadCrc)
            ) {
                break;
            }
            result = self.read_with_data_once(index, argument, buffer).await;
        }
        result
    }

    /// One attempt: one chip-select assertion when the token arrives inside the window and polling otherwise.
    async fn read_with_data_once(
        &mut self,
        index: u8,
        argument: u32,
        buffer: &mut [u8],
    ) -> Result<(), SpiError<SPI::Error>> {
        let frame = command_frame(index, argument);
        let window = self.token_window;
        let length = R1_WINDOW + window + 1 + buffer.len() + 2;
        let scratch = &mut self.scratch[..length];
        scratch.fill(0xff);
        self.spi
            .transaction(&mut [
                Operation::Write(&frame),
                Operation::TransferInPlace(scratch),
            ])
            .await
            .map_err(SpiError::Spi)?;

        let r1_at = scratch[..R1_WINDOW]
            .iter()
            .position(|byte| *byte != 0xff)
            .ok_or(SpiError::Timeout)?;
        let r1 = scratch[r1_at];
        if r1 != 0x00 {
            return Err(SpiError::BadResponse(r1));
        }

        // The token can be anywhere after the R1, including past the window: the transaction clocks
        // idle bytes through the block's own part of the buffer too, so search all of it.
        let first_idle = r1_at + 1;
        let wanted = buffer.len() + 2;
        let mut crc = [0xffu8; 2];
        if let Some(gap) = scratch[first_idle..].iter().position(|byte| *byte != 0xff) {
            let token_at = first_idle + gap;
            let token = scratch[token_at];
            let after = &scratch[token_at + 1..];
            let have = after.len().min(wanted);
            let data_have = have.min(buffer.len());
            if token == 0xfe {
                buffer[..data_have].copy_from_slice(&after[..data_have]);
                crc[..have - data_have].copy_from_slice(&after[data_have..have]);
            }
            self.learn_gap(gap);
            if token != 0xfe {
                return Err(SpiError::BadResponse(token));
            }
            return if have == wanted {
                self.check_data(buffer, u16::from_be_bytes(crc))
            } else {
                self.finish_block(buffer, data_have, crc, have - data_have)
                    .await
            };
        }

        let mut waited = length - first_idle;
        self.token_window = MAX_TOKEN_WINDOW;
        let mut token = 0xff;
        for _ in 0..READ_TOKEN_POLLS {
            let mut byte = [0xff];
            self.spi
                .transaction(&mut [Operation::TransferInPlace(&mut byte)])
                .await
                .map_err(SpiError::Spi)?;
            token = byte[0];
            if token != 0xff {
                break;
            }
            waited += 1;
            self.delay.delay_us(READ_TOKEN_POLL_US).await;
        }
        if token != 0xfe {
            return Err(SpiError::BadResponse(token));
        }
        self.learn_gap(waited);
        self.finish_block(buffer, 0, crc, 0).await
    }

    /// Reads the rest of a block whose first `data_have` data bytes and `crc_have` CRC bytes are in hand.
    async fn finish_block(
        &mut self,
        buffer: &mut [u8],
        data_have: usize,
        mut crc: [u8; 2],
        crc_have: usize,
    ) -> Result<(), SpiError<SPI::Error>> {
        buffer[data_have..].fill(0xff);
        self.spi
            .transaction(&mut [
                Operation::TransferInPlace(&mut buffer[data_have..]),
                Operation::TransferInPlace(&mut crc[crc_have..]),
            ])
            .await
            .map_err(SpiError::Spi)?;
        self.check_data(buffer, u16::from_be_bytes(crc))
    }

    fn learn_gap(&mut self, gap: usize) {
        self.token_window = (gap * 2 + MIN_TOKEN_WINDOW).clamp(MIN_TOKEN_WINDOW, MAX_TOKEN_WINDOW);
    }

    fn check_data(&self, buffer: &[u8], received: u16) -> Result<(), SpiError<SPI::Error>> {
        if self.check_crc && crc16(buffer) != received {
            return Err(SpiError::BadCrc);
        }
        Ok(())
    }

    /// One chip-select assertion covers the command frame and its response whenever the response
    /// length is known, so the card never sees chip select drop between them.
    async fn command(
        &mut self,
        index: u8,
        argument: u32,
        reply: Reply,
    ) -> Result<Response, SpiError<SPI::Error>> {
        let frame = command_frame(index, argument);

        let mut bytes = [0xffu8; 12];
        self.spi
            .transaction(&mut [
                Operation::Write(&frame),
                Operation::TransferInPlace(&mut bytes),
            ])
            .await
            .map_err(SpiError::Spi)?;
        let position = bytes[..8]
            .iter()
            .position(|byte| *byte != 0xff)
            .ok_or(SpiError::Timeout)?;
        let mut extra = [0u8; 4];
        if let Reply::R1WithFourBytes = reply {
            extra.copy_from_slice(&bytes[position + 1..position + 5]);
        }
        Ok(Response {
            r1: bytes[position],
            extra,
        })
    }
}

impl<SPI, D> BlockDevice for SpiSdCard<SPI, D>
where
    SPI: SpiDevice<u8>,
    D: DelayNs,
{
    type Error = SpiError<SPI::Error>;

    async fn read_block(
        &mut self,
        block: u32,
        buffer: &mut [u8; BLOCK_SIZE],
    ) -> Result<(), Self::Error> {
        self.read_block_inner(block, buffer).await
    }

    async fn num_blocks(&mut self) -> Result<u32, Self::Error> {
        Ok(self.blocks)
    }
}

#[cfg(feature = "write")]
impl<SPI, D> crate::BlockWrite for SpiSdCard<SPI, D>
where
    SPI: SpiDevice<u8>,
    D: DelayNs,
{
    async fn write_block(
        &mut self,
        block: u32,
        buffer: &[u8; BLOCK_SIZE],
    ) -> Result<(), Self::Error> {
        self.write_block_inner(block, buffer).await
    }
}

fn command_frame(index: u8, argument: u32) -> [u8; 6] {
    let mut frame = [
        0x40 | index,
        (argument >> 24) as u8,
        (argument >> 16) as u8,
        (argument >> 8) as u8,
        argument as u8,
        0,
    ];
    frame[5] = crc7(&frame[..5]);
    frame
}

fn blocks_from_csd(csd: &[u8; 16]) -> Option<u32> {
    let structure = csd[0] >> 6;
    let blocks = match structure {
        0 => {
            let read_bl_len = u32::from(csd[5] & 0x0f);
            let c_size = (u64::from(csd[6] & 0x03) << 10)
                | (u64::from(csd[7]) << 2)
                | (u64::from(csd[8]) >> 6);
            let c_size_mult = (u32::from(csd[9] & 0x03) << 1) | (u32::from(csd[10]) >> 7);
            ((c_size + 1) << (c_size_mult + 2 + read_bl_len)) / BLOCK_SIZE as u64
        }
        1 => {
            let c_size =
                (u64::from(csd[7] & 0x3f) << 16) | (u64::from(csd[8]) << 8) | u64::from(csd[9]);
            (c_size + 1) * 1024
        }
        _ => return None,
    };
    Some(u32::try_from(blocks).unwrap_or(u32::MAX).max(1))
}

/// CRC7 with the SD polynomial x^7 + x^3 + 1, returned as the command frame's
/// CRC byte (shifted left and with the end bit set).
pub fn crc7(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &byte in data {
        for bit in (0..8).rev() {
            crc <<= 1;
            if ((byte >> bit) & 1) ^ ((crc >> 7) & 1) != 0 {
                crc ^= 0x09;
            }
        }
    }
    ((crc & 0x7f) << 1) | 1
}

/// CRC16 with the SD data polynomial x^16 + x^12 + x^5 + 1.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &byte in data {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::{blocks_from_csd, crc16, crc7};

    #[test]
    fn a_version_1_csd_with_the_largest_fields_does_not_overflow() {
        let mut csd = [0u8; 16];
        csd[5] = 0x0b;
        csd[6] = 0x03;
        csd[7] = 0xff;
        csd[8] = 0xc0;
        csd[9] = 0x03;
        csd[10] = 0x80;
        assert_eq!(blocks_from_csd(&csd), Some(1 << 23));
    }

    #[test]
    fn a_version_2_csd_at_the_maximum_c_size_saturates_instead_of_overflowing() {
        let mut csd = [0u8; 16];
        csd[0] = 0x40;
        csd[7] = 0x3f;
        csd[8] = 0xff;
        csd[9] = 0xff;
        assert_eq!(blocks_from_csd(&csd), Some(u32::MAX));
    }

    #[test]
    fn an_unknown_csd_structure_is_unsupported() {
        let mut csd = [0u8; 16];
        csd[0] = 0x80;
        assert_eq!(blocks_from_csd(&csd), None);
    }

    #[test]
    fn crc7_matches_the_sd_command_frames() {
        assert_eq!(crc7(&[0x40, 0x00, 0x00, 0x00, 0x00]), 0x95);
        assert_eq!(crc7(&[0x48, 0x00, 0x00, 0x01, 0xaa]), 0x87);
    }

    #[test]
    fn crc16_matches_the_sd_data_reference() {
        // CRC-16/XMODEM (initial value 0), the SD data CRC.
        assert_eq!(crc16(b"123456789"), 0x31c3);
    }
}
