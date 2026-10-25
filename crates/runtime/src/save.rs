//! Cartridge save media: SRAM, flash, and EEPROM.
//!
//! The GBA cartridge slot may carry one backup medium behind the Game
//! Pak SRAM window (0x0E000000) or, for EEPROM, behind the upper ROM
//! window (0x0D000000). `SaveKind` names the medium, [`Save`] owns the
//! backing image plus the chip command state, and
//! [`SaveKind::detect`] picks a kind from the ROM's backup ID string
//! (GBATEK "GBA Cart Backup IDs"). Everything follows GBATEK "Backup
//! Media": write and erase complete instantly, so the games'
//! poll-until-done loops see the finished state on the first read.

use std::path::Path;

use thiserror::Error;

/// Which backup medium the cartridge carries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SaveKind {
    /// Static RAM, 32 KiB, plain byte reads and writes.
    Sram,
    /// Flash, 64 KiB, command-driven.
    Flash64,
    /// Flash, 128 KiB, command-driven with bank switching.
    Flash128,
    /// Serial EEPROM, 512 bytes (6-bit addresses).
    Eeprom512,
    /// Serial EEPROM, 8 KiB (14-bit addresses).
    Eeprom8k,
    /// No backup medium: the SRAM window floats (reads 0xFF, the
    /// external save suite's absent-chip convention) and writes are
    /// discarded.
    None,
}

impl SaveKind {
    /// The save image size in bytes.
    #[must_use]
    pub fn size(self) -> usize {
        match self {
            Self::Sram => 32 * 1024,
            Self::Flash64 => 64 * 1024,
            Self::Flash128 => 128 * 1024,
            Self::Eeprom512 => 512,
            Self::Eeprom8k => 8 * 1024,
            Self::None => 0,
        }
    }

    /// The canonical name used by the ROM database override.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Sram => "sram",
            Self::Flash64 => "flash64",
            Self::Flash128 => "flash128",
            Self::Eeprom512 => "eeprom512",
            Self::Eeprom8k => "eeprom8k",
            Self::None => "none",
        }
    }

    /// Parses the canonical name from [`Self::name`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        let kind = match name {
            "sram" => Self::Sram,
            "flash64" => Self::Flash64,
            "flash128" => Self::Flash128,
            "eeprom512" => Self::Eeprom512,
            "eeprom8k" => Self::Eeprom8k,
            "none" => Self::None,
            _ => return None,
        };
        Some(kind)
    }

    /// Detects the medium from the ROM's backup ID string.
    ///
    /// Nintendo's library inserts one of the ID strings (word-aligned,
    /// zero padded) into the image: `EEPROM_V`, `SRAM_V`, `FLASH_V`,
    /// `FLASH512_V`, or `FLASH1M_V` (GBATEK "GBA Cart Backup IDs"). The
    /// `EEPROM_V` string does not distinguish the two chip sizes; the
    /// size is settled later by the first DMA access width (see
    /// [`Eeprom`]). With no ID at all the cart has no medium
    /// ([`SaveKind::None`]).
    #[must_use]
    pub fn detect(rom: &[u8]) -> Self {
        let ids: &[(&[u8], Self)] = &[
            (b"FLASH1M_V", Self::Flash128),
            (b"FLASH512_V", Self::Flash64),
            (b"FLASH_V", Self::Flash64),
            (b"EEPROM_V", Self::Eeprom512),
            (b"SRAM_V", Self::Sram),
        ];
        // The string must sit at a word-aligned offset, so a step-4 scan
        // sees every compliant placement.
        for off in (0..rom.len()).step_by(4) {
            for (id, kind) in ids {
                if rom[off..].starts_with(id) {
                    return *kind;
                }
            }
        }
        Self::None
    }
}

/// Errors produced when loading a save image.
#[derive(Debug, Error)]
pub enum SaveError {
    /// The image size does not match the medium.
    #[error("save image is {} bytes, expected {}", got, expected)]
    WrongSize {
        /// The size of the loaded image.
        got: usize,
        /// The size the medium requires.
        expected: usize,
    },

