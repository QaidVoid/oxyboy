//! Golden-framebuffer tests for OBJ rendering
//! (regular, affine, 1D/2D mapping), OBJ-vs-BG and OBJ-vs-OAM
//! priority, windows (WIN0/WIN1/OBJ window/outside), and the BLDCNT
//! color effects including semi-transparent OBJs.
//!
//! Each test renders full frames on a bare bus plus PPU with small
//! deterministic OAM/VRAM/palette setups, then asserts a text golden
//! file (`tests/golden/`): a 30x20 grid of hex RGB samples taken every
//! 8th pixel of the 240x160 framebuffer, one labeled section per
//! subframe. Regenerate with `OXYBOY_UPDATE_GOLDEN=1 cargo test`.
//! Register addresses and bit layouts follow GBATEK "LCD I/O
//! Registers" and "OAM - OBJ Attributes".

use runtime::bus::Bus;
use runtime::ppu::{CYCLES_PER_LINE, Ppu, VISIBLE_LINES, WIDTH};
use testutil::golden;

const VRAM: u32 = 0x0600_0000;
const OBJ_VRAM: u32 = VRAM + 0x1_0000;
const DISPCNT: u32 = 0x0400_0000;
const OAM: u32 = 0x0700_0000;

/// Palette entry `i` as a spread-bit BGR555 color, distinct per index
/// (0 stays black, the backdrop).
fn color(i: u16) -> u16 {
    i.wrapping_mul(0x0421) & 0x7FFF
}

fn set_palette(bus: &mut Bus, n: u16) {
    for i in 0..n {
        bus.write_u16(0x0500_0000 + 2 * u32::from(i), color(i));
    }
}

