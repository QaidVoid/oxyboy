//! Interrupt controller semantics and the
//! BIOS-HLE dispatch path.
//!
//! The register-level tests run on a bare bus; the dispatch tests use
//! tiny hand-assembled synthetic ROMs (`b .` and `bx lr` only, both
//! verified with arm-none-eabi-as) so the whole file runs without the
//! assembler toolchain.

use rom::Image;
use runtime::bus::Bus;
use runtime::cpu::{Cpu, IRQ_HLE_RETURN};
use runtime::system::System;
use testutil::testrom;

const DISPSTAT: u32 = 0x0400_0004;
const IE: u32 = 0x0400_0200;
const IF: u32 = 0x0400_0202;
const IME: u32 = 0x0400_0208;
const IRQ_HANDLER_PTR: u32 = 0x0300_7FFC;

/// `b .` in ARM state (verified: `ea ff ff fe`).
const PARK: u32 = 0xEAFF_FFFE;
/// `bx lr` in ARM state (verified: `e1 2f ff 1e`).
const BX_LR: u32 = 0xE12F_FF1E;

#[test]
fn if_acknowledge_clears_exactly_the_written_one_bits() {
    let mut bus = Bus::new(Vec::new());
    bus.raise_if(0x0111);
    bus.write_u16(IF, 0x0001);
    assert_eq!(bus.int_flags(), 0x0110, "written 1 bits clear, 0 bits keep");
    bus.write_u16(IF, 0x0110);
    assert_eq!(bus.int_flags(), 0x0000);
    // A write of zero acknowledges nothing.
    bus.raise_if(0x0002);
    bus.write_u16(IF, 0);
    assert_eq!(bus.int_flags(), 0x0002);
    // Byte writes acknowledge their own byte only.
    bus.raise_if(0x0102);
    bus.write_u8(IF, 0x02);
    assert_eq!(bus.int_flags(), 0x0100);
    bus.write_u8(IF + 1, 0x01);
    assert_eq!(bus.int_flags(), 0x0000);
    // Word writes split into the two halfword lanes.
    bus.raise_if(0xFFFF);
    bus.write_u32(IF, 0x0001_F000);
    assert_eq!(bus.int_flags(), 0x0FFF);
}

#[test]
fn ie_and_ime_are_plain_storage() {
    let mut bus = Bus::new(Vec::new());
    bus.write_u16(IE, 0x1234);
    assert_eq!(bus.int_enable(), 0x1234);
    bus.write_u16(IME, 0x0001);
    assert_eq!(bus.ime(), 0x0001);
    bus.write_u32(IME, 0xFFFF_FFFF);
    assert_eq!(bus.ime(), 0xFFFF, "IME stores the low halfword");
    // DISPSTAT is unaffected by IE/IF/IME writes (independent registers).
    assert_eq!(bus.dispstat(), 0);
}

#[test]
fn irq_pending_requires_ie_if_and_ime() {
    let mut bus = Bus::new(Vec::new());
    bus.raise_if(0x0008);
    assert!(!bus.irq_pending(), "IF alone never reaches the CPU");
    bus.write_u16(IE, 0x0004);
    assert!(!bus.irq_pending(), "IE must select a pending IF bit");
    bus.write_u16(IE, 0x0008);
    assert!(!bus.irq_pending(), "IME = 0 masks everything");
    bus.write_u16(IME, 0x0001);
    assert!(bus.irq_pending());
    bus.write_u16(IME, 0x0000);
    assert!(!bus.irq_pending());
}

/// A system parked on `b .` with a `bx lr` handler installed at
/// 0x02000000; the interrupted-context canaries are preset.
fn dispatch_system() -> System {
    let mut rom = vec![0u8; 0x20];
    rom[..4].copy_from_slice(&PARK.to_le_bytes());
    let mut system = System::new(rom, 0x0800_0000);
    // ARM, System state, I clear: interruptible.
    system.cpu.set_cpsr_raw(0x1F);
    for (i, v) in [0x11u32, 0x22, 0x33, 0x44].iter().enumerate() {
        system.cpu.set_reg_for_test(i as u8, *v);
    }
    system.cpu.set_reg_for_test(12, 0x55);
    system.cpu.set_reg_for_test(14, 0x0ADC_0DE0);
    system.bus_mut().write_u32(0x0200_0000, BX_LR);
    system.bus_mut().write_u32(IRQ_HANDLER_PTR, 0x0200_0000);
    system.bus_mut().write_u16(DISPSTAT, 0x0008);
    system.bus_mut().write_u16(IE, 0x0001);
    system.bus_mut().write_u16(IME, 0x0001);
    system
}

