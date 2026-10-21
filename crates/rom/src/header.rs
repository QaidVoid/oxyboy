//! GBA cartridge header parsing.
//!
//! Field offsets and sizes follow GBATEK, "The Cartridge Header": entry
//! point word at `0x000`, Nintendo logo at `0x004..0x0A0`, title at
//! `0x0A0..0x0AC`, game code at `0x0AC..0x0B0`, maker code at
//! `0x0B0..0x0B2`, unit code at `0x0B2`, main unit code at `0x0B3`,
//! device type at `0x0B4`, seven reserved bytes at `0x0B5..0x0BC`,
//! software version at `0x0BC`, and the complement checksum at `0x0BD`.

use std::fmt;

/// Length of the header in bytes.
const HEADER_LEN: usize = 0x100;

/// Start of the title field.
const TITLE_OFFSET: usize = 0x0A0;

/// Offset of the stored complement checksum.
const CHECKSUM_OFFSET: usize = 0x0BD;

/// A parsed GBA cartridge header.
///
/// All fields are the raw bytes of the image; no normalization is applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    /// Raw 32-bit word at `0x000`. On a real image this is an ARM branch
    /// to the entry code.
    pub entry_word: u32,

    /// Nintendo logo bytes at `0x004..0x0A0`. The real bitmap is
    /// copyrighted and is not distributed with this crate; see
    /// [`Header::logo_matches`].
    pub logo: [u8; 156],

    /// Game title at `0x0A0..0x0AC`, uppercase ASCII padded with NUL.
    pub title: [u8; 12],

    /// Game code at `0x0AC..0x0B0`, four ASCII characters.
    pub game_code: [u8; 4],

    /// Maker code at `0x0B0..0x0B2`, two ASCII characters.
    pub maker_code: [u8; 2],

    /// Unit code at `0x0B2`. Valid images carry the fixed value `0x96`
    /// (GBATEK, "The Cartridge Header").
    pub unit_code: u8,

    /// Main unit code at `0x0B3`; `0x00` on all known images.
    pub main_unit_code: u8,

    /// Device type at `0x0B4`; `0x00` marks a normal GBA game.
    pub device_type: u8,

    /// Reserved bytes at `0x0B5..0x0BC`. Must be zero but participate in
    /// the checksum, so they are preserved.
    pub reserved: [u8; 7],

    /// Software version at `0x0BC`, usually `0x00`.
    pub version: u8,

    /// Stored complement checksum at `0x0BD`.
    pub complement_checksum: u8,
}

/// Errors produced while parsing a header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderError {
    /// The image is shorter than the 256-byte header.
    TooShort {
        /// Number of bytes the image actually has.
        len: usize,
    },
}

impl fmt::Display for HeaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HeaderError::TooShort { len } => {
                write!(f, "image is {len} bytes; the header needs 256")
            }
        }
    }
}

impl std::error::Error for HeaderError {}

impl Header {
    /// Parses the header from the first 256 bytes of a full image.
    ///
    /// # Errors
    ///
    /// Returns [`HeaderError::TooShort`] when `image` is shorter than the
    /// header. Parsing never validates the complement checksum; see
    /// [`Header::checksum_valid`].
    pub fn parse(image: &[u8]) -> Result<Self, HeaderError> {
        if image.len() < HEADER_LEN {
            return Err(HeaderError::TooShort { len: image.len() });
        }
        let mut header = Self {
            entry_word: 0,
            logo: [0; 156],
            title: [0; 12],
            game_code: [0; 4],
            maker_code: [0; 2],
            unit_code: 0,
            main_unit_code: 0,
            device_type: 0,
            reserved: [0; 7],
            version: 0,
            complement_checksum: 0,
        };
        header.entry_word = {
            let mut word = [0u8; 4];
            word.copy_from_slice(&image[0x000..0x004]);
            u32::from_le_bytes(word)
        };
        header.logo.copy_from_slice(&image[0x004..0x0A0]);
        header.title.copy_from_slice(&image[TITLE_OFFSET..0x0AC]);
        header.game_code.copy_from_slice(&image[0x0AC..0x0B0]);
        header.maker_code.copy_from_slice(&image[0x0B0..0x0B2]);
        header.unit_code = image[0x0B2];
        header.main_unit_code = image[0x0B3];
        header.device_type = image[0x0B4];
        header.reserved.copy_from_slice(&image[0x0B5..0x0BC]);
        header.version = image[0x0BC];
        header.complement_checksum = image[CHECKSUM_OFFSET];
        Ok(header)
    }