/// Fills both palette halves (BG and OBJ, entries 0..511).
fn set_full_palette(bus: &mut Bus) {
    set_palette(bus, 512);
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

/// A 4bpp OBJ tile with pixel indices `base..base+3` across each row;
/// `base` must stay at most 13 so every index fits a 4-bit nibble.
fn obj_tile4(bus: &mut Bus, n: u32, base: u8) {
    for row in 0..8u32 {
        let mut word = 0u32;
        for col in 0..8u32 {
            let index = base + ((row * 8 + col) % 3) as u8;
            word |= u32::from(index) << (4 * col);
        }
        bus.write_u32(OBJ_VRAM + n * 32 + row * 4, word);
    }
}

/// A 4bpp OBJ tile filled with one solid color index.
fn obj_tile4_solid(bus: &mut Bus, n: u32, index: u8) {
    let word = u32::from(index) * 0x1111_1111;
    for row in 0..8u32 {
        bus.write_u32(OBJ_VRAM + n * 32 + row * 4, word);
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

/// Fills a text tilemap with opaque tile 1 on the left half, tile 0 on
/// the right half.
fn left_map(bus: &mut Bus, sbb: u32, bank: u16) {
    for ty in 0..32u32 {
        for tx in 0..32u32 {
            let entry = if tx < 16 { (bank << 12) | 1 } else { 0 };
            bus.write_u16(VRAM + sbb * 0x800 + (ty * 32 + tx) * 2, entry);
        }
    }
}

/// Programs OAM entry `i`'s three attributes.
fn oam_obj(bus: &mut Bus, i: u32, a0: u16, a1: u16, a2: u16) {
    bus.write_u16(OAM + i * 8, a0);
    bus.write_u16(OAM + i * 8 + 2, a1);
    bus.write_u16(OAM + i * 8 + 4, a2);
}

/// Programs OBJ rot/scale parameter group `sel` (PA/PB/PC/PD), which
/// lives in the OAM gaps at 07000006 + sel*32 + idx*8.
fn oam_params(bus: &mut Bus, sel: u32, pa: i16, pb: i16, pc: i16, pd: i16) {
    let base = OAM + 6 + sel * 32;
    bus.write_u16(base, pa as u16);
    bus.write_u16(base + 8, pb as u16);
    bus.write_u16(base + 16, pc as u16);
    bus.write_u16(base + 24, pd as u16);
}

/// OAM attribute packers matching GBATEK "OBJ Attribute 0/1/2".
fn attr0(y: u16, rot: bool, double: bool, mode: u16, shape: u16, bpp8: bool) -> u16 {
    y | u16::from(rot) << 8
        | u16::from(double) << 9
        | mode << 10
        | u16::from(bpp8) << 13
        | shape << 14
}

fn attr1(x: u16, flip_x: bool, flip_y: bool, sel: u16, size: u16) -> u16 {
    x | u16::from(flip_x) << 12 | u16::from(flip_y) << 13 | sel << 9 | size << 14
}

fn attr2(tile: u16, prio: u16, bank: u16) -> u16 {
    tile | prio << 10 | bank << 12
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
fn regular_sprites_priority_oam_order_and_flips() {
    let mut bus = Bus::new(Vec::new());
    set_full_palette(&mut bus);
    tile4(&mut bus, 0, 1, 1); // BG0 pattern, bank 0, CBB0
    left_map(&mut bus, 1, 1); // BG0 map at SBB1: opaque left half only
    // Sprite S0 (OAM 0): 32x16, tiles 10..17 (1D), bank 5, h+v flip.
    // Per-tile bases keep every tile's color distinct within the
    // 4-bit nibble range.
    for n in 10..18u32 {
        obj_tile4(&mut bus, n, (n % 13) as u8 + 1);
    }
    // Sprite S1 (OAM 1): 16x16, tiles 20..23 (1D), bank 6.
    for n in 20..24u32 {
        obj_tile4(&mut bus, n, (n % 13) as u8 + 1);
    }
    let s0 = |prio| {
        (
            attr0(32, false, false, 0, 1, false),
            attr1(32, true, true, 0, 2),
            attr2(10, prio, 5),
        )
    };
    let s1 = |prio| {
        (
            attr0(40, false, false, 0, 0, false),
            attr1(40, false, false, 0, 1),
            attr2(20, prio, 6),
        )
    };
    bus.write_u16(0x0400_0008, 0x0101); // BG0CNT prio 1, SBB1, CBB0
    bus.write_u16(DISPCNT, 0x1140); // mode 0, BG0 + OBJ, 1D mapping

    let mut out = String::new();
    // A: OBJ prio 0 over BG0 prio 1: both sprites fully visible.
    let (a0, a1x, a2) = s0(0);
    oam_obj(&mut bus, 0, a0, a1x, a2);
    let (b0, b1, b2) = s1(0);
    oam_obj(&mut bus, 1, b0, b1, b2);
    out += "A: sprites over the BG (OBJ prio 0, BG prio 1)\n";
    out += &grid(&run(&mut bus, 1));
    // B: BG0 prio 0 covers its opaque left half; sprites survive only
    // on the transparent right half (x >= 120).
    let (a0, a1x, a2) = s0(1);
    oam_obj(&mut bus, 0, a0, a1x, a2);
    let (b0, b1, b2) = s1(1);
    oam_obj(&mut bus, 1, b0, b1, b2);
    bus.write_u16(0x0400_0008, 0x0100);
    out += "B: BG in front on its opaque left half (both prio 1)\n";
    out += &grid(&run(&mut bus, 1));
    // C: OBJ prio 1 vs BG prio 1: OBJ wins at equal priority, and OAM
    // entry 0 (S0) beats entry 1 (S1) where they overlap.
    let (a0, a1x, a2) = s0(1);
    oam_obj(&mut bus, 0, a0, a1x, a2);
    bus.write_u16(0x0400_0008, 0x0101);
    out += "C: equal priority goes to OBJ; overlap goes to OAM entry 0\n";
    out += &grid(&run(&mut bus, 1));
    golden::assert_matches(golden_path("obj-regular.txt"), &out);
}

#[test]
fn obj_tile_mapping_1d_vs_2d() {
    let mut bus = Bus::new(Vec::new());
    set_full_palette(&mut bus);
    // A 32x32 sprite spanning 16 tiles; each tile is a solid color
    // index derived from its tile number so the mapping determines
    // which square shows where. Tiles 32..39 exist so 2D rows 1-2
    // differ from 1D rows 1-2.
    for n in 0..40u32 {
        obj_tile4_solid(&mut bus, n, (n % 15) as u8 + 1);
    }
    oam_obj(
        &mut bus,
        0,
        attr0(32, false, false, 0, 0, false),
        attr1(32, false, false, 0, 2),
        attr2(0, 0, 5),
    );
    bus.write_u16(DISPCNT, 0x1000); // mode 0, OBJ only, 2D mapping

    let mut out = String::new();
    out += "A: 2D mapping (rows pick tiles 0-3, 32-35, ...)\n";
    out += &grid(&run(&mut bus, 1));
    bus.write_u16(DISPCNT, 0x1040); // OBJ 1D mapping bit set
    out += "B: 1D mapping (rows pick tiles 0-3, 4-7, 8-11, 12-15)\n";
    out += &grid(&run(&mut bus, 1));
    golden::assert_matches(golden_path("obj-mapping.txt"), &out);
}

#[test]
fn affine_sprite_rotation_and_double_size() {
    let mut bus = Bus::new(Vec::new());
    set_full_palette(&mut bus);
    // 32x32 affine sprite (shape 0, size 2), tiles 4..19 (1D),
    // distinct per tile.
    for n in 4..20u32 {
        obj_tile4(&mut bus, n, (n % 13) as u8 + 1);
    }
    bus.write_u16(DISPCNT, 0x1040); // mode 0, OBJ only, 1D mapping

    let mut out = String::new();
    // A: identity transform.
    oam_obj(
        &mut bus,
        0,
        attr0(32, true, false, 0, 0, false),
        attr1(64, false, false, 0, 2),
        attr2(4, 0, 7),
    );
    oam_params(&mut bus, 0, 0x0100, 0, 0, 0x0100);
    out += "A: identity affine equals a regular sprite\n";
    out += &grid(&run(&mut bus, 1));
    // B: 90-degree rotation (PA=0, PB=+1.0, PC=-1.0, PD=0).
    oam_params(&mut bus, 0, 0, 0x0100, -0x0100, 0);
    out += "B: 90-degree rotation, display box stays 32x32\n";
    out += &grid(&run(&mut bus, 1));
    // C: same rotation with the double-size bit: the display box
    // doubles to 64x64 and the texture is scaled up 2x.
    oam_obj(
        &mut bus,
        0,
        attr0(32, true, true, 0, 0, false),
        attr1(64, false, false, 0, 2),
        attr2(4, 0, 7),
    );
    out += "C: double-size doubles the display box, texture scales 2x\n";
    out += &grid(&run(&mut bus, 1));
    golden::assert_matches(golden_path("obj-affine.txt"), &out);
}

#[test]
fn windows_gate_layers_and_effects() {
    let mut bus = Bus::new(Vec::new());
    set_full_palette(&mut bus);
    tile4(&mut bus, 0, 1, 1); // BG0 pattern, bank 0, CBB0
    tile4(&mut bus, 1, 1, 4); // BG1 pattern, bank 1, CBB1
    banked_map(&mut bus, 1); // BG0: opaque everywhere, SBB1
    left_map(&mut bus, 3, 3); // BG1: opaque left half only, SBB3
    for n in 4..8u32 {
        obj_tile4(&mut bus, n, (n % 13) as u8 + 1);
    }
    // OAM 0: a visible 8x8 sprite; OAM 1: a 32x32 OBJ-window sprite.
    oam_obj(
        &mut bus,
        0,
        attr0(48, false, false, 0, 0, false),
        attr1(96, false, false, 0, 0),
        attr2(4, 1, 8),
    );
    oam_obj(
        &mut bus,
        1,
        attr0(96, false, false, 2, 0, false),
        attr1(56, false, false, 0, 3),
        attr2(4, 1, 0),
    );
    bus.write_u16(0x0400_0008, 0x0101); // BG0CNT prio 1, SBB1
    bus.write_u16(0x0400_000A, 0x0300); // BG1CNT prio 0, SBB3
    // Alpha blend: 1st targets BG0+BG1, 2nd targets BG1+backdrop.
    bus.write_u16(0x0400_0050, 0x2243);
    bus.write_u16(0x0400_0052, 10 | (6 << 8));

    // WIN0 rect x 40..120, y 24..88; WIN1 rect x 152..216, y 96..152.
    bus.write_u16(0x0400_0040, 40 | (120 << 8));
    bus.write_u16(0x0400_0044, 24 | (88 << 8));
    bus.write_u16(0x0400_0042, 152 | (216 << 8));
    bus.write_u16(0x0400_0046, 96 | (152 << 8));
    // WININ: WIN0 shows only BG1, WIN1 shows BG0 with effects.
    bus.write_u16(0x0400_0048, 0x2102);
    // WINOUT: outside = BG0+BG1+OBJ+effects, OBJ window = BG1+OBJ.
    bus.write_u16(0x0400_004A, 0x1233);

    let mut out = String::new();
    // A: WIN0 shows only BG1, WIN1 only BG0 with blend, outside all.
    bus.write_u16(DISPCNT, 0x7340);
    out += "A: WIN0 shows only BG1, WIN1 only BG0 blended, outside all\n";
    out += &grid(&run(&mut bus, 1));
    // B: windows 0/1 off, the OBJ-window sprite gates its region.
    bus.write_u16(DISPCNT, 0x9340);
    out += "B: OBJ-window sprite region shows only BG1+OBJ\n";
    out += &grid(&run(&mut bus, 1));
    // C: WIN0 with inverted bounds is empty; everything falls to the
    // outside mask.
    bus.write_u16(0x0400_0040, 200 | (40 << 8)); // x1=200 > x2=40
    bus.write_u16(DISPCNT, 0x7340);
    out += "C: inverted WIN0 bounds make it empty (outside mask rules)\n";
    out += &grid(&run(&mut bus, 1));
    golden::assert_matches(golden_path("obj-windows.txt"), &out);
}

#[test]
fn blend_effects_and_semitransparent_obj() {
    let mut bus = Bus::new(Vec::new());
    set_full_palette(&mut bus);
    tile4(&mut bus, 0, 1, 1); // BG0 pattern, bank 0, CBB0
    tile4(&mut bus, 1, 1, 4); // BG1 pattern, bank 1, CBB1
    banked_map(&mut bus, 1); // BG0: opaque everywhere, SBB1, prio 1
    left_map(&mut bus, 3, 3); // BG1: opaque left half only, SBB3, prio 0
    for n in 4..12u32 {
        obj_tile4(&mut bus, n, (n % 13) as u8 + 1);
    }
    // A 32x16 sprite at (96, 48), prio 1, bank 9.
    let sprite = |mode| {
        (
            attr0(48, false, false, mode, 1, false),
            attr1(96, false, false, 0, 2),
            attr2(4, 1, 9),
        )
    };
    let (a0, a1x, a2) = sprite(0);
    oam_obj(&mut bus, 0, a0, a1x, a2);
    bus.write_u16(0x0400_0008, 0x0101); // BG0CNT prio 1, SBB1
    bus.write_u16(0x0400_000A, 0x0300); // BG1CNT prio 0, SBB3
    bus.write_u16(DISPCNT, 0x1340); // mode 0, BG0+BG1+OBJ, 1D

    let mut out = String::new();
    // A: alpha blend, 1st targets BG0+OBJ, 2nd target BG1 only: on
    // BG1's opaque left half top colors blend with BG1; on the right
    // the backdrop below is not a 2nd target so colors stay plain.
    bus.write_u16(0x0400_0050, 0x0251);
    bus.write_u16(0x0400_0052, 12 | (8 << 8));
    out += "A: alpha blend, 2nd target BG1 only\n";
    out += &grid(&run(&mut bus, 1));
    // B: brightness increase by 8/16 on BG0+OBJ.
    bus.write_u16(0x0400_0050, 0x0091);
    bus.write_u16(0x0400_0054, 8);
    out += "B: brightness increase by 8/16 on BG0+OBJ\n";
    out += &grid(&run(&mut bus, 1));
    // C: brightness decrease by 8/16 on BG0+OBJ.
    bus.write_u16(0x0400_0050, 0x00D1);
    out += "C: brightness decrease by 8/16 on BG0+OBJ\n";
    out += &grid(&run(&mut bus, 1));
    // D: semi-transparent OBJ with no effect mode: it still blends
    // with the pixel behind when that pixel is a 2nd target
    // (GBATEK "Semi-Transparent OBJs").
    bus.write_u16(0x0400_0050, 0x0200); // mode 0, 2nd target BG1 only
    let (a0, a1x, a2) = sprite(1);
    oam_obj(&mut bus, 0, a0, a1x, a2);
    out += "D: semi-transparent OBJ blends over BG1, plain over backdrop\n";
    out += &grid(&run(&mut bus, 1));
    golden::assert_matches(golden_path("obj-blend.txt"), &out);
}