#[test]
fn hle_dispatch_preserves_the_interrupted_context_across_a_return() {
    let mut system = dispatch_system();
    let mut seen_handler = false;
    let mut steps = 0;
    loop {
        system.cpu.set_irq_line(true);
        system.step();
        steps += 1;
        assert!(steps < 100, "dispatch never came back: {steps} steps");
        if system.cpu.pc() == 0x0200_0000 {
            seen_handler = true;
        }
        if seen_handler && system.cpu.pc() == 0x0800_0000 {
            break;
        }
    }
    // Back at the park loop with every preserved register restored.
    assert_eq!(system.cpu.pc(), 0x0800_0000);
    assert_eq!(system.cpu.mode(), runtime::cpu::Mode::System);
    assert_eq!(system.cpu.cpsr() & 0x80, 0, "I bit restored");
    for (i, v) in [0x11u32, 0x22, 0x33, 0x44].iter().enumerate() {
        assert_eq!(system.cpu.reg_for_test(i as u8), *v, "r{i} restored");
    }
    assert_eq!(system.cpu.reg_for_test(12), 0x55, "r12 restored");
    assert_eq!(system.cpu.reg_for_test(14), 0x0ADC_0DE0, "lr restored");
}

#[test]
fn hle_dispatch_passes_the_io_base_in_r0() {
    let mut system = dispatch_system();
    system.bus_mut().raise_if(0x0002); // an unrelated second source
    system.cpu.set_irq_line(true);
    // Step until the handler is running (pc at the handler address).
    let mut steps = 0;
    while system.cpu.pc() != 0x0200_0000 {
        system.step();
        steps += 1;
        assert!(steps < 100, "dispatch never happened");
    }
    // The BIOS dispatcher leaves `mov r0, #0x04000000` in r0.
    assert_eq!(system.cpu.reg_for_test(0), 0x0400_0000);
    // Its preamble frame sits on the live IRQ stack.
    assert_eq!(system.cpu.reg_for_test(13), 0x0300_7F88);
    assert_eq!(system.cpu.reg_for_test(14), IRQ_HLE_RETURN);
}

#[test]
fn hle_dispatch_updates_the_bios_protection_latch() {
    // Entering the dispatch latches the IRQ path's prefetch; the
    // return latches the postamble's (the external bios suite reads
    // BIOS[0] during and after the handler).
    let mut system = dispatch_system();
    system.cpu.set_irq_line(true);
    let mut steps = 0;
    while system.cpu.pc() != 0x0200_0000 {
        system.step();
        steps += 1;
        assert!(steps < 100, "dispatch never happened");
    }
    assert_eq!(
        system.bus_mut().read_u32(0x0000_0000),
        runtime::bus::BIOS_LATCH_IRQ,
        "latched while the handler runs"
    );
    while system.cpu.pc() != 0x0800_0000 {
        system.step();
        steps += 1;
        assert!(steps < 100, "return never happened");
    }
    assert_eq!(
        system.bus_mut().read_u32(0x0000_0000),
        runtime::bus::BIOS_LATCH_IRQ_RETURN,
        "latched after the return"
    );
}

#[test]
fn ime_zero_masks_the_irq_line_entirely() {
    let mut rom = vec![0u8; 0x20];
    rom[..4].copy_from_slice(&PARK.to_le_bytes());
    let mut system = System::new(rom, 0x0800_0000);
    system.cpu.set_cpsr_raw(0x1F);
    system.bus_mut().write_u16(DISPSTAT, 0x0008);
    system.bus_mut().write_u16(IE, 0x0001);
    system.bus_mut().raise_if(0x0001);
    // IME stays 0: even a held IRQ line must not disturb the CPU.

    let mut steps = 0;
    while steps < 1_000 {
        system.cpu.set_irq_line(true);
        system.step();
        steps += 1;
        assert_eq!(system.cpu.pc(), 0x0800_0000, "no dispatch may happen");
    }
    assert_eq!(system.cpu.mode(), runtime::cpu::Mode::System);
    assert_ne!(system.bus().int_flags() & 0x0001, 0, "IF stays pending");
}

