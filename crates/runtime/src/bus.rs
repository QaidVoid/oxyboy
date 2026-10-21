//! GBA memory map and system bus.
//!
//! Implements the address map, region mirroring, access-width rules, and
//! open-bus behavior of the GBA system bus, per GBATEK "GBA Memory Map"
//! and "Address Bus Width and CPU Read/Write Access Widths".
//!
//! All operations are total: no address panics. Unmapped reads return the
//! documented open-bus value and writes to read-only regions are discarded.

/// On-board work RAM (EWRAM): 256 KiB.
pub const EWRAM_SIZE: usize = 256 * 1024;
/// On-chip work RAM (IWRAM): 32 KiB.
pub const IWRAM_SIZE: usize = 32 * 1024;
/// BG/OBJ palette RAM: 1 KiB.
pub const PALETTE_SIZE: usize = 1024;
/// Video RAM: 96 KiB (64 KiB BG bank plus 32 KiB OBJ bank).
pub const VRAM_SIZE: usize = 96 * 1024;
/// OBJ attribute memory (OAM): 1 KiB.
pub const OAM_SIZE: usize = 1024;
/// Maximum Game Pak ROM image size: 32 MiB.
pub const ROM_MAX_SIZE: usize = 32 * 1024 * 1024;
/// Maximum Game Pak SRAM size: 64 KiB.
pub const SRAM_MAX_SIZE: usize = 64 * 1024;

/// Value returned by reads of unmapped locations.
///
/// Real hardware drives the data bus with the most recently fetched opcode
/// (GBATEK "Open Bus"); until the CPU exists to track that, the skeleton
/// documents and returns zero.
const OPEN_BUS_BYTE: u8 = 0;

/// A GBA bus region selected by the top address bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    /// 0x00000000: 16 KiB system ROM (no BIOS image is shipped; reads 0).
    Bios,
    /// 0x02000000: 256 KiB on-board work RAM.
    Ewram,
    /// 0x03000000: 32 KiB on-chip work RAM.
    Iwram,
    /// 0x04000000: IO registers (stubbed for now).
    Io,
    /// 0x05000000: 1 KiB palette RAM.
    Palette,
    /// 0x06000000: 96 KiB video RAM.
    Vram,
    /// 0x07000000: 1 KiB OBJ attributes.
    Oam,
    /// 0x08000000: Game Pak ROM window.
    Rom,
    /// 0x0E000000: Game Pak SRAM, 8-bit bus.
    Sram,
    /// Address not covered by any region (BIOS and IO gaps).
    OpenBus,
}

/// The GBA memory system: region arrays plus the address decoder.
///
/// The full 4 GiB address space aliases the 256 MiB window at 0x00000000
/// because the top four address bits are unused (GBATEK "Unused Memory
/// Area"); every access is therefore masked with 0x0FFF_FFFF first.
pub struct Bus {
    bios: Box<[u8]>,
    ewram: Box<[u8]>,
    iwram: Box<[u8]>,
    palette: Box<[u8]>,
    vram: Box<[u8]>,
    oam: Box<[u8]>,
    rom: Box<[u8]>,
    /// `next_power_of_two(rom.len()) - 1`, for mirroring beyond the image.
    rom_mask: usize,
    sram: Box<[u8]>,
}

impl Bus {
    /// Builds a bus around a Game Pak ROM image, truncated to 32 MiB.
    pub fn new(rom: Vec<u8>) -> Self {
        let mut rom = rom;
        rom.truncate(ROM_MAX_SIZE);
        let rom_mask = if rom.len().is_power_of_two() {
            rom.len() - 1
        } else {
            rom.len().next_power_of_two() - 1
        };
        Bus {
            bios: vec![0; 0x4000].into_boxed_slice(),
            ewram: vec![0; EWRAM_SIZE].into_boxed_slice(),
            iwram: vec![0; IWRAM_SIZE].into_boxed_slice(),
            palette: vec![0; PALETTE_SIZE].into_boxed_slice(),
            vram: vec![0; VRAM_SIZE].into_boxed_slice(),
            oam: vec![0; OAM_SIZE].into_boxed_slice(),
            rom: rom.into_boxed_slice(),
            rom_mask,
            sram: vec![0; SRAM_MAX_SIZE].into_boxed_slice(),
        }
    }

