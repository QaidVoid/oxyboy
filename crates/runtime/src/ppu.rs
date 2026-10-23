//! PPU scanline renderer (modes 0-3 backgrounds) and video timing.
//!
//! Renders text backgrounds (modes 0-1), affine backgrounds (modes 1-2),
//! and the mode-3 bitmap through one layer pipeline: enabled backgrounds
//! are painted back-to-front by priority (BGxCNT bits 0-1, ties to the
//! lower BG number) over the backdrop color (palette entry 0), with
//! tile color 0 transparent. GREENSWAP bit 0 exchanges the green
//! components of each two-pixel group at output. Register addresses and
//! bit layouts follow GBATEK "LCD I/O Registers" (BGxCNT, BGxOFS, BG
//! affine transform matrices); BG mosaic (BGxCNT bit 4) and modes 4-5
//! are not implemented yet.

use crate::bus::Bus;
use crate::dma::DmaTiming;

/// Pixels per scanline.
pub const WIDTH: usize = 240;
/// Visible scanlines per frame; lines 160..=227 are V-Blank (GBATEK
/// DISPSTAT: the V-Blank flag is "set in line 160..226; not 227", so
/// the frame has 228 lines total).
pub const VISIBLE_LINES: usize = 160;
/// Total scanlines per frame including V-Blank.
pub const LINES_PER_FRAME: usize = 228;
/// System cycles per scanline (GBATEK "LCD Dimensions and Timings":
/// 1232 draw + 304 blank).
pub const CYCLES_PER_LINE: u32 = 1536;
/// System cycles from line start until the H-Blank flag rises. GBATEK
/// DISPSTAT note: "the H-Blank flag is '0' for a total of 1006 cycles",
/// so the flag is high for the remaining 530 cycles of every line; the
/// cruder 1232/304 split would put the flag edge 226 cycles late.
pub const HBLANK_FLAG_START: u32 = 1006;

/// One RGBA8888 frame, row-major, `WIDTH * VISIBLE_LINES * 4` bytes.
pub type Frame = Box<[u8]>;

/// Video state: scanline/dot position plus the output framebuffer.
pub struct Ppu {
    /// System cycles elapsed in the current scanline (0..1536).
    dot: u32,
    /// Current scanline (0..=227).
    line: u16,
    /// H-Blank flag state for the current line segment.
    hblank: bool,
    /// Set when the scanline counter wraps 227 -> 0; consumed by the
    /// frame driver.
    frame_ended: bool,
    /// RGBA8888 output, row-major.
    frame: Frame,
}

impl Default for Ppu {
    fn default() -> Self {
        Self::new()
    }
}

impl Ppu {
    /// Creates a PPU parked at the start of scanline 0 with a black
    /// framebuffer. Scanline 0 renders when the clock first advances
    /// past it, so a ROM that writes DISPCNT at boot is still honored
    /// for line 0.
    pub fn new() -> Self {
        Ppu {
            dot: 0,
            line: 0,
            hblank: false,
            frame_ended: false,
            frame: vec![0; WIDTH * VISIBLE_LINES * 4].into_boxed_slice(),
        }
    }

    /// Advances the video clock by `cycles` system cycles, rendering
    /// each finished visible scanline and maintaining VCOUNT, the
    /// DISPSTAT flags, and the IRQ stub.
    pub fn step(&mut self, bus: &mut Bus, cycles: u32) {
        let mut remaining = cycles;
        while remaining > 0 {
            // Distance to the next event: the H-Blank flag edge inside
            // the line, or the line end.
            let next_edge = if self.hblank {
                CYCLES_PER_LINE - self.dot
            } else {
                HBLANK_FLAG_START.saturating_sub(self.dot)
            };
            let take = remaining.min(next_edge);
            self.dot += take;
            remaining -= take;
            if !self.hblank && self.dot >= HBLANK_FLAG_START {
                self.hblank = true;
                let was = bus.dispstat();
                bus.set_dispstat_flag(0x0002, true);
                if was & 0x0010 != 0 {
                    // H-Blank IRQ enable (DISPSTAT bit 4) gates IF bit 1.
                    bus.raise_if(0x0002);
                }
                // H-Blank-timed DMA channels fire on this flag edge;
                // their cycles advance the PPU clock too.
                self.dot += bus.dma_flag_edge(DmaTiming::HBlank);
            }
            if self.dot >= CYCLES_PER_LINE {
                self.finish_line(bus);
            }
        }
    }

