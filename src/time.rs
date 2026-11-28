//! The time a volume stamps on what it writes.

/// A date and time. FAT stores them to a two-second resolution and the years 1980 to 2107.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timestamp {
    /// 1980 to 2107.
    pub year: u16,
    /// 1 to 12.
    pub month: u8,
    /// 1 to 31.
    pub day: u8,
    /// 0 to 23.
    pub hour: u8,
    /// 0 to 59.
    pub minute: u8,
    /// 0 to 59.
    pub second: u8,
}

impl Timestamp {
    /// 1980-01-01 00:00:00, the earliest time FAT can hold. A volume stamps this until it is given a clock.
    pub const FAT_EPOCH: Timestamp = Timestamp {
        year: 1980,
        month: 1,
        day: 1,
        hour: 0,
        minute: 0,
        second: 0,
    };

    /// The FAT date word: year since 1980 in bits 9 to 15, month in 5 to 8, day in 0 to 4. Out-of-range fields are
    /// clamped.
    pub(crate) fn fat_date(&self) -> u16 {
        let year = self.year.clamp(1980, 2107) - 1980;
        let month = u16::from(self.month.clamp(1, 12));
        let day = u16::from(self.day.clamp(1, 31));
        (year << 9) | (month << 5) | day
    }

    /// The FAT time word: hour in bits 11 to 15, minute in 5 to 10, seconds divided by two in 0 to 4.
    pub(crate) fn fat_time(&self) -> u16 {
        let hour = u16::from(self.hour.min(23));
        let minute = u16::from(self.minute.min(59));
        let second = u16::from(self.second.min(59));
        (hour << 11) | (minute << 5) | (second / 2)
    }
}

pub(crate) fn default_clock() -> Timestamp {
    Timestamp::FAT_EPOCH
}