    /// Classifies a logical address into its bus region.
    pub fn region_of(&self, addr: u32) -> Region {
        region_of(addr)
    }

    /// The Game Pak ROM image backing the ROM window.
    pub fn rom(&self) -> &[u8] {
        &self.rom
    }

    /// The 256 KiB EWRAM array (address 0x02000000).
    pub fn ewram(&self) -> &[u8] {
        &self.ewram
    }

    /// The 32 KiB IWRAM array (address 0x03000000).
    pub fn iwram(&self) -> &[u8] {
        &self.iwram
    }

    /// The 1 KiB palette array (address 0x05000000).
    pub fn palette(&self) -> &[u8] {
        &self.palette
    }

    /// The 96 KiB VRAM array (address 0x06000000).
    pub fn vram(&self) -> &[u8] {
        &self.vram
    }

    /// The 1 KiB OAM array (address 0x07000000).
    pub fn oam(&self) -> &[u8] {
        &self.oam
    }

    /// The 64 KiB SRAM array (address 0x0E000000).
    pub fn sram(&self) -> &[u8] {
        &self.sram
    }

    /// Reads one byte.
    pub fn read_u8(&self, addr: u32) -> u8 {
        let a = addr & 0x0FFF_FFFF;
        match a >> 24 {
            0x00 if a < 0x4000 => self.bios[(a & 0x3FFF) as usize],
            0x02 => self.ewram[(a & 0x0003_FFFF) as usize],
            0x03 => self.iwram[(a & 0x0000_7FFF) as usize],
            // IO is a stub region for now and reads 0.
            0x04 if a < 0x0400_0400 => OPEN_BUS_BYTE,
            // Palette and OAM sit on 16/32-bit buses with no 8-bit data
            // lane; 8-bit reads return 0 (GBATEK bus-width table).
            0x05 | 0x07 => OPEN_BUS_BYTE,
            // VRAM multiplexes its 16-bit bus: an 8-bit read yields the
            // low byte of the containing halfword.
            0x06 => {
                let i = (Self::vram_index(a) & !1) as usize;
                self.vram[i]
            }
            0x08..=0x0D => match self.rom_index(a) {
                Some(i) => self.rom[i],
                None => OPEN_BUS_BYTE,
            },
            0x0E | 0x0F => self.sram[(a & 0x0000_FFFF) as usize],
            _ => OPEN_BUS_BYTE,
        }
    }

    /// Reads a little-endian halfword.
    pub fn read_u16(&self, addr: u32) -> u16 {
        let a = addr & 0x0FFF_FFFF;
        match a >> 24 {
            0x05 => halfword(&self.palette, ((a & 0x0000_03FF) & !1) as usize),
            0x06 => halfword(&self.vram, (Self::vram_index(a) & !1) as usize),
            0x07 => halfword(&self.oam, ((a & 0x0000_03FF) & !1) as usize),
            // SRAM has an 8-bit bus: wide reads execute as sequential
            // byte reads of consecutive addresses.
            0x0E | 0x0F => {
                u16::from(self.sram[(a & 0x0000_FFFF) as usize])
                    | (u16::from(self.read_u8(addr.wrapping_add(1))) << 8)
            }
            _ => {
                u16::from(self.read_u8(addr)) | (u16::from(self.read_u8(addr.wrapping_add(1))) << 8)
            }
        }
    }

    /// Reads a little-endian word.
    ///
    /// Palette, VRAM, and OAM are 16-bit regions, so their word reads are
    /// two halfword lanes; everywhere else the access is four byte lanes.
    pub fn read_u32(&self, addr: u32) -> u32 {
        let a = addr & 0x0FFF_FFFF;
        if matches!(a >> 24, 0x05..=0x07) {
            u32::from(self.read_u16(addr)) | (u32::from(self.read_u16(addr.wrapping_add(2))) << 16)
        } else {
            u32::from(self.read_u8(addr))
                | (u32::from(self.read_u8(addr.wrapping_add(1))) << 8)
                | (u32::from(self.read_u8(addr.wrapping_add(2))) << 16)
                | (u32::from(self.read_u8(addr.wrapping_add(3))) << 24)
        }
    }

