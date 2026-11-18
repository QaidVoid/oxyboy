//! DMA timing modes fire at the right point,
//! width/adjust modes behave per GBATEK, and simultaneous enables run
//! in channel-priority order.
//!
//! Register-level tests run either on a bare bus plus PPU (flag-edge
//! timing) or through `System::step` (immediate timing, which the
//! system driver defers to after the enabling instruction). Encodings
//! used in comments were checked against GBATEK "DMA Transfer
//! Channels"; these tests need no assembler-built ROMs.

use runtime::bus::Bus;
use runtime::ppu::{CYCLES_PER_LINE, Ppu};
use runtime::system::System;

/// Channel N register base (`0x040000B0 + 12*N`).
fn ch(n: u32) -> u32 {
    0x0400_00B0 + 12 * n
}
const SAD: u32 = 0;
const DAD: u32 = 4;
const CNT_L: u32 = 8;
const CNT_H: u32 = 0xA;

const SRC: u32 = 0x0200_0000;
const DST: u32 = 0x0200_0100;

/// `b .` (0xEAFFFFFE, little-endian bytes): parks the CPU while DMA
/// runs.
fn parked() -> Vec<u8> {
    vec![0xFE, 0xFF, 0xFF, 0xEA]
}

/// CNT_H builders: `(dst_adj, src_adj, timing, extra_bits)`.
fn cnt_h(dst_adj: u16, src_adj: u16, timing: u16, extra: u16) -> u16 {
    0x8000 | extra | (timing << 12) | (src_adj << 7) | (dst_adj << 5)
}

/// Fills `count` halfwords at `addr` with `seed + i` patterns.
fn fill_halfwords(bus: &mut Bus, addr: u32, count: u32, seed: u16) {
    for i in 0..count {
        bus.write_u16(addr + 2 * i, seed.wrapping_add(i as u16));
    }
}

#[test]
fn immediate_halfword_transfer_copies_on_the_next_system_step() {
    let mut system = System::new(parked(), 0x0800_0000);
    let bus = system.bus_mut();
    fill_halfwords(bus, SRC, 4, 0x1000);
    bus.write_u32(ch(0) + SAD, SRC);
    bus.write_u32(ch(0) + DAD, DST);
    bus.write_u16(ch(0) + CNT_L, 4);
    bus.write_u16(ch(0) + CNT_H, cnt_h(0b00, 0b00, 0b00, 0));

    // Nothing runs until the system drains the deferred request.
    assert_eq!(bus.read_u16(DST), 0);

    system.step();
    let bus = system.bus_mut();
    for i in 0..4u32 {
        assert_eq!(
            bus.read_u16(DST + 2 * i),
            0x1000u16.wrapping_add(i as u16),
            "unit {i} not copied"
        );
    }
    // Non-repeat immediate transfers self-disable (GBATEK: the enable
    // bit clears when the transfer ends).
    assert_eq!(bus.read_u16(ch(0) + CNT_H) & 0x8000, 0);
}

#[test]
fn word_width_transfers_32_bit_units() {
    let mut system = System::new(parked(), 0x0800_0000);
    let bus = system.bus_mut();
    for (i, v) in [0x1122_3344u32, 0x5566_7788].iter().enumerate() {
        bus.write_u32(SRC + 4 * i as u32, *v);
    }
    bus.write_u32(ch(1) + SAD, SRC);
    bus.write_u32(ch(1) + DAD, DST);
    bus.write_u16(ch(1) + CNT_L, 2);
    // Width bit 5 selects 32-bit units.
    bus.write_u16(ch(1) + CNT_H, cnt_h(0b00, 0b00, 0b00, 0x0400));
    system.step();

    let bus = system.bus_mut();
    assert_eq!(bus.read_u32(DST), 0x1122_3344);
    assert_eq!(bus.read_u32(DST + 4), 0x5566_7788);
}

