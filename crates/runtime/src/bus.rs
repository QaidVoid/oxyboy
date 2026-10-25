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

use crate::apu::Apu;
use crate::dma::{Dma, DmaTiming};
use crate::input::Key;
use crate::save::{Save, SaveKind};
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

/// Log2 of the invalidation page size the recomp driver uses:
/// translated code occupies 1 KiB pages, and any write into a page
/// holding translated code invalidates that code.
pub const PAGE_SHIFT: u32 = 10;

/// Value returned by reads of unmapped locations.
///
/// Real hardware drives the data bus with the most recently fetched opcode
/// (GBATEK "Open Bus"); until the CPU exists to track that, the skeleton
/// documents and returns zero.
const OPEN_BUS_BYTE: u8 = 0;

/// The BIOS read-protection snapshots (GBATEK "BIOS read protection":
/// reads from outside the BIOS return the last word prefetched from
/// the BIOS bus). The external bios suite pins the four values the
/// real BIOS's reset, SWI, and IRQ paths leave latched; with no BIOS
/// image mapped, the HLE dispatch replays them.
pub const BIOS_LATCH_BOOT: u32 = 0xE129_F000;
pub const BIOS_LATCH_SWI: u32 = 0xE3A0_2004;
pub const BIOS_LATCH_IRQ: u32 = 0xE25E_F004;
pub const BIOS_LATCH_IRQ_RETURN: u32 = 0xE55E_C002;

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

/// One IO window bus access, recorded when the log is armed.
/// Wide accesses appear as their real bus lanes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoAccess {
    /// Absolute bus address inside the IO window.
    pub addr: u32,
    /// The value on the data lane.
    pub value: u32,
    /// Access width in bytes (1 or 2).
    pub width: u8,
    /// True for reads, false for writes.
    pub read: bool,
}

