//! Golden-framebuffer tests for the mode-4 and
//! mode-5 page-flipped bitmaps and the BG/OBJ mosaic effects.
//!
//! Each test renders full frames on a bare bus plus PPU with small
//! deterministic VRAM/palette setups, then asserts a text golden file
//! (`tests/golden/`): a 30x20 grid of hex RGB samples taken every 8th
//! pixel of the 240x160 framebuffer, one labeled section per subframe.
//! Regenerate with `OXYBOY_UPDATE_GOLDEN=1 cargo test`. Register
//! addresses and bit layouts follow GBATEK "DISPCNT" (BG modes 4-5,
//! page-flip bit 4, MOSAIC) and "BGxCNT"/"OBJ Attribute 0" (mosaic
//! enables).

use runtime::bus::Bus;
use runtime::ppu::{CYCLES_PER_LINE, Ppu, VISIBLE_LINES, WIDTH};
use testutil::golden;

const VRAM: u32 = 0x0600_0000;
const DISPCNT: u32 = 0x0400_0000;
const MOSAIC: u32 = 0x0400_004C;
const OAM: u32 = 0x0700_0000;

/// Palette entry `i` as a spread-bit BGR555 color, distinct per index
/// (0 stays black, the backdrop).
fn color(i: u16) -> u16 {
    i.wrapping_mul(0x0421) & 0x7FFF
}

fn set_full_palette(bus: &mut Bus) {
    for i in 0..512u16 {
        bus.write_u16(0x0500_0000 + 2 * u32::from(i), color(i));
    }
}

/// Writes a 240x160 byte plane two bytes at a time (VRAM byte writes
/// fill the whole containing cell).
fn write_bitmap4(bus: &mut Bus, base: u32, byte: impl Fn(u32, u32) -> u8) {
    for y in 0..160u32 {
        for x in (0..240u32).step_by(2) {
            let lo = u16::from(byte(x, y));
            let hi = u16::from(byte(x + 1, y));
            bus.write_u16(VRAM + base + y * 240 + x, lo | hi << 8);
        }
    }
}

/// A 4bpp OBJ tile with pixel indices `base..base+3` across each row;
/// `base` stays at most 13 so every index fits a 4-bit nibble.
fn obj_tile4(bus: &mut Bus, n: u32, base: u8) {
    for row in 0..8u32 {
        let mut word = 0u32;
        for col in 0..8u32 {
            let index = base + ((row * 8 + col) % 3) as u8;
            word |= u32::from(index) << (4 * col);
        }
        bus.write_u32(VRAM + 0x1_0000 + n * 32 + row * 4, word);
    }
}

/// Programs OAM entry `i`'s three attributes.
fn oam_obj(bus: &mut Bus, i: u32, a0: u16, a1: u16, a2: u16) {
    bus.write_u16(OAM + i * 8, a0);
    bus.write_u16(OAM + i * 8 + 2, a1);
    bus.write_u16(OAM + i * 8 + 4, a2);
}

/// Fills a text tilemap with tile 1 whose palette bank stripes every
/// four tiles; even rows are h-flipped so flips are visible.
fn banked_map(bus: &mut Bus, sbb: u32) {
    for ty in 0..32u32 {
        for tx in 0..32u32 {
            let mut entry = (((tx / 4) as u16 & 3) << 12) | 1;
            if ty % 2 == 0 {
                entry |= 0x0400;
            }
            bus.write_u16(VRAM + sbb * 0x800 + (ty * 32 + tx) * 2, entry);
        }
    }
}

/// A 4bpp BG tile with pixel indices `base..base+3` across each row.
fn tile4(bus: &mut Bus, cb: u32, n: u32, base: u8) {
    for row in 0..8u32 {
        let mut word = 0u32;
        for col in 0..8u32 {
            let index = base + ((row * 8 + col) % 3) as u8;
            word |= u32::from(index) << (4 * col);
        }
        bus.write_u32(VRAM + cb * 0x4000 + n * 32 + row * 4, word);
    }
}

/// Renders `frames` full visible frames and returns the framebuffer.
fn run(bus: &mut Bus, frames: usize) -> Vec<u8> {
    let mut ppu = Ppu::new();
    for _ in 0..frames {
        ppu.step(bus, CYCLES_PER_LINE * VISIBLE_LINES as u32);
    }
    ppu.frame().to_vec()
}

