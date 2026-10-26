//! Hline-granularity timing assertions. The PPU
//! clock is deterministic, so "within documented tolerance" here means
//! exact: the H-Blank flag rises at cycle 1006 of every line (GBATEK
//! DISPSTAT: the flag is low for 1006 cycles, high for the remaining
//! 226 of the 1232-cycle line), VCOUNT/V-Blank flags update at line
//! boundaries, and each rising flag edge raises its IF bit once when
//! the matching DISPSTAT enable is set (GBATEK "LCD I/O Registers",
//! "DISPSTAT", "Interrupt Control").

use runtime::bus::Bus;
use runtime::ppu::{CYCLES_PER_LINE, Ppu};

const DISPCNT: u32 = 0x0400_0000;
const DISPSTAT: u32 = 0x0400_0004;
const IF: u32 = 0x0400_0202;

#[test]
fn hblank_flag_rises_at_cycle_1006_and_falls_at_line_end() {
    let mut bus = Bus::new(Vec::new());
    let mut ppu = Ppu::new();
    ppu.step(&mut bus, 1005);
    assert_eq!(bus.dispstat() & 0x0002, 0, "flag low through cycle 1005");
    ppu.step(&mut bus, 1);
    assert_ne!(bus.dispstat() & 0x0002, 0, "flag high at cycle 1006");
    ppu.step(&mut bus, CYCLES_PER_LINE - 1006);
    assert_eq!(bus.dispstat() & 0x0002, 0, "flag falls at the line end");
    assert_eq!(bus.vcount(), 1);
}

#[test]
fn hblank_irq_only_when_dispstat_enable_set() {
    let mut bus = Bus::new(Vec::new());
    let mut ppu = Ppu::new();
    ppu.step(&mut bus, 1006);
    assert_eq!(bus.int_flags() & 0x0002, 0, "no IRQ without enable");

    let mut bus = Bus::new(Vec::new());
    bus.write_u16(DISPSTAT, 0x0010); // H-Blank IRQ enable
    let mut ppu = Ppu::new();
    ppu.step(&mut bus, 1006);
    assert_ne!(bus.int_flags() & 0x0002, 0, "IF bit 1 raised");
    // Stays asserted until acknowledged; the next line's edge does not
    // double it.
    ppu.step(&mut bus, CYCLES_PER_LINE - 1006 + 1006);
    assert_eq!(bus.int_flags() & 0x0002, 0x0002);
}

#[test]
fn vblank_flag_and_irq_span_lines_160_to_226() {
    let mut bus = Bus::new(Vec::new());
    bus.write_u16(DISPSTAT, 0x0008); // V-Blank IRQ enable
    let mut ppu = Ppu::new();
    let mut edges = 0;
    for next_line in 1..=228u32 {
        ppu.step(&mut bus, CYCLES_PER_LINE);
        let line = (next_line % 228) as u16;
        assert_eq!(bus.vcount(), line);
        let expected = (160..=226).contains(&(line as u32));
        assert_eq!(
            bus.dispstat() & 0x0001 != 0,
            expected,
            "line {line}: V-Blank flag mismatch"
        );
        if next_line == 160 {
            assert_ne!(bus.int_flags() & 0x0001, 0, "IF bit 0 raised");
            edges += 1;
        }
    }
    assert_eq!(edges, 1, "one V-Blank IRQ edge per frame");
}

#[test]
fn vcount_flag_and_irq_fire_at_the_lyc_line() {
    let mut bus = Bus::new(Vec::new());
    bus.write_u16(DISPSTAT, (10 << 8) | 0x0020); // LYC=10, IRQ enable
    let mut ppu = Ppu::new();
    let mut irq_lines = Vec::new();
    for next_line in 1..=228u32 {
        ppu.step(&mut bus, CYCLES_PER_LINE);
        let line = (next_line % 228) as u16;
        assert_eq!(
            bus.dispstat() & 0x0004 != 0,
            line == 10,
            "line {line}: VCOUNT flag mismatch"
        );
        if bus.int_flags() & 0x0004 != 0 {
            irq_lines.push(line);
            bus.write_u16(IF, 0xFFFF); // acknowledge, so later lines re-arm
        }
    }
    assert_eq!(irq_lines, vec![10], "exactly one VCOUNT IRQ at line 10");

    // A LYC above 227 never matches (GBATEK: comparisons use the full
    // 8-bit counter against values 0..=227).
    let mut bus = Bus::new(Vec::new());
    bus.write_u16(DISPSTAT, (0xFF << 8) | 0x0020);
    let mut ppu = Ppu::new();
    for _ in 0..LINES_FRAME {
        ppu.step(&mut bus, CYCLES_PER_LINE);
        assert_eq!(bus.dispstat() & 0x0004, 0);
    }
}

#[test]
fn dispcnt_forced_blank_paints_white_immediately() {
    let mut bus = Bus::new(Vec::new());
    let mut ppu = Ppu::new();
    bus.write_u16(DISPCNT, 0x0080); // forced blank
    ppu.step(&mut bus, CYCLES_PER_LINE);
    let frame = ppu.frame();
    assert_eq!(&frame[0..4], &[0xFF, 0xFF, 0xFF, 0xFF]);
}

const LINES_FRAME: u32 = 228;