/// The GBA memory system: region arrays plus the address decoder.
///
/// The full 4 GiB address space aliases the 256 MiB window at 0x00000000
/// because the top four address bits are unused (GBATEK "Unused Memory
/// Area"); every access is therefore masked with 0x0FFF_FFFF first.
pub struct Bus {
    bios: Box<[u8]>,
    /// Whether a real BIOS image is mapped ([`Bus::with_bios`]); with
    /// the default zero image the window serves the read-protection
    /// latch instead.
    bios_mapped: bool,
    /// The word a protected BIOS read returns (see the
    /// `BIOS_LATCH_*` constants).
    bios_latch: u32,
    ewram: Box<[u8]>,
    iwram: Box<[u8]>,
    palette: Box<[u8]>,
    vram: Box<[u8]>,
    oam: Box<[u8]>,
    rom: Box<[u8]>,
    /// `next_power_of_two(rom.len()) - 1`, for mirroring beyond the image.
    rom_mask: usize,
    /// The cartridge backup medium (SRAM, flash, or EEPROM).
    save: Save,
    /// 0x04000000: halfword-granular IO register image (1 KiB window).
    io: Box<[u8]>,
    /// The four DMA channels (registers in `io`, engine state here).
    dma: Dma,
    /// The four timers (registers in `io`, live counters here).
    timers: Timers,
    /// The PSG sound channels (registers in `io`, engine state here).
    apu: Apu,
    /// The pressed keypad buttons (bit set = pressed, active high;
    /// KEYINPUT renders the inverse image).
    keys: u16,
    /// The last evaluated KEYCNT condition, for the request edge.
    key_irq_line: bool,
    /// The previous bus operation (address, width in bytes), for
    /// sequential-access timing. Width 0 means "no previous access".
    last_access: (u32, u8),
    /// The opcode-fetch address held by the Game Pak prefetch buffer
    /// (WAITCNT bit 14), if any. Hit-only model: see `fetch_cost`.
    prefetch: Option<u32>,
    /// Whether bus writes are recorded for the tracer. The log only
    /// fills while a [`System::step_recorded`](crate::system::System::step_recorded)
    /// step is in progress.
    write_trace: bool,
    /// Bus writes captured since the last drain, oldest first.
    write_log: Vec<trace::MemWrite>,
    /// Whether bus writes are recorded for the recomp driver's
    /// self-modifying-code detection. Independent of `write_trace` so
    /// tracing and recompilation do not steal each other's records.
    write_watch: bool,
    /// Addresses of the writes observed while `write_watch` is armed,
    /// oldest first; one entry per page touched (a write straddling a
    /// page boundary contributes both pages).
    watch_log: Vec<u32>,
    /// Nesting depth of the capture guard: write widths funnel into
    /// narrower writes, and only the outermost call is recorded.
    capture_depth: u32,
    /// Armed IO access log, oldest first; `None` while disarmed so
    /// the per-access cost is one branch.
    io_log: Option<Vec<IoAccess>>,
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
        let rom = rom.into_boxed_slice();
        let save = Save::new(SaveKind::detect(&rom));
        let mut bus = Bus {
            bios: vec![0; 0x4000].into_boxed_slice(),
            bios_mapped: false,
            bios_latch: BIOS_LATCH_BOOT,
            ewram: vec![0; EWRAM_SIZE].into_boxed_slice(),
            iwram: vec![0; IWRAM_SIZE].into_boxed_slice(),
            palette: vec![0; PALETTE_SIZE].into_boxed_slice(),
            vram: vec![0; VRAM_SIZE].into_boxed_slice(),
            oam: vec![0; OAM_SIZE].into_boxed_slice(),
            rom,
            rom_mask,
            save,
            io: vec![0; 0x400].into_boxed_slice(),
            dma: Dma::default(),
            timers: Timers::default(),
            apu: Apu::new(),
            keys: 0,
            key_irq_line: false,
            last_access: (0, 0),
            prefetch: None,
            write_trace: false,
            write_log: Vec::new(),
            write_watch: false,
            watch_log: Vec::new(),
            capture_depth: 0,
            io_log: None,
        };
        // SOUNDBIAS resets to 0200h: bias level 100h, the 10-bit output
        // midpoint (GBATEK SOUNDBIAS).
        bus.io[0x089] = 0x02;
        bus.refresh_keyinput();
        bus
    }

    /// Builds a bus with a real BIOS image mapped at 0x00000000. The
    /// image is padded or truncated to the 16 KiB BIOS window; reads
    /// outside the image return the zero padding as on an empty map.
    pub fn with_bios(rom: Vec<u8>, bios: Vec<u8>) -> Self {
        let mut bus = Self::new(rom);
        let n = bios.len().min(0x4000);
        bus.bios[..n].copy_from_slice(&bios[..n]);
        bus.bios_mapped = true;
        bus
    }

    /// Replaces the BIOS read-protection latch (the BIOS-HLE dispatch
    /// paths replay the real BIOS's prefetch snapshots).
    pub fn set_bios_latch(&mut self, value: u32) {
        self.bios_latch = value;
    }

    /// Classifies a logical address into its bus region.
    pub fn region_of(&self, addr: u32) -> Region {
        region_of(addr)
    }

    /// The WAITCNT register image (0x04000204), plain IO storage.
    pub fn waitcnt(&self) -> u16 {
        halfword(&self.io, Self::WAITCNT as usize)
    }

    /// True when an access at `addr` is sequential: same region as,
    /// and immediately consecutive to, the previous bus operation, and
    /// (Game Pak ROM only) not the start of a 128 KiB block, where the
    /// cart forces a non-sequential access (GBATEK WAITCNT notes).
    fn is_sequential(&self, addr: u32) -> bool {
        let (last, last_width) = self.last_access;
        if last_width == 0 {
            return false;
        }
        let region = region_of(addr);
        if region != region_of(last) {
            return false;
        }
        if region == Region::Rom && addr & 0x0001_FFFF == 0 {
            return false;
        }
        addr == last.wrapping_add(u32::from(last_width))
    }

    /// Bus-level cycle cost of one CPU data access or DMA unit,
    /// updating the sequential-access tracker. Any bus operation
    /// invalidates the opcode prefetch buffer. Public for the CPU,
    /// the DMA engine, and timing tests.
    pub fn access_cost(&mut self, addr: u32, width: u8) -> u32 {
        self.prefetch = None;
        let seq = self.is_sequential(addr);
        self.last_access = (addr, width);
        self.access_cycles(addr, width, seq)
    }

    /// Bus-level cycle cost of one opcode fetch, honoring the Game Pak
    /// prefetch buffer (WAITCNT bit 14) with a hit-only model: a fetch
    /// at the buffered successor address completes in one cycle, and
    /// the buffer then advances; any miss charges the plain waitstate
    /// cost. Hardware fills the buffer during idle bus cycles, which
    /// are not modeled, so real code can hit where this model misses
    /// (never the reverse). Only ROM fetches populate the buffer.
    /// Public for the CPU and timing tests.
    pub fn fetch_cost(&mut self, addr: u32, width: u8) -> u32 {
        let hit = self.prefetch == Some(addr);
        let cost = if hit {
            self.last_access = (addr, width);
            1
        } else {
            self.access_cost(addr, width)
        };
        self.prefetch = if self.waitcnt() & 0x4000 != 0 && region_of(addr) == Region::Rom {
            Some(addr.wrapping_add(u32::from(width)))
        } else {
            None
        };
        cost
    }

    /// The WAITCNT-driven cost of one bus operation at `addr`.
    ///
    /// Game Pak ROM mirrors map to waitstate groups WS0-2 (GBATEK
    /// WAITCNT bits 2-10); SRAM uses bits 0-1 for both access kinds.
    /// A word access costs two 16-bit lanes, the second always
    /// sequential because the cart bus is 16 bits wide.
    fn access_cycles(&self, addr: u32, width: u8, seq: bool) -> u32 {
        let wcnt = self.waitcnt();
        match (region_of(addr), width) {
            (Region::Rom, 4) => {
                rom_lane_cost(wcnt, addr, seq) + rom_lane_cost(wcnt, addr.wrapping_add(2), true)
            }
            (Region::Rom, _) => rom_lane_cost(wcnt, addr, seq),
            (Region::Sram, _) => first_access_cycles(wcnt),
            (Region::Ewram, 4) => 6,
            (Region::Ewram, _) => 3,
            (Region::Palette | Region::Vram, 4) => 2,
            _ => 1,
        }
    }

    /// The Game Pak ROM image backing the ROM window.
    pub fn rom(&self) -> &[u8] {
        &self.rom
    }

    /// Zeroes the backing storage behind `addr..addr+len` through each
    /// region's own addressing rules (no byte-doubling side effects).
    /// Used by the RegisterRamReset HLE; other regions and spans that
    /// wrap a region boundary are ignored.
    pub fn zero_mem(&mut self, addr: u32, len: u32) {
        if len == 0 {
            return;
        }
        let a = addr & 0x0FFF_FFFF;
        let end = a.saturating_add(len);
        let clear = |mem: &mut [u8], s: usize, e: usize| {
            if e > s {
                mem[s..e].fill(0);
            }
        };
        match region_of(a) {
            Region::Ewram => {
                let s = (a & 0x0003_FFFF) as usize;
                let e = (((end - 1) & 0x0003_FFFF) as usize) + 1;
                clear(&mut self.ewram, s, e);
            }
            Region::Iwram => {
                let s = (a & 0x0000_7FFF) as usize;
                let e = (((end - 1) & 0x0000_7FFF) as usize) + 1;
                clear(&mut self.iwram, s, e);
            }
            Region::Palette => {
                let s = (a & 0x0000_03FF) as usize;
                let e = (((end - 1) & 0x0000_03FF) as usize) + 1;
                clear(&mut self.palette, s, e);
            }
            Region::Vram => {
                let s = Self::vram_index(a) as usize;
                let e = Self::vram_index(end - 1) as usize + 1;
                clear(&mut self.vram, s, e);
            }
            Region::Oam => {
                let s = (a & 0x0000_03FF) as usize;
                let e = (((end - 1) & 0x0000_03FF) as usize) + 1;
                clear(&mut self.oam, s, e);
            }
            _ => {}
        }
    }

    /// The 256 KiB EWRAM array (address 0x02000000).
    pub fn ewram(&self) -> &[u8] {
        &self.ewram
    }

    /// Copies `bytes` to the start of EWRAM, truncating at 256 KiB.
    /// This is how a multiboot boot places the image the BIOS copied.
    pub fn load_ewram(&mut self, bytes: &[u8]) {
        let len = bytes.len().min(EWRAM_SIZE);
        self.ewram[..len].copy_from_slice(&bytes[..len]);
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

    /// The cartridge backup medium.
    pub fn save(&self) -> &Save {
        &self.save
    }

    /// The cartridge backup medium, mutably (tests and tools).
    pub fn save_mut(&mut self) -> &mut Save {
        &mut self.save
    }

    /// Reads one byte.
    pub fn read_u8(&mut self, addr: u32) -> u8 {
        let a = addr & 0x0FFF_FFFF;
        match a >> 24 {
            // A mapped BIOS image reads directly; the zero-image
            // default serves the read-protection latch: reads from
            // outside the BIOS return the last BIOS-bus prefetch
            // (GBATEK), lane-selected for narrow accesses.
            0x00 if a < 0x4000 && self.bios_mapped => self.bios[(a & 0x3FFF) as usize],
            0x00 if a < 0x4000 => (self.bios_latch >> ((a & 3) * 8)) as u8,
            0x02 => self.ewram[(a & 0x0003_FFFF) as usize],
            0x03 => self.iwram[(a & 0x0000_7FFF) as usize],
            0x04 if a < 0x0400_0400 => {
                let value = self.io_read_byte(a);
                self.log_io(a, u32::from(value), 1, true);
                value
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
            // The EEPROM answers on the upper ROM window when it is
            // the cart's medium; otherwise 0x0D is plain ROM.
            0x0D if matches!(self.save.kind(), SaveKind::Eeprom512 | SaveKind::Eeprom8k) => self
                .save
                .read_eeprom_half()
                .map_or(OPEN_BUS_BYTE, |v| v as u8),
            0x08..=0x0D => match self.rom_index(a) {
                Some(i) => self.rom[i],
                None => OPEN_BUS_BYTE,
            },
            0x0E | 0x0F => self.save.read_u8((a & 0x0000_FFFF) as u16),
            _ => OPEN_BUS_BYTE,
        }
    }

    /// Reads a little-endian halfword.
    pub fn read_u16(&mut self, addr: u32) -> u16 {
        let a = addr & 0x0FFF_FFFF;
        match a >> 24 {
            0x05 => halfword(&self.palette, ((a & 0x0000_03FF) & !1) as usize),
            0x06 => halfword(&self.vram, (Self::vram_index(a) & !1) as usize),
            0x07 => halfword(&self.oam, ((a & 0x0000_03FF) & !1) as usize),
            // The 8-bit SRAM/flash bus has one data lane: a wide read
            // returns the single byte replicated into both halves
            // (GBATEK; the external save suite pins this).
            0x0E | 0x0F => {
                let b = self.read_u8(addr);
                u16::from(b) | (u16::from(b) << 8)
            }
            // The EEPROM drives bit 0 of the 16-bit ROM window lane:
            // one halfword access, one bit (not two byte lanes).
            0x0D => {
                if let Some(v) = self.save.read_eeprom_half() {
                    v
                } else {
                    u16::from(self.read_u8(addr))
                        | (u16::from(self.read_u8(addr.wrapping_add(1))) << 8)
                }
            }
            _ => {
                u16::from(self.read_u8(addr)) | (u16::from(self.read_u8(addr.wrapping_add(1))) << 8)
            }
        }
    }

    /// Reads a little-endian word.
    ///
    /// Palette, VRAM, OAM, and the 16-bit Game Pak ROM window resolve
    /// word reads as two halfword lanes (GBATEK WAITCNT: a 32-bit
    /// access on the cart bus is two 16-bit accesses); everywhere
    /// else the access is four byte lanes.
    pub fn read_u32(&mut self, addr: u32) -> u32 {
        let a = addr & 0x0FFF_FFFF;
        if matches!(a >> 24, 0x0E | 0x0F) {
            let b = u32::from(self.read_u8(addr));
            b | (b << 8) | (b << 16) | (b << 24)
        } else if a >> 24 == 0x00 && a < 0x4000 && !self.bios_mapped {
            // The protected read returns the whole latched word; the
            // CPU applies its own LDR rotation on top.
            self.bios_latch
        } else if matches!(a >> 24, 0x05..=0x0D) {
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
    /// 8-bit writes are accepted on 8/32-bit-bus regions. Palette and
    /// VRAM have no 8-bit data lane, so the byte lands in both halves
    /// of the containing halfword (GBATEK "VRAM": 8-bit writes "write
    /// 8bit data to both upper and lower bytes"; the palette behaves
    /// identically on its 16-bit bus, which the external memory suite
    /// asserts). OAM discards byte stores (GBATEK bus-width table);
    /// BIOS and Game Pak ROM have no write path, and IO byte writes go
    /// through the containing halfword's register semantics.
    pub fn write_u8(&mut self, addr: u32, value: u8) {
        self.begin_capture(addr, 1, u32::from(value));
        let a = addr & 0x0FFF_FFFF;
        match a >> 24 {
            0x02 => self.ewram[(a & 0x0003_FFFF) as usize] = value,
            0x03 => self.iwram[(a & 0x0000_7FFF) as usize] = value,
            0x04 if a < 0x0400_0400 => self.io_write_byte(a, value),
            // VRAM has no 8-bit store: the byte lands in both halves
            // of the containing halfword (GBATEK "VRAM": 8-bit writes
            // "write 8bit data to both upper and lower bytes").
            0x06 => {
                let i = (Self::vram_index(a) & !1) as usize;
                self.vram[i] = value;
                self.vram[i + 1] = value;
            }
            0x05 => {
                let i = ((a & 0x0000_03FF) & !1) as usize;
                self.palette[i] = value;
                self.palette[i + 1] = value;
            }
            0x0E | 0x0F => self.save.write_u8((a & 0x0000_FFFF) as u16, value),
            _ => {}
        }
        self.end_capture();
    }

    /// Writes a little-endian halfword.
    pub fn write_u16(&mut self, addr: u32, value: u16) {
        self.begin_capture(addr, 2, u32::from(value));
        let a = addr & 0x0FFF_FFFF;
        let lo = value as u8;
        let hi = (value >> 8) as u8;
        match a >> 24 {
            // Word and halfword accesses ignore A[0]/A[1:0] on the
            // wide buses: the value lands in the aligned unit
            // unrotated (the external arm suite pins misaligned
            // stores read back by aligned loads).
            0x02 | 0x03 => {
                let w = a & !1;
                self.write_u8(w, lo);
                self.write_u8(w.wrapping_add(1), hi);
            }
            0x04 if a < 0x0400_0400 => self.io_write_u16(a & !1, value),
            // The 8-bit SRAM/flash bus takes one byte per access: the
            // halfword store writes the lane the address selects (the
            // data is rotated by the address's bit 0; the external
            // save suite pins both parities).
            0x0E | 0x0F => {
                let b = (value >> (8 * (a & 1))) as u8;
                self.write_u8(addr, b);
            }
            // The EEPROM takes one bit per halfword access: the write
            // is not split into byte lanes (GBATEK "GBA Cart Backup
            // EEPROM": only bit 0 of the data bus reaches the chip).
            0x0D => {
                self.save.write_eeprom_half(value);
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
            _ => {}
        }
        self.end_capture();
    }

    /// Writes a little-endian word.
    ///
    /// On the 8-bit SRAM/flash bus the store is one cycle: the byte
    /// the word lane selects (`a & 3`), so a word store writes exactly
    /// one address (the external save suite pins all four parities).
    pub fn write_u32(&mut self, addr: u32, value: u32) {
        self.begin_capture(addr, 4, value);
        let a = addr & 0x0FFF_FFFF;
        if matches!(a >> 24, 0x0E | 0x0F) {
            let b = (value >> (8 * (a & 3))) as u8;
            self.write_u8(addr, b);
        } else if matches!(a >> 24, 0x04 | 0x0D) {
            // The 16-bit IO and EEPROM lanes sequence A[1:0] through:
            // the word splits at the address as given (the IRQ model
            // pins a word acknowledge at IF+0/IF+2).
            self.write_u16(a, value as u16);
            self.write_u16(a.wrapping_add(2), (value >> 16) as u16);
        } else {
            // Word accesses ignore A[1:0]: the value lands in the
            // aligned word unrotated (the external arm suite pins a
            // store at +3 read back by an aligned load).
            let w = a & !3;
            self.write_u16(w, value as u16);
            self.write_u16(w.wrapping_add(2), (value >> 16) as u16);
        }
        self.end_capture();
    }

    /// Records one outermost bus write while tracing is on.
    fn begin_capture(&mut self, addr: u32, width: u8, value: u32) {
        if self.write_trace && self.capture_depth == 0 {
            self.write_log.push(trace::MemWrite { addr, width, value });
        }
        if self.write_watch && self.capture_depth == 0 {
            let last = addr + u32::from(width) - 1;
            self.watch_log.push(addr);
            if Self::page_of(addr) != Self::page_of(last) {
                self.watch_log.push(last);
            }
        }
        self.capture_depth += 1;
    }

    /// The 1 KiB page index of a bus address, the granularity the
    /// recomp driver uses for translated-code invalidation.
    fn page_of(addr: u32) -> u32 {
        (addr & 0x0FFF_FFFF) >> PAGE_SHIFT
    }

    /// Closes the capture guard opened by [`Bus::begin_capture`].
    fn end_capture(&mut self) {
        self.capture_depth -= 1;
    }

    /// Turns bus-write capture for the tracer on or off and discards
    /// anything captured so far. Owned by
    /// [`System::step_recorded`](crate::system::System::step_recorded).
    pub fn set_write_trace(&mut self, on: bool) {
        self.write_trace = on;
        self.write_log.clear();
    }

    /// Drains the captured bus writes, oldest first.
    pub fn take_writes(&mut self) -> Vec<trace::MemWrite> {
        std::mem::take(&mut self.write_log)
    }

    /// Arms or disarms the IO access log, discarding anything
    /// captured so far. While armed, every access to the
    /// `0x04000000` window from any bus client is recorded; drain
    /// with [`Bus::take_io_log`].
    pub fn set_io_log(&mut self, on: bool) {
        self.io_log = if on { Some(Vec::new()) } else { None };
    }

    /// Drains the IO access log, oldest first.
    pub fn take_io_log(&mut self) -> Vec<IoAccess> {
        match self.io_log.as_mut() {
            Some(log) => std::mem::take(log),
            None => Vec::new(),
        }
    }

    /// Arms the write watch for one execution unit and discards
    /// anything captured so far. Owned by the recomp driver: writes
    /// observed while armed are the SMC-detection channel.
    pub fn arm_write_watch(&mut self) {
        self.write_watch = true;
        self.watch_log.clear();
    }

    /// Disarms the write watch and returns the pages' worth of write
    /// addresses captured while it was armed, oldest first.
    pub fn take_write_watch(&mut self) -> Vec<u32> {
        self.write_watch = false;
        std::mem::take(&mut self.watch_log)
    }

    /// The IO register image, for state-equality tests.
    pub fn io_image(&self) -> &[u8] {
        &self.io
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
    /// The stored-or-live byte value of one IO byte lane: live
    /// counters and APU state override the register image (GBATEK
    /// "TMxCNT_L", "SOUNDCNT_X", "SOUND3CNT_L").
    fn io_read_byte(&mut self, a: u32) -> u8 {
        let off = a & 0x3FF;
        // TMxCNT_L reads return the live counter, not the
        // stored reload value (GBATEK "TMxCNT_L").
        if let Some(ch) = Self::timer_channel(off, 0) {
            let shift = 8 * (off & 1);
            (self.timers.counter(ch) >> shift) as u8
        } else if off == Self::SOUNDCNT_X {
            // Only the master enable bit is stored; the
            // channel-active flags are live APU state (GBATEK
            // SOUNDCNT_X).
            (self.io[off as usize] & 0x80) | self.apu.active_mask()
        } else if (0x090..0x0A0).contains(&off) {
            // The WAVE_RAM window addresses the bank not being
            // played (GBATEK SOUND3CNT_L).
            self.apu.wave_cpu_byte(self.wave_window_bank(), off - 0x090)
        } else {
            self.io[(off) as usize]
        }
    }

    /// Records one IO bus access when the log is armed: every CPU,
    /// PPU, timer, APU, and DMA touch of the `0x04000000` window.
    /// Wide accesses appear as their real bus lanes (a word store is
    /// two halfword accesses); the log records the bus access, which
    /// may still be discarded by register semantics.
    fn log_io(&mut self, addr: u32, value: u32, width: u8, read: bool) {
        if let Some(log) = self.io_log.as_mut() {
            log.push(IoAccess {
                addr,
                value,
                width,
                read,
            });
        }
    }

    fn io_write_byte(&mut self, a: u32, value: u8) {
        self.log_io(a, u32::from(value), 1, false);
        let idx = (a & 0x03FF) as usize;
        if (0x090..0x0A0).contains(&idx) {
            // The WAVE_RAM window addresses the bank not being played
            // (GBATEK SOUND3CNT_L); the data lives in the APU, not the
            // register image.
            let off = (idx - 0x090) as u32;
            let bank = self.wave_window_bank();
            self.apu.wave_cpu_set_byte(bank, off, value);
            return;
        }
        if (0x0A0..=0x0A7).contains(&idx) {
            // FIFO sound registers are write-only queues; each written
            // byte enters the queue (GBATEK FIFO_A/B).
            self.apu.fifo_push_byte((idx >> 2) & 1, value);
            return;
        }
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
        self.log_io(half, u32::from(value), 2, false);
        let off = (half & 0x3FF) as usize;
        let snd = off as u32;
        if (0x060..0x082).contains(&snd) {
            // PSG registers: while the master enable is clear the image
            // is zero and writes are ignored (GBATEK SOUNDCNT_X). The
            // restart strobes (bit 15 of the four CNT_X halfwords) are
            // write-only and stored as zero, then fire once here.
            if self.io[Self::SOUNDCNT_X as usize] & 0x80 == 0 {
                return;
            }
            let strobe = Self::SND_STROBES.contains(&snd) && value & 0x8000 != 0;
            let stored = if strobe { value & !0x8000 } else { value };
            self.io[off] = stored as u8;
            self.io[off + 1] = (stored >> 8) as u8;
            if strobe {
                match snd {
                    0x064 => {
                        let env = halfword(&self.io, 0x062);
                        let sweep = halfword(&self.io, 0x060);
                        self.apu.restart_square(0, env, value, sweep);
                    }
                    0x06C => {
                        let env = halfword(&self.io, 0x068);
                        self.apu.restart_square(1, env, value, 0);
                    }
                    0x074 => {
                        let len = halfword(&self.io, 0x072);
                        self.apu.restart_wave(len);
                    }
                    _ => {
                        let env = halfword(&self.io, 0x078);
                        self.apu.restart_noise(env, value);
                    }
                }
            }
            return;
        }
        if (0x0A0..0x0A8).contains(&snd) {
            // FIFO sound queues take halfword writes as two bytes,
            // least significant first (a 32-bit write arrives as two
            // halfword lanes).
            let which = ((snd >> 2) & 1) as usize;
            self.apu.fifo_push_byte(which, value as u8);
            self.apu.fifo_push_byte(which, (value >> 8) as u8);
            return;
        }
        if (0x090..0x0A0).contains(&snd) {
            // WAVE_RAM window: two bytes into the non-playing bank.
            let off = snd - 0x090;
            let bank = self.wave_window_bank();
            self.apu.wave_cpu_set_byte(bank, off, value as u8);
            self.apu
                .wave_cpu_set_byte(bank, off + 1, (value >> 8) as u8);
            return;
        }
        let dma_cnt_h = Self::dma_cnt_h_channel(snd);
        let timer_cnt_h = Self::timer_channel(snd, 2);
        let was_enabled = dma_cnt_h.is_some() && halfword(&self.io, off) & 0x8000 != 0
            || timer_cnt_h.is_some() && halfword(&self.io, off) & 0x0080 != 0;
        let stored = match off as u32 {
            Self::DISPSTAT => {
                let flags = halfword(&self.io, off) & 0x0007;
                flags | (value & !0x0007)
            }
            Self::VCOUNT => return,
            Self::KEYINPUT => return,
            Self::IF => halfword(&self.io, off) & !value,
            Self::SOUNDCNT_H => {
                if value & 0x0800 != 0 {
                    self.apu.fifo_clear(0);
                }
                if value & 0x8000 != 0 {
                    self.apu.fifo_clear(1);
                }
                value
            }
            Self::SOUNDCNT_X => {
                let was_on = halfword(&self.io, off) & 0x0080 != 0;
                if was_on && value & 0x0080 == 0 {
                    // Master disable zeroes the PSG image (0x060..0x081)
                    // and resets the engine until re-enabled; SOUNDCNT_H
                    // and WAVE_RAM are unaffected (GBATEK SOUNDCNT_X).
                    for h in (0x060..=0x080).step_by(2) {
                        self.io[h] = 0;
                        self.io[h + 1] = 0;
                    }
                    self.apu.reset();
                }
                value & 0x0080
            }
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
        if snd == Self::KEYCNT {
            // A KEYCNT rewrite can make the request condition true
            // (mask or condition change over held keys).
            self.eval_key_irq();
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
    /// worth, CPU plus DMA cycles). Public so hardware tests can drive
    /// timed events (sound FIFO latches) without a CPU.
    pub fn step_timers(&mut self, cycles: u32) {
        let mut timers = std::mem::take(&mut self.timers);
        timers.step(self, cycles);
        self.timers = timers;
    }

    /// Advances the APU by `cycles` system cycles. `System::step`
    /// calls this with every step's cycle budget; exposing it also lets
    /// tests and tools run the audio pipeline without a CPU.
    pub fn step_apu(&mut self, cycles: u32) {
        let mut apu = std::mem::take(&mut self.apu);
        apu.step(self, cycles);
        self.apu = apu;
    }

    /// Drains the interleaved stereo samples the APU mixed since the
    /// last drain: the headless capture API (`System::step_frame` plus
    /// this yields one mix-down per frame).
    pub fn take_audio(&mut self) -> Vec<i16> {
        self.apu.take_samples()
    }

    /// The queued byte count of FIFO sound channel `which` (0 = A,
    /// 1 = B), for headless inspection of playback health.
    pub fn fifo_len(&self, which: usize) -> usize {
        self.apu.fifo_len(which)
    }

    /// Presses one keypad button (the scripted test input API; a
    /// press takes effect immediately and re-evaluates the KEYCNT
    /// request condition).
    pub fn press(&mut self, key: Key) {
        self.keys |= key.bit();
        self.refresh_keyinput();
        self.eval_key_irq();
    }

    /// Releases one keypad button.
    pub fn release(&mut self, key: Key) {
        self.keys &= !key.bit();
        self.refresh_keyinput();
        self.eval_key_irq();
    }

    /// Stores the KEYINPUT register image: pressed bits read 0
    /// (GBATEK KEYINPUT, active low); the unused bits 10-15 read 0,
    /// matching mGBA and VBA (GBATEK lists them unused without a
    /// value).
    fn refresh_keyinput(&mut self) {
        let image = !self.keys & 0x03FF;
        self.io[Self::KEYINPUT as usize] = image as u8;
        self.io[Self::KEYINPUT as usize + 1] = (image >> 8) as u8;
    }

    /// Evaluates the KEYCNT condition (OR: at least one selected key
    /// down; AND: all selected keys down; an empty mask is never
    /// true) and raises IF bit 12 on the false -> true edge while
    /// bit 14 enables the request.
    fn eval_key_irq(&mut self) {
        let cnt = self.io_half(Self::KEYCNT);
        let mask = cnt & 0x03FF;
        let pressed = self.keys & mask;
        let cond = if cnt & 0x8000 != 0 {
            mask != 0 && pressed == mask
        } else {
            pressed != 0
        };
        let was = self.key_irq_line;
        self.key_irq_line = cond;
        if cond && !was && cnt & 0x4000 != 0 {
            self.raise_if(0x1000);
        }
    }

    /// Latches the next FIFO byte per the SOUNDCNT_H timer selects for
    /// the overflowing timer (0 or 1) and runs any sound-FIFO DMA
    /// refill the latch demands (GBATEK "DMA-Sound Playback
    /// Procedure").
    pub(crate) fn apu_timer_latch(&mut self, timer: usize) {
        let cnt_h = self.io_half(Self::SOUNDCNT_H);
        let a_select = ((cnt_h >> 10) & 1) as usize;
        let b_select = ((cnt_h >> 14) & 1) as usize;
        let mut apu = std::mem::take(&mut self.apu);
        let requests = apu.timer_latch(timer, a_select, b_select);
        self.apu = apu;
        if requests != 0 {
            let mut dma = std::mem::take(&mut self.dma);
            dma.sound_refill(self, requests);
            self.dma = dma;
        }
    }

    /// The wave RAM bank the CPU window (0x090..) addresses: the one
    /// NOT selected for playback by NR30 bit 6 (GBATEK SOUND3CNT_L).
    fn wave_window_bank(&self) -> usize {
        1 - usize::from(self.io_half(Self::NR30) & 0x0040 != 0)
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

    /// The raw IF register (pending IRQ request bits).
    pub fn int_flags(&self) -> u16 {
        halfword(&self.io, Self::IF as usize)
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

    /// The IME register (master IRQ enable).
    pub fn ime(&self) -> u16 {
        halfword(&self.io, Self::IME as usize)
    }

    /// IO window offsets of the implemented registers.
    const DISPCNT: u32 = 0x000;
    const GREENSWAP: u32 = 0x002;
    const DISPSTAT: u32 = 0x004;
    const VCOUNT: u32 = 0x006;
    const NR30: u32 = 0x070;
    const SOUNDCNT_H: u32 = 0x082;
    const SOUNDCNT_X: u32 = 0x084;
    /// KEYINPUT (read only) and KEYCNT (key IRQ control).
    const KEYINPUT: u32 = 0x130;
    const KEYCNT: u32 = 0x132;
    /// IO offsets of the four sound CNT_X halfwords whose bit 15
    /// restarts a channel (NR14/NR24/NR34/NR44).
    const SND_STROBES: [u32; 4] = [0x064, 0x06C, 0x074, 0x07C];
    const IE: u32 = 0x200;
    const IF: u32 = 0x202;
    const IME: u32 = 0x208;
    const WAITCNT: u32 = 0x204;
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

/// First-access (non-sequential) cycles for a WAITCNT code field,
/// bits carried in the low two bits of `code` (GBATEK WAITCNT:
/// codes 0..3 cost 4, 3, 2, 8 cycles).
fn first_access_cycles(code: u16) -> u32 {
    const FIRST: [u32; 4] = [4, 3, 2, 8];
    FIRST[(code & 3) as usize]
}

/// One 16-bit Game Pak ROM lane's cost from WAITCNT (GBATEK WAITCNT
/// bits 2-10). The lane is non-sequential at 128 KiB block starts;
/// otherwise the sequential bit selects the second-access setting
/// (WS0 2/1, WS1 4/1, WS2 8/1 cycles).
fn rom_lane_cost(wcnt: u16, addr: u32, seq: bool) -> u32 {
    let ws = (addr >> 24).wrapping_sub(8) / 2;
    if addr & 0x0001_FFFF == 0 || !seq {
        return match ws {
            0 => first_access_cycles(wcnt >> 2),
            1 => first_access_cycles(wcnt >> 5),
            _ => first_access_cycles(wcnt >> 8),
        };
    }
    match ws {
        0 => {
            if wcnt & 0x0010 == 0 {
                2
            } else {
                1
            }
        }
        1 => {
            if wcnt & 0x0080 == 0 {
                4
            } else {
                1
            }
        }
        _ => {
            if wcnt & 0x0400 == 0 {
                8
            } else {
                1
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_log_records_reads_and_writes_when_armed() {
        let mut bus = Bus::new(vec![0; 256]);
        bus.write_u16(0x0400_0000, 0x0403);
        assert!(
            bus.take_io_log().is_empty(),
            "disarmed log must record nothing"
        );
        bus.set_io_log(true);
        bus.write_u16(0x0400_0000, 0x0403);
        let _ = bus.read_u8(0x0400_0000);
        let log = bus.take_io_log();
        assert_eq!(log.len(), 2, "{log:?}");
        assert_eq!(
            log[0],
            IoAccess {
                addr: 0x0400_0000,
                value: 0x0403,
                width: 2,
                read: false
            }
        );
        assert_eq!(
            log[1],
            IoAccess {
                addr: 0x0400_0000,
                value: 0x03,
                width: 1,
                read: true
            }
        );
        assert!(bus.take_io_log().is_empty(), "drain must empty the log");
        bus.set_io_log(false);
        bus.write_u16(0x0400_0000, 0);
        assert!(bus.take_io_log().is_empty(), "disarmed again");
    }

    #[test]
    fn palette_byte_store_fills_the_halfword() {
        let mut bus = Bus::new(vec![0; 256]);
        bus.write_u8(0x0500_0021, 0x01);
        assert_eq!(bus.read_u16(0x0500_0020), 0x0101);
    }

    #[test]
    fn oam_byte_store_is_discarded() {
        let mut bus = Bus::new(vec![0; 256]);
        bus.write_u16(0x0700_0010, 0x1234);
        bus.write_u8(0x0700_0010, 0xFF);
        assert_eq!(bus.read_u16(0x0700_0010), 0x1234);
    }
}