    /// The file could not be read or written.
    #[error("cannot access save file: {0}")]
    Io(#[from] std::io::Error),
}

/// The cartridge's backup medium: the image plus the chip state.
#[derive(Clone, Debug)]
pub enum Save {
    /// Static RAM: no command state.
    Sram(Box<[u8]>),
    /// Flash: command-driven chip.
    Flash(Flash),
    /// Serial EEPROM: bitstream-driven chip.
    Eeprom(Eeprom),
    /// No medium behind the window.
    None,
}

impl Save {
    /// Builds a blank save of `kind` (erased flash and SRAM read
    /// 0xFF; a fresh SRAM chip's floating bus settles high, which the
    /// external save suite pins as the uninitialized value).
    #[must_use]
    pub fn new(kind: SaveKind) -> Self {
        match kind {
            SaveKind::Sram => Self::Sram(vec![0xFF; kind.size()].into_boxed_slice()),
            SaveKind::Flash64 => Self::Flash(Flash::new(false)),
            SaveKind::Flash128 => Self::Flash(Flash::new(true)),
            SaveKind::Eeprom512 => Self::Eeprom(Eeprom::new(Some(6))),
            SaveKind::Eeprom8k => Self::Eeprom(Eeprom::new(Some(14))),
            SaveKind::None => Self::None,
        }
    }

    /// The medium's kind.
    #[must_use]
    pub fn kind(&self) -> SaveKind {
        match self {
            Self::Sram(_) => SaveKind::Sram,
            Self::Flash(f) => f.kind(),
            Self::Eeprom(e) => e.kind(),
            Self::None => SaveKind::None,
        }
    }

    /// The full save image, sized exactly as [`SaveKind::size`].
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Self::Sram(data)
            | Self::Flash(Flash { data, .. })
            | Self::Eeprom(Eeprom { data, .. }) => data.to_vec(),
            Self::None => Vec::new(),
        }
    }

    /// Replaces the image with `bytes`.
    ///
    /// # Errors
    ///
    /// Returns [`SaveError::WrongSize`] unless the length matches
    /// [`SaveKind::size`] exactly; chip state resets to blank.
    pub fn load(&mut self, bytes: &[u8]) -> Result<(), SaveError> {
        let kind = self.kind();
        if bytes.len() != kind.size() {
            return Err(SaveError::WrongSize {
                got: bytes.len(),
                expected: kind.size(),
            });
        }
        *self = {
            let mut save = Self::new(kind);
            let data = match &mut save {
                Self::Sram(data) => data,
                Self::Flash(f) => &mut f.data,
                Self::Eeprom(e) => &mut e.data,
                Self::None => return Ok(()),
            };
            data.copy_from_slice(bytes);
            save
        };
        Ok(())
    }

    /// Writes the image to `path`.
    ///
    /// # Errors
    ///
    /// See [`std::fs::write`].
    pub fn to_file(&self, path: impl AsRef<Path>) -> Result<(), SaveError> {
        std::fs::write(path, self.to_bytes())?;
        Ok(())
    }

    /// Replaces the image from `path`.
    ///
    /// # Errors
    ///
    /// See [`Self::load`] and [`std::fs::read`].
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, SaveError> {
        let bytes = std::fs::read(path)?;
        // The kind comes from the image size, which every kind has a
        // distinct value of except the two flashes; their images are
        // 64K/128K and unambiguous.
        let kind = match bytes.len() {
            n if n == SaveKind::Sram.size() => SaveKind::Sram,
            n if n == SaveKind::Flash64.size() => SaveKind::Flash64,
            n if n == SaveKind::Flash128.size() => SaveKind::Flash128,
            n if n == SaveKind::Eeprom512.size() => SaveKind::Eeprom512,
            n if n == SaveKind::Eeprom8k.size() => SaveKind::Eeprom8k,
            got => return Err(SaveError::WrongSize { got, expected: 0 }),
        };
        let mut save = Self::new(kind);
        save.load(&bytes)?;
        Ok(save)
    }

    /// Reads one byte through the Game Pak SRAM window
    /// (0x0E000000..0x0F000000 mapped to the low 16 address bits).
    pub fn read_u8(&mut self, off: u16) -> u8 {
        match self {
            Self::Sram(data) => data[(off as usize) & sram_mask(data.len())],
            Self::Flash(f) => f.read_u8(off),
            // The EEPROM answers on the ROM window instead; the SRAM
            // window has nothing behind it.
            Self::Eeprom(_) => 0,
            // No chip drives the data lines; the bus floats high.
            Self::None => 0xFF,
        }
    }

    /// Writes one byte through the Game Pak SRAM window.
    pub fn write_u8(&mut self, off: u16, value: u8) {
        match self {
            Self::Sram(data) => data[(off as usize) & sram_mask(data.len())] = value,
            Self::Flash(f) => f.write_u8(off, value),
            Self::Eeprom(_) => {}
            Self::None => {}
        }
    }

    /// Reads one halfword through the EEPROM window (0x0D000000): the
    /// chip drives bit 0 of the data bus (GBATEK "GBA Cart Backup
    /// EEPROM").
    ///
    /// Returns `None` when the medium is not an EEPROM.
    pub fn read_eeprom_half(&mut self) -> Option<u16> {
        match self {
            Self::Eeprom(e) => Some(u16::from(e.read_bit())),
            _ => None,
        }
    }

    /// Writes one halfword through the EEPROM window: only bit 0
    /// reaches the chip.
    ///
    /// Returns `true` when the medium is an EEPROM.
    pub fn write_eeprom_half(&mut self, value: u16) -> bool {
        match self {
            Self::Eeprom(e) => {
                e.write_bit((value & 1) as u8);
                true
            }
            _ => false,
        }
    }

    /// Announces the length in bits of the EEPROM request stream that
    /// the next DMA write transfer will shift in.
    ///
    /// Returns `true` when the medium is an EEPROM.
    pub fn announce_eeprom_stream(&mut self, bits: u8) -> bool {
        match self {
            Self::Eeprom(e) => {
                e.announce(bits);
                true
            }
            _ => false,
        }
    }
}