#[test]
fn i_bit_set_masks_the_irq_line_even_with_ime_on() {
    let mut rom = vec![0u8; 0x20];
    rom[..4].copy_from_slice(&PARK.to_le_bytes());
    let mut system = System::new(rom, 0x0800_0000);
    // System state with I set: hardware masks IRQs on the I bit alone.
    system.cpu.set_cpsr_raw(0x9F);
    system.bus_mut().write_u16(DISPSTAT, 0x0008);
    system.bus_mut().write_u16(IE, 0x0001);
    system.bus_mut().write_u16(IME, 0x0001);
    system.bus_mut().raise_if(0x0001);

    let mut steps = 0;
    while steps < 1_000 {
        system.cpu.set_irq_line(true);
        system.step();
        steps += 1;
        assert_eq!(system.cpu.pc(), 0x0800_0000, "no dispatch may happen");
    }
}

#[test]
fn a_cpu_with_no_irq_support_in_flight_is_untouched_by_the_sentinel() {
    // A bare CPU jumping to the sentinel outside IRQ mode must not pop
    // anything.
    let mut rom = vec![0u8; 0x20];
    rom[..4].copy_from_slice(&PARK.to_le_bytes());
    let mut bus = Bus::new(rom);
    let mut cpu = Cpu::new(0x0800_0000);
    cpu.set_pc(IRQ_HLE_RETURN);
    cpu.set_irq_line(false);
    let _ = cpu.step(&mut bus);
    // It fell through to the (zero) BIOS read: pc advanced, no panic.
    assert_eq!(cpu.pc(), IRQ_HLE_RETURN + 4);
}

/// ROM-level check: the assembled `irqtest` micro-ROM
/// parks on a heartbeat counter with register canaries and a VBlank
/// handler that checks the HLE contract, counts entries, and
/// acknowledges IF. The driver verifies IME masking over the first
/// frames, then enables IME and counts dispatches.
#[test]
fn irq_rom_vblank_masking_dispatch_and_return() {
    let Some(path) = testrom::require("irqtest") else {
        return;
    };
    let image = Image::from_file(&path).expect("script-built ROM must parse");
    let mut system = System::new(image.bytes().to_vec(), 0x0800_0000);
    let word = |system: &mut System, addr: u32| system.bus_mut().read_u32(addr);
    const ENTRIES: u32 = 0x0300_0000;
    const CONTRACT: u32 = 0x0300_0008;
    const HEARTBEAT: u32 = 0x0300_0010;

    // Frames 1-2, IME = 0 (reset default): the request stays pending,
    // nothing dispatches (GBATEK: IME = 0 masks all).
    system.step_frame();
    system.step_frame();
    assert_eq!(word(&mut system, ENTRIES), 0, "IME=0 must mask all");
    assert_ne!(
        system.bus().int_flags() & 0x0001,
        0,
        "the VBlank request must stay pending while masked"
    );

    // IME = 1 (the driver plays the role of the ROM's IME step).
    // Idiomatic ROM init acknowledges stale requests first: otherwise
    // the pending mask-period request dispatches the instant IME
    // rises, which is correct hardware behavior but muddies the
    // exactly-once-per-frame count.
    system.bus_mut().write_u16(0x0400_0202, 0xFFFF);
    system.bus_mut().write_u32(0x0400_0208, 1);
    system.step_frame();
    system.step_frame();
    system.step_frame();
    assert_eq!(
        word(&mut system, ENTRIES),
        3,
        "handler must run exactly once per VBlank"
    );
    assert_eq!(
        word(&mut system, CONTRACT),
        0x0003,
        "r0 = 0x04000000 and canary preservation"
    );
    assert_eq!(
        system.bus().int_flags() & 0x0001,
        0,
        "the handler acknowledged IF"
    );
    let heartbeat = word(&mut system, HEARTBEAT);
    assert!(heartbeat > 0, "the interrupted loop must have run");
    system.step_frame();
    assert!(
        word(&mut system, HEARTBEAT) > heartbeat,
        "the interrupted loop must keep running after returns"
    );
}

