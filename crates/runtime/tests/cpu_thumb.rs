//! THUMB micro-programs over the interpreter.
//!
//! Every halfword below was verified by assembling the equivalent
//! source with `arm-none-eabi-as` (Binutils 2.32) and extracting
//! `.text`, per the project rule that encodings are checked, never
//! hand-derived. Literal words and layout constants are addresses, not
//! encodings.

use runtime::bus::Bus;
use runtime::cpu::{Cpu, Mode};

/// Builds a bus whose ROM contains `halfwords` at `0x08000000`,
/// padded with zero halfwords.
fn thumb_rom(halfwords: &[u16]) -> Bus {
    let mut bytes = vec![0u8; halfwords.len() * 2 + 0x40];
    for (i, h) in halfwords.iter().enumerate() {
        bytes[i * 2..][..2].copy_from_slice(&h.to_le_bytes());
    }
    Bus::new(bytes)
}

/// Boots the CPU directly in THUMB state (SVC mode with T set).
fn thumb_cpu() -> Cpu {
    let mut cpu = Cpu::new(0x0800_0000);
    cpu.set_cpsr_raw(0x33); // SVC, I | F | T
    cpu
}

fn run(cpu: &mut Cpu, bus: &mut Bus, steps: u32) {
    for _ in 0..steps {
        cpu.step(bus);
    }
}

fn flags(cpu: &Cpu) -> (bool, bool, bool, bool) {
    let c = cpu.cpsr();
    (
        c & 0x8000_0000 != 0,
        c & 0x4000_0000 != 0,
        c & 0x2000_0000 != 0,
        c & 0x1000_0000 != 0,
    )
}

fn reg(cpu: &Cpu, index: u8) -> u32 {
    cpu.reg_for_test(index)
}

#[test]
fn formats_1_to_5_alu_and_flags() {
    // lsl/lsr/asr imm; add/sub reg and imm3; mov/cmp/add/sub imm8;
    // and,eor,adc,sbc,ror,tst,neg,cmp,cmn,orr,mul,bic,mvn,lsl;
    // add r8 hi, cmp r8 hi, mov r0,r8 hi; ldr r0,[pc,#0]; b over
    // the literal; bx r2.
    let words = [
        0x011A, 0x0FD4, 0x1065, 0x1888, 0x1EC6, 0x20FF, 0x2800, 0x3001, 0x3801, 0x3001, 0x3801,
        0x4008, 0x4051, 0x415A, 0x41A3, 0x41FC, 0x4208, 0x4245, 0x4288, 0x42C8, 0x4308, 0x4348,
        0x4388, 0x43C8, 0x4090, 0x4480, 0x4580, 0x4640, 0x4800, 0xE002, 0xBABE, 0xCAFE, 0x46C0,
        0x4710,
    ];
    let mut bus = thumb_rom(&words);
    let mut cpu = thumb_cpu();
    cpu.set_reg_for_test(1, 0x1000_0000);
    cpu.set_reg_for_test(2, 0x1234_5678);
    cpu.set_reg_for_test(3, 0x1234_5678);

    run(&mut cpu, &mut bus, 3);
    assert_eq!(reg(&cpu, 2), 0x2345_6780); // LSL 4
    assert_eq!(reg(&cpu, 4), 0); // LSR 31: 0x23456780 < 2^31
    assert_eq!(reg(&cpu, 5), 0); // ASR 1
    assert_eq!(flags(&cpu), (false, true, false, false));

    run(&mut cpu, &mut bus, 2);
    assert_eq!(reg(&cpu, 0), 0x3345_6780); // ADD reg
    assert_eq!(reg(&cpu, 6), 0x3345_677D); // SUB imm3, no borrow

    run(&mut cpu, &mut bus, 4);
    assert_eq!(reg(&cpu, 0), 0xFF);
    assert_eq!(flags(&cpu), (false, false, true, false)); // CMP 0xFF, 0

    run(&mut cpu, &mut bus, 2);
    assert_eq!(reg(&cpu, 0), 0xFF); // ADD #1 then SUB #1

    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 0), 0); // AND: 0xFF & 0x10000000
    assert_eq!(flags(&cpu), (false, true, true, false)); // C unchanged

    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 1), 0x3345_6780); // EOR

    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 2), 0x3579_BDF9); // ADC consumes C=1

    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 3), 0x1234_5677); // SBC propagates

    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 4), 0); // ROR by 0 leaves the value
    assert_eq!(flags(&cpu), (false, true, true, false)); // C passed through

    run(&mut cpu, &mut bus, 1);
    assert_eq!(flags(&cpu), (false, true, true, false)); // TST

    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 5), 0); // NEG 0
    assert_eq!(flags(&cpu), (false, true, true, false));

    run(&mut cpu, &mut bus, 1);
    assert_eq!(flags(&cpu), (true, false, false, false)); // CMP 0, big

    run(&mut cpu, &mut bus, 1);
    assert_eq!(flags(&cpu), (false, false, false, false)); // CMN

    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 0), 0x3345_6780); // ORR

    run(&mut cpu, &mut bus, 1);
    let prod = 0x3345_6780u32.wrapping_mul(0x3345_6780);
    assert_eq!(reg(&cpu, 0), prod); // MUL

    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 0), prod & !0x3345_6780); // BIC

    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 0), !0x3345_6780); // MVN
    assert_eq!(flags(&cpu), (true, false, false, false));

    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 0), 0); // LSL by 249 (>= 32): zero, C clear
    assert_eq!(flags(&cpu), (false, true, false, false));

    run(&mut cpu, &mut bus, 2);
    assert_eq!(reg(&cpu, 8), 0); // ADD hi: r8 = 0 + 0
    assert_eq!(flags(&cpu), (false, true, true, false)); // CMP equal
    assert_eq!(reg(&cpu, 0), 0); // MOV r0, r8 hi

    run(&mut cpu, &mut bus, 3);
    assert_eq!(reg(&cpu, 0), 0xCAFE_BABE); // PC-relative load, b skipped

    // BX r2 with bit 0 set stays in THUMB state (interworking).
    cpu.set_reg_for_test(2, 0x1234_5679);
    run(&mut cpu, &mut bus, 1);
    assert_eq!(cpu.cpsr() & 0x20, 0x20);
    assert_eq!(cpu.pc(), 0x1234_5678 & !1);
    assert_eq!(cpu.mode(), Mode::Supervisor);
}