    /// Whether the scanline counter wrapped since the last poll.
    pub fn take_frame_ended(&mut self) -> bool {
        std::mem::take(&mut self.frame_ended)
    }

    /// The rendered frame.
    pub fn frame(&self) -> &Frame {
        &self.frame
    }

    /// Completes the current scanline: renders it if visible, then
    /// moves to the next line and applies the line-start register
    /// effects.
    fn finish_line(&mut self, bus: &mut Bus) {
        self.dot -= CYCLES_PER_LINE;
        if (self.line as usize) < VISIBLE_LINES {
            self.render_scanline(bus);
        }
        self.hblank = false;
        bus.set_dispstat_flag(0x0002, false);
        self.line = if self.line + 1 >= LINES_PER_FRAME as u16 {
            self.frame_ended = true;
            0
        } else {
            self.line + 1
        };
        bus.set_vcount(self.line);

        let dispstat = bus.dispstat();
        let was_vblank = dispstat & 0x0001 != 0;
        let was_vcount = dispstat & 0x0004 != 0;
        let lyc = dispstat >> 8;
        let vblank = (160..=226).contains(&self.line);
        let vcount_flag = lyc <= 227 && self.line == lyc;
        bus.set_dispstat_flag(0x0001, vblank);
        bus.set_dispstat_flag(0x0004, vcount_flag);
        // Rising flag edges raise IRQ requests when the matching
        // DISPSTAT enable is set (bits 3-5 gate IF bits 0-2).
        if vblank && !was_vblank {
            self.raise_irq(bus, 3, 0x0001);
            // V-Blank-timed DMA channels fire on this flag edge.
            self.dot += bus.dma_flag_edge(DmaTiming::VBlank);
        }
        if vcount_flag && !was_vcount {
            self.raise_irq(bus, 5, 0x0004);
        }
    }

    /// Raises IF bit `if_bit` when DISPSTAT enable bit `enable` is set.
    fn raise_irq(&self, bus: &mut Bus, enable: u16, if_bit: u16) {
        if bus.dispstat() & (1 << enable) != 0 {
            bus.raise_if(if_bit);
        }
    }

    /// Renders one visible scanline of the mode-3 bitmap into the
    /// framebuffer (GBATEK DISPCNT: mode 3 is a 240x160 16-bit bitmap
    /// at VRAM 0x06000000, shown when BG2 (bit 10) is enabled; bit 7
    /// is forced blank).
    ///
    /// A blanked screen shows white (GBATEK: "Forced Blank (whiter
    /// screen)"). Each VRAM halfword is BGR555: bits 0-4 red, 5-9
    /// green, 10-14 blue, expanded to RGBA8888 with `(c << 3) | (c >> 2)`.
    fn render_scanline(&mut self, bus: &Bus) {
        let line = self.line as usize;
        let dispcnt = bus.dispcnt();
        let mode = dispcnt & 0x0007;
        let row = &mut self.frame[line * WIDTH * 4..(line + 1) * WIDTH * 4];

        if dispcnt & 0x0080 != 0 {
            for px in row.as_chunks_mut::<4>().0 {
                px[0] = 0xFF;
                px[1] = 0xFF;
                px[2] = 0xFF;
                px[3] = 0xFF;
            }
            return;
        }

        // Compose the line in BGR555: backdrop first, then each enabled
        // background back-to-front (priority descending, BG number
        // descending for ties). Tile color 0 is transparent.
        let mut colors = [0u16; WIDTH];
        colors.fill(bus.palette_u16(0));
        let mut layers = [(0u8, 0usize, BgKind::Text); 4];
        let mut count = 0;
        for (id, kind) in enabled_bgs(dispcnt, mode) {
            let priority = (bus.io_half(0x0008 + 2 * id as u32) & 0x0003) as u8;
            layers[count] = (priority, id, kind);
            count += 1;
        }
        layers[..count].sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        for &(_, id, kind) in &layers[..count] {
            for (x, slot) in colors.iter_mut().enumerate() {
                let color = match kind {
                    BgKind::Text => text_pixel(bus, id, line as u32, x as u32),
                    BgKind::Affine => affine_pixel(bus, id, line as u32, x as u32),
                    BgKind::Bitmap3 => Some(bus.vram_u16(((line * WIDTH + x) * 2) as u32)),
                };
                if let Some(color) = color {
                    *slot = color;
                }
            }
        }

        if bus.greenswap() & 1 != 0 {
            for pair in colors.as_chunks_mut::<2>().0 {
                let (a, b) = (pair[0], pair[1]);
                let ga = (a >> 5) & 0x1F;
                let gb = (b >> 5) & 0x1F;
                pair[0] = (a & !0x03E0) | (gb << 5);
                pair[1] = (b & !0x03E0) | (ga << 5);
            }
        }
        for (x, &color) in colors.iter().enumerate() {
            put(row, x, expand555(color));
        }
    }
}

