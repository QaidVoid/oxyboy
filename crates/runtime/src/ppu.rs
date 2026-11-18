//! PPU scanline renderer (modes 0-5 backgrounds, OBJ) and video timing.
//!
//! Renders text backgrounds (modes 0-1), affine backgrounds (modes 1-2),
//! the mode-3 bitmap, the page-flipped mode-4 palette bitmap and the
//! mode-5 16bpp bitmap, and sprites through one layer pipeline: enabled
//! layers are resolved front-to-back by priority (OBJ beats a BG of the
//! same priority, lower OAM index beats higher), with the first opaque
//! pixel as the top layer and the next opaque pixel as the alpha
//! second target (GBATEK "Color Special Effects"). GREENSWAP bit 0
//! exchanges the green components of each two-pixel group at output.
//! BG mosaic (BGxCNT bit 6) samples the block's top-left screen pixel;
//! OBJ mosaic (attr0 bit 12) anchors its grid at the sprite origin.
//! Register addresses and bit layouts follow GBATEK "LCD I/O
//! Registers" and "LCD OBJ" (OAM attributes, OBJ tile mapping,
//! windows, BLDCNT/BLDALPHA/BLDY, MOSAIC).

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
/// 308 dots per line at 4 cycles per dot = 1232 cycles, 228 lines =
/// 280896 cycles per frame at 59.737 Hz).
pub const CYCLES_PER_LINE: u32 = 1232;
/// System cycles from line start until the H-Blank flag rises. GBATEK
/// DISPSTAT note: "the H-Blank flag is '0' for a total of 1006 cycles",
/// so the flag is high for the remaining 226 cycles of every line.
pub const HBLANK_FLAG_START: u32 = 1006;

/// One RGBA8888 frame, row-major, `WIDTH * VISIBLE_LINES * 4` bytes.
pub type Frame = Box<[u8]>;