#[test]
fn memory_transfer_formats() {
    // str/ldrb/ldsb/ldsh/strh/ldr reg-offset; strb/ldr/strh/ldrh
    // imm-offset; str/ldr sp-relative; adr; add sp-based; add/sub sp.
    let words = [
        0x5088, 0x5C8B, 0x568C, 0x5E8D, 0x528C, 0x588B, 0x7108, 0x688B, 0x8048, 0x884B, 0x9004,
        0x9804, 0xA001, 0xA802, 0xB008, 0xB088,
    ];
    let mut bus = thumb_rom(&words);
    let mut cpu = thumb_cpu();
    let base = 0x0300_0100u32;
    cpu.set_reg_for_test(13, 0x0300_0200);
    cpu.set_reg_for_test(1, base);
    cpu.set_reg_for_test(2, 0);
    cpu.set_reg_for_test(0, 0x0000_0080);

    run(&mut cpu, &mut bus, 1);
    assert_eq!(bus.read_u32(base), 0x80); // STR reg-offset

    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 3), 0x80); // LDRB zero-extends

    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 4), 0xFFFF_FF80); // LDSB sign-extends

    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 5), 0x80); // LDSH of a positive halfword

    // Shift the reg-offset to base + 4 and store the negative byte
    // value as a halfword.
    cpu.set_reg_for_test(2, 4);
    run(&mut cpu, &mut bus, 2);
    assert_eq!(bus.read_u16(base + 4), 0xFF80); // STRH reg-offset
    assert_eq!(reg(&cpu, 3), 0x0000_FF80); // LDR reg-offset

    cpu.set_reg_for_test(0, 0x1234_5600);
    run(&mut cpu, &mut bus, 1);
    assert_eq!(bus.read_u8(base + 4), 0x00); // STRB imm-offset

    bus.write_u32(base + 8, 0xDEAD_BEEF);
    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 3), 0xDEAD_BEEF); // LDR imm-offset

    run(&mut cpu, &mut bus, 2);
    assert_eq!(bus.read_u16(base + 2), 0x5600); // STRH imm-offset
    assert_eq!(reg(&cpu, 3), 0x5600); // LDRH zero-extends

    run(&mut cpu, &mut bus, 2);
    assert_eq!(bus.read_u32(0x0300_0210), 0x1234_5600); // STR sp-rel
    assert_eq!(reg(&cpu, 0), 0x1234_5600); // LDR sp-rel

    run(&mut cpu, &mut bus, 1);
    // ADR: (pc+4 & !3) + 4 with pc = 0x08000018 -> 0x08000020.
    assert_eq!(reg(&cpu, 0), 0x0800_0020);

    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 0), 0x0300_0208); // ADD r0, sp, #8

    run(&mut cpu, &mut bus, 2);
    assert_eq!(reg(&cpu, 13), 0x0300_0200); // ADD #32 then SUB #32
}

