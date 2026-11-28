#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use sdmmc_async::crc16;

#[derive(Clone, Copy, Debug)]
pub struct SpiFaults {
    pub silent: bool,
    pub bad_crc: bool,
    pub corrupt_data: Option<(u32, usize, u8)>,
    pub card_present: bool,
    /// Idle bytes the card sends between the R1 and the data token.
    pub token_gap: usize,
    /// A strict card forgets a queued response when chip select rises.
    pub drop_response_on_cs_rise: bool,
    pub data_error_token: bool,
    pub bad_cmd8_echo: bool,
    /// Drop this many CMD0 responses (the card misses the command).
    pub cmd0_failures: u32,
    /// Clocks the card holds the line low (busy) after accepting a written block.
    pub write_busy_clocks: usize,
    /// The card never finishes programming: busy forever after a write.
    pub write_busy_forever: bool,
    /// Answer the next this-many written blocks with a CRC-error response and do not store them.
    pub write_crc_errors: u32,
    /// Answer every written block with this data response token (low five bits) and do not store it.
    pub write_response: Option<u8>,
}

impl Default for SpiFaults {
    fn default() -> Self {
        Self {
            silent: false,
            bad_crc: false,
            corrupt_data: None,
            card_present: true,
            token_gap: 2,
            drop_response_on_cs_rise: false,
            data_error_token: false,
            bad_cmd8_echo: false,
            cmd0_failures: 0,
            write_busy_clocks: 4,
            write_busy_forever: false,
            write_crc_errors: 0,
            write_response: None,
        }
    }
}

pub struct FakeCard {
    image: Vec<u8>,
    high_capacity: bool,
    app_cmd: bool,
    selected: bool,
    command: [u8; 6],
    command_len: usize,
    response: VecDeque<u8>,
    faults: SpiFaults,
    csd: [u8; 16],
    ocr: [u8; 4],
    reads: usize,
    legacy_v1: bool,
    host_capacity_bits: Vec<bool>,
    deselected_clocks: usize,
    assertions: usize,
    bad_mosi_bytes: usize,
    write: Option<WriteInProgress>,
    busy: usize,
    writes: usize,
}

struct WriteInProgress {
    lba: u32,
    token_seen: bool,
    received: Vec<u8>,
}

impl FakeCard {
    pub fn new(image: Vec<u8>, high_capacity: bool) -> Self {
        let blocks = ((image.len() / 512) as u32).max(1);
        let csd = if high_capacity {
            csd_v2(blocks.max(1024))
        } else {
            csd_v1(blocks)
        };
        let ocr = if high_capacity {
            [0xc0, 0xff, 0x80, 0x00]
        } else {
            [0x80, 0xff, 0x80, 0x00]
        };
        Self {
            image,
            high_capacity,
            app_cmd: false,
            selected: false,
            command: [0; 6],
            command_len: 0,
            response: VecDeque::new(),
            faults: SpiFaults::default(),
            csd,
            ocr,
            reads: 0,
            legacy_v1: false,
            host_capacity_bits: Vec::new(),
            deselected_clocks: 0,
            assertions: 0,
            bad_mosi_bytes: 0,
            write: None,
            busy: 0,
            writes: 0,
        }
    }

    /// The card's contents.
    pub fn image(&self) -> &[u8] {
        &self.image
    }

    /// Blocks stored by CMD24 so far.
    pub fn writes(&self) -> usize {
        self.writes
    }

    pub fn set_faults(&mut self, faults: SpiFaults) {
        self.faults = faults;
    }

    pub fn faults(&self) -> SpiFaults {
        self.faults
    }

    pub fn reads(&self) -> usize {
        self.reads
    }

    /// Bytes the host clocked while reading a response or data with MOSI not held high (SD needs 0xFF).
    pub fn bad_mosi_bytes(&self) -> usize {
        self.bad_mosi_bytes
    }

    pub fn assertions(&self) -> usize {
        self.assertions
    }

    pub fn selected(&self) -> bool {
        self.selected
    }