/// The address mask for an SRAM image (power-of-two sizes).
fn sram_mask(len: usize) -> usize {
    len - 1
}

/// The AT-style flash chip: 64K or 128K (two 64K banks).
///
/// Commands follow the GBATEK sequences: three strobe writes unlock a
/// command byte; ID mode reports the manufacturer and device codes;
/// chip and 4K-sector erase fill with 0xFF; byte writes clear bits in
/// the erased state (data AND old data); 128K devices select the upper
/// bank with the B0 command. Write and erase complete instantly, so
/// status polls see the finished state (GBATEK "GBA Cart Backup Flash
/// ROM").
#[derive(Clone, Debug)]
pub struct Flash {
    /// The whole chip image; for 128K devices the upper half is only
    /// visible while bank 1 is selected.
    data: Box<[u8]>,
    /// The command decoder state.
    state: FlashState,
    /// The selected 64K bank (128K devices only).
    bank: u8,
}

/// The flash command decoder state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FlashState {
    /// No command in progress; writes run the strobe detector.
    Idle,
    /// Saw 0x5555 = 0xAA.
    GotAA,
    /// Saw 0x2AAA = 0x55; the next write at 0x5555 is a command.
    Got55,
    /// Erase command armed (0x80); expecting the confirm sequence.
    EraseArmed,
    /// Confirm sequence in progress: saw 0x5555 = 0xAA.
    EraseGotAA,
    /// Saw 0x2AAA = 0x55; the next write is the erase confirm.
    EraseGot55,
    /// Byte-write command armed (0xA0); the next write programs.
    WriteArmed,
    /// Bank command armed (0xB0); the next write selects the bank.
    BankArmed,
    /// ID mode: reads report the device and manufacturer codes.
    IdMode,
}

impl Flash {
    fn new(banked: bool) -> Self {
        let size = if banked {
            SaveKind::Flash128.size()
        } else {
            SaveKind::Flash64.size()
        };
        Self {
            data: vec![0xFF; size].into_boxed_slice(),
            state: FlashState::Idle,
            bank: 0,
        }
    }

    /// The medium's kind.
    fn kind(&self) -> SaveKind {
        if self.data.len() == SaveKind::Flash128.size() {
            SaveKind::Flash128
        } else {
            SaveKind::Flash64
        }
    }

    /// Device identification codes: manufacturer at 0x0000, device at
    /// 0x0001. The 128K chip reports the Macronix 09C2 pair, the 64K
    /// chip the SST D4BF pair (GBATEK device table: MSB device, LSB
    /// manufacturer).
    fn device_id(&self) -> [u8; 2] {
        if self.data.len() == SaveKind::Flash128.size() {
            [0xC2, 0x09]
        } else {
            [0xBF, 0xD4]
        }
    }

    /// Maps a window offset onto the physical image address.
    fn map(&self, off: u16) -> usize {
        (u32::from(self.bank) * 0x1_0000 + u32::from(off)) as usize % self.data.len()
    }

    fn read_u8(&mut self, off: u16) -> u8 {
        if self.state == FlashState::IdMode {
            let id = self.device_id();
            return match off & 0x0001 {
                0 => id[0],
                _ => id[1],
            };
        }
        self.data[self.map(off)]
    }

