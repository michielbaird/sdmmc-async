/// The longest name, in bytes.
pub const MAX_NAME: usize = 255;

/// A file or directory name, stored as UTF-8. FAT names are ASCII; exFAT names
/// are UTF-16 and are transcoded. Names longer than `MAX_NAME` bytes are
/// rejected rather than truncated.
#[derive(Clone, Copy)]
pub struct Name {
    bytes: [u8; MAX_NAME],
    len: u8,
}

impl Name {
    /// A name with no characters.
    pub const fn empty() -> Self {
        Self {
            bytes: [0; MAX_NAME],
            len: 0,
        }
    }

    /// The name as text.
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len as usize]).unwrap_or("")
    }

    /// Whether the name has no characters.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether the name equals `other`, ignoring ASCII case.
    pub fn eq_ignore_ascii_case(&self, other: &str) -> bool {
        self.as_str().eq_ignore_ascii_case(other)
    }

    #[cfg(feature = "fat32")]
    pub(crate) fn push_byte(&mut self, byte: u8) -> Result<(), ()> {
        if self.len as usize >= MAX_NAME {
            return Err(());
        }
        self.bytes[self.len as usize] = byte;
        self.len += 1;
        Ok(())
    }

    pub(crate) fn push_char(&mut self, c: char) -> Result<(), ()> {
        let mut buffer = [0u8; 4];
        let encoded = c.encode_utf8(&mut buffer);
        let start = self.len as usize;
        if start + encoded.len() > MAX_NAME {
            return Err(());
        }
        self.bytes[start..start + encoded.len()].copy_from_slice(encoded.as_bytes());
        self.len += encoded.len() as u8;
        Ok(())
    }
}

impl core::fmt::Debug for Name {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("Name").field(&self.as_str()).finish()
    }
}

/// Whether `name` can be stored on FAT32 or exFAT at all: 1 to 255 UTF-16 units, no control characters or any of
/// `\ / : * ? " < > |`, and no trailing space or dot.
#[cfg(feature = "write")]
pub(crate) fn is_valid_name(name: &str) -> bool {
    if name.is_empty() || name.encode_utf16().count() > MAX_NAME {
        return false;
    }
    if name.ends_with(' ') || name.ends_with('.') {
        return false;
    }
    !name
        .chars()
        .any(|c| (c as u32) < 0x20 || "\\/:*?\"<>|".contains(c))
}