/// The background kinds each mode exposes (GBATEK "DISPCNT" BG mode
/// table). Modes 4-5 are not implemented yet.
fn enabled_bgs(dispcnt: u16, mode: u16) -> impl Iterator<Item = (usize, BgKind)> {
    let kinds: [(usize, BgKind); 4] = match mode {
        0 => [
            (0, BgKind::Text),
            (1, BgKind::Text),
            (2, BgKind::Text),
            (3, BgKind::Text),
        ],
        1 => [
            (0, BgKind::Text),
            (1, BgKind::Text),
            (2, BgKind::Affine),
            (0, BgKind::Text),
        ],
        2 => [
            (2, BgKind::Affine),
            (3, BgKind::Affine),
            (0, BgKind::Text),
            (0, BgKind::Text),
        ],
        3 => [
            (2, BgKind::Bitmap3),
            (0, BgKind::Text),
            (0, BgKind::Text),
            (0, BgKind::Text),
        ],
        _ => [
            (0, BgKind::Text),
            (0, BgKind::Text),
            (0, BgKind::Text),
            (0, BgKind::Text),
        ],
    };
    let len = match mode {
        0 => 4,
        1 => 3,
        2 => 2,
        3 => 1,
        _ => 0,
    };
    kinds
        .into_iter()
        .take(len)
        .filter(move |&(id, _)| dispcnt & (0x0100 << id) != 0)
}

/// The kind of background engine behind a BG number in the current
/// mode.
#[derive(Clone, Copy)]
enum BgKind {
    /// Tile-based text background (modes 0-1).
    Text,
    /// Rot/scale affine background (modes 1-2).
    Affine,
    /// The mode-3 240x160 16bpp bitmap (BG2 only).
    Bitmap3,
}

/// The BGR555 color of text BG `id` at line `y`, pixel `x`, or None
/// for the transparent tile color 0 (GBATEK "Text BG Screens/tiles").
fn text_pixel(bus: &Bus, id: usize, y: u32, x: u32) -> Option<u16> {
    let cnt = bus.io_half(0x0008 + 2 * id as u32);
    let cbb = u32::from((cnt >> 2) & 0x0003) * 0x4000;
    let sbb = u32::from((cnt >> 8) & 0x001F) * 0x0800;
    let size8bpp = cnt & 0x0080 != 0;
    let (w, h) = match (cnt >> 14) & 0x0003 {
        0 => (256, 256),
        1 => (512, 256),
        2 => (256, 512),
        _ => (512, 512),
    };
    let hofs = u32::from(bus.io_half(0x0010 + 4 * id as u32) & 0x01FF);
    let vofs = u32::from(bus.io_half(0x0012 + 4 * id as u32) & 0x01FF);
    let tx = (x + hofs) % w;
    let ty = (y + vofs) % h;

    let entry = bus.vram_u16(sbb + 2 * ((ty / 8) * (w / 8) + tx / 8));
    let tile = u32::from(entry & 0x03FF);
    let hflip = entry & 0x0400 != 0;
    let vflip = entry & 0x0800 != 0;
    let row = ty % 8;
    let col = tx % 8;
    let row = if vflip { 7 - row } else { row };
    let col = if hflip { 7 - col } else { col };

    if size8bpp {
        // One byte per pixel, one 256-color palette bank.
        let index = vram_byte(bus, cbb + tile * 64 + row * 8 + col);
        if index == 0 {
            return None;
        }
        Some(bus.palette_u16(2 * u32::from(index)))
    } else {
        // Two 4bpp pixels per byte, 16 palette banks of 16 colors.
        let byte = vram_byte(bus, cbb + tile * 32 + row * 4 + col / 2);
        let index = if col % 2 == 0 { byte & 0x0F } else { byte >> 4 };
        if index == 0 {
            return None;
        }
        let bank = u32::from(entry >> 12);
        Some(bus.palette_u16(2 * (bank * 16 + u32::from(index))))
    }
}

