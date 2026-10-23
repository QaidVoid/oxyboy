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

use crate::dma::{Dma, DmaTiming};
use crate::timer::Timers;
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
    /// 0x04000000: halfword-granular IO register image (1 KiB window).
    io: Box<[u8]>,
    /// The four DMA channels (registers in `io`, engine state here).
    dma: Dma,
    /// The four timers (registers in `io`, live counters here).
    timers: Timers,
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
            io: vec![0; 0x400].into_boxed_slice(),
            dma: Dma::default(),
            timers: Timers::default(),
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
            0x04 if a < 0x0400_0400 => {
                let off = a & 0x3FF;
                // TMxCNT_L reads return the live counter, not the
                // stored reload value (GBATEK "TMxCNT_L").
                if let Some(ch) = Self::timer_channel(off, 0) {
                    let shift = 8 * (off & 1);
                    (self.timers.counter(ch) >> shift) as u8
                } else {
                    self.io[(off) as usize]
                }
            }
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
    /// Pak ROM have no write path, and IO byte writes go through the
    /// containing halfword's register semantics.
    pub fn write_u8(&mut self, addr: u32, value: u8) {
        let a = addr & 0x0FFF_FFFF;
        match a >> 24 {
            0x02 => self.ewram[(a & 0x0003_FFFF) as usize] = value,
            0x03 => self.iwram[(a & 0x0000_7FFF) as usize] = value,
            0x04 if a < 0x0400_0400 => self.io_write_byte(a, value),
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
            0x04 if a < 0x0400_0400 => self.io_write_u16(a & !1, value),
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

    /// Writes one byte of the IO window through the containing
    /// halfword's register semantics.
    fn io_write_byte(&mut self, a: u32, value: u8) {
        let idx = (a & 0x03FF) as usize;
        if idx == Self::IF as usize || idx == Self::IF as usize + 1 {
            // IF acknowledge applies to the written byte only: a byte
            // write cannot acknowledge pending bits in the other byte
            // (GBATEK IF: "Writing 1 to a bit acknowledges it").
            let half = Self::IF as usize;
            let shift = if idx == half { 0 } else { 8 };
            let cur = halfword(&self.io, half);
            let cleared = cur & !(u16::from(value) << shift);
            self.io[half] = cleared as u8;
            self.io[half + 1] = (cleared >> 8) as u8;
            return;
        }
        let half = a & 0x03FE;
        let old = halfword(&self.io, half as usize);
        let merged = if a & 1 == 0 {
            (old & 0xFF00) | u16::from(value)
        } else {
            (old & 0x00FF) | (u16::from(value) << 8)
        };
        self.io_write_u16(half, merged);
    }

    /// Writes an aligned IO halfword with per-register semantics.
    ///
    /// Implemented: DISPCNT, GREENSWAP, DISPSTAT, VCOUNT,
    /// IE, IF, and IME (GBATEK "LCD I/O Registers" and "Interrupt
    /// Control"). Unimplemented registers are plain storage, which is
    /// indistinguishable from write-only stubs for now.
    /// DISPSTAT bits 0-2 are read-only flags maintained by the PPU
    /// (GBATEK: "(R)" on V-Blank/H-Blank/V-Counter), VCOUNT is
    /// read-only, and IF acknowledges on written 1 bits.
    fn io_write_u16(&mut self, half: u32, value: u16) {
        let off = (half & 0x3FF) as usize;
        let dma_cnt_h = Self::dma_cnt_h_channel(off as u32);
        let timer_cnt_h = Self::timer_channel(off as u32, 2);
        let was_enabled = dma_cnt_h.is_some() && halfword(&self.io, off) & 0x8000 != 0
            || timer_cnt_h.is_some() && halfword(&self.io, off) & 0x0080 != 0;
        let stored = match off as u32 {
            Self::DISPSTAT => {
                let flags = halfword(&self.io, off) & 0x0007;
                flags | (value & !0x0007)
            }
            Self::VCOUNT => return,
            Self::IF => halfword(&self.io, off) & !value,
            _ => value,
        };
        self.io[off] = stored as u8;
        self.io[off + 1] = (stored >> 8) as u8;
        if let Some(ch) = dma_cnt_h {
            // Latch internal SAD/DAD/count on the enable 0 -> 1 edge
            // (GBATEK "DMA Transfer Channels"), after the store so the
            // new CNT_H timing bits are visible.
            if !was_enabled && value & 0x8000 != 0 {
                self.dma.on_enable(&self.io, ch);
            }
        }
        if let Some(ch) = timer_cnt_h {
            // The timer counter reloads on the enable 0 -> 1 edge
            // (GBATEK "TMxCNT_L") from the just-latched CNT_L value.
            if !was_enabled && value & 0x0080 != 0 {
                let reload = halfword(&self.io, off - 2);
                self.timers.on_enable(ch, reload);
            }
        }
    }

    /// The channel index if `off` is timer N's CNT_L (`base`) or CNT_H
    /// (`base + 2`) halfword offset at `0x04000100 + 4*N`.
    fn timer_channel(off: u32, which: u32) -> Option<usize> {
        if (0x100..0x110).contains(&off) && (off - 0x100) & 0x3 == which {
            Some(((off - 0x100) / 4) as usize)
        } else {
            None
        }
    }

    /// Advances every enabled timer by `cycles` (one system step's
    /// worth, CPU plus DMA cycles).
    pub(crate) fn step_timers(&mut self, cycles: u32) {
        let mut timers = std::mem::take(&mut self.timers);
        timers.step(self, cycles);
        self.timers = timers;
    }

    /// The raw IO halfword at `off` (masked into the 1 KiB window).
    pub(crate) fn io_half(&self, off: u32) -> u16 {
        halfword(&self.io, (off & 0x3FF) as usize)
    }

    /// Overwrites the raw IO halfword at `off` without the usual write
    /// side effects (write-1-to-clear and read-only masking are for
    /// CPU-visible writes only).
    pub(crate) fn io_set_half(&mut self, off: u32, value: u16) {
        let i = (off & 0x3FF) as usize;
        self.io[i] = value as u8;
        self.io[i + 1] = (value >> 8) as u8;
    }

    /// Arms and runs every channel triggered by a rising `timing`
    /// flag edge (DISPSTAT V-Blank / H-Blank), returning the charged
    /// cycle count.
    pub(crate) fn dma_flag_edge(&mut self, timing: DmaTiming) -> u32 {
        let mut engine = std::mem::take(&mut self.dma);
        engine.rearm(&self.io, timing);
        let cycles = engine.run(self, timing);
        self.dma = engine;
        cycles
    }

    /// Runs immediate-timing channels deferred from a CNT_H enable
    /// write, returning the charged cycle count.
    pub(crate) fn dma_run_deferred(&mut self) -> u32 {
        let mut engine = std::mem::take(&mut self.dma);
        let cycles = engine.run_deferred(self);
        self.dma = engine;
        cycles
    }

    /// The channel index if `off` is one of the four CNT_H halfword
    /// offsets (`0x040000B0 + 12*N + 0xA`).
    fn dma_cnt_h_channel(off: u32) -> Option<usize> {
        match off {
            0x0BA => Some(0),
            0x0C6 => Some(1),
            0x0D2 => Some(2),
            0x0DE => Some(3),
            _ => None,
        }
    }

    /// The current line (0..=227) as seen through the VCOUNT register.
    pub fn vcount(&self) -> u16 {
        halfword(&self.io, Self::VCOUNT as usize)
    }

    /// The DISPSTAT register image: flags in bits 0-2, IRQ enables in
    /// bits 3-5, the V-Count match line in bits 8-15.
    pub fn dispstat(&self) -> u16 {
        halfword(&self.io, Self::DISPSTAT as usize)
    }

    /// The DISPCNT register.
    pub fn dispcnt(&self) -> u16 {
        halfword(&self.io, Self::DISPCNT as usize)
    }

    /// The GREENSWAP register (bit 0 swaps green between each two-pixel
    /// group at output, GBATEK "Green Swap").
    pub fn greenswap(&self) -> u16 {
        halfword(&self.io, Self::GREENSWAP as usize)
    }

    /// Reads a palette halfword by array offset (must be even).
    pub fn palette_u16(&self, off: u32) -> u16 {
        halfword(&self.palette, (off & !1) as usize)
    }

    /// Reads a VRAM halfword by array offset, applying the 96 KiB
    /// bank mapping of the VRAM window.
    pub fn vram_u16(&self, off: u32) -> u16 {
        halfword(&self.vram, (Self::vram_index(off) & !1) as usize)
    }

    /// Sets or clears DISPSTAT flag bits (bit 0 V-Blank, bit 1
    /// H-Blank, bit 2 V-Counter) without disturbing the other flags.
    pub fn set_dispstat_flag(&mut self, mask: u16, set: bool) {
        let off = Self::DISPSTAT as usize;
        let cur = halfword(&self.io, off);
        let mut flags = cur & 0x0007;
        if set {
            flags |= mask & 0x0007;
        } else {
            flags &= !(mask & 0x0007);
        }
        let merged = (cur & !0x0007) | flags;
        self.io[off] = merged as u8;
        self.io[off + 1] = (merged >> 8) as u8;
    }

    /// Publishes the current scanline into the read-only VCOUNT
    /// register.
    pub fn set_vcount(&mut self, line: u16) {
        self.io[Self::VCOUNT as usize] = line as u8;
        self.io[Self::VCOUNT as usize + 1] = (line >> 8) as u8;
    }

    /// Raises IF bits (GBATEK: sources set request bits; the CPU sees
    /// the interrupt when IE selects them and IME is set).
    pub fn raise_if(&mut self, bits: u16) {
        let off = Self::IF as usize;
        let cur = halfword(&self.io, off) | bits;
        self.io[off] = cur as u8;
        self.io[off + 1] = (cur >> 8) as u8;
    }

    /// Whether an interrupt is reaching the CPU: IE selecting a pending
    /// IF bit and IME set (GBATEK "Interrupt Control").
    pub fn irq_pending(&self) -> bool {
        self.ime() != 0 && (self.int_enable() & self.int_flags()) != 0
    }

    /// The IE register (IRQ enable mask).
    pub fn int_enable(&self) -> u16 {
        halfword(&self.io, Self::IE as usize)
    }

    /// The IF register (pending IRQs).
    pub fn int_flags(&self) -> u16 {
        halfword(&self.io, Self::IF as usize)
    }

    /// The IME register (master IRQ enable).
    pub fn ime(&self) -> u16 {
        halfword(&self.io, Self::IME as usize)
    }

    /// IO window offsets of the implemented registers.
    const DISPCNT: u32 = 0x000;
    const GREENSWAP: u32 = 0x002;
    const DISPSTAT: u32 = 0x004;
    const VCOUNT: u32 = 0x006;
    const IE: u32 = 0x200;
    const IF: u32 = 0x202;
    const IME: u32 = 0x208;
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