#[test]
fn push_pop_thumb() {
    let words = [0xB503, 0xBC03, 0xBD04];
    let mut bus = thumb_rom(&words);
    let mut cpu = thumb_cpu();
    let sp = 0x0300_0200u32;
    cpu.set_reg_for_test(13, sp);
    cpu.set_reg_for_test(0, 0x1111_1111);
    cpu.set_reg_for_test(1, 0x2222_2222);
    cpu.set_reg_for_test(14, 0x0800_0101);
    // The stacked word below LR becomes the POP {pc} target.
    bus.write_u32(sp, 0x0800_0101);

    // PUSH {r0, r1, lr}: full descending, LR highest.
    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 13), sp - 12);
    assert_eq!(bus.read_u32(sp - 12), 0x1111_1111);
    assert_eq!(bus.read_u32(sp - 8), 0x2222_2222);
    assert_eq!(bus.read_u32(sp - 4), 0x0800_0101);

    // POP {r0, r1} restores values and advances SP by 8 only.
    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 0), 0x1111_1111);
    assert_eq!(reg(&cpu, 1), 0x2222_2222);
    assert_eq!(reg(&cpu, 13), sp - 4);

    // POP {r2, pc}: r2 takes the stacked LR; PC takes the stacked word
    // above it minus bit 0; the CPU stays in THUMB state (GBATEK
    // THUMB.14 note).
    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 2), 0x0800_0101);
    assert_eq!(cpu.pc(), 0x0800_0100);
    assert_eq!(cpu.cpsr() & 0x20, 0x20);
    // Net SP: -12 (push) +8 +8 (two pops) = +4.
    assert_eq!(reg(&cpu, 13), sp + 4);
}

#[test]
fn load_store_multiple_thumb() {
    // stmia r0!,{r1,r2}; ldmia r0!,{r3,r4}; stmia r0!,{r0,r1} (base in
    // list, first entry: old base stored); ldmia r0!,{r0,r1} (no
    // writeback); stmia r1!,{} (empty list, hand-encoded from the
    // verified stmia r1!,{r1,r2} by clearing the register bits).
    let words = [0xC006, 0xC818, 0xC003, 0xC803, 0xC100];
    let mut bus = thumb_rom(&words);
    let mut cpu = thumb_cpu();
    let area = 0x0300_0100u32;
    cpu.set_reg_for_test(13, 0x0300_0200);
    cpu.set_reg_for_test(0, area);
    cpu.set_reg_for_test(1, 1);
    cpu.set_reg_for_test(2, 2);

    // Pre-store the LDM targets: the STM advanced r0 to area + 8, so
    // the LDM reads area + 8 and area + 12.
    bus.write_u32(area + 8, 5);
    bus.write_u32(area + 12, 6);
    run(&mut cpu, &mut bus, 2);
    assert_eq!(bus.read_u32(area), 1);
    assert_eq!(bus.read_u32(area + 4), 2);
    assert_eq!(reg(&cpu, 0), area + 16);
    assert_eq!(reg(&cpu, 3), 5);
    assert_eq!(reg(&cpu, 4), 6);

    // Base as the first list entry stores the OLD base (ARMv4).
    cpu.set_reg_for_test(0, area);
    cpu.set_reg_for_test(1, 0xBB);
    run(&mut cpu, &mut bus, 1);
    assert_eq!(bus.read_u32(area), area);
    assert_eq!(bus.read_u32(area + 4), 0xBB);
    assert_eq!(reg(&cpu, 0), area + 8);

    // LDM with the base in the list performs no writeback.
    cpu.set_reg_for_test(0, area);
    bus.write_u32(area, 0x1234);
    bus.write_u32(area + 4, 0x5678);
    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 0), 0x1234);
    assert_eq!(reg(&cpu, 1), 0x5678);

    // Empty list: R15 stored late (address+6, the block-transfer
    // pipeline offset; the ARM-state equivalent stores address+12),
    // base + 0x40.
    cpu.set_reg_for_test(1, area);
    let instr_addr = cpu.pc();
    run(&mut cpu, &mut bus, 1);
    assert_eq!(bus.read_u32(area), instr_addr + 6);
    assert_eq!(reg(&cpu, 1), area + 0x40);
}