#[test]
fn dst_fixed_writes_every_unit_to_the_same_address() {
    let mut system = System::new(parked(), 0x0800_0000);
    let bus = system.bus_mut();
    fill_halfwords(bus, SRC, 3, 0x0100);
    bus.write_u32(ch(0) + SAD, SRC);
    bus.write_u32(ch(0) + DAD, DST);
    bus.write_u16(ch(0) + CNT_L, 3);
    bus.write_u16(ch(0) + CNT_H, cnt_h(0b10, 0b00, 0b00, 0));
    system.step();

    let bus = system.bus_mut();
    assert_eq!(bus.read_u16(DST), 0x0102, "the last unit lands alone");
    assert_eq!(bus.read_u16(DST + 2), 0);
}

#[test]
fn dst_dec_walks_backwards_from_the_destination() {
    let mut system = System::new(parked(), 0x0800_0000);
    let bus = system.bus_mut();
    fill_halfwords(bus, SRC, 2, 0x0100);
    bus.write_u32(ch(0) + SAD, SRC);
    // The first unit writes DAD itself; later units step down.
    bus.write_u32(ch(0) + DAD, DST + 2);
    bus.write_u16(ch(0) + CNT_L, 2);
    bus.write_u16(ch(0) + CNT_H, cnt_h(0b01, 0b00, 0b00, 0));
    system.step();

    let bus = system.bus_mut();
    assert_eq!(bus.read_u16(DST + 2), 0x0100);
    assert_eq!(bus.read_u16(DST), 0x0101);
}

#[test]
fn src_dec_reads_backwards_from_the_source() {
    let mut system = System::new(parked(), 0x0800_0000);
    let bus = system.bus_mut();
    fill_halfwords(bus, SRC, 2, 0x0100);
    bus.write_u32(ch(0) + SAD, SRC + 2);
    bus.write_u32(ch(0) + DAD, DST);
    bus.write_u16(ch(0) + CNT_L, 2);
    bus.write_u16(ch(0) + CNT_H, cnt_h(0b00, 0b01, 0b00, 0));
    system.step();

    let bus = system.bus_mut();
    assert_eq!(bus.read_u16(DST), 0x0101);
    assert_eq!(bus.read_u16(DST + 2), 0x0100);
}

#[test]
fn count_zero_means_the_channel_maximum() {
    // Channel 3's maximum is 0x10000 halfword units (0x20000 bytes, the
    // whole EWRAM); a channel 0-2 maximum would stop at 0x8000 bytes.
    let mut system = System::new(parked(), 0x0800_0000);
    let bus = system.bus_mut();
    fill_halfwords(bus, SRC, 0x1_0000, 0x0200);
    bus.write_u32(ch(3) + SAD, SRC);
    bus.write_u32(ch(3) + DAD, 0x0300_0000);
    bus.write_u16(ch(3) + CNT_L, 0);
    bus.write_u16(ch(3) + CNT_H, cnt_h(0b10, 0b00, 0b00, 0));
    system.step();

    let bus = system.bus_mut();
    // A fixed destination keeps only the final unit: 0x1FFFE bytes in.
    assert_eq!(bus.read_u16(0x0300_0000), 0x0200u16.wrapping_add(0xFFFF));
}

#[test]
fn latched_addresses_survive_later_register_writes() {
    let mut system = System::new(parked(), 0x0800_0000);
    let bus = system.bus_mut();
    fill_halfwords(bus, SRC, 1, 0x0A0A);
    fill_halfwords(bus, SRC + 0x40, 1, 0x0B0B);
    bus.write_u32(ch(0) + SAD, SRC);
    bus.write_u32(ch(0) + DAD, DST);
    bus.write_u16(ch(0) + CNT_L, 1);
    // The enable write latches SAD; a later rewrite must be ignored.
    bus.write_u16(ch(0) + CNT_H, cnt_h(0b00, 0b00, 0b00, 0));
    bus.write_u32(ch(0) + SAD, SRC + 0x40);
    system.step();

    assert_eq!(system.bus_mut().read_u16(DST), 0x0A0A);
}

