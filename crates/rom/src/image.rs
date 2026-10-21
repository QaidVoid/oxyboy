//! Loading and classification of whole `.gba` images.
//!
//! An [`Image`] owns the raw bytes plus the parsed [`Header`] and a
//! heuristic [`ImageKind`] classification.

use std::fmt;
use std::path::Path;

use crate::header::{Header, HeaderError};

/// Largest accepted image: 32 MiB, the maximum GBA ROM size (GBATEK, "GBA
/// Memory Map").
pub const MAX_ROM_SIZE: usize = 32 * 1024 * 1024;

/// Largest size considered for multiboot classification: 256 KiB, the
/// maximum EWRAM-resident image (GBATEK, "GBA Multiboot").
const MULTIBOOT_MAX_SIZE: usize = 256 * 1024;

/// EWRAM address range, where multiboot images execute.
const EWRAM: std::ops::Range<u32> = 0x0200_0000..0x0204_0000;

/// Execution base of a multiboot image, which the BIOS copies to EWRAM.
const MULTIBOOT_BASE: u32 = 0x0200_0000;

/// Errors produced while loading an image.
#[derive(Debug)]
pub enum LoadError {
    /// Filesystem access failed.
    Io(std::io::Error),
    /// The image exceeds [`MAX_ROM_SIZE`].
    TooLarge {
        /// Size of the image in bytes.
        size: usize,
    },
    /// The header could not be parsed.
    Header(HeaderError),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::Io(e) => write!(f, "io error: {e}"),
            LoadError::TooLarge { size } => {
                write!(f, "image is {size} bytes; the limit is {MAX_ROM_SIZE}")
            }
            LoadError::Header(e) => write!(f, "bad header: {e}"),
        }
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LoadError::Io(e) => Some(e),
            LoadError::Header(e) => Some(e),
            LoadError::TooLarge { .. } => None,
        }
    }
}

impl From<std::io::Error> for LoadError {
    fn from(e: std::io::Error) -> Self {
        LoadError::Io(e)
    }
}

impl From<HeaderError> for LoadError {
    fn from(e: HeaderError) -> Self {
        LoadError::Header(e)
    }
}

/// Shape of a loaded image.
///
/// Classification is a content heuristic and is ambiguous for small images;
/// see [`ImageKind::Multiboot`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageKind {
    /// A normal cartridge image executing from ROM at `0x08000000`.
    Cart,
    /// A multiboot image the BIOS copies to EWRAM and executes from
    /// `0x02000000`.
    ///
    /// Detected when the image is at most 256 KiB and the entry word is an
    /// unconditional ARM branch whose target, decoded at the multiboot
    /// base `0x02000000`, lands inside EWRAM. The entry word only encodes
    /// a PC-relative offset, so a small cart ROM with a short forward
    /// entry branch decodes to the same EWRAM target and is misclassified;
    /// the shapes are indistinguishable from content alone. VBA-M
    /// sidesteps this by trusting the `.mb` file extension instead
    /// (VBA-M 1.9.9, `src/gba/GBA.cpp`).
    Multiboot,
}

/// A loaded `.gba` image: raw bytes, parsed header, and classification.
#[derive(Clone, Debug)]
pub struct Image {
    header: Header,
    kind: ImageKind,
    bytes: Vec<u8>,
}

impl Image {
    /// Loads an image from bytes.
    ///
    /// # Errors
    ///
    /// Returns [`LoadError::TooLarge`] when the image exceeds
    /// [`MAX_ROM_SIZE`] and [`LoadError::Header`] when the header cannot
    /// be parsed. Images whose size is not a multiple of four are accepted
    /// and can be flagged with [`Image::size_is_multiple_of_4`].
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, LoadError> {
        if bytes.len() > MAX_ROM_SIZE {
            return Err(LoadError::TooLarge { size: bytes.len() });
        }
        let header = Header::parse(&bytes)?;
        let kind = classify(&bytes, &header);
        Ok(Self {
            header,
            kind,
            bytes,
        })
    }

    /// Loads an image from a file.
    ///
    /// # Errors
    ///
    /// Returns [`LoadError::Io`] when the file cannot be read, otherwise
    /// the errors of [`Image::from_bytes`].
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, LoadError> {
        Self::from_bytes(std::fs::read(path)?)
    }

    /// The parsed cartridge header.
    #[must_use]
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// The heuristic classification of the image.
    #[must_use]
    pub fn kind(&self) -> ImageKind {
        self.kind
    }

    /// The full raw bytes of the image.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Whether the image size is a multiple of four. Real cartridges are
    /// padded to four bytes; shorter sizes hint at a truncated dump.
    #[must_use]
    pub fn size_is_multiple_of_4(&self) -> bool {
        self.bytes.len().is_multiple_of(4)
    }
}

/// Classifies an image by size and entry branch target.
fn classify(bytes: &[u8], header: &Header) -> ImageKind {
    if bytes.len() > MULTIBOOT_MAX_SIZE {
        return ImageKind::Cart;
    }
    match decode_branch_target(header.entry_word, MULTIBOOT_BASE) {
        Some(target) if EWRAM.contains(&target) => ImageKind::Multiboot,
        _ => ImageKind::Cart,
    }
}

/// Decodes an ARM `b` instruction word into its target address.
///
/// Returns `None` unless the word is an unconditional branch (`cond` =
/// `0xE`, ARM ARM section A4.1.2). The target is `at + 8 + offset` with
/// the 24-bit offset sign extended and scaled by four (ARM ARM A4.1.2).
fn decode_branch_target(word: u32, at: u32) -> Option<u32> {
    if word & 0xFF00_0000 != 0xEA00_0000 {
        return None;
    }
    let imm24 = (word & 0x00FF_FFFF) as i32;
    let offset = ((imm24 << 8) >> 6) as u32;
    Some(at.wrapping_add(8).wrapping_add(offset))
}

#[cfg(test)]
mod tests {
    use super::{ImageKind, decode_branch_target};

    #[test]
    fn decodes_forward_and_backward_branches() {
        // b 0x020000C0 assembled at 0x02000000: offset 0xB8, imm24 0x2E.
        assert_eq!(
            decode_branch_target(0xEA00_002E, 0x0200_0000),
            Some(0x0200_00C0)
        );
        // b <self> is 0xEAFFFFFE at any base.
        assert_eq!(
            decode_branch_target(0xEAFF_FFFE, 0x0800_0000),
            Some(0x0800_0000)
        );
        // Not an unconditional branch.
        assert_eq!(decode_branch_target(0x0800_0000, 0x0800_0000), None);
    }

    #[test]
    fn kinds_are_copyable_and_distinct() {
        assert_ne!(ImageKind::Cart, ImageKind::Multiboot);
    }
}