    fn write_u8(&mut self, off: u16, value: u8) {
        let strobe = match off {
            0x5555 => 0,
            0x2AAA => 1,
            _ => 2,
        };
        match (self.state, strobe, value) {
            (FlashState::Idle, 0, 0xAA) => self.state = FlashState::GotAA,
            (FlashState::GotAA, 1, 0x55) => self.state = FlashState::Got55,
            (FlashState::Got55, 0, 0x90) => self.state = FlashState::IdMode,
            (FlashState::Got55, 0, 0x80) => self.state = FlashState::EraseArmed,
            (FlashState::Got55, 0, 0xA0) => self.state = FlashState::WriteArmed,
            (FlashState::Got55, 0, 0xB0) => self.state = FlashState::BankArmed,
            (FlashState::Got55, _, 0xF0) => self.state = FlashState::Idle,
            (FlashState::EraseArmed, 0, 0xAA) => self.state = FlashState::EraseGotAA,
            (FlashState::EraseGotAA, 1, 0x55) => self.state = FlashState::EraseGot55,
            (FlashState::EraseGot55, 0, 0x10) => {
                self.erase_chip();
                self.state = FlashState::Idle;
            }
            (FlashState::EraseGot55, _, 0x30) => {
                self.erase_sector(off);
                self.state = FlashState::Idle;
            }
            (FlashState::WriteArmed, _, _) => {
                let i = self.map(off);
                self.data[i] &= value;
                self.state = FlashState::Idle;
            }
            (FlashState::BankArmed, _, _) => {
                self.bank = value & 1;
                self.state = FlashState::Idle;
            }
            _ => self.state = FlashState::Idle,
        }
    }

    fn erase_chip(&mut self) {
        self.data.fill(0xFF);
    }

    fn erase_sector(&mut self, off: u16) {
        let base = (self.map(off)) & !0x0FFF;
        let end = (base + 0x1000).min(self.data.len());
        self.data[base..end].fill(0xFF);
    }
}

/// The serial EEPROM chip: 512 bytes (6-bit page addresses) or 8 KiB
/// (14-bit). The CPU drives it one bit at a time through bit 0 of
/// halfword accesses on the ROM window, normally via DMA3 (GBATEK "GBA
/// Cart Backup EEPROM").
///
/// Requests are two-bit prefixes: "11" reads, "10" writes, each
/// followed by the address MSB first. A write appends 64 data bits and
/// ends with a 0 bit; a read ends with a 0 bit and is answered by a
/// 68-bit stream (4 ignored bits, 64 data bits MSB first, then the
/// ready bit).
///
/// The two chip sizes share the "EEPROM_V" ID string, so the address
/// width is detected from the request stream itself: the DMA transfer
/// that carries a request announces its unit count, which fixes the
/// stream total (2 + width + 1 bits for a read request, 67 + width for
/// a write) and thereby the width (GBATEK: "there seems to be no
/// autodetection mechanism, so that a hardcoded bus width must be
/// used" on real hardware; the transfer length is the software's way
/// of hardcoding it). Streams that arrive without an announced count
/// fall back to terminating on the first 0 bit with a total of 9 or 17
/// bits.
#[derive(Clone, Debug)]
pub struct Eeprom {
    /// The chip image, 8 bytes per page.
    data: Box<[u8]>,
    /// The detected address width: 6 (512B) or 14 (8K).
    width: Option<u8>,
    /// The announced request length in bits, if any.
    expect: Option<u8>,
    /// The request bits received so far, oldest bit highest.
    rx: u128,
    /// How many bits of `rx` are valid.
    rx_bits: u8,
    /// The two-bit request prefix: 0b11 read, 0b10 write.
    shape: Option<u8>,
    /// The reply stream state after a read request or a write.
    reply: Reply,
    /// The byte offset of the page addressed by the last request.
    page: usize,
}

/// The reply stream state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Reply {
    /// Not in a reply; reads return the ready bit.
    None,
    /// A read is being answered: `sent` bits have gone out.
    Read { sent: u16 },
    /// A write completed: reads report ready (writes finish
    /// instantly, so the poll loop succeeds on the first read).
    Wrote,
}

impl Eeprom {
    fn new(width: Option<u8>) -> Self {
        let size = match width {
            Some(6) => SaveKind::Eeprom512.size(),
            _ => SaveKind::Eeprom8k.size(),
        };
        Self {
            data: vec![0; size].into_boxed_slice(),
            width,
            expect: None,
            rx: 0,
            rx_bits: 0,
            shape: None,
            reply: Reply::None,
            page: 0,
        }
    }

    /// The medium's kind.
    fn kind(&self) -> SaveKind {
        match self.width {
            Some(6) => SaveKind::Eeprom512,
            _ => SaveKind::Eeprom8k,
        }
    }

    /// Announces the length in bits of the request stream that is
    /// about to be shifted in (the DMA unit count).
    pub fn announce(&mut self, bits: u8) {
        self.expect = Some(bits);
    }