    /// Computes the complement checksum the header should carry:
    /// `0x100 - sum(bytes 0x0A0..0x0BD) mod 256` (GBATEK, "The Cartridge
    /// Header").
    #[must_use]
    pub fn expected_checksum(&self) -> u8 {
        let mut sum = 0u32;
        for byte in self
            .title
            .iter()
            .chain(&self.game_code)
            .chain(&self.maker_code)
        {
            sum += u32::from(*byte);
        }
        for byte in [
            self.unit_code,
            self.main_unit_code,
            self.device_type,
            self.version,
        ]
        .into_iter()
        .chain(self.reserved)
        {
            sum += u32::from(byte);
        }
        (0x100u32.wrapping_sub(sum) & 0xFF) as u8
    }

    /// Reports whether the stored complement checksum matches the header
    /// contents. False on patched or homebrew images with a stale header.
    #[must_use]
    pub fn checksum_valid(&self) -> bool {
        self.complement_checksum == self.expected_checksum()
    }

    /// Compares the stored logo against `expected`. The real Nintendo logo
    /// is not shipped with this crate; callers supply their own reference.
    #[must_use]
    pub fn logo_matches(&self, expected: &[u8; 156]) -> bool {
        self.logo == *expected
    }

    /// Title as UTF-8 with trailing NUL and space padding removed, or
    /// `None` when the field is not valid UTF-8.
    #[must_use]
    pub fn title_str(&self) -> Option<&str> {
        ascii_field(&self.title)
    }

    /// Game code as UTF-8 with trailing NUL and space padding removed, or
    /// `None` when the field is not valid UTF-8.
    #[must_use]
    pub fn game_code_str(&self) -> Option<&str> {
        ascii_field(&self.game_code)
    }

    /// Maker code as UTF-8 with trailing NUL and space padding removed, or
    /// `None` when the field is not valid UTF-8.
    #[must_use]
    pub fn maker_code_str(&self) -> Option<&str> {
        ascii_field(&self.maker_code)
    }
}

/// Trims NUL and space padding and validates the field as UTF-8.
fn ascii_field(bytes: &[u8]) -> Option<&str> {
    let end = bytes
        .iter()
        .rposition(|&b| b != 0 && b != b' ')
        .map_or(0, |last| last + 1);
    std::str::from_utf8(&bytes[..end]).ok()
}

#[cfg(test)]
mod tests {
    use super::Header;

    fn header_from(bytes: &[u8]) -> Header {
        Header::parse(bytes).expect("256-byte image")
    }

    #[test]
    fn parses_every_field() {
        let mut image = [0u8; 0x100];
        image[0x000..0x004].copy_from_slice(&0xEAFF_FFFEu32.to_le_bytes());
        image[0x004..0x0A0].fill(0xA5);
        image[0x0A0..0x0A5].copy_from_slice(b"TITLE");
        image[0x0AC..0x0B0].copy_from_slice(b"ABCD");
        image[0x0B0..0x0B2].copy_from_slice(b"01");
        image[0x0B2] = 0x96;
        image[0x0B4] = 0x03;
        image[0x0BC] = 0x07;
        let checksum = {
            let sum: u32 = image[0x0A0..0x0BD].iter().map(|&b| u32::from(b)).sum();
            (0x100u32.wrapping_sub(sum) & 0xFF) as u8
        };
        image[0x0BD] = checksum;
        let header = header_from(&image);
        assert_eq!(header.entry_word, 0xEAFF_FFFE);
        assert!(header.logo.iter().all(|&b| b == 0xA5));
        assert_eq!(header.title_str(), Some("TITLE"));
        assert_eq!(header.game_code_str(), Some("ABCD"));
        assert_eq!(header.maker_code_str(), Some("01"));
        assert_eq!(header.unit_code, 0x96);
        assert_eq!(header.main_unit_code, 0);
        assert_eq!(header.device_type, 0x03);
        assert_eq!(header.version, 0x07);
        assert_eq!(header.complement_checksum, checksum);
        assert!(header.checksum_valid());
    }

    #[test]
    fn detects_bad_checksum() {
        let mut image = [0u8; 0x100];
        image[0x0A0..0x0A6].copy_from_slice(b"BROKEN");
        let mut header = header_from(&image);
        assert!(!header.checksum_valid());
        header.complement_checksum = header.expected_checksum();
        assert!(header.checksum_valid());
    }

    #[test]
    fn rejects_short_images() {
        let image = [0u8; 255];
        assert_eq!(
            Header::parse(&image),
            Err(super::HeaderError::TooShort { len: 255 })
        );
    }

    #[test]
    fn ascii_fields_reject_non_utf8() {
        let mut image = [0u8; 0x100];
        image[0x0A0..0x0AC].copy_from_slice(&[0x41, 0xFF, 0x42, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let header = header_from(&image);
        assert_eq!(header.title_str(), None);
    }
}
