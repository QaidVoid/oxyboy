//! IO basics, mode-3 rendering, and frame stepping.
//!
//! Instruction encodings in the synthetic ROM were assembled with
//! `arm-none-eabi-as` (Binutils 2.32) and extracted from the listing;
//! register addresses and flag windows follow GBATEK "LCD I/O
//! Registers".

use std::collections::HashSet;

use runtime::bus::Bus;
use runtime::cpu::Mode;
use runtime::ppu::{CYCLES_PER_LINE, LINES_PER_FRAME, Ppu, VISIBLE_LINES, WIDTH};
use runtime::system::System;

const VRAM: u32 = 0x0600_0000;
const DISPCNT: u32 = 0x0400_0000;
const GREENSWAP: u32 = 0x0400_0002;
const DISPSTAT: u32 = 0x0400_0004;
const VCOUNT: u32 = 0x0400_0006;
const IE: u32 = 0x0400_0200;
const IF: u32 = 0x0400_0202;
const IME: u32 = 0x0400_0208;

/// Pixel (x, y) as RGBA8888 from the capture buffer.
fn pixel(frame: &[u8], x: usize, y: usize) -> [u8; 4] {
    let i = (y * WIDTH + x) * 4;
    [frame[i], frame[i + 1], frame[i + 2], frame[i + 3]]
}

fn vram_pixel(bus: &mut runtime::bus::Bus, x: usize, color: u16) {
    bus.write_u16(VRAM + (x as u32) * 2, color);
}

#[test]
fn mode3_15bit_layout_expands_to_rgba() {
    let mut bus = Bus::new(vec![0; 0x100]);
    bus.write_u16(DISPCNT, 0x0403); // mode 3, BG2 enabled
    // BGR555 samples covering each channel extreme and a mid tone.
    vram_pixel(&mut bus, 1, 0x7FFF);
    vram_pixel(&mut bus, 2, 0x001F);
    vram_pixel(&mut bus, 3, 0x03E0);
    vram_pixel(&mut bus, 4, 0x7C00);
    vram_pixel(&mut bus, 5, 0x7FE0);
    vram_pixel(&mut bus, 6, 0x7C1F);
    vram_pixel(&mut bus, 7, 0x03FF);
    vram_pixel(&mut bus, 8, 0x4210);

    let mut ppu = Ppu::new();
    ppu.step(&mut bus, CYCLES_PER_LINE);

    assert_eq!(pixel(ppu.frame(), 0, 0), [0, 0, 0, 0xFF]);
    assert_eq!(pixel(ppu.frame(), 1, 0), [0xFF, 0xFF, 0xFF, 0xFF]);
    // Bit 0-4 red, 5-9 green, 10-14 blue; (c << 3) | (c >> 2) scaling.
    assert_eq!(pixel(ppu.frame(), 2, 0), [0xFF, 0x00, 0x00, 0xFF]);
    assert_eq!(pixel(ppu.frame(), 3, 0), [0x00, 0xFF, 0x00, 0xFF]);
    assert_eq!(pixel(ppu.frame(), 4, 0), [0x00, 0x00, 0xFF, 0xFF]);
    assert_eq!(pixel(ppu.frame(), 5, 0), [0x00, 0xFF, 0xFF, 0xFF]);
    assert_eq!(pixel(ppu.frame(), 6, 0), [0xFF, 0x00, 0xFF, 0xFF]);
    assert_eq!(pixel(ppu.frame(), 7, 0), [0xFF, 0xFF, 0x00, 0xFF]);
    assert_eq!(pixel(ppu.frame(), 8, 0), [0x84, 0x84, 0x84, 0xFF]);
    // Line 1 is still untouched until its own line completes.
    ppu.step(&mut bus, CYCLES_PER_LINE);
    assert_eq!(pixel(ppu.frame(), 1, 1), [0, 0, 0, 0xFF]);
}

#[test]
fn greenswap_exchanges_green_within_pixel_pairs() {
    let mut bus = Bus::new(vec![0; 0x100]);
    bus.write_u16(DISPCNT, 0x0403);
    vram_pixel(&mut bus, 0, 0x03E0); // green
    vram_pixel(&mut bus, 1, 0x7C00); // blue

    let mut ppu = Ppu::new();
    ppu.step(&mut bus, CYCLES_PER_LINE);
    assert_eq!(pixel(ppu.frame(), 0, 0), [0x00, 0xFF, 0x00, 0xFF]);
    assert_eq!(pixel(ppu.frame(), 1, 0), [0x00, 0x00, 0xFF, 0xFF]);

    bus.write_u16(GREENSWAP, 0x0001);
    let mut ppu = Ppu::new();
    ppu.step(&mut bus, CYCLES_PER_LINE);
    // The pair outputs as BgRbGr: pixel 0 takes pixel 1's green (0),
    // pixel 1 takes pixel 0's green (31).
    assert_eq!(pixel(ppu.frame(), 0, 0), [0x00, 0x00, 0x00, 0xFF]);
    assert_eq!(pixel(ppu.frame(), 1, 0), [0x00, 0xFF, 0xFF, 0xFF]);
}