/// Video state: scanline/dot position plus the output framebuffer.
pub struct Ppu {
    /// System cycles elapsed in the current scanline (0..1232).
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
                let visible = (self.line as usize) < VISIBLE_LINES;
                // The line is drawn as H-Blank starts, so writes made
                // by the H-Blank IRQ handler or DMA land on the next
                // line.
                if visible {
                    self.render_scanline(bus);
                }
                let was = bus.dispstat();
                bus.set_dispstat_flag(0x0002, true);
                if was & 0x0010 != 0 {
                    // H-Blank IRQ enable (DISPSTAT bit 4) gates IF bit 1.
                    bus.raise_if(0x0002);
                }
                // H-Blank DMA runs on visible lines only (GBATEK); its
                // cycles advance the PPU clock too.
                if visible {
                    self.dot += bus.dma_flag_edge(DmaTiming::HBlank);
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

    /// Whether a frame boundary was crossed, without consuming it.
    #[must_use]
    pub fn frame_ended(&self) -> bool {
        self.frame_ended
    }

    /// System cycles to the next video event that can raise an
    /// interrupt or run DMA: the H-Blank flag edge or the line end.
    #[must_use]
    pub fn cycles_to_next_edge(&self) -> u32 {
        let edge = if self.hblank {
            CYCLES_PER_LINE
        } else {
            HBLANK_FLAG_START
        };
        edge.saturating_sub(self.dot).max(1)
    }

    /// System cycles from the current dot to the next frame boundary.
    /// The frame runner budgets native units to this so a chunk cannot
    /// run past the boundary it is supposed to stop at.
    #[must_use]
    pub fn cycles_to_frame_end(&self) -> u32 {
        let frame = LINES_PER_FRAME as u64 * CYCLES_PER_LINE as u64;
        let elapsed = self.line as u64 * CYCLES_PER_LINE as u64 + self.dot as u64;
        (frame - elapsed % frame) as u32
    }

    /// The rendered frame.
    pub fn frame(&self) -> &Frame {
        &self.frame
    }

    /// Completes the current scanline: moves to the next line and
    /// applies the line-start register effects.
    fn finish_line(&mut self, bus: &mut Bus) {
        self.dot -= CYCLES_PER_LINE;
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

    /// Renders one visible scanline into the framebuffer: forced blank
    /// paints white (GBATEK DISPCNT bit 7), otherwise the enabled BG
    /// layers plus the OBJ layer are resolved front-to-back over the
    /// backdrop, window masks gate layer visibility per pixel, and the
    /// BLDCNT color effects apply to the top two opaque pixels.
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

        // Per-pixel top OBJ pixel: OAM order decides OBJ-vs-OBJ ties
        // (GBATEK "Priority": OBJ0 beats OBJ1-127), and OBJ-window
        // sprites contribute their opaque dots as the window mask.
        let mut obj_color = [None; WIDTH];
        let mut obj_prio = [0u8; WIDTH];
        let mut obj_semi = [false; WIDTH];
        let mut objwin = [false; WIDTH];
        obj_line(
            bus,
            dispcnt,
            line as u32,
            &mut obj_color,
            &mut obj_prio,
            &mut obj_semi,
            &mut objwin,
        );

        let mut colors = [0u16; WIDTH];
        colors.fill(bus.palette_u16(0));

        // Candidate layers, front-first keys assigned per pixel: OBJ
        // priority comes from attr2 per pixel, so ordering is resolved
        // per pixel by the key (priority, OBJ-before-BG, BG number);
        // the backdrop sits behind everything.
        let mut bg_prio = [0u8; 4];
        let mut bg_mosaic = [false; 4];
        let mut bgs: Vec<(usize, BgKind)> = Vec::with_capacity(4);
        for (id, kind) in enabled_bgs(dispcnt, mode) {
            bg_prio[id] = (bus.io_half(0x0008 + 2 * id as u32) & 0x0003) as u8;
            // BGxCNT bit 6 enables BG mosaic for this layer.
            bg_mosaic[id] = bus.io_half(0x0008 + 2 * id as u32) & 0x0040 != 0;
            bgs.push((id, kind));
        }
        let obj_on = dispcnt & 0x1000 != 0;

        let bldcnt = bus.io_half(0x0050);
        let mosaic = bus.io_half(0x004C);
        let bldalpha = bus.io_half(0x0052);
        let eva = u32::from(bldalpha & 0x001F).min(16);
        let evb = u32::from((bldalpha >> 8) & 0x001F).min(16);
        let evy = u32::from(bus.io_half(0x0054) & 0x001F).min(16);
        for x in 0..WIDTH {
            let (mask, fx_on) = window_mask(bus, dispcnt, x as u32, line as u32, objwin[x]);
            // The two front-most opaque pixels. GBATEK "Color Special
            // Effects": the 2nd target is the next-lower opaque pixel,
            // whether or not it is a selected target.
            let mut top: Option<Candidate> = None;
            let mut below: Option<((u8, u8, u8), u16, bool)> = None;
            let mut consider = |key: (u8, u8, u8), color: u16, t1: bool, t2: bool, semi: bool| {
                if top.is_none_or(|(k, ..)| key < k) {
                    below = top.map(|(k, c, _, t2, _)| (k, c, t2));
                    top = Some((key, color, t1, t2, semi));
                } else if below.is_none_or(|(k, ..)| key < k) {
                    below = Some((key, color, t2));
                }
            };
            for &(id, kind) in &bgs {
                if mask >> id & 1 == 0 {
                    continue;
                }
                let color = match kind {
                    BgKind::Text => {
                        let (sx, sy) = if bg_mosaic[id] {
                            mosaic_coords(x as u32, line as u32, mosaic)
                        } else {
                            (x as u32, line as u32)
                        };
                        text_pixel(bus, id, sy, sx)
                    }
                    BgKind::Affine => {
                        let (sx, sy) = if bg_mosaic[id] {
                            mosaic_coords(x as u32, line as u32, mosaic)
                        } else {
                            (x as u32, line as u32)
                        };
                        affine_pixel(bus, id, sy, sx)
                    }
                    BgKind::Bitmap3 => Some(bus.vram_u16(((line * WIDTH + x) * 2) as u32)),
                    BgKind::Bitmap4 => {
                        let page = u32::from(dispcnt & 0x0010 != 0) * 0xA000;
                        let index =
                            bus.vram()[(page + line as u32 * WIDTH as u32 + x as u32) as usize];
                        (index != 0).then(|| bus.palette_u16(2 * u32::from(index)))
                    }
                    BgKind::Bitmap5 => {
                        let page = u32::from(dispcnt & 0x0010 != 0) * 0xA000;
                        (x < 160 && line < 128)
                            .then(|| bus.vram_u16(page + 2 * (line as u32 * 160 + x as u32)))
                    }
                };
                let Some(color) = color else { continue };
                let key = (bg_prio[id], 1, id as u8);
                consider(
                    key,
                    color,
                    bldcnt >> id as u16 & 1 != 0,
                    bldcnt >> (8 + id as u16) & 1 != 0,
                    false,
                );
            }
            let obj_pixel = if obj_on && mask & 0x10 != 0 {
                obj_color[x]
            } else {
                None
            };
            if let Some(color) = obj_pixel {
                consider(
                    (obj_prio[x], 0, 0),
                    color,
                    bldcnt & 0x0010 != 0,
                    bldcnt & 0x1000 != 0,
                    obj_semi[x],
                );
            }
            // The backdrop is always visible (GBATEK windows expose no
            // backdrop enable bit) and is the bottom-most layer.
            consider(
                (3, 2, 0),
                colors[x],
                bldcnt & 0x0020 != 0,
                bldcnt & 0x2000 != 0,
                false,
            );

            let (_, top, target1, _, semi) =
                top.unwrap_or(((0, 0, 0), colors[x], false, false, false));
            let effect = if fx_on { (bldcnt >> 6) & 0x0003 } else { 0 };
            let second_is_target = matches!(below, Some((_, _, true)));
            let color = if semi && second_is_target {
                // Semi-transparent OBJs blend regardless of BLDCNT
                // effect bits 6-7 (GBATEK "Semi-Transparent OBJs").
                blend555(top, below.unwrap().1, eva, evb)
            } else if effect == 1 && target1 && second_is_target {
                blend555(top, below.unwrap().1, eva, evb)
            } else if effect == 2 && target1 {
                fade555(top, evy, true)
            } else if effect == 3 && target1 {
                fade555(top, evy, false)
            } else {
                top
            };
            colors[x] = color;
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

/// The two front-most opaque pixel candidates: sort key, RGB555
/// color, first/second blend-target flags and semi-transparency.
type Candidate = ((u8, u8, u8), u16, bool, bool, bool);

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
        4 => [
            (2, BgKind::Bitmap4),
            (0, BgKind::Text),
            (0, BgKind::Text),
            (0, BgKind::Text),
        ],
        5 => [
            (2, BgKind::Bitmap5),
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
        3..=5 => 1,
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
    /// The mode-4 240x160 8bpp palette bitmap (BG2 only, page-flipped).
    Bitmap4,
    /// The mode-5 160x128 16bpp bitmap (BG2 only, page-flipped).
    Bitmap5,
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

    // Maps wider or taller than 256 pixels are 32x32-entry
    // screenblocks laid out SC0 SC1 / SC2 SC3, not one linear map.
    let block = tx / 256 + (ty / 256) * (w / 256);
    let cell = ((ty % 256) / 8) * 32 + (tx % 256) / 8;
    let entry = bus.vram_u16(sbb + block * 0x0800 + 2 * cell);
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

/// Computes the OBJ contribution for one scanline: the top-most opaque
/// pixel per column (OAM order breaks OBJ-vs-OBJ ties, GBATEK
/// "Priority"), its attr2 priority and semi-transparent flag, plus the
/// OBJ-window mask from OBJ-mode-2 sprites. Sprites render only when
/// DISPCNT bit 12 is set.
fn obj_line(
    bus: &Bus,
    dispcnt: u16,
    line: u32,
    obj_color: &mut [Option<u16>; WIDTH],
    obj_prio: &mut [u8; WIDTH],
    obj_semi: &mut [bool; WIDTH],
    objwin: &mut [bool; WIDTH],
) {
    if dispcnt & 0x1000 == 0 {
        return;
    }
    let mapping_1d = dispcnt & 0x0040 != 0;
    let oam = bus.oam();
    for i in 0..128u32 {
        let attr = |k: u32| {
            u16::from(oam[(i * 8 + k * 2) as usize])
                | u16::from(oam[(i * 8 + k * 2 + 1) as usize]) << 8
        };
        let a0 = attr(0);
        let a1 = attr(1);
        let a2 = attr(2);
        let rot = a0 & 0x0100 != 0;
        // Attr0 bits 8-9: double-size for rot/scale OBJs, hidden
        // otherwise (GBATEK "OBJ Attribute 0").
        if !rot && a0 & 0x0300 != 0 {
            continue;
        }
        let mode = (a0 >> 10) & 0x0003;
        if mode == 3 {
            continue; // Bitmap OBJs arrive with bitmap BG modes.
        }
        let double = rot && a0 & 0x0200 != 0;
        let size8bpp = a0 & 0x2000 != 0;
        let shape = (a0 >> 14) & 0x0003;
        if shape == 3 {
            continue;
        }
        let size = (a1 >> 14) & 0x0003;
        let (w, h) = obj_size(shape, size);
        let (w_disp, h_disp) = if double { (w * 2, h * 2) } else { (w, h) };
        let dims = ObjDims {
            w,
            h,
            w_disp,
            h_disp,
        };
        // Y wraps in 256-line OBJ space, so tall sprites at Y>160
        // reappear at the top of the screen (GBATEK attr0 caution).
        let y0 = u32::from(a0 & 0x00FF);
        let py = (line + 256 - y0) & 0xFF;
        if py >= h_disp {
            continue;
        }
        let x0 = u32::from(a1 & 0x01FF);
        let prio = (a2 >> 10 & 0x0003) as u8;
        let semi = mode == 1;
        let window_sprite = mode == 2;
        let pal_bank = (a2 >> 12) & 0x000F;
        // OBJ mosaic (attr0 bit 12) quantizes display coordinates into
        // blocks anchored at the sprite origin (GBATEK "MOSAIC").
        let (obj_hw, obj_vv) = if a0 & 0x1000 != 0 {
            let m = bus.io_half(0x004C);
            (
                u32::from((m >> 8) & 0x000F) + 1,
                u32::from((m >> 12) & 0x000F) + 1,
            )
        } else {
            (1, 1)
        };
        for px in 0..w_disp {
            // X is 9 bits and wraps in 512-pixel OBJ space.
            let sx = ((x0 + px) & 0x1FF) as usize;
            if sx >= WIDTH {
                continue;
            }
            let px = px - px % obj_hw;
            let py = py - py % obj_vv;
            let index = if rot {
                let sel = (a1 >> 9) & 0x001F;
                affine_texture(bus, sel, px, py, dims).and_then(|(tx, ty)| {
                    obj_index(bus, mapping_1d, size8bpp, a2 as u32 & 0x03FF, w, tx, ty)
                })
            } else {
                let mut tx = px;
                let mut ty = py;
                if a1 & 0x1000 != 0 {
                    tx = w - 1 - tx;
                }
                if a1 & 0x2000 != 0 {
                    ty = h - 1 - ty;
                }
                obj_index(bus, mapping_1d, size8bpp, a2 as u32 & 0x03FF, w, tx, ty)
            };
            let Some(index) = index.filter(|&i| i != 0) else {
                continue;
            };
            let color = if size8bpp {
                bus.palette_u16(0x200 + 2 * u32::from(index))
            } else {
                bus.palette_u16(0x200 + 2 * (u32::from(pal_bank) * 16 + u32::from(index)))
            };
            if window_sprite {
                objwin[sx] = true;
            } else if obj_color[sx].is_none() {
                obj_color[sx] = Some(color);
                obj_prio[sx] = prio;
                obj_semi[sx] = semi;
            }
        }
    }
}

/// The OBJ pixel color-index at sprite-local texture coordinates, or
/// None for palette index 0 (transparent).
fn obj_index(
    bus: &Bus,
    mapping_1d: bool,
    size8bpp: bool,
    base_tile: u32,
    w: u32,
    tx: u32,
    ty: u32,
) -> Option<u8> {
    // Column and row tile strides per DISPCNT bit 6 (GBATEK "OBJ VRAM
    // Character (Tile) Mapping"); 8bpp tiles consume two consecutive
    // tile numbers in both mappings.
    let stride = if size8bpp { 2 } else { 1 };
    let tile = if mapping_1d {
        base_tile + (ty / 8) * (w / 8) * stride + (tx / 8) * stride
    } else {
        base_tile + (ty / 8) * 32 + (tx / 8) * stride
    };
    if size8bpp {
        Some(obj_vram_byte(bus, tile * 64 + (ty % 8) * 8 + tx % 8))
    } else {
        let byte = obj_vram_byte(bus, tile * 32 + (ty % 8) * 4 + (tx % 8) / 2);
        Some(if tx.is_multiple_of(2) {
            byte & 0x0F
        } else {
            byte >> 4
        })
    }
}

/// Sprite texture size plus display box size; the box doubles for
/// rot/scale OBJs with the double-size bit set (GBATEK "OBJ
/// Double-Size Bit").
#[derive(Clone, Copy)]
struct ObjDims {
    w: u32,
    h: u32,
    w_disp: u32,
    h_disp: u32,
}

/// The OBJ pixel color-index under a rot/scale transform, or None when
/// the sample lands outside the texture (GBATEK "OBJ Double-Size
/// Bit": display clips to the unrotated texture size in both normal
/// and double-size modes).
fn affine_texture(bus: &Bus, sel: u16, px: u32, py: u32, dims: ObjDims) -> Option<(u32, u32)> {
    let pa = obj_param(bus, sel, 0);
    let pb = obj_param(bus, sel, 1);
    let pc = obj_param(bus, sel, 2);
    let pd = obj_param(bus, sel, 3);
    // The rotation center is the middle of the display box, sampling
    // the middle of the texture (GBATEK "OBJ Reference Point &
    // Rotation Center").
    let dx = px as i32 - dims.w_disp as i32 / 2;
    let dy = py as i32 - dims.h_disp as i32 / 2;
    let tx = ((pa * dx + pb * dy) >> 8) + dims.w as i32 / 2;
    let ty = ((pc * dx + pd * dy) >> 8) + dims.h as i32 / 2;
    if tx < 0 || ty < 0 || tx >= dims.w as i32 || ty >= dims.h as i32 {
        return None;
    }
    Some((tx as u32, ty as u32))
}

fn obj_size(shape: u16, size: u16) -> (u32, u32) {
    match (shape, size) {
        (0, s) => (8 << s, 8 << s),
        (1, 0) => (16, 8),
        (1, 1) => (32, 8),
        (1, 2) => (32, 16),
        (1, _) => (64, 32),
        (2, 0) => (8, 16),
        (2, 1) => (8, 32),
        (2, 2) => (16, 32),
        (_, _) => (32, 64),
    }
}

/// One OBJ VRAM byte, wrapping inside the 32 KiB OBJ tile bank
/// (GBATEK "VRAM - Character Data": OBJ tiles live at 06010000 in BG
/// modes 0-2).
fn obj_vram_byte(bus: &Bus, off: u32) -> u8 {
    bus.vram()[(0x10000 + (off & 0x7FFF)) as usize]
}

/// One s8.8 OBJ rot/scale parameter; group `sel` lives in the OAM
/// gaps at 07000006 + sel*32 + idx*8 (GBATEK "Location of
/// Rotation/Scaling Parameters in OAM").
fn obj_param(bus: &Bus, sel: u16, idx: usize) -> i32 {
    let oam = bus.oam();
    let off = (sel as usize * 32 + idx * 8 + 6) & 0x3FF;
    i32::from(i16::from_le_bytes([oam[off], oam[off + 1]]))
}

/// The mosaic block origin for screen pixel (x, y): the grid is
/// anchored at the screen's top-left, blocks are (H+1) x (V+1) pixels
/// (GBATEK "MOSAIC").
fn mosaic_coords(x: u32, y: u32, mosaic: u16) -> (u32, u32) {
    let hw = u32::from(mosaic & 0x000F) + 1;
    let vv = u32::from((mosaic >> 4) & 0x000F) + 1;
    (x - x % hw, y - y % vv)
}

/// The per-pixel window region: a 6-bit layer mask (bits 0-3 BG0-3,
/// bit 4 OBJ) plus whether color special effects are enabled there.
/// Region priority is window 0, window 1, OBJ window, outside
/// (GBATEK "Window Priority"); each gate needs both its DISPCNT
/// enable bit and an inside hit.
fn window_mask(bus: &Bus, dispcnt: u16, x: u32, line: u32, objwin: bool) -> (u8, bool) {
    if dispcnt & 0xE000 == 0 {
        return (0x1F, true);
    }
    let config = |half: u32| -> (u8, bool) {
        let value = bus.io_half(half);
        let mask = (value & 0x001F) as u8;
        (mask, value & 0x0020 != 0)
    };
    if dispcnt & 0x2000 != 0 && inside_window(bus, 0, x, line) {
        return config(0x0048);
    }
    if dispcnt & 0x4000 != 0 && inside_window(bus, 1, x, line) {
        let value = bus.io_half(0x0048);
        return (((value >> 8) & 0x001F) as u8, value & 0x2000 != 0);
    }
    if dispcnt & 0x8000 != 0 && objwin {
        let value = bus.io_half(0x004A);
        return (((value >> 8) & 0x001F) as u8, value & 0x2000 != 0);
    }
    config(0x004A)
}

/// Whether the pixel lies inside window `n` (0 or 1). Bounds clamp to
/// the screen; inverted bounds make the window empty, with no
/// wrap-around (GBATEK "WIN0H/WIN0V" cautions).
fn inside_window(bus: &Bus, n: u32, x: u32, line: u32) -> bool {
    let hv = bus.io_half(0x0040 + 2 * n);
    let vv = bus.io_half(0x0044 + 2 * n);
    let x1 = hv >> 8;
    let x2 = (hv & 0x00FF).min(240);
    let y1 = vv >> 8;
    let y2 = (vv & 0x00FF).min(160);
    x >= u32::from(x1) && x < u32::from(x2) && line >= u32::from(y1) && line < u32::from(y2)
}

/// Alpha-blends two BGR555 colors, channel-wise
/// `I = min(31, I1*EVA + I2*EVB)` (GBATEK "BLDALPHA").
fn blend555(top: u16, below: u16, eva: u32, evb: u32) -> u16 {
    let channels = [0, 5, 10];
    let mut out = 0;
    for &shift in channels.iter() {
        let i1 = u32::from((top >> shift) & 0x001F);
        let i2 = u32::from((below >> shift) & 0x001F);
        let mixed = (i1 * eva + i2 * evb).min(31 * 16) / 16;
        out |= (mixed as u16) << shift;
    }
    out
}

/// Fades a BGR555 color toward white (`up`) or black by EVY/16
/// (GBATEK "BLDY": `I = I1 +/- ... * EVY`).
fn fade555(color: u16, evy: u32, up: bool) -> u16 {
    let channels = [0, 5, 10];
    let mut out = 0;
    for &shift in channels.iter() {
        let i = u32::from((color >> shift) & 0x001F);
        let faded = if up {
            i + (31 - i) * evy / 16
        } else {
            i - i * evy / 16
        };
        out |= (faded as u16) << shift;
    }
    out
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