    /// Writes one byte.
    ///
    /// 8-bit writes are accepted only on 8/32-bit-bus regions: palette,
    /// VRAM, and OAM discard them (GBATEK bus-width table), BIOS and Game
    /// Pak ROM have no write path, and IO is stubbed for now.
    pub fn write_u8(&mut self, addr: u32, value: u8) {
        let a = addr & 0x0FFF_FFFF;
        match a >> 24 {
            0x02 => self.ewram[(a & 0x0003_FFFF) as usize] = value,
            0x03 => self.iwram[(a & 0x0000_7FFF) as usize] = value,
            0x0E | 0x0F => self.sram[(a & 0x0000_FFFF) as usize] = value,
            _ => {}
        }
    }

    /// Writes a little-endian halfword.
    pub fn write_u16(&mut self, addr: u32, value: u16) {
        let a = addr & 0x0FFF_FFFF;
        let lo = value as u8;
        let hi = (value >> 8) as u8;
        match a >> 24 {
            0x02 | 0x03 => {
                self.write_u8(addr, lo);
                self.write_u8(addr.wrapping_add(1), hi);
            }
            0x05 => {
                let i = ((a & 0x0000_03FF) & !1) as usize;
                self.palette[i] = lo;
                self.palette[i + 1] = hi;
            }
            0x06 => {
                let i = (Self::vram_index(a) & !1) as usize;
                self.vram[i] = lo;
                self.vram[i + 1] = hi;
            }
            0x07 => {
                let i = ((a & 0x0000_03FF) & !1) as usize;
                self.oam[i] = lo;
                self.oam[i + 1] = hi;
            }
            0x0E | 0x0F => {
                self.write_u8(addr, lo);
                self.write_u8(addr.wrapping_add(1), hi);
            }
            _ => {}
        }
    }

    /// Writes a little-endian word as two halfword lanes.
    pub fn write_u32(&mut self, addr: u32, value: u32) {
        self.write_u16(addr, value as u16);
        self.write_u16(addr.wrapping_add(2), (value >> 16) as u16);
    }

    /// Maps an address in the VRAM window onto the 96 KiB array.
    ///
    /// The 64 KiB BG bank repeats every 64 KiB and the 32 KiB OBJ bank
    /// every 32 KiB, so 0x06018000 aliases 0x06010000 even though a naive
    /// size mask would land in the BG bank (GBATEK "GBA Memory Map").
    fn vram_index(a: u32) -> u32 {
        let off = a & 0x0001_FFFF;
        if off < 0x0001_8000 {
            off
        } else {
            0x0001_0000 | (off & 0x0000_7FFF)
        }
    }

    /// Resolves an address in the 96 MiB Game Pak ROM window.
    ///
    /// The window repeats the cart every 32 MiB (`& 0x01FF_FFFF`); beyond
    /// the image the address wraps modulo the next power of two of the
    /// image size, mirroring how real carts decode only the top address
    /// lines their chip implements. A tail past that power of two (only
    /// possible for non-power-of-two images) has no hardware behind it.
    fn rom_index(&self, a: u32) -> Option<usize> {
        let mut off = (a & 0x01FF_FFFF) as usize;
        if off >= self.rom.len() {
            off &= self.rom_mask;
            if off >= self.rom.len() {
                return None;
            }
        }
        Some(off)
    }
}

/// Classifies a logical address into its bus region, applying the
/// 256 MiB alias so the whole 4 GiB space is covered.
pub fn region_of(addr: u32) -> Region {
    let a = addr & 0x0FFF_FFFF;
    match a >> 24 {
        0x00 if a < 0x4000 => Region::Bios,
        0x00 | 0x01 => Region::OpenBus,
        0x02 => Region::Ewram,
        0x03 => Region::Iwram,
        0x04 if a < 0x0400_0400 => Region::Io,
        0x04 => Region::OpenBus,
        0x05 => Region::Palette,
        0x06 => Region::Vram,
        0x07 => Region::Oam,
        0x08..=0x0D => Region::Rom,
        _ => Region::Sram,
    }
}

/// Little-endian halfword at `i`, which must leave room for `i + 1`.
fn halfword(bytes: &[u8], i: usize) -> u16 {
    u16::from(bytes[i]) | (u16::from(bytes[i + 1]) << 8)
}