#[test]
fn branch_formats_and_swi() {
    // mov r0,#1 (flags all clear); the 14 conditional branches each
    // skip the following nop(s) when taken; then swi 0. With NZCV
    // clear: taken are BNE, BCC, BPL, BVC, BLS, BGE, BGT (7); not
    // taken are BEQ, BCS, BMI, BVS, BHI, BLT, BLE (7). BNE carries a
    // nonzero offset (+1 halfword, skipping two nops) to pin the
    // branch-displacement scaling. Condition 1110 (AL) has no THUMB
    // branch encoding (GBATEK THUMB.16), so BAL is untested here.
    let words = [
        0x2001, // mov r0, #1
        0xD000, 0x46C0, // BEQ +0 (not taken)
        0xD101, 0x46C0, 0x46C0, // BNE +1 (taken, skips both nops)
        0xD200, 0x46C0, // BCS (not taken)
        0xD300, 0x46C0, // BCC (taken)
        0xD400, 0x46C0, // BMI (not taken)
        0xD500, 0x46C0, // BPL (taken)
        0xD600, 0x46C0, // BVS (not taken)
        0xD700, 0x46C0, // BVC (taken)
        0xD800, 0x46C0, // BHI (not taken)
        0xD900, 0x46C0, // BLS (taken)
        0xDA00, 0x46C0, // BGE (taken, N == V)
        0xDB00, 0x46C0, // BLT (not taken)
        0xDC00, 0x46C0, // BGT (taken)
        0xDD00, 0x46C0, // BLE (not taken)
        0xDF00, // SWI 0
    ];
    let mut bus = thumb_rom(&words);
    let mut cpu = thumb_cpu();
    // The SWI tests exception entry, not a BIOS call.
    cpu.set_bios_hle(false);

    // 1 (mov) + 7*2 (not-taken pairs) + 7 (taken branches) = 22 steps
    // to reach the SWI.
    run(&mut cpu, &mut bus, 22);
    assert_eq!(cpu.pc(), 0x0800_003C);
    run(&mut cpu, &mut bus, 1);
    // SWI from THUMB: R14_svc = PC+2, vector 0x08, ARM state, SPSR
    // keeps the THUMB CPSR (GBATEK THUMB.17).
    assert_eq!(cpu.pc(), 0x0000_0008);
    assert_eq!(cpu.mode(), Mode::Supervisor);
    assert_eq!(cpu.cpsr() & 0x20, 0);
    assert_eq!(reg(&cpu, 14), 0x0800_003E);
    assert_eq!(cpu.spsr_for_test(), Some(0x33));
}

#[test]
fn undefined_thumb_encoding_raises_exception() {
    // 0xE800 is the permanently undefined 1110 1 space (GBATEK THUMB
    // opcode map); the decoder routes it to Undefined.
    let mut bus = thumb_rom(&[0xE800]);
    let mut cpu = thumb_cpu();
    run(&mut cpu, &mut bus, 1);
    assert_eq!(cpu.pc(), 0x0000_0004);
    assert_eq!(cpu.mode(), Mode::Undefined);
    assert_eq!(cpu.cpsr() & 0x20, 0);
    assert_eq!(reg(&cpu, 14), 0x0800_0002);
    assert_eq!(cpu.spsr_for_test(), Some(0x33));
}