#[test]
fn simultaneous_enables_run_lowest_channel_first() {
    let mut system = System::new(parked(), 0x0800_0000);
    let bus = system.bus_mut();
    bus.write_u16(SRC, 0xAAAA);
    bus.write_u16(SRC + 0x40, 0xBBBB);
    for (n, src) in [(0u32, SRC), (1, SRC + 0x40)] {
        bus.write_u32(ch(n) + SAD, src);
        bus.write_u32(ch(n) + DAD, DST);
        bus.write_u16(ch(n) + CNT_L, 1);
        // Fixed destination: the later (higher-priority-losing)
        // channel's unit stays visible; IRQ bits prove both ran.
        bus.write_u16(ch(n) + CNT_H, cnt_h(0b10, 0b00, 0b00, 0x4000));
    }
    system.step();

    let bus = system.bus_mut();
    assert_eq!(bus.read_u16(DST), 0xBBBB, "channel 0 runs first");
    assert_eq!(bus.int_flags() & 0x0300, 0x0300, "both channels ran");
}

#[test]
fn dma_irq_raises_the_channel_if_bit_on_completion() {
    let mut system = System::new(parked(), 0x0800_0000);
    let bus = system.bus_mut();
    bus.write_u16(SRC, 0x1234);
    bus.write_u32(ch(2) + SAD, SRC);
    bus.write_u32(ch(2) + DAD, DST);
    bus.write_u16(ch(2) + CNT_L, 1);
    bus.write_u16(ch(2) + CNT_H, cnt_h(0b00, 0b00, 0b00, 0x4000));
    system.step();
    assert_eq!(system.bus().int_flags(), 1 << 10, "DMA2 is IF bit 10");
}

#[test]
fn vblank_dma_fires_exactly_at_the_vblank_flag_edge() {
    let mut bus = Bus::new(Vec::new());
    let mut ppu = Ppu::new();
    fill_halfwords(&mut bus, SRC, 1, 0x0C0C);
    bus.write_u32(ch(0) + SAD, SRC);
    bus.write_u32(ch(0) + DAD, DST);
    bus.write_u16(ch(0) + CNT_L, 1);
    bus.write_u16(ch(0) + CNT_H, cnt_h(0b00, 0b00, 0b01, 0));

    // Line 100 of 160: the V-Blank flag has not risen yet.
    ppu.step(&mut bus, 100 * CYCLES_PER_LINE);
    assert_eq!(bus.dispstat() & 0x0001, 0);
    assert_eq!(bus.read_u16(DST), 0, "V-Blank DMA must not run early");

    // Crossing into line 160 raises the flag and runs the channel.
    ppu.step(&mut bus, 60 * CYCLES_PER_LINE);
    assert_eq!(bus.dispstat() & 0x0001, 0x0001);
    assert_eq!(bus.read_u16(DST), 0x0C0C);
    // Non-repeat: the channel is spent.
    assert_eq!(bus.read_u16(ch(0) + CNT_H) & 0x8000, 0);

    // Later V-Blank lines are level, not edges: no second run.
    bus.write_u16(DST, 0);
    ppu.step(&mut bus, 10 * CYCLES_PER_LINE);
    assert_eq!(bus.read_u16(DST), 0);
}

