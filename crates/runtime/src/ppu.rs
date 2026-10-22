//! Mode-3 PPU scanline renderer and video timing.
//!
//! Implements the walking-skeleton video path: DISPCNT-driven
//! mode-3 bitmap rendering, VCOUNT/DISPSTAT progression, and the
//! V-Blank/H-Blank/V-Counter IRQ stub. Register addresses and bit
//! layouts follow GBATEK "LCD I/O Registers".

use crate::bus::Bus;

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
    /// screen)"); a non-mode-3 or BG2-disabled screen shows the
    /// backdrop color from palette entry 0. Each VRAM halfword is
    /// BGR555: bits 0-4 red, 5-9 green, 10-14 blue, expanded to RGBA8888
    /// with `(c << 3) | (c >> 2)`. GREENSWAP bit 0 exchanges the green
    /// components of each two-pixel group at output (GBATEK "Green
    /// Swap": the pair "is output as BgRbGr").
    fn render_scanline(&mut self, bus: &Bus) {
        let line = self.line as usize;
        let dispcnt = bus.dispcnt();
        let mode = dispcnt & 0x0007;
        let forced_blank = dispcnt & 0x0080 != 0;
        let bg2_enabled = dispcnt & 0x0400 != 0;
        let row = &mut self.frame[line * WIDTH * 4..(line + 1) * WIDTH * 4];

        if forced_blank {
            for px in row.as_chunks_mut::<4>().0 {
                px[0] = 0xFF;
                px[1] = 0xFF;
                px[2] = 0xFF;
                px[3] = 0xFF;
            }
            return;
        }
        if mode != 3 || !bg2_enabled {
            let (r, g, b) = expand555(bus.palette_u16(0));
            for px in row.as_chunks_mut::<4>().0 {
                px[0] = r;
                px[1] = g;
                px[2] = b;
                px[3] = 0xFF;
            }
            return;
        }

        let swap = bus.greenswap() & 1 != 0;
        let base = (line * WIDTH) as u32;
        let mut x = 0;
        while x < WIDTH {
            if swap && x + 1 < WIDTH {
                let c0 = bus.vram_u16((base + x as u32) * 2);
                let c1 = bus.vram_u16((base + x as u32 + 1) * 2);
                let g0 = (c0 >> 5) & 0x1F;
                let g1 = (c1 >> 5) & 0x1F;
                put(row, x, expand555((c0 & !0x03E0) | (g1 << 5)));
                put(row, x + 1, expand555((c1 & !0x03E0) | (g0 << 5)));
                x += 2;
            } else {
                let color = bus.vram_u16((base + x as u32) * 2);
                put(row, x, expand555(color));
                x += 1;
            }
        }
    }
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