    /// Accepts one request bit (bit 0 of a halfword write).
    pub fn write_bit(&mut self, bit: u8) {
        if self.rx_bits >= 81 {
            // A malformed stream: drop it and resynchronize.
            self.reset();
        }
        self.rx = (self.rx << 1) | u128::from(bit & 1);
        self.rx_bits += 1;
        if self.rx_bits < 2 {
            return;
        }

        // With an announced length, the stream shape and address
        // width follow from the prefix and the count.
        if self.rx_bits == 2 {
            self.shape = Some((self.rx & 0b11) as u8);
            if let Some(total) = self.expect {
                let width = match (self.rx & 0b11, total) {
                    (0b11, n) => n.checked_sub(3),
                    (0b10, n) => n.checked_sub(67),
                    _ => None,
                };
                match width {
                    Some(6) => self.set_width(6),
                    Some(14) => self.set_width(14),
                    _ => {}
                }
            }
        }

        // Complete on the announced total, or heuristically on the
        // terminating 0 bit when nothing was announced.
        let shape = self.shape.unwrap_or(0);
        let total = match (self.expect, shape, self.bits_ending_zero()) {
            (Some(n), _, _) if self.rx_bits == n => true,
            (Some(_), _, _) => false,
            (None, 0b11, true) => match self.width {
                Some(w) => self.rx_bits == w + 3,
                None => self.rx_bits == 9 || self.rx_bits == 17,
            },
            (None, 0b10, true) => self.width.is_some_and(|w| self.rx_bits == w + 67),
            (None, _, _) => false,
        };
        if !total {
            return;
        }
        match shape {
            0b11 => self.finish_read(),
            0b10 => self.finish_write(),
            _ => self.reset(),
        }
    }

    /// Whether the bit just received is a 0 (the stream terminator).
    fn bits_ending_zero(&self) -> bool {
        self.rx & 1 == 0
    }

    /// Completes a read request: latches the page and starts the
    /// reply stream.
    fn finish_read(&mut self) {
        let width = self.width.unwrap_or(6);
        if self.rx_bits != width + 3 {
            self.reset();
            return;
        }
        let addr = ((self.rx >> 1) & ((1 << width) - 1)) as usize;
        self.page = addr * 8;
        self.reply = Reply::Read { sent: 0 };
        self.reset();
    }

    /// Completes a write request: programs the 64-bit page.
    fn finish_write(&mut self) {
        let width = self.width.unwrap_or(6);
        if self.rx_bits != width + 67 {
            self.reset();
            return;
        }
        // Stream from the LSB: terminator, 64 data bits, width
        // address bits, 2-bit prefix.
        let data = ((self.rx >> 1) & 0xFFFF_FFFF_FFFF_FFFF) as u64;
        let addr = ((self.rx >> 65) & ((1 << width) - 1)) as usize;
        for (i, cell) in self.data[addr * 8..addr * 8 + 8].iter_mut().enumerate() {
            *cell = (data >> (56 - 8 * i)) as u8;
        }
        self.reply = Reply::Wrote;
        self.reset();
    }

    /// Fixes the address width, resizing the image if the first
    /// detection contradicts the provisional size.
    fn set_width(&mut self, width: u8) {
        if self.width == Some(width) {
            return;
        }
        let size = if width == 6 {
            SaveKind::Eeprom512.size()
        } else {
            SaveKind::Eeprom8k.size()
        };
        self.data = vec![0; size].into_boxed_slice();
        self.width = Some(width);
    }

    fn reset(&mut self) {
        self.rx = 0;
        self.rx_bits = 0;
        self.shape = None;
        self.expect = None;
    }