#[test]
fn forced_blank_and_backdrop() {
    let mut bus = Bus::new(vec![0; 0x100]);
    vram_pixel(&mut bus, 0, 0x7FFF);

    let mut ppu = Ppu::new();
    // DISPCNT 0: mode 0, BG2 off -> backdrop (palette entry 0) = black.
    ppu.step(&mut bus, CYCLES_PER_LINE);
    assert_eq!(pixel(ppu.frame(), 0, 0), [0, 0, 0, 0xFF]);

    bus.write_u16(0x0500_0000, 0x4000); // backdrop: blue 16/31
    let mut ppu = Ppu::new();
    ppu.step(&mut bus, CYCLES_PER_LINE);
    assert_eq!(pixel(ppu.frame(), 0, 0), [0x00, 0x00, 0x84, 0xFF]);

    // Forced blank shows white regardless of the bitmap.
    bus.write_u16(DISPCNT, 0x0403 | 0x0080);
    let mut ppu = Ppu::new();
    ppu.step(&mut bus, CYCLES_PER_LINE);
    assert_eq!(pixel(ppu.frame(), 0, 0), [0xFF, 0xFF, 0xFF, 0xFF]);
}

#[test]
fn io_register_semantics() {
    let mut bus = Bus::new(vec![0; 0x100]);
    // VCOUNT is read-only.
    bus.set_vcount(5);
    bus.write_u16(VCOUNT, 0x1234);
    bus.write_u8(VCOUNT, 0x99);
    assert_eq!(bus.vcount(), 5);
    assert_eq!(bus.read_u8(VCOUNT), 5);
    assert_eq!(bus.read_u16(VCOUNT), 5);
    assert_eq!(bus.read_u32(VCOUNT), 5);

    // DISPSTAT flag bits are read-only; enables and LYC are writable.
    bus.write_u16(DISPSTAT, 0x0007 | 0x0008 | (100 << 8));
    assert_eq!(bus.dispstat(), 0x0008 | (100 << 8));
    bus.write_u8(DISPSTAT, 0x07); // byte write to the flag byte
    assert_eq!(bus.dispstat() & 0x0007, 0);
    bus.write_u8(DISPSTAT + 1, 0x40); // LYC high-ish halfword write
    assert_eq!(bus.dispstat() >> 8, 0x40);

    // IF acknowledges on written 1 bits.
    bus.raise_if(0x0003);
    assert_eq!(bus.int_flags(), 0x0003);
    bus.write_u16(IF, 0x0001);
    assert_eq!(bus.int_flags(), 0x0002);

    // The CPU IRQ line: IME and IE both gate.
    assert!(!bus.irq_pending());
    bus.write_u16(IME, 0x0001);
    assert!(!bus.irq_pending());
    bus.write_u16(IE, 0x0002);
    assert!(bus.irq_pending());
    bus.write_u16(IME, 0x0000);
    assert!(!bus.irq_pending());
}

/// The verified mode-3 writer ROM: sets DISPCNT, draws three pixels,
/// then idles (assembled listing in the module comment above).
fn mode3_rom() -> Vec<u8> {
    let halfwords: [u16; 12] = [
        0x4805, 0x4907, 0x8001, 0x4805, 0x4906, 0x8001, 0x4906, 0x8041, 0x4906, 0x8081, 0xE7FE,
        0x46C0,
    ];
    let literals: [u32; 6] = [
        0x0400_0000u32,
        0x0600_0000,
        0x0000_0403,
        0x0000_7FFF,
        0x0000_001F,
        0x0000_7C00,
    ];
    let mut rom = vec![0u8; 0x40];
    for (i, h) in halfwords.iter().enumerate() {
        rom[i * 2..][..2].copy_from_slice(&h.to_le_bytes());
    }
    for (i, w) in literals.iter().enumerate() {
        rom[0x18 + i * 4..][..4].copy_from_slice(&w.to_le_bytes());
    }
    rom
}

fn idle_rom() -> Vec<u8> {
    // b . (ARM), verified by assembling.
    let mut rom = vec![0u8; 0x20];
    rom[..4].copy_from_slice(&0xEAFF_FFFEu32.to_le_bytes());
    rom
}

fn thumb_system(rom: Vec<u8>) -> System {
    let mut system = System::new(rom, 0x0800_0000);
    // Boot straight into THUMB state (SVC, I/F masked): the ROM above
    // is THUMB code.
    system.cpu.set_cpsr_raw(0x33);
    system
}