/// The BGR555 color of affine BG `id` at line `y`, pixel `x`, or None
/// for transparent color 0 / off-texture pixels without wraparound
/// (GBATEK "Text BG Screens/tiles", "Affine BG Screen").
fn affine_pixel(bus: &Bus, id: usize, y: u32, x: u32) -> Option<u16> {
    let cnt = bus.io_half(0x000C + 2 * (id - 2) as u32);
    let cbb = u32::from((cnt >> 2) & 0x0003) * 0x4000;
    let sbb = u32::from((cnt >> 8) & 0x001F) * 0x0800;
    let wrap = cnt & 0x2000 != 0;
    let tex = 128u32 << ((cnt >> 14) & 0x0003);

    // The transform is s8.8 fixed point; X/Y are s20.8 reference
    // points (GBATEK "Affine BG"). Reference-register evaluation per
    // line is equivalent to the internal-register hardware behavior
    // while parameters are only written outside scanout.
    let base = 0x0020 + 0x10 * (id - 2) as u32;
    let pa = i32::from(bus.io_half(base) as i16);
    let pb = i32::from(bus.io_half(base + 2) as i16);
    let pc = i32::from(bus.io_half(base + 4) as i16);
    let pd = i32::from(bus.io_half(base + 6) as i16);
    let x0 = affine_ref(bus, base + 8);
    let y0 = affine_ref(bus, base + 10);

    let cx = x0 + pb * (y as i32) + pa * (x as i32);
    let cy = y0 + pd * (y as i32) + pc * (x as i32);
    let tx = cx >> 8;
    let ty = cy >> 8;

    let outside = tx < 0 || ty < 0 || tx >= tex as i32 || ty >= tex as i32;
    if outside {
        if !wrap {
            return None;
        }
        let tx = (tx as u32) & (tex - 1);
        let ty = (ty as u32) & (tex - 1);
        return affine_color(bus, cbb, sbb, tex, tx, ty);
    }
    affine_color(bus, cbb, sbb, tex, tx as u32, ty as u32)
}

/// Fetches one affine texture pixel through the u8 tile map and 8bpp
/// tiles.
fn affine_color(bus: &Bus, cbb: u32, sbb: u32, tex: u32, tx: u32, ty: u32) -> Option<u16> {
    let tile = u32::from(bus.vram()[(sbb + (ty / 8) * (tex / 8) + tx / 8) as usize]);
    let index = vram_byte(bus, cbb + tile * 64 + (ty % 8) * 8 + tx % 8);
    if index == 0 {
        None
    } else {
        Some(bus.palette_u16(2 * u32::from(index)))
    }
}

/// The s20.8 signed reference coordinate assembled from two IO
/// halfwords.
fn affine_ref(bus: &Bus, off: u32) -> i32 {
    i32::from(bus.io_half(off)) | (i32::from(bus.io_half(off + 2)) << 16)
}

/// One BG VRAM byte, wrapping inside the 64 KiB BG area (GBATEK:
/// "BG ... data addresses wrap around at the end of the 64K area").
fn vram_byte(bus: &Bus, off: u32) -> u8 {
    bus.vram()[(off & 0xFFFF) as usize]
}

/// Stores one RGBA8888 pixel into a framebuffer row.
fn put(row: &mut [u8], x: usize, (r, g, b): (u8, u8, u8)) {
    row[x * 4] = r;
    row[x * 4 + 1] = g;
    row[x * 4 + 2] = b;
    row[x * 4 + 3] = 0xFF;
}

/// Expands a 5-bit channel to 8 bits with the standard full-range
/// replication `(c << 3) | (c >> 2)`.
fn expand555(color: u16) -> (u8, u8, u8) {
    let r5 = color & 0x001F;
    let g5 = (color >> 5) & 0x001F;
    let b5 = (color >> 10) & 0x001F;
    let expand = |c: u16| ((c << 3) | (c >> 2)) as u8;
    (expand(r5), expand(g5), expand(b5))
}
