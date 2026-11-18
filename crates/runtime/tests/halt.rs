//! A halted CPU sleeps through whole spans at once; the result must
//! match one-cycle stepping exactly.

use runtime::system::System;

const IO: u32 = 0x0400_0000;
const IWRAM: u32 = 0x0300_0000;

/// An IRQ handler at 0x02000000 that acknowledges IF and ORs it into
/// the BIOS flag word, as IntrWait users do.
const FLAG_HANDLER: [u32; 10] = [
    0xE3A0_3301, // mov r3, #0x04000000
    0xE283_3C02, // add r3, r3, #0x200
    0xE1D3_20B2, // ldrh r2, [r3, #2]
    0xE1C3_20B2, // strh r2, [r3, #2]
    0xE59F_300C, // ldr r3, [pc, #12]
    0xE1D3_10B0, // ldrh r1, [r3]
    0xE181_1002, // orr r1, r1, r2
    0xE1C3_10B0, // strh r1, [r3]
    0xE12F_FF1E, // bx lr
    0x0300_7FF8,
];

/// A game-shaped idle loop: VBlankIntrWait, count, repeat; V-Blank and
/// timer 1 interrupts, and timer 0 driving FIFO A through DMA 1.
fn halting_system() -> System {
    let mut rom = vec![0u8; 0x200];
    for (i, word) in [0xEF05_0000u32, 0xE285_5001, 0xEAFF_FFFC]
        .iter()
        .enumerate()
    {
        rom[4 * i..4 * i + 4].copy_from_slice(&word.to_le_bytes());
    }
    let mut system = System::new(rom, 0x0800_0000);
    let bus = system.bus_mut();
    for (i, word) in FLAG_HANDLER.iter().enumerate() {
        bus.write_u32(0x0200_0000 + 4 * i as u32, *word);
    }
    bus.write_u32(0x0300_7FFC, 0x0200_0000);
    bus.write_u16(IO + 0x004, 0x0008); // DISPSTAT: V-Blank IRQ
    bus.write_u16(IO + 0x200, 0x0011); // IE: V-Blank, timer 1
    // Sound: FIFO A on timer 0, fed by DMA 1 from an IWRAM ramp.
    bus.write_u16(IO + 0x084, 0x0080);
    bus.write_u16(IO + 0x082, 0x0800);
    bus.write_u16(IO + 0x082, 0x0002 | 0x0004 | 0x0300);
    for k in 0..1024u32 {
        bus.write_u8(IWRAM + k, (7 * k + 13) as u8);
    }
    bus.write_u32(IO + 0x0BC, IWRAM);
    bus.write_u32(IO + 0x0C0, IO + 0x0A0);
    bus.write_u16(IO + 0x0C4, 4);
    bus.write_u16(IO + 0x0C6, 0x0002 | 0x0010 | 0x0020 | 0x0300 | 0x8000);
    // Timer 0: one overflow per 512 cycles; timer 1: IRQ at prescaler 64.
    bus.write_u16(IO + 0x100, 0xFE00);
    bus.write_u16(IO + 0x102, 0x0080);
    bus.write_u16(IO + 0x104, 0xF000);
    bus.write_u16(IO + 0x106, 0x00C1);
    system
}

#[test]
fn halted_spans_match_one_cycle_stepping() {
    let mut fast = halting_system();
    let mut exact = halting_system();
    let mut fast_steps = 0u32;
    for _ in 0..3 {
        while !fast.take_frame_ended() {
            fast.step();
            fast_steps += 1;
        }
    }
    let mut exact_steps = 0u32;
    for _ in 0..3 {
        while !exact.take_frame_ended() {
            let cycles = exact.step_deferred();
            exact.advance_clock(cycles);
            exact_steps += 1;
        }
    }
    assert!(
        fast_steps * 20 < exact_steps,
        "{fast_steps} vs {exact_steps} steps"
    );
    assert_eq!(fast.cpu.cycles(), exact.cpu.cycles(), "cycles");
    for i in 0..16u8 {
        assert_eq!(fast.cpu.reg_for_test(i), exact.cpu.reg_for_test(i), "r{i}");
    }
    assert_eq!(fast.cpu.cpsr(), exact.cpu.cpsr(), "cpsr");
    assert!(fast.cpu.reg_for_test(5) >= 2, "the loop woke every frame");
    assert_eq!(fast.bus().ewram(), exact.bus().ewram(), "EWRAM");
    assert_eq!(fast.bus().iwram(), exact.bus().iwram(), "IWRAM");
    assert_eq!(fast.bus().io_image(), exact.bus().io_image(), "IO");
    assert_eq!(
        fast.bus_mut().take_audio(),
        exact.bus_mut().take_audio(),
        "audio"
    );
    assert_eq!(fast.framebuffer(), exact.framebuffer(), "frame");
}