#[test]
fn step_frame_advances_vcount_through_all_lines() {
    // ARM state on purpose: `b .` then has one stable cost per step
    // (two 16-bit ROM lanes), so the CPU-driven frame lands exactly on
    // the PPU wrap at 228 * 1536 cycles.
    let mut system = System::new(idle_rom(), 0x0800_0000);
    system.cpu.set_cpsr_raw(0x13);
    let mut seen: HashSet<u16> = HashSet::new();
    let mut max_seen = 0u16;
    let mut total_cycles = 0u64;
    let mut saw_vblank = false;
    let mut saw_hblank_set = false;
    let mut saw_hblank_clear = false;
    let mut vblank_window_held = true;

    loop {
        total_cycles += u64::from(system.step());
        let vc = system.vcount();
        seen.insert(vc);
        max_seen = max_seen.max(vc);
        let dispstat = system.dispstat();
        saw_vblank |= dispstat & 0x0001 != 0;
        saw_hblank_set |= dispstat & 0x0002 != 0;
        saw_hblank_clear |= dispstat & 0x0002 == 0;
        if (dispstat & 0x0001 != 0) != (160..=226).contains(&vc) {
            vblank_window_held = false;
        }
        assert!(total_cycles < 10 * 350_208, "frame never ended");
        if max_seen >= 227 && vc == 0 {
            break;
        }
    }

    assert_eq!(seen.len(), LINES_PER_FRAME);
    assert!((0..LINES_PER_FRAME as u16).all(|l| seen.contains(&l)));
    assert!(saw_vblank);
    assert!(saw_hblank_set && saw_hblank_clear);
    assert!(vblank_window_held);
    // A full frame is exactly 228 scanlines of 1536 cycles.
    assert_eq!(
        total_cycles,
        u64::from(LINES_PER_FRAME as u32 * CYCLES_PER_LINE)
    );
    assert_eq!(system.vcount(), 0);
    assert_eq!(system.framebuffer().len(), WIDTH * VISIBLE_LINES * 4);
}

#[test]
fn vblank_irq_dispatches_through_the_bios_hle_path() {
    let mut system = System::new(idle_rom(), 0x0800_0000);
    // Unmask IRQs (I bit clear, SVC mode, ARM state).
    system.cpu.set_cpsr_raw(0x13);
    system.bus_mut().write_u16(DISPSTAT, 0x0008); // V-Blank IRQ enable
    system.bus_mut().write_u16(IE, 0x0001);
    system.bus_mut().write_u16(IME, 0x0001);
    // Install a parked handler (`b .`) and its [0x03007FFC] pointer.
    system.bus_mut().write_u32(0x0200_0000, 0xEAFF_FFFE);
    system.bus_mut().write_u32(0x0300_7FFC, 0x0200_0000);

    let mut steps = 0;
    while system.cpu.pc() != 0x0200_0000 {
        system.step();
        steps += 1;
        assert!(steps < 200_000, "IRQ never dispatched");
    }
    assert_ne!(system.bus().int_flags() & 0x0001, 0);
    assert!(system.vcount() >= 160);
    // HLE dispatch contract: the handler runs in IRQ state (IntrMain
    // keeps its frame on sp_irq) with IRQs disabled, r0 = IE & IF, lr
    // on the HLE return sentinel.
    assert_eq!(system.cpu.mode(), Mode::Irq);
    assert_eq!(system.cpu.cpsr() & 0x80, 0x80);
    assert_eq!(system.cpu.cpsr() & 0x20, 0);
    assert_eq!(system.cpu.reg_for_test(0), 0x0001);
    assert_eq!(system.cpu.reg_for_test(14), runtime::cpu::IRQ_HLE_RETURN);
}

#[test]
fn irq_without_an_installed_handler_stays_pending() {
    let mut system = System::new(idle_rom(), 0x0800_0000);
    system.cpu.set_cpsr_raw(0x13);
    system.bus_mut().write_u16(DISPSTAT, 0x0008);
    system.bus_mut().write_u16(IE, 0x0001);
    system.bus_mut().write_u16(IME, 0x0001);
    // [0x03007FFC] is zero: nothing to dispatch to, so the request
    // stays pending and the CPU keeps running the interrupted code.

    let mut steps = 0;
    while system.vcount() < 200 {
        system.step();
        steps += 1;
        assert!(steps < 400_000, "vblank never arrived");
    }
    assert_ne!(system.bus().int_flags() & 0x0001, 0, "IF must be pending");
    assert_eq!(system.cpu.pc(), 0x0800_0000, "ROM must keep running");
    assert_eq!(system.cpu.mode(), Mode::Supervisor);
}

#[test]
fn synthetic_mode3_rom_draws_known_pixels() {
    let mut system = thumb_system(mode3_rom());
    system.step_frame();

    let frame = system.framebuffer();
    assert_eq!(frame.len(), WIDTH * VISIBLE_LINES * 4);
    assert_eq!(pixel(frame, 0, 0), [0xFF, 0xFF, 0xFF, 0xFF]); // 0x7FFF
    assert_eq!(pixel(frame, 1, 0), [0xFF, 0x00, 0x00, 0xFF]); // 0x001F
    assert_eq!(pixel(frame, 2, 0), [0x00, 0x00, 0xFF, 0xFF]); // 0x7C00
    assert_eq!(pixel(frame, 3, 0), [0x00, 0x00, 0x00, 0xFF]);
    assert_eq!(pixel(frame, 5, 120), [0x00, 0x00, 0x00, 0xFF]);

    // The capture API hands back the same buffer the PPU rendered.
    assert_eq!(frame.as_ptr(), system.framebuffer().as_ptr());
}