    pub fn deselected_clocks(&self) -> usize {
        self.deselected_clocks
    }

    pub fn host_capacity_bits(&self) -> &[bool] {
        &self.host_capacity_bits
    }

    pub fn set_legacy_v1(&mut self, legacy: bool) {
        self.legacy_v1 = legacy;
    }

    /// The card keeps a queued response while chip select is high and only restarts command parsing
    /// when it is asserted again, which is how real cards tolerate a transaction ending between a
    /// command and its response.
    pub fn set_cs(&mut self, high: bool) {
        self.selected = !high;
        if high && self.faults.drop_response_on_cs_rise {
            self.response.clear();
        }
        if high {
            // A block the host did not finish sending is abandoned.
            self.write = None;
        }
        if !high {
            self.command_len = 0;
            self.assertions += 1;
        }
    }

    pub fn next_byte(&mut self, mosi: u8) -> u8 {
        if !self.selected {
            self.deselected_clocks += 1;
        }
        if !self.selected || !self.faults.card_present {
            return 0xff;
        }
        if let Some(byte) = self.response.pop_front() {
            if mosi != 0xff {
                self.bad_mosi_bytes += 1;
            }
            return byte;
        }
        if self.busy > 0 {
            if self.busy != usize::MAX {
                self.busy -= 1;
            }
            return 0x00;
        }
        if let Some(write) = self.write.as_mut() {
            if !write.token_seen {
                write.token_seen = mosi == 0xfe;
            } else {
                write.received.push(mosi);
                if write.received.len() == 514 {
                    self.finish_write();
                }
            }
            return 0xff;
        }
        if self.command_len < 6 {
            self.command[self.command_len] = mosi;
            self.command_len += 1;
            if self.command_len == 6 {
                self.process_command();
            }
            return 0xff;
        }
        0xff
    }

