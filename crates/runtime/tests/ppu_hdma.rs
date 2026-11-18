//! An HBlank-DMA (HDMA) color-fade scene. DMA
//! channel 3 fires on every visible line's H-Blank (repeat on, count
//! 1, fixed destination) and copies the next entry of a RAM table into
//! BLDY, so with BLDCNT brightness-down on BG0 every scanline fades a
//! bit further toward black: the classic HDMA gradient.
//!
//! Hardware order (GBATEK, mGBA): a line is drawn when its H-Blank
//! starts, so the transfer at line n's H-Blank shows on line n+1, and
//! H-Blank DMA does not run during V-Blank. Each frame therefore takes
//! exactly 160 entries. The source is NOT re-armed on repeat (GBATEK
//! reload rules cover count and destination only), so the table holds
//! a 160-periodic sweep and BLDY starts at the entry before it, which
//! makes both frames identical.

use runtime::bus::Bus;
use runtime::ppu::{CYCLES_PER_LINE, Ppu, VISIBLE_LINES, WIDTH};
use testutil::golden;

const VRAM: u32 = 0x0600_0000;
const DISPCNT: u32 = 0x0400_0000;
const TABLE: u32 = 0x0200_0000;

/// BLDY sweep entry `i`: 0 at the top of the frame rising to 15 at the
/// bottom, repeating every 160 entries (one frame of transfers).
fn sweep(i: u32) -> u16 {
    ((i % 160) * 16 / 160) as u16
}

fn set_up(bus: &mut Bus) {
    // BG0: 4bpp tile 1 (all indices 1..3), full-screen map, palette
    // entry 1 white.
    for row in 0..8u32 {
        bus.write_u32(VRAM + 32 + row * 4, 0x1111_1111);
    }
    for ty in 0..32u32 {
        for tx in 0..32u32 {
            bus.write_u16(VRAM + 0x800 + (ty * 32 + tx) * 2, 1);
        }
    }
    bus.write_u16(0x0500_0002, 0x7FFF); // palette entry 1 = white
    bus.write_u16(0x0400_0008, 0x0101); // BG0CNT: 4bpp CBB0 SBB1
    bus.write_u16(0x0400_0050, 0x00C1); // BLDCNT: BG0, brightness down
    bus.write_u16(0x0400_0054, sweep(159)); // BLDY for line 0
    bus.write_u16(DISPCNT, 0x0100); // mode 0, BG0

    // DMA3: table -> BLDY, halfword, count 1, repeat, HBlank timing.
    bus.write_u32(0x0400_00D4, TABLE); // 3SAD
    bus.write_u32(0x0400_00D8, 0x0400_0054); // 3DAD
    bus.write_u16(0x0400_00DC, 1); // 3CNT_L: one unit per trigger
    bus.write_u16(0x0400_00DE, 0x8212); // enable, HBlank, repeat, dst fixed
}

fn run_two_frames(bus: &mut Bus) -> (Vec<u8>, Vec<u8>) {
    let mut ppu = Ppu::new();
    let per_frame = CYCLES_PER_LINE * 228;
    ppu.step(bus, per_frame);
    let first = ppu.frame().to_vec();
    ppu.step(bus, per_frame);
    let second = ppu.frame().to_vec();
    (first, second)
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
fn hdma_fade_scene_renders_two_identical_gradient_frames() {
    let mut bus = Bus::new(Vec::new());
    for i in 0..320u32 {
        bus.write_u16(TABLE + 2 * i, sweep(i));
    }
    set_up(&mut bus);
    let (first, second) = run_two_frames(&mut bus);

    let mut out = String::new();
    out += "A: HDMA gradient frame 1 (BLDY sweeps 0..15 down the screen)\n";
    out += &grid(&first);
    out += "B: HDMA gradient frame 2 (identical: table repeats the sweep)\n";
    out += &grid(&second);
    golden::assert_matches(golden_path("hdma-fade.txt"), &out);

    // The repeat channel stays armed across frames, and V-Blank lines
    // take no entries: a self-disabled channel would leave frame 2
    // flat, and 228 transfers per frame would shift it by 68 entries.
    assert_eq!(first, second, "both frames show the same gradient");

    // Line 0 shows the BLDY set before the frame; line 1 shows the
    // first table entry, written during line 0's H-Blank.
    let pixel = |frame: &[u8], y: usize| frame[y * WIDTH * 4];
    assert_eq!(pixel(&first, 0), pixel(&first, 159), "line 0 = sweep(159)");
    assert!(pixel(&first, 1) > pixel(&first, 159), "line 1 = sweep(0)");
}