#[test]
fn hblank_dma_fires_between_hdraw_and_hblank_flags() {
    let mut bus = Bus::new(Vec::new());
    let mut ppu = Ppu::new();
    bus.write_u16(SRC, 0x0D0D);
    bus.write_u32(ch(1) + SAD, SRC);
    bus.write_u32(ch(1) + DAD, DST);
    bus.write_u16(ch(1) + CNT_L, 1);
    // H-Blank timing (10) with repeat (CNT_H bit 4); src and dst fixed
    // so every line's run reads and writes the same cells.
    bus.write_u16(ch(1) + CNT_H, cnt_h(0b10, 0b10, 0b10, 0x0200));

    // Dot 1005 of line 0: HDraw still holds (the flag rises at dot
    // 1006, GBATEK DISPSTAT: the flag is low for 1006 of the line's
    // 1232 cycles).
    ppu.step(&mut bus, 1005);
    assert_eq!(bus.dispstat() & 0x0002, 0);
    assert_eq!(bus.read_u16(DST), 0, "H-Blank DMA must not run in HDraw");

    // One cycle later the H-Blank flag has risen and the transfer ran.
    ppu.step(&mut bus, 1);
    assert_eq!(bus.dispstat() & 0x0002, 0x0002);
    assert_eq!(bus.read_u16(DST), 0x0D0D);
    // Repeat keeps the channel armed.
    assert_eq!(bus.read_u16(ch(1) + CNT_H) & 0x8000, 0x8000);

    // The next line's H-Blank edge re-triggers and re-latches count.
    bus.write_u16(DST, 0);
    ppu.step(&mut bus, CYCLES_PER_LINE);
    assert_eq!(bus.read_u16(DST), 0x0D0D);
}

#[test]
fn repeat_with_inc_reload_restores_the_destination_each_frame() {
    let mut bus = Bus::new(Vec::new());
    let mut ppu = Ppu::new();
    bus.write_u32(ch(0) + SAD, SRC);
    bus.write_u32(ch(0) + DAD, DST);
    bus.write_u16(ch(0) + CNT_L, 2);
    // V-Blank timing, repeat (CNT_H bit 4), dst adjust inc+reload, src
    // inc (the latched source advances across frames).
    bus.write_u16(ch(0) + CNT_H, cnt_h(0b11, 0b00, 0b01, 0x0200));
    fill_halfwords(&mut bus, SRC, 2, 0x0100);
    ppu.step(&mut bus, 161 * CYCLES_PER_LINE);
    assert_eq!(bus.read_u16(DST), 0x0100);
    assert_eq!(bus.read_u16(DST + 2), 0x0101);

    // Extend the source stream where the latched source pointer now
    // sits (src inc continues); the next frame's V-Blank re-copies to
    // the reloaded destination, not from DST + 4.
    fill_halfwords(&mut bus, SRC + 4, 2, 0x0200);
    ppu.step(&mut bus, 228 * CYCLES_PER_LINE);
    assert_eq!(bus.read_u16(DST), 0x0200, "destination reloaded");
    assert_eq!(bus.read_u16(DST + 2), 0x0201);
    assert_eq!(bus.read_u16(DST + 4), 0, "no third slot was touched");
}

#[test]
fn special_timing_ignores_immediate_and_flag_edges() {
    // Channel 1 special timing is the sound FIFO drain; with
    // no FIFO request line it must not fire through other triggers.
    let mut system = System::new(parked(), 0x0800_0000);
    let bus = system.bus_mut();
    bus.write_u16(SRC, 0x0E0E);
    bus.write_u32(ch(1) + SAD, SRC);
    bus.write_u32(ch(1) + DAD, 0x0400_00A0);
    bus.write_u16(ch(1) + CNT_L, 4);
    bus.write_u16(ch(1) + CNT_H, cnt_h(0b11, 0b00, 0b11, 0));
    system.step();
    assert_eq!(
        system.bus_mut().read_u16(0x0400_00A0),
        0,
        "no immediate run"
    );
    assert_ne!(
        system.bus_mut().read_u16(ch(1) + CNT_H) & 0x8000,
        0,
        "FIFO channels stay enabled"
    );

    // The flag edges ignore special-timing channels too.
    let mut bus = Bus::new(Vec::new());
    let mut ppu = Ppu::new();
    bus.write_u16(SRC, 0x0E0E);
    bus.write_u32(ch(1) + SAD, SRC);
    bus.write_u32(ch(1) + DAD, 0x0400_00A0);
    bus.write_u16(ch(1) + CNT_L, 4);
    bus.write_u16(ch(1) + CNT_H, cnt_h(0b11, 0b00, 0b11, 0));
    ppu.step(&mut bus, 228 * CYCLES_PER_LINE);
    assert_eq!(bus.read_u32(0x0400_00A0), 0);
}
