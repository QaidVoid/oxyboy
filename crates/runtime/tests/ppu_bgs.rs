//! Golden-framebuffer tests for text and affine
//! background modes, and BG priority ordering.
//!
//! Each test renders full frames on a bare bus plus PPU with small
//! deterministic VRAM/palette setups, then asserts a text golden file
//! (`tests/golden/`): a 30x20 grid of hex RGB samples taken every 8th
//! pixel of the 240x160 framebuffer. Regenerate with
//! `OXYBOY_UPDATE_GOLDEN=1 cargo test`. Register addresses and bit
//! layouts follow GBATEK "LCD I/O Registers" (BGxCNT, BGxOFS, the BG2
//! and BG3 affine parameters).

use runtime::bus::Bus;
use runtime::ppu::{CYCLES_PER_LINE, Ppu, VISIBLE_LINES, WIDTH};
use testutil::golden;

const VRAM: u32 = 0x0600_0000;
const DISPCNT: u32 = 0x0400_0000;
const PALETTE: u32 = 0x0500_0000;

/// Palette entry `i` as a spread-bit BGR555 color, distinct per index
/// (0 stays black, the backdrop).
fn color(i: u16) -> u16 {
    i.wrapping_mul(0x0421) & 0x7FFF
}

fn set_palette(bus: &mut Bus, n: u16) {
    for i in 0..n {
        bus.write_u16(PALETTE + 2 * u32::from(i), color(i));
    }
}

/// A 4bpp tile with pixel indices `base..base+3` across each row.
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