    fn process_command(&mut self) {
        let frame = self.command;
        let index = frame[0] & 0x3f;
        let argument = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]);
        if self.faults.silent {
            return;
        }
        if index == 0 && self.faults.cmd0_failures > 0 {
            self.faults.cmd0_failures -= 1;
            return;
        }
        if index == 55 {
            self.app_cmd = true;
            self.response.push_back(0x01);
            return;
        }
        if index == 41 {
            self.host_capacity_bits.push(argument & 0x4000_0000 != 0);
            if self.app_cmd {
                self.app_cmd = false;
                self.response.push_back(0x00);
            } else {
                self.response.push_back(0x05);
            }
            return;
        }
        self.app_cmd = false;
        match index {
            0 => self.response.push_back(0x01),
            8 if self.legacy_v1 => self.response.push_back(0x05),
            8 => {
                self.response.push_back(0x01);
                let echo = if self.faults.bad_cmd8_echo {
                    [0x00, 0x00, 0x00, 0x55]
                } else {
                    [0x00, 0x00, 0x01, 0xaa]
                };
                self.response.extend(echo);
            }
            58 => {
                let ocr = self.ocr;
                self.response.push_back(0x00);
                self.response.extend(ocr);
            }
            16 | 59 => self.response.push_back(0x00),
            13 => {
                self.response.push_back(0x00);
                self.response.push_back(0x00);
            }
            9 => {
                let csd = self.csd;
                self.response.push_back(0x00);
                self.queue_data(&csd);
            }
            10 => {
                let cid = [0u8; 16];
                self.response.push_back(0x00);
                self.queue_data(&cid);
            }
            17 => {
                let lba = if self.high_capacity {
                    argument
                } else {
                    argument / 512
                };
                let start = lba as usize * 512;
                if start + 512 <= self.image.len() {
                    let mut data = self.image[start..start + 512].to_vec();
                    let original_crc = crc16(&data);
                    if let Some((block, offset, mask)) = self.faults.corrupt_data {
                        if block == lba {
                            data[offset % 512] ^= mask;
                        }
                    }
                    self.response.push_back(0x00);
                    if self.faults.data_error_token {
                        self.response.push_back(0x01);
                    } else {
                        self.queue_data_with_crc(&data, Some(original_crc));
                    }
                    self.reads += 1;
                } else {
                    self.response.push_back(0x08);
                }
            }
            24 => {
                let lba = if self.high_capacity {
                    argument
                } else {
                    argument / 512
                };
                if lba as usize * 512 + 512 <= self.image.len() {
                    self.response.push_back(0x00);
                    self.write = Some(WriteInProgress {
                        lba,
                        token_seen: false,
                        received: Vec::new(),
                    });
                } else {
                    self.response.push_back(0x08);
                }
            }
            _ => self.response.push_back(0x05),
        }
    }

    /// The host has sent a whole block and its CRC: answer with the data response, then hold the line low while the card
    /// programs.
    fn finish_write(&mut self) {
        let write = self.write.take().unwrap();
        let (data, crc) = write.received.split_at(512);
        let crc_ok = crc16(data) == u16::from_be_bytes([crc[0], crc[1]]);
        let token = if self.faults.write_crc_errors > 0 {
            self.faults.write_crc_errors -= 1;
            0x0b
        } else if !crc_ok {
            0x0b
        } else if let Some(token) = self.faults.write_response {
            token
        } else {
            let start = write.lba as usize * 512;
            self.image[start..start + 512].copy_from_slice(data);
            self.writes += 1;
            0x05
        };
        // `xxx0sss1`: the status is in bits 1 to 3, the top three bits are undefined.
        self.response.push_back(0xe0 | token);
        self.busy = if self.faults.write_busy_forever {
            usize::MAX
        } else {
            self.faults.write_busy_clocks
        };
    }

    fn queue_data(&mut self, data: &[u8]) {
        self.queue_data_with_crc(data, None);
    }

    fn queue_data_with_crc(&mut self, data: &[u8], crc_override: Option<u16>) {
        for _ in 0..self.faults.token_gap {
            self.response.push_back(0xff);
        }
        self.response.push_back(0xfe);
        self.response.extend(data.iter().copied());
        let mut crc = crc_override.unwrap_or_else(|| crc16(data));
        if self.faults.bad_crc {
            crc ^= 0xffff;
        }
        self.response.push_back((crc >> 8) as u8);
        self.response.push_back(crc as u8);
    }
}

fn csd_v1(blocks: u32) -> [u8; 16] {
    let blocks = blocks.max(4);
    let c_size = (blocks / 4).saturating_sub(1);
    let mut csd = [0u8; 16];
    csd[0] = 0x00;
    csd[5] = 9;
    csd[6] = ((c_size >> 10) & 0x03) as u8;
    csd[7] = (c_size >> 2) as u8;
    csd[8] = ((c_size & 0x03) << 6) as u8;
    csd
}

fn csd_v2(blocks: u32) -> [u8; 16] {
    let c_size = (blocks / 1024).saturating_sub(1);
    let mut csd = [0u8; 16];
    csd[0] = 0x40;
    csd[5] = 9;
    csd[7] = ((c_size >> 16) & 0x3f) as u8;
    csd[8] = (c_size >> 8) as u8;
    csd[9] = c_size as u8;
    csd
}

/// A device on the bus: chip select is asserted around each transaction and released after it.
pub struct SimSpi {
    card: Rc<RefCell<FakeCard>>,
}

impl embedded_hal::spi::ErrorType for SimSpi {
    type Error = core::convert::Infallible;
}