    /// Produces the next reply bit (bit 0 of a halfword read).
    pub fn read_bit(&mut self) -> u8 {
        match self.reply {
            Reply::None => 1,
            Reply::Wrote => 1,
            Reply::Read { sent } => {
                let bit = if sent < 4 {
                    // The first four bits are ignored by convention.
                    0
                } else if sent < 68 {
                    let i = usize::from(sent - 4);
                    let cell = (self.page + i / 8) & (self.data.len() - 1);
                    let mask = 0x80 >> (i % 8);
                    u8::from(self.data[cell] & mask != 0)
                } else {
                    // After the data, the chip reports ready.
                    1
                };
                self.reply = Reply::Read { sent: sent + 1 };
                bit
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Save, SaveError, SaveKind};

    /// A minimal ROM image: 256 bytes of 0x00 plus one word-aligned ID.
    fn rom_with_id(id: &[u8]) -> Vec<u8> {
        let mut rom = vec![0u8; 256];
        rom[64..64 + id.len()].copy_from_slice(id);
        rom
    }

    #[test]
    fn detect_reads_each_backup_id() {
        assert_eq!(SaveKind::detect(&rom_with_id(b"SRAM_V123")), SaveKind::Sram);
        assert_eq!(
            SaveKind::detect(&rom_with_id(b"FLASH_V123")),
            SaveKind::Flash64
        );
        assert_eq!(
            SaveKind::detect(&rom_with_id(b"FLASH512_V")),
            SaveKind::Flash64
        );
        assert_eq!(
            SaveKind::detect(&rom_with_id(b"FLASH1M_V0")),
            SaveKind::Flash128
        );
        assert_eq!(
            SaveKind::detect(&rom_with_id(b"EEPROM_Vnn")),
            SaveKind::Eeprom512
        );
    }

    #[test]
    fn detect_yields_none_without_id() {
        assert_eq!(SaveKind::detect(&vec![0u8; 256]), SaveKind::None);
        // The ID must sit at a word-aligned offset; an unaligned string
        // is not seen.
        let mut rom = vec![0u8; 256];
        rom[65..65 + 6].copy_from_slice(b"SRAM_V");
        assert_eq!(SaveKind::detect(&rom), SaveKind::None);
    }

    #[test]
    fn no_medium_reads_float_high() {
        let mut save = Save::new(SaveKind::None);
        assert_eq!(save.read_u8(0), 0xFF);
        save.write_u8(0, 0xAA);
        assert_eq!(save.read_u8(0), 0xFF);
        assert!(save.to_bytes().is_empty());
    }

    #[test]
    fn names_round_trip() {
        for kind in [
            SaveKind::Sram,
            SaveKind::Flash64,
            SaveKind::Flash128,
            SaveKind::Eeprom512,
            SaveKind::Eeprom8k,
        ] {
            assert_eq!(SaveKind::from_name(kind.name()), Some(kind));
        }
        assert_eq!(SaveKind::from_name("eeprom"), None);
    }

    #[test]
    fn sram_reads_and_writes_bytes() {
        let mut save = Save::new(SaveKind::Sram);
        assert_eq!(save.kind().size(), 32 * 1024);
        save.write_u8(0x1234, 0x5A);
        assert_eq!(save.read_u8(0x1234), 0x5A);
        // The image wraps at the 32 KiB boundary (the window decodes 16
        // address bits, the chip decodes 15).
        assert_eq!(save.read_u8(0x9234), 0x5A);
    }

    #[test]
    fn load_rejects_wrong_size_images() {
        let mut save = Save::new(SaveKind::Sram);
        match save.load(&[0u8; 10]) {
            Err(SaveError::WrongSize { got, expected }) => {
                assert_eq!(got, 10);
                assert_eq!(expected, 32 * 1024);
            }
            other => panic!("expected WrongSize, got {other:?}"),
        }
    }

    #[test]
    fn flash_reports_device_ids_in_id_mode() {
        let mut save = Save::new(SaveKind::Flash64);
        // Enter ID mode: [5555]=AA, [2AAA]=55, [5555]=90 (GBATEK).
        save.write_u8(0x5555, 0xAA);
        save.write_u8(0x2AAA, 0x55);
        save.write_u8(0x5555, 0x90);
        assert_eq!(save.read_u8(0x0000), 0xBF, "manufacturer (SST)");
        assert_eq!(save.read_u8(0x0001), 0xD4, "device");
        // Terminate ID mode with F0.
        save.write_u8(0x5555, 0xF0);
        assert_eq!(save.read_u8(0x0000), 0xFF, "back to erased data");
        assert_eq!(save.read_u8(0x0001), 0xFF);
    }

    #[test]
    fn flash_128k_reports_macronix_ids() {
        let mut save = Save::new(SaveKind::Flash128);
        save.write_u8(0x5555, 0xAA);
        save.write_u8(0x2AAA, 0x55);
        save.write_u8(0x5555, 0x90);
        assert_eq!(save.read_u8(0x0000), 0xC2, "manufacturer (Macronix)");
        assert_eq!(save.read_u8(0x0001), 0x09, "device");
    }

    #[test]
    fn flash_erases_a_sector_and_programs_bytes() {
        let mut save = Save::new(SaveKind::Flash64);
        // Erase sector 2: [5555]=AA, [2AAA]=55, [5555]=80, [5555]=AA,
        // [2AAA]=55, [2000]=30 (GBATEK).
        for (off, v) in [
            (0x5555, 0xAA),
            (0x2AAA, 0x55),
            (0x5555, 0x80),
            (0x5555, 0xAA),
            (0x2AAA, 0x55),
        ] {
            save.write_u8(off, v);
        }
        save.write_u8(0x2000, 0x30);
        // The sector is 0xFF-filled; neighbors are untouched (also FF,
        // so prove the sector erase by writing first).
        save.write_u8(0x5555, 0xAA);
        save.write_u8(0x2AAA, 0x55);
        save.write_u8(0x5555, 0xA0);
        save.write_u8(0x2001, 0xA5);
        save.write_u8(0x5555, 0xAA);
        save.write_u8(0x2AAA, 0x55);
        save.write_u8(0x5555, 0xA0);
        save.write_u8(0x3001, 0x96);
        assert_eq!(save.read_u8(0x2001), 0xA5);
        assert_eq!(save.read_u8(0x3001), 0x96);
        // Re-erase sector 2 only; sector 3 keeps its byte.
        for (off, v) in [
            (0x5555, 0xAA),
            (0x2AAA, 0x55),
            (0x5555, 0x80),
            (0x5555, 0xAA),
            (0x2AAA, 0x55),
        ] {
            save.write_u8(off, v);
        }
        save.write_u8(0x2000, 0x30);
        assert_eq!(save.read_u8(0x2001), 0xFF, "sector 2 erased");
        assert_eq!(save.read_u8(0x3001), 0x96, "sector 3 untouched");
    }

    #[test]
    fn flash_writes_clear_bits_only() {
        let mut save = Save::new(SaveKind::Flash64);
        for (off, v) in [
            (0x5555, 0xAA),
            (0x2AAA, 0x55),
            (0x5555, 0x80),
            (0x5555, 0xAA),
            (0x2AAA, 0x55),
        ] {
            save.write_u8(off, v);
        }
        save.write_u8(0x1000, 0x30);
        // Program 0x0F, then 0xF0 over it: the AND leaves 0x00.
        save.write_u8(0x5555, 0xAA);
        save.write_u8(0x2AAA, 0x55);
        save.write_u8(0x5555, 0xA0);
        save.write_u8(0x1004, 0x0F);
        save.write_u8(0x5555, 0xAA);
        save.write_u8(0x2AAA, 0x55);
        save.write_u8(0x5555, 0xA0);
        save.write_u8(0x1004, 0xF0);
        assert_eq!(save.read_u8(0x1004), 0x00);
    }

    #[test]
    fn flash_broken_sequence_is_ignored() {
        let mut save = Save::new(SaveKind::Flash64);
        // The strobe addresses must match exactly; AA elsewhere does
        // not start a command.
        save.write_u8(0x0000, 0xAA);
        save.write_u8(0x2AAA, 0x55);
        save.write_u8(0x5555, 0x90);
        assert_eq!(save.read_u8(0x0000), 0xFF, "still data, not ID mode");
    }

    #[test]
    fn flash_128k_switches_banks() {
        let mut save = Save::new(SaveKind::Flash128);
        // Bank switch: [5555]=AA, [2AAA]=55, [5555]=B0, [0000]=bank
        // (GBATEK "Bank Switching").
        let bank = |save: &mut Save, n: u8| {
            save.write_u8(0x5555, 0xAA);
            save.write_u8(0x2AAA, 0x55);
            save.write_u8(0x5555, 0xB0);
            save.write_u8(0x0000, n);
        };
        // Program a byte in bank 0 and bank 1 at the same window
        // offset, then check both survive independently.
        for (off, v) in [
            (0x5555, 0xAA),
            (0x2AAA, 0x55),
            (0x5555, 0x80),
            (0x5555, 0xAA),
            (0x2AAA, 0x55),
        ] {
            save.write_u8(off, v);
        }
        save.write_u8(0x7000, 0x30);
        save.write_u8(0x5555, 0xAA);
        save.write_u8(0x2AAA, 0x55);
        save.write_u8(0x5555, 0xA0);
        save.write_u8(0x7001, 0x11);
        bank(&mut save, 1);
        save.write_u8(0x5555, 0xAA);
        save.write_u8(0x2AAA, 0x55);
        save.write_u8(0x5555, 0xA0);
        save.write_u8(0x7001, 0x22);
        assert_eq!(save.read_u8(0x7001), 0x22, "bank 1 window");
        bank(&mut save, 0);
        assert_eq!(save.read_u8(0x7001), 0x11, "bank 0 window");
        assert_eq!(save.to_bytes()[0x1_7001], 0x22, "bank 1 image");
    }

    /// Shifts a request stream into the chip bit by bit.
    fn push_bits(save: &mut Save, bits: &[u8]) {
        for bit in bits {
            assert!(save.write_eeprom_half(u16::from(*bit)));
        }
    }

    /// Builds a 512B write request: 10 + addr(6) + data(64) + 0.
    fn eeprom512_write(addr: u8, data: u64) -> Vec<u8> {
        let mut bits = vec![1, 0];
        for i in (0..6).rev() {
            bits.push((addr >> i) & 1);
        }
        for i in (0..64).rev() {
            bits.push(u8::from((data >> i) & 1 == 1));
        }
        bits.push(0);
        bits
    }

    /// Builds a 512B read request: 11 + addr(6) + 0.
    fn eeprom512_read(addr: u8) -> Vec<u8> {
        let mut bits = vec![1, 1];
        for i in (0..6).rev() {
            bits.push((addr >> i) & 1);
        }
        bits.push(0);
        bits
    }

    #[test]
    fn eeprom_512b_write_then_read_back() {
        let mut save = Save::new(SaveKind::Eeprom512);
        // Write A5A5... to page 0x15: the DMA announces 73 bits
        // (2 + 6 + 64 + 1), then shifts the stream in.
        save.announce_eeprom_stream(73);
        push_bits(&mut save, &eeprom512_write(0x15, 0xA5A5_5A5A_A5A5_5A5A));
        assert_eq!(
            save.to_bytes()[0x15 * 8..0x15 * 8 + 8],
            [0xA5, 0xA5, 0x5A, 0x5A, 0xA5, 0xA5, 0x5A, 0x5A]
        );
        // Read it back: announce 9 bits (2 + 6 + 1), then the reply is
        // 4 ignored bits, 64 data bits MSB first, then ready.
        save.announce_eeprom_stream(9);
        push_bits(&mut save, &eeprom512_read(0x15));
        let mut data = 0u64;
        for i in 0..68 {
            let bit = save.read_eeprom_half().map_or(0xFF, |v| v as u8);
            if i < 4 {
                assert_eq!(bit, 0, "ignored bit {i}");
            } else if i < 68 {
                data = (data << 1) | u64::from(bit);
            }
        }
        assert_eq!(data, 0xA5A5_5A5A_A5A5_5A5A);
        assert_eq!(save.read_eeprom_half(), Some(1), "ready after the data");
    }

    #[test]
    fn eeprom_8k_addresses_14_bits() {
        let mut save = Save::new(SaveKind::Eeprom8k);
        let addr = 0x3FF;
        let data = 0x0123_4567_89AB_CDEF;
        // 8K streams: 2 + 14 + 64 + 1 = 81 bits to write, 17 to read.
        let mut bits = vec![1, 0];
        for i in (0..14).rev() {
            bits.push(u8::from((addr >> i) & 1 == 1));
        }
        for i in (0..64).rev() {
            bits.push(u8::from((data >> i) & 1 == 1));
        }
        bits.push(0);
        save.announce_eeprom_stream(81);
        push_bits(&mut save, &bits);
        assert_eq!(
            save.to_bytes()[addr as usize * 8..addr as usize * 8 + 8],
            [0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF]
        );

        let mut bits = vec![1, 1];
        for i in (0..14).rev() {
            bits.push(u8::from((addr >> i) & 1 == 1));
        }
        bits.push(0);
        save.announce_eeprom_stream(17);
        push_bits(&mut save, &bits);
        let mut got = 0u64;
        for _ in 0..68 {
            got = (got << 1) | u64::from(save.read_eeprom_half().unwrap_or(0xFF));
        }
        assert_eq!(got, data);
    }

    #[test]
    fn eeprom_width_detected_from_stream_length() {
        // A fresh EEPROM512 medium (the detect default) settles to 8K
        // when the first announced stream carries a 14-bit address.
        let mut save = Save::new(SaveKind::Eeprom512);
        let mut bits = vec![1, 1];
        for i in (0..14).rev() {
            bits.push(u8::from((0x201 >> i) & 1 == 1));
        }
        bits.push(0);
        save.announce_eeprom_stream(17);
        push_bits(&mut save, &bits);
        assert_eq!(save.kind(), SaveKind::Eeprom8k);
        assert_eq!(save.kind().size(), 8 * 1024);
    }

    #[test]
    fn eeprom_ready_poll_after_write() {
        let mut save = Save::new(SaveKind::Eeprom512);
        save.announce_eeprom_stream(73);
        push_bits(&mut save, &eeprom512_write(0x01, 0x42));
        assert_eq!(save.read_eeprom_half(), Some(1), "writes finish instantly");
    }

    #[test]
    fn save_file_round_trips() {
        let mut save = Save::new(SaveKind::Sram);
        save.write_u8(0x0100, 0xC7);
        let path =
            std::env::temp_dir().join(format!("oxyboy-save-test-{}.sav", std::process::id()));
        save.to_file(&path).expect("store save");
        let mut loaded = Save::from_file(&path).expect("reload save");
        assert_eq!(loaded.read_u8(0x0100), 0xC7);
        std::fs::remove_file(&path).expect("cleanup save");
    }
}