/// An 8bpp tile with pixel indices `base..base+4` across each row.
fn tile8(bus: &mut Bus, cb: u32, n: u32, base: u8) {
    for row in 0..8u32 {
        for half in 0..2u32 {
            let mut word = 0u32;
            for col in 0..4u32 {
                let index = base + ((row * 8 + half * 4 + col) % 5) as u8;
                word |= u32::from(index) << (8 * col);
            }
            bus.write_u32(VRAM + cb * 0x4000 + n * 64 + row * 8 + half * 4, word);
        }
    }
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

/// Fills a text tilemap with tile 1 in one palette bank, transparent
/// tile 0 on the right half.
fn half_map(bus: &mut Bus, sbb: u32, bank: u16) {
    for ty in 0..32u32 {
        for tx in 0..32u32 {
            let entry = if tx < 16 { (bank << 12) | 1 } else { 0 };
            bus.write_u16(VRAM + sbb * 0x800 + (ty * 32 + tx) * 2, entry);
        }
    }
}

/// Fills a 64x64 affine u8 tilemap with a tile-1/tile-0 checker.
/// Written as halfword pairs because a VRAM byte write fills the whole
/// containing cell (GBATEK), so byte-at-a-time setup cannot express a
/// mixed map.
fn affine_map(bus: &mut Bus, sbb: u32, tile: u8) {
    for ty in 0..64u32 {
        for tx in (0..64u32).step_by(2) {
            let lo = if (tx ^ ty) & 1 == 0 { tile } else { 0 };
            let hi = if ((tx + 1) ^ ty) & 1 == 0 { tile } else { 0 };
            bus.write_u16(
                VRAM + sbb * 0x800 + ty * 64 + tx,
                u16::from(lo) | (u16::from(hi) << 8),
            );
        }
    }
}

/// Writes one affine BG's 16.8 transform: scale (PA/PD) and rotation
/// cross terms (PB/PC) around the reference point (0, 0).
fn affine_params(bus: &mut Bus, n: u32, scale: i16, cross: i16, off_x: i32) {
    let base = 0x0400_0020 + 0x10 * n;
    bus.write_u16(base, scale as u16); // PA
    bus.write_u16(base + 2, cross as u16); // PB
    bus.write_u16(base + 4, cross.wrapping_neg() as u16); // PC = -PB
    bus.write_u16(base + 6, scale as u16); // PD
    bus.write_u32(base + 8, off_x as u32); // X reference
    bus.write_u32(base + 12, 0); // Y reference
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
fn mode0_text_tiles_scroll_and_palette_banks() {
    let mut bus = Bus::new(Vec::new());
    set_palette(&mut bus, 256);
    tile4(&mut bus, 0, 1, 1);
    tile8(&mut bus, 1, 1, 100);
    tile8(&mut bus, 1, 2, 120);
    banked_map(&mut bus, 1);
    // BG1's map alternates tile 1/tile 2 on even map rows and is
    // transparent on odd rows, so BG0's scrolled bank stripes show
    // through on the stride-8 grid rows y=8, 24, 40, ...
    for ty in 0..32u32 {
        for tx in 0..32u32 {
            let tile = if tx % 2 == 0 { 1 } else { 2 };
            let entry = if ty % 2 == 0 { tile } else { 0 };
            bus.write_u16(VRAM + 2 * 0x800 + (ty * 32 + tx) * 2, entry);
        }
    }
    // BG0: 4bpp, CBB0, SBB1, priority 1, scrolled by (16, 8).
    bus.write_u16(0x0400_0008, 0x0101);
    bus.write_u16(0x0400_0010, 16);
    bus.write_u16(0x0400_0012, 8);
    // BG1: 8bpp, CBB1, SBB2, priority 0 (in front).
    bus.write_u16(0x0400_000A, 0x0284);
    bus.write_u16(DISPCNT, 0x0300); // mode 0, BG0 + BG1

    let frame = run(&mut bus, 1);
    golden::assert_matches(golden_path("mode0-text.txt"), &grid(&frame));
}

#[test]
fn mode1_text_layer_under_a_rotating_affine_layer() {
    let mut bus = Bus::new(Vec::new());
    set_palette(&mut bus, 256);
    tile4(&mut bus, 0, 1, 1);
    tile8(&mut bus, 2, 1, 40);
    banked_map(&mut bus, 1);
    affine_map(&mut bus, 2, 1);
    // BG0 text, priority 1 (behind); BG2 affine identity, priority 0.
    bus.write_u16(0x0400_0008, 0x0101);
    bus.write_u16(0x0400_000C, 0x0288); // BG2CNT: CBB2 SBB2 8bpp size0
    bus.write_u16(DISPCNT, 0x0501); // mode 1, BG0 + BG2
    affine_params(&mut bus, 0, 0x0100, 0, 0);

    let mut out = String::from("A: identity affine\n");
    out += &grid(&run(&mut bus, 1));
    affine_params(&mut bus, 0, 0x00B5, 0x00B5, 0); // rotate ~45 degrees
    out += "B: 45-degree rotation, no wrap (transparent corners)\n";
    out += &grid(&run(&mut bus, 1));
    golden::assert_matches(golden_path("mode1-affine.txt"), &out);
}

#[test]
fn mode2_affine_wrap_and_transparent_edges() {
    let mut bus = Bus::new(Vec::new());
    set_palette(&mut bus, 256);
    tile8(&mut bus, 2, 1, 60);
    tile8(&mut bus, 3, 1, 90);
    affine_map(&mut bus, 2, 1);
    // BG3's map: all tile 1 (halfword pairs; see affine_map).
    for ty in 0..64u32 {
        for tx in (0..64u32).step_by(2) {
            bus.write_u16(VRAM + 3 * 0x800 + ty * 64 + tx, 0x0101);
        }
    }
    // BG2: 512x512 texture, wraparound on, priority 1, scaled 2x so
    // the texture repeats across the screen.
    bus.write_u16(0x0400_000C, 0x6209);
    affine_params(&mut bus, 0, 0x0200, 0, 0);
    // BG3: 512x512 texture, no wrap, priority 0 (front), scaled 3x so
    // everything past x>=86 falls outside and shows the layer behind.
    bus.write_u16(0x0400_000E, 0x430C);
    affine_params(&mut bus, 1, 0x0300, 0, 0);
    bus.write_u16(DISPCNT, 0x0C02); // mode 2, BG2 + BG3

    let frame = run(&mut bus, 1);
    golden::assert_matches(golden_path("mode2-wrap.txt"), &grid(&frame));
}

#[test]
fn bg_priority_orders_overlapping_layers() {
    let mut bus = Bus::new(Vec::new());
    set_palette(&mut bus, 256);
    tile4(&mut bus, 0, 1, 1); // BG0 pattern, palette bank 0
    tile4(&mut bus, 1, 1, 4); // BG1 pattern, palette bank 1
    // BG0's map lives at SBB2: SBB0 would overlap CBB0 tile data in
    // VRAM (map 2KB at 0x0000 vs tile 1 at 0x0020).
    banked_map(&mut bus, 2);
    half_map(&mut bus, 1, 1); // BG1 covers only the left half
    bus.write_u16(DISPCNT, 0x0300); // mode 0, BG0 + BG1

    let mut out = String::new();
    // A: BG0 priority 0, BG1 priority 1. BG0 wins everywhere.
    bus.write_u16(0x0400_0008, 0x0200);
    bus.write_u16(0x0400_000A, 0x0105);
    out += "A: BG0 in front (BG1 invisible under the full-screen layer)\n";
    out += &grid(&run(&mut bus, 1));
    // B: priorities swapped. BG1 (left half only) covers BG0 there.
    bus.write_u16(0x0400_0008, 0x0201);
    bus.write_u16(0x0400_000A, 0x0104);
    out += "B: BG1 in front on its left half, BG0 on the right\n";
    out += &grid(&run(&mut bus, 1));
    // C: equal priorities. The lower BG number (BG0) wins.
    bus.write_u16(0x0400_0008, 0x0200);
    bus.write_u16(0x0400_000A, 0x0104);
    out += "C: tie goes to BG0\n";
    out += &grid(&run(&mut bus, 1));
    golden::assert_matches(golden_path("bg-priority.txt"), &out);
}