#[test]
fn bl_long_branch_and_lr_form() {
    // bl .+6 (F000 F803): imm_hi = 0, imm_lo = 3 -> 6 halfwords;
    // jumps forward over three halfwords, LR = ($+4) | 1.
    let words = [
        0xF000, 0xF803, // bl target
        0x46C0, 0x46C0, 0x46C0, // skipped
        0x3001, // target: adds r0, #1
        0x4770, // bx lr -> returns
        0x46C0,
    ];
    let mut bus = thumb_rom(&words);
    let mut cpu = thumb_cpu();
    run(&mut cpu, &mut bus, 1);
    assert_eq!(cpu.pc(), 0x0800_000A);
    assert_eq!(reg(&cpu, 14), 0x0800_0005); // (0 + 4) | 1
    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 0), 1);
    run(&mut cpu, &mut bus, 1);
    assert_eq!(cpu.pc(), 0x0800_0004); // LR & !1
    assert_eq!(cpu.cpsr() & 0x20, 0x20);

    // F800 standing alone: PC = LR + 0, LR = $+2 | 1 (GBATEK THUMB.19
    // note: real games use it as a cheap call).
    let words = [0xF800, 0x46C0];
    let mut bus = thumb_rom(&words);
    let mut cpu = thumb_cpu();
    cpu.set_reg_for_test(14, 0x0800_0100 | 1);
    run(&mut cpu, &mut bus, 1);
    assert_eq!(cpu.pc(), 0x0800_0100);
    assert_eq!(reg(&cpu, 14), 0x0800_0003);
}

#[test]
fn arm_thumb_interworking_round_trip() {
    // ARM: ldr r3,[pc,#12]; ldr r0,[pc,#12]; bx r0; mov r1,#0x55; nop;
    // literals. THUMB: movs r0,#0x40; bl thumb_sub; bx r3;
    // thumb_sub: adds r0,#2; bx lr.
    let mut bytes = vec![0u8; 0x40];
    for (off, w) in [
        (0x00u32, 0xE59F_300Cu32),
        (0x04, 0xE59F_000C),
        (0x08, 0xE12F_FF10),
        (0x0C, 0xE3A0_1055),
        (0x10, 0xE1A0_0000),
        (0x14, 0x0800_000C), // r3: ARM return address
        (0x18, 0x0800_001D), // r0: thumb entry 0x1C | 1
    ] {
        bytes[off as usize..off as usize + 4].copy_from_slice(&w.to_le_bytes());
    }
    for (off, h) in [
        (0x1Cu32, 0x2040u16),
        (0x1E, 0xF000),
        (0x20, 0xF802),
        (0x22, 0x4718),
        (0x24, 0x46C0),
        (0x26, 0x3002),
        (0x28, 0x4770),
        (0x2A, 0x46C0),
    ] {
        bytes[off as usize..off as usize + 2].copy_from_slice(&h.to_le_bytes());
    }
    let mut bus = Bus::new(bytes);
    let mut cpu = Cpu::new(0x0800_0000);

    run(&mut cpu, &mut bus, 3);
    assert_eq!(cpu.cpsr() & 0x20, 0x20); // BX switched to THUMB
    assert_eq!(cpu.pc(), 0x0800_001C);
    // BX changes T state only; the mode stays the boot handover's.
    assert_eq!(cpu.mode(), Mode::System);

    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 0), 0x40); // MOVS

    run(&mut cpu, &mut bus, 1);
    assert_eq!(cpu.pc(), 0x0800_0026); // BL pair
    assert_eq!(reg(&cpu, 14), 0x0800_0023); // (0x1E + 4) | 1

    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 0), 0x42); // ADDS in the subroutine

    run(&mut cpu, &mut bus, 1);
    assert_eq!(cpu.pc(), 0x0800_0022); // BX LR returned, still THUMB

    run(&mut cpu, &mut bus, 1);
    // BX r3 with an even address switched back to ARM.
    assert_eq!(cpu.cpsr() & 0x20, 0);
    assert_eq!(cpu.pc(), 0x0800_000C);

    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 1), 0x55);
    assert_eq!(cpu.pc(), 0x0800_0010);
}