impl embedded_hal_async::spi::SpiDevice<u8> for SimSpi {
    async fn transaction(
        &mut self,
        operations: &mut [embedded_hal_async::spi::Operation<'_, u8>],
    ) -> Result<(), core::convert::Infallible> {
        use embedded_hal_async::spi::Operation;
        self.card.borrow_mut().set_cs(false);
        for operation in operations.iter_mut() {
            match operation {
                Operation::Read(words) => {
                    for word in words.iter_mut() {
                        *word = self.card.borrow_mut().next_byte(0xff);
                    }
                }
                Operation::Write(words) => {
                    for &word in words.iter() {
                        let _ = self.card.borrow_mut().next_byte(word);
                    }
                }
                Operation::Transfer(read, write) => {
                    for (index, word) in read.iter_mut().enumerate() {
                        let mosi = write.get(index).copied().unwrap_or(0xff);
                        *word = self.card.borrow_mut().next_byte(mosi);
                    }
                }
                Operation::TransferInPlace(words) => {
                    for word in words.iter_mut() {
                        *word = self.card.borrow_mut().next_byte(*word);
                    }
                }
                Operation::DelayNs(_) => {}
            }
        }
        self.card.borrow_mut().set_cs(true);
        Ok(())
    }
}

/// The raw bus with no chip select, for the clocks a card needs before its first command.
pub struct SimBus {
    card: Rc<RefCell<FakeCard>>,
}

impl embedded_hal::spi::ErrorType for SimBus {
    type Error = core::convert::Infallible;
}

impl embedded_hal_async::spi::SpiBus<u8> for SimBus {
    async fn read(&mut self, words: &mut [u8]) -> Result<(), core::convert::Infallible> {
        for word in words {
            *word = self.card.borrow_mut().next_byte(0xff);
        }
        Ok(())
    }

    async fn write(&mut self, words: &[u8]) -> Result<(), core::convert::Infallible> {
        for &word in words {
            let _ = self.card.borrow_mut().next_byte(word);
        }
        Ok(())
    }

    async fn transfer(
        &mut self,
        read: &mut [u8],
        write: &[u8],
    ) -> Result<(), core::convert::Infallible> {
        for (index, word) in read.iter_mut().enumerate() {
            let mosi = write.get(index).copied().unwrap_or(0xff);
            *word = self.card.borrow_mut().next_byte(mosi);
        }
        Ok(())
    }

    async fn transfer_in_place(
        &mut self,
        words: &mut [u8],
    ) -> Result<(), core::convert::Infallible> {
        for word in words {
            *word = self.card.borrow_mut().next_byte(*word);
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), core::convert::Infallible> {
        Ok(())
    }
}

pub struct SimDelay;

impl embedded_hal_async::delay::DelayNs for SimDelay {
    async fn delay_ns(&mut self, _ns: u32) {}
}

pub struct SpiBench {
    card: Rc<RefCell<FakeCard>>,
}

impl SpiBench {
    pub fn new(image: Vec<u8>, high_capacity: bool) -> Self {
        Self {
            card: Rc::new(RefCell::new(FakeCard::new(image, high_capacity))),
        }
    }

    pub fn spi(&self) -> SimSpi {
        SimSpi {
            card: Rc::clone(&self.card),
        }
    }

    pub fn bus(&self) -> SimBus {
        SimBus {
            card: Rc::clone(&self.card),
        }
    }

    pub fn delay(&self) -> SimDelay {
        SimDelay
    }

    pub fn set_faults(&self, faults: SpiFaults) {
        self.card.borrow_mut().set_faults(faults);
    }

    pub fn faults(&self) -> SpiFaults {
        self.card.borrow().faults()
    }

    pub fn reads(&self) -> usize {
        self.card.borrow().reads()
    }

    pub fn writes(&self) -> usize {
        self.card.borrow().writes()
    }

    pub fn image(&self) -> Vec<u8> {
        self.card.borrow().image().to_vec()
    }

    pub fn selected(&self) -> bool {
        self.card.borrow().selected()
    }

    pub fn assertions(&self) -> usize {
        self.card.borrow().assertions()
    }

    pub fn bad_mosi_bytes(&self) -> usize {
        self.card.borrow().bad_mosi_bytes()
    }

    pub fn deselected_clocks(&self) -> usize {
        self.card.borrow().deselected_clocks()
    }

    pub fn host_capacity_bits(&self) -> Vec<bool> {
        self.card.borrow().host_capacity_bits().to_vec()
    }

    pub fn set_legacy_v1(&self, legacy: bool) {
        self.card.borrow_mut().set_legacy_v1(legacy);
    }
}