/// Samples a framebuffer into the golden text grid.
fn grid(frame: &[u8]) -> String {
    let mut out = String::new();
    for y in (0..VISIBLE_LINES).step_by(8) {
        for x in (0..WIDTH).step_by(8) {
            let i = (y * WIDTH + x) * 4;
            out += &format!("{:02x}{:02x}{:02x} ", frame[i], frame[i + 1], frame[i + 2]);
        }
        out.push('\n');
    }
    out
}

fn golden_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name)
}

#[test]
fn mode4_page_flip_picks_the_bitmap_base() {
    let mut bus = Bus::new(Vec::new());
    set_full_palette(&mut bus);
    // Page 0: block numbers increase along x and y; page 1 adds 128 so
    // every sample cell differs between pages.
    write_bitmap4(&mut bus, 0, |x, y| ((x / 8 + y / 8) % 256) as u8);
    write_bitmap4(&mut bus, 0xA000, |x, y| ((x / 8 + y / 8 + 128) % 256) as u8);
    bus.write_u16(DISPCNT, 0x0404); // mode 4, BG2, page 0

    let mut out = String::new();
    out += "A: mode 4 page 0 (DISPCNT bit 4 clear)\n";
    out += &grid(&run(&mut bus, 1));
    bus.write_u16(DISPCNT, 0x0414); // page 1
    out += "B: mode 4 page 1 (DISPCNT bit 4 set)\n";
    out += &grid(&run(&mut bus, 1));
    golden::assert_matches(golden_path("mode4-pageflip.txt"), &out);
}

#[test]
fn mode5_bitmap_clips_to_160x128() {
    let mut bus = Bus::new(Vec::new());
    set_full_palette(&mut bus);
    // Distinct per-pixel 555 colors inside the 160x128 bitmap.
    for y in 0..128u32 {
        for x in (0..160u32).step_by(2) {
            let a = color(((x + y) % 32 + 1) as u16);
            let b = color(((x + 1 + y) % 32 + 1) as u16);
            bus.write_u32(VRAM + 2 * (y * 160 + x), u32::from(a) | u32::from(b) << 16);
        }
    }
    bus.write_u16(DISPCNT, 0x0405); // mode 5, BG2

    let frame = run(&mut bus, 1);
    golden::assert_matches(golden_path("mode5-clip.txt"), &grid(&frame));
}

#[test]
fn mosaic_quantizes_bg_and_obj_sampling() {
    // A: BG mosaic: a full-screen text BG sampled at 4x6 screen blocks.
    let mut bus = Bus::new(Vec::new());
    set_full_palette(&mut bus);
    tile4(&mut bus, 0, 1, 1); // BG0 pattern, bank 0
    banked_map(&mut bus, 1);
    bus.write_u16(0x0400_0008, 0x0140); // BG0CNT: mosaic on, SBB1
    bus.write_u16(MOSAIC, 3 | 5 << 4); // BG 4-wide, 6-tall blocks
    bus.write_u16(DISPCNT, 0x0100); // mode 0, BG0

    let mut out = String::new();
    out += "A: BG mosaic 4x6 (BGxCNT bit 6)\n";
    out += &grid(&run(&mut bus, 1));

    // B: OBJ mosaic: a 32x32 sprite quantized into 3x4 blocks anchored
    // at the sprite origin.
    let mut bus = Bus::new(Vec::new());
    set_full_palette(&mut bus);
    for n in 4..20u32 {
        obj_tile4(&mut bus, n, (n % 13) as u8 + 1);
    }
    let a0: u16 = 32 | 1 << 12; // y=32, mosaic on, mode 0, shape 0
    let a1: u16 = 32 | 2 << 14; // x=32, size 2 (32x32)
    let a2: u16 = 4 | 5 << 12; // tile 4, prio 0, bank 5
    oam_obj(&mut bus, 0, a0, a1, a2);
    bus.write_u16(MOSAIC, (2 << 8) | (3 << 12)); // OBJ 3-wide, 4-tall blocks
    bus.write_u16(DISPCNT, 0x1040); // mode 0, OBJ, 1D mapping
    out += "B: OBJ mosaic 3x4 (attr0 bit 12)\n";
    out += &grid(&run(&mut bus, 1));
    golden::assert_matches(golden_path("mosaic.txt"), &out);
}