/// An SDK-style re-entrant handler at 0x02000000: it saves SPSR and
/// lr_irq on the IRQ stack, acknowledges IF, counts the entry at
/// 0x03000000, and opens a two-instruction window with IRQs enabled
/// before restoring and returning to the BIOS dispatcher.
const NESTING_HANDLER: [u32; 22] = [
    0xE14F_3000, // mrs r3, spsr
    0xE92D_4008, // stmfd sp!, {r3, lr}
    0xE3A0_3301, // mov r3, #0x04000000
    0xE283_3C02, // add r3, r3, #0x200
    0xE1D3_20B2, // ldrh r2, [r3, #2]
    0xE1C3_20B2, // strh r2, [r3, #2]
    0xE59F_1034, // ldr r1, [pc, #0x34]   ; =0x03000000
    0xE591_0000, // ldr r0, [r1]
    0xE280_0001, // add r0, r0, #1
    0xE581_0000, // str r0, [r1]
    0xE10F_3000, // mrs r3, cpsr
    0xE3C3_3080, // bic r3, r3, #0x80
    0xE121_F003, // msr cpsr_c, r3        ; IRQs on
    0xE1A0_0000, // nop                   ; (window)
    0xE1A0_0000, // nop
    0xE10F_3000, // mrs r3, cpsr
    0xE383_3080, // orr r3, r3, #0x80
    0xE121_F003, // msr cpsr_c, r3        ; IRQs off
    0xE8BD_4008, // ldmfd sp!, {r3, lr}
    0xE16F_F003, // msr spsr_fsxc, r3
    0xE12F_FF1E, // bx lr
    0x0300_0000,
];

#[test]
fn nested_irqs_keep_each_frame_on_the_irq_stack() {
    let mut rom = vec![0u8; 0x20];
    rom[..4].copy_from_slice(&PARK.to_le_bytes());
    let mut bus = Bus::new(rom);
    for (i, word) in NESTING_HANDLER.iter().enumerate() {
        bus.write_u32(0x0200_0000 + 4 * i as u32, *word);
    }
    bus.write_u32(IRQ_HANDLER_PTR, 0x0200_0000);
    bus.write_u16(IE, 0x0003);
    bus.write_u16(IME, 0x0001);
    let mut cpu = Cpu::new(0x0800_0000);
    cpu.set_cpsr_raw(0x1F);
    for (i, v) in [0x11u32, 0x22, 0x33, 0x44].iter().enumerate() {
        cpu.set_reg_for_test(i as u8, *v);
    }
    cpu.set_reg_for_test(12, 0x55);
    cpu.set_reg_for_test(14, 0x0ADC_0DE0);
    let sp = cpu.reg_for_test(13);

    let step_until = |cpu: &mut Cpu, bus: &mut Bus, done: &dyn Fn(&Cpu, &mut Bus) -> bool| {
        for _ in 0..1_000 {
            if done(cpu, bus) {
                return;
            }
            cpu.set_irq_line(bus.irq_pending());
            cpu.step(bus);
        }
        panic!("condition never held; pc {:#010x}", cpu.pc());
    };

    // The outer IRQ reaches the handler's IRQs-on window...
    bus.raise_if(0x0001);
    step_until(&mut cpu, &mut bus, &|cpu, _| cpu.pc() == 0x0200_0034);
    // ...where a second source nests a full dispatch.
    bus.raise_if(0x0002);
    step_until(&mut cpu, &mut bus, &|cpu, _| {
        cpu.pc() == 0x0800_0000 && cpu.mode() == runtime::cpu::Mode::System
    });
    assert_eq!(bus.read_u32(0x0300_0000), 2, "both dispatches ran");
    for (i, v) in [0x11u32, 0x22, 0x33, 0x44].iter().enumerate() {
        assert_eq!(cpu.reg_for_test(i as u8), *v, "r{i} restored");
    }
    assert_eq!(cpu.reg_for_test(12), 0x55, "r12 restored");
    assert_eq!(cpu.reg_for_test(13), sp, "sp restored");
    assert_eq!(cpu.reg_for_test(14), 0x0ADC_0DE0, "lr restored");
    assert_eq!(cpu.cpsr() & 0x80, 0, "I bit restored");

    // The IRQ stack unwound fully: a later dispatch works the same.
    bus.raise_if(0x0001);
    step_until(&mut cpu, &mut bus, &|cpu, bus| {
        bus.read_u32(0x0300_0000) == 3 && cpu.pc() == 0x0800_0000
    });
    assert_eq!(cpu.reg_for_test(0), 0x11);
}
