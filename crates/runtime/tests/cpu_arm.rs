//! Micro-programs over the ARM interpreter.
//!
//! Every word below was verified by assembling the equivalent source
//! with `arm-none-eabi-as` (Binutils 2.32) and extracting `.text`, per
//! the project rule that encodings are checked, never hand-derived.

use decode::Condition;
use runtime::bus::Bus;
use runtime::cpu::{Cpu, Mode};

/// Builds a bus whose ROM contains `words` at `0x08000000`, padded with
/// zeros.
fn rom(words: &[u32]) -> Bus {
    let mut bytes = vec![0u8; words.len() * 4 + 0x40];
    for (i, w) in words.iter().enumerate() {
        bytes[i * 4..][..4].copy_from_slice(&w.to_le_bytes());
    }
    Bus::new(bytes)
}

fn run(cpu: &mut Cpu, bus: &mut Bus, steps: u32) {
    for _ in 0..steps {
        cpu.step(bus);
    }
}

/// `(N, Z, C, V)` from the raw CPSR.
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
fn data_processing_basic() {
    let mut bus = rom(&[0xE3A0_00FF, 0xE3A0_1001, 0xE080_2001, 0xE080_3201]);
    let mut cpu = Cpu::new(0x0800_0000);
    run(&mut cpu, &mut bus, 4);
    assert_eq!(reg(&cpu, 0), 0xFF);
    assert_eq!(reg(&cpu, 1), 1);
    assert_eq!(reg(&cpu, 2), 0x100);
    assert_eq!(reg(&cpu, 3), 0xFF + (1 << 4));
}

#[test]
fn shifter_carry_semantics() {
    // mov r0,#0x80000000; movs r1,r0,lsl#1; mov r0,#3; movs r1,r0,lsr#1;
    // mvn r0,#0; movs r2,r0,asr#32; movs r3,r0,lsr#32; mov r0,#0;
    // subs r0,r0,#1; movs r4,r0,rrx; mov r5,#32; movs r6,r0,lsl r5;
    // movs r7,r0,lsr r5; mov r5,#35; movs r8,r0,lsl r5; mov r5,#0;
    // movs r9,r0,lsr r5; movs r10,r0,ror r5
    let words = [
        0xE3A0_0102,
        0xE1B0_1080,
        0xE3A0_0003,
        0xE1B0_10A0,
        0xE3E0_0000,
        0xE1B0_2040,
        0xE1B0_3020,
        0xE3A0_0000,
        0xE250_0001,
        0xE1B0_4060,
        0xE3A0_5020,
        0xE1B0_6510,
        0xE1B0_7530,
        0xE3A0_5023,
        0xE1B0_8510,
        0xE3A0_5000,
        0xE1B0_9530,
        0xE1B0_A570,
    ];
    let mut bus = rom(&words);
    let mut cpu = Cpu::new(0x0800_0000);

    // LSL #1 of 0x80000000: zero result, carry out of bit 31.
    run(&mut cpu, &mut bus, 2);
    assert_eq!(reg(&cpu, 1), 0);
    assert_eq!(flags(&cpu), (false, true, true, false));

    // LSR #1 of 3: 1, carry from bit 0.
    run(&mut cpu, &mut bus, 2);
    assert_eq!(reg(&cpu, 1), 1);
    assert_eq!(flags(&cpu), (false, false, true, false));

    // mvn r0, #0 -> 0xFFFFFFFF, then ASR #32: sign fill, carry = bit 31.
    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 0), 0xFFFF_FFFF);
    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 2), 0xFFFF_FFFF);
    assert_eq!(flags(&cpu), (true, false, true, false));

    // LSR #32: zero result, carry = bit 31.
    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 3), 0);
    assert_eq!(flags(&cpu), (false, true, true, false));

    // subs r0, r0, #1: 0xFFFFFFFF, borrow clears carry.
    run(&mut cpu, &mut bus, 2);
    assert_eq!(reg(&cpu, 0), 0xFFFF_FFFF);
    assert_eq!(flags(&cpu), (true, false, false, false));

    // RRX with carry-in 0: 0x7FFFFFFF, carry = old bit 0.
    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 4), 0x7FFF_FFFF);
    assert_eq!(flags(&cpu), (false, false, true, false));

    // Register-specified LSL by 32: zero, carry = old bit 0.
    run(&mut cpu, &mut bus, 2);
    assert_eq!(reg(&cpu, 6), 0);
    assert_eq!(flags(&cpu), (false, true, true, false));

    // Register-specified LSR by 32: zero, carry = old bit 31.
    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 7), 0);
    assert_eq!(flags(&cpu), (false, true, true, false));

    // Register-specified LSL by 35: zero, carry clear.
    run(&mut cpu, &mut bus, 2);
    assert_eq!(reg(&cpu, 8), 0);
    assert_eq!(flags(&cpu), (false, true, false, false));

    // Register-specified LSR by 0: unchanged, carry unchanged.
    run(&mut cpu, &mut bus, 2);
    assert_eq!(reg(&cpu, 9), 0xFFFF_FFFF);
    assert_eq!(flags(&cpu), (true, false, false, false));

    // Register-specified ROR by 0: unchanged, carry unchanged.
    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 10), 0xFFFF_FFFF);
    assert_eq!(flags(&cpu), (true, false, false, false));
}

#[test]
fn adc_sbc_carry_chains() {
    // mov r0,#0; mvn r0,#0; adds r0,r0,#1; mov r1,#0; adc r2,r1,r1;
    // sbc r3,r1,r1; subs r4,r1,r1; subs r5,r1,#1; sbc r6,r1,r1;
    // rsb r7,r1,#8; rsc r8,r1,#8; and/orr/eor/bic r9..; tst/teq/cmp/cmn
    let words = [
        0xE3A0_0000,
        0xE3E0_0000,
        0xE290_0001,
        0xE3A0_1000,
        0xE0A1_2001,
        0xE0C1_3001,
        0xE051_4001,
        0xE251_5001,
        0xE0C1_6001,
        0xE261_7008,
        0xE2E1_8008,
        0xE000_9001,
        0xE180_9001,
        0xE020_9001,
        0xE1C0_9001,
        0xE110_0001,
        0xE130_0001,
        0xE150_0001,
        0xE170_0001,
    ];
    let mut bus = rom(&words);
    let mut cpu = Cpu::new(0x0800_0000);

    // 0xFFFFFFFF + 1: zero result with carry.
    run(&mut cpu, &mut bus, 3);
    assert_eq!(reg(&cpu, 0), 0);
    assert_eq!(flags(&cpu), (false, true, true, false));

    // ADC consumes the carry: 0 + 0 + 1.
    run(&mut cpu, &mut bus, 2);
    assert_eq!(reg(&cpu, 2), 1);

    // SBC with carry set (no borrow): 0 - 0 - 0.
    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 3), 0);

    // SUBS equal operands: zero with carry set.
    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 4), 0);
    assert_eq!(flags(&cpu), (false, true, true, false));

    // 0 - 1: borrow.
    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 5), 0xFFFF_FFFF);
    assert_eq!(flags(&cpu), (true, false, false, false));

    // SBC propagates the borrow: 0 - 0 - 1.
    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 6), 0xFFFF_FFFF);
    assert_eq!(flags(&cpu), (true, false, false, false));

    run(&mut cpu, &mut bus, 2);
    assert_eq!(reg(&cpu, 7), 8); // RSB: 8 - 0
    assert_eq!(reg(&cpu, 8), 7); // RSC with borrow: 8 - 0 - 1

    // The adds above left r0 = 0, so the logic ops all see (0, 0).
    run(&mut cpu, &mut bus, 4);
    assert_eq!(reg(&cpu, 9), 0);

    // TST/TEQ pass the unshifted register carry through (C stays 0);
    // CMP of equal zeros sets Z and C (no borrow).
    run(&mut cpu, &mut bus, 3);
    assert_eq!(flags(&cpu), (false, true, true, false));
    // CMN of equal zeros: Z set, C clear (no carry out of 0 + 0).
    run(&mut cpu, &mut bus, 1);
    assert_eq!(flags(&cpu), (false, true, false, false));
}

#[test]
fn signed_overflow_flags() {
    // mov r0,#0x40000000; adds r0,r0,#0x40000000 -> V set, N set, C clear.
    let mut bus = rom(&[0xE3A0_0101, 0xE290_0101]);
    let mut cpu = Cpu::new(0x0800_0000);
    run(&mut cpu, &mut bus, 2);
    assert_eq!(reg(&cpu, 0), 0x8000_0000);
    assert_eq!(flags(&cpu), (true, false, false, true));

    // mov r0,#0x80000000; subs r0,r0,#1 -> 0x7FFFFFFF: V set, C set.
    let mut bus = rom(&[0xE3A0_0102, 0xE250_0001]);
    let mut cpu = Cpu::new(0x0800_0000);
    run(&mut cpu, &mut bus, 2);
    assert_eq!(reg(&cpu, 0), 0x7FFF_FFFF);
    assert_eq!(flags(&cpu), (false, false, true, true));
}

#[test]
fn multiply_and_accumulate() {
    // mov r1,#3; mov r2,#7; mul r0,r2,r1; mov r3,#100; mla r0,r2,r1,r3;
    // mvn r1,#0; mov r2,#2; umull r4,r5,r2,r1; mov r6,#1; umull r4,r5,r6,r6;
    // umlal r4,r5,r2,r1; smull r4,r5,r2,r1; mov r2,#5; mvn r1,#0;
    // smlal r4,r5,r2,r1
    let words = [
        0xE3A0_1003,
        0xE3A0_2007,
        0xE000_0192,
        0xE3A0_3064,
        0xE020_3192,
        0xE3E0_1000,
        0xE3A0_2002,
        0xE085_4192,
        0xE3A0_6001,
        0xE085_4696,
        0xE0A5_4192,
        0xE0C5_4192,
        0xE3A0_2005,
        0xE3E0_1000,
        0xE0E5_4192,
    ];
    let mut bus = rom(&words);
    let mut cpu = Cpu::new(0x0800_0000);

    run(&mut cpu, &mut bus, 3);
    assert_eq!(reg(&cpu, 0), 21); // 7 * 3
    run(&mut cpu, &mut bus, 2);
    assert_eq!(reg(&cpu, 0), 121); // 7 * 3 + 100

    run(&mut cpu, &mut bus, 3);
    // UMULL: 2 * 0xFFFFFFFF = 0x1_FFFF_FFFE.
    assert_eq!(reg(&cpu, 4), 0xFFFF_FFFE);
    assert_eq!(reg(&cpu, 5), 1);

    run(&mut cpu, &mut bus, 2);
    assert_eq!(reg(&cpu, 4), 1); // UMULL 1*1 resets both halves
    assert_eq!(reg(&cpu, 5), 0);

    run(&mut cpu, &mut bus, 1);
    // UMLAL: 1 + 2 * 0xFFFFFFFF = 0x1_FFFF_FFFF.
    assert_eq!(reg(&cpu, 4), 0xFFFF_FFFF);
    assert_eq!(reg(&cpu, 5), 1);

    run(&mut cpu, &mut bus, 1);
    // SMULL: 2 * -1 = -2.
    assert_eq!(reg(&cpu, 4), 0xFFFF_FFFE);
    assert_eq!(reg(&cpu, 5), 0xFFFF_FFFF);

    run(&mut cpu, &mut bus, 3);
    // SMLAL: -2 + 5 * -1 = -7.
    assert_eq!(reg(&cpu, 4), 0xFFFF_FFF9);
    assert_eq!(reg(&cpu, 5), 0xFFFF_FFFF);
}

#[test]
fn swap_word_and_byte() {
    // mov r1,#0x03000000; mov r0,#0x11; str r0,[r1]; mov r0,#0x22;
    // swp r2,r0,[r1]; swpb r3,r0,[r1]
    let words = [
        0xE3A0_1403,
        0xE3A0_0011,
        0xE581_0000,
        0xE3A0_0022,
        0xE101_2090,
        0xE141_3090,
    ];
    let mut bus = rom(&words);
    let mut cpu = Cpu::new(0x0800_0000);
    run(&mut cpu, &mut bus, 6);
    assert_eq!(reg(&cpu, 2), 0x11); // loaded old value
    assert_eq!(reg(&cpu, 3), 0x22); // byte swap loads the stored byte
    assert_eq!(bus.read_u32(0x0300_0000), 0x22);
}

#[test]
fn load_store_widths_and_addressing() {
    // Give the LDRSH case a negative halfword: index 13 becomes
    // `mov r0, #0x8000` (encoding verified against GNU as).
    let words = [
        0xE3A0_1403, // mov r1, #0x03000000
        0xE3A0_0044, // mov r0, #0x44
        0xE581_0000, // str r0, [r1]
        0xE591_2000, // ldr r2, [r1]
        0xE3A0_0080, // mov r0, #0x80
        0xE5C1_0001, // strb r0, [r1, #1]
        0xE5D1_2001, // ldrb r2, [r1, #1]
        0xE3A0_00FF, // mov r0, #0xFF
        0xE1C1_00B4, // strh r0, [r1, #4]
        0xE1D1_20B4, // ldrh r2, [r1, #4]
        0xE3A0_0080, // mov r0, #0x80
        0xE5C1_0008, // strb r0, [r1, #8]
        0xE1D1_20D8, // ldrsb r2, [r1, #8]
        0xE1A0_0000, // mov r0, r0 (nop; halfword written by the test)
        0xE1D1_20FA, // ldrsh r2, [r1, #10]
        0xE3A0_0311, // mov r0, #0x44000000
        0xE380_0833, // orr r0, r0, #0x330000
        0xE380_0C22, // orr r0, r0, #0x2200
        0xE380_0011, // orr r0, r0, #0x11
        0xE581_0000, // str r0, [r1]      -> 0x44332211
        0xE591_2002, // ldr r2, [r1, #2]  -> unaligned rotation
        0xE5B1_2004, // ldr r2, [r1, #4]!
        0xE491_2004, // ldr r2, [r1], #4
        0xE1A0_200F, // mov r2, pc
        0xE581_F000, // str pc, [r1]
        0xE591_2000, // ldr r2, [r1]
    ];
    let mut bus = rom(&words);
    let mut cpu = Cpu::new(0x0800_0000);
    let base = 0x0300_0000u32;

    run(&mut cpu, &mut bus, 4);
    assert_eq!(reg(&cpu, 2), 0x44);

    run(&mut cpu, &mut bus, 3);
    assert_eq!(reg(&cpu, 2), 0x80); // LDRB zero-extends

    run(&mut cpu, &mut bus, 3);
    assert_eq!(reg(&cpu, 2), 0x00FF); // LDRH zero-extends

    run(&mut cpu, &mut bus, 3);
    assert_eq!(reg(&cpu, 2), 0xFFFF_FF80); // LDRSB sign-extends

    bus.write_u16(base + 10, 0x8000);
    run(&mut cpu, &mut bus, 2);
    assert_eq!(reg(&cpu, 2), 0xFFFF_8000); // LDRSH sign-extends

    run(&mut cpu, &mut bus, 6);
    assert_eq!(reg(&cpu, 0), 0x4433_2211);
    // Unaligned word load rotates by (addr & 3) * 8.
    assert_eq!(reg(&cpu, 2), 0x2211_4433);

    // Pre-indexed with writeback: r1 advances by 4, load sees base+4.
    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 2), 0x0000_00FF);
    assert_eq!(reg(&cpu, 1), base + 4);

    // Post-indexed with writeback: load uses the old base.
    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 2), 0x0000_00FF);
    assert_eq!(reg(&cpu, 1), base + 8);

    // PC reads as instruction address + 8 (index 23 = 0x5C).
    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 2), 0x0800_0000 + (23 * 4) as u32 + 8);
    // STR PC stores the instruction address + 12 (index 24 = 0x60).
    run(&mut cpu, &mut bus, 2);
    assert_eq!(reg(&cpu, 2), 0x0800_0000 + (24 * 4) as u32 + 12);
}

#[test]
fn block_transfer_modes() {
    // mov r0,#0x03000000; add r0,r0,#0x100; mov r1,#1; mov r2,#2;
    // mov r3,#3; stmia r0!,{r1,r2,r3}; mov r1,#0; mov r2,#0; mov r3,#0;
    // ldmdb r0!,{r1,r2,r3}; ldm r0,{r1,r2}; stmib r0,{r1,r2};
    // ldmda r0!,{r1,r2}; mov sp,#0x03000000; add sp,sp,#0x200;
    // push {r1,r2}; pop {r1,r2}
    let words = [
        0xE3A0_0403,
        0xE280_0C01,
        0xE3A0_1001,
        0xE3A0_2002,
        0xE3A0_3003,
        0xE8A0_000E,
        0xE3A0_1000,
        0xE3A0_2000,
        0xE3A0_3000,
        0xE930_000E,
        0xE890_0006,
        0xE980_0006,
        0xE830_0006,
        0xE3A0_D403,
        0xE28D_DC02,
        0xE92D_0006,
        0xE8BD_0006,
    ];
    let mut bus = rom(&words);
    let mut cpu = Cpu::new(0x0800_0000);
    let area = 0x0300_0100u32;

    // STMIA stores ascending and writes back base + 12.
    run(&mut cpu, &mut bus, 6);
    assert_eq!(bus.read_u32(area), 1);
    assert_eq!(bus.read_u32(area + 4), 2);
    assert_eq!(bus.read_u32(area + 8), 3);
    assert_eq!(reg(&cpu, 0), area + 12);

    // Zero the registers, then LDMDB walks down and restores base - 12.
    run(&mut cpu, &mut bus, 4);
    assert_eq!(reg(&cpu, 0), area);
    assert_eq!((reg(&cpu, 1), reg(&cpu, 2), reg(&cpu, 3)), (1, 2, 3));

    // LDM without writeback leaves the base alone.
    run(&mut cpu, &mut bus, 1);
    assert_eq!((reg(&cpu, 1), reg(&cpu, 2)), (1, 2));
    assert_eq!(reg(&cpu, 0), area);

    // STMIB without writeback stores at base+4 and base+8.
    run(&mut cpu, &mut bus, 1);
    assert_eq!(bus.read_u32(area + 4), 1);
    assert_eq!(bus.read_u32(area + 8), 2);

    // LDMDA with writeback: descending from base, base ends at base - 8.
    run(&mut cpu, &mut bus, 1);
    assert_eq!((reg(&cpu, 1), reg(&cpu, 2)), (0, 1));
    assert_eq!(reg(&cpu, 0), area - 8);

    // Full-descending push/pop store and restore the two registers.
    run(&mut cpu, &mut bus, 2);
    assert_eq!(reg(&cpu, 13), 0x0300_0200);
    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 13), 0x0300_01F8);
    assert_eq!(bus.read_u32(0x0300_01FC), 1);
    assert_eq!(bus.read_u32(0x0300_01F8), 0);
    run(&mut cpu, &mut bus, 1);
    assert_eq!((reg(&cpu, 1), reg(&cpu, 2)), (0, 1));
    assert_eq!(reg(&cpu, 13), 0x0300_0200);
}

#[test]
fn psr_transfers_between_modes() {
    // msr CPSR_c,#0xD3; mov sp,#0x1100; mov r0,#0x03000000;
    // stmia r0,{sp}; ldr r1,[r0]; msr CPSR_c,#0x11; mov sp,#0x2200;
    // msr CPSR_c,#0xD3; stmia r0,{sp}^; ldr r2,[r0]; msr CPSR_c,#0x12;
    // mov sp,#0x3300; msr CPSR_c,#0xD3; mov r3,sp; msr CPSR_c,#0x10;
    // mov r4,sp; msr cpsr_f,#0x80000000; mrs r5,cpsr; msr CPSR_c,#0xD3;
    // mrs r6,cpsr; swi 0; mov r7,sp; movs pc,lr
    let words = [
        0xE321_F0D3,
        0xE3A0_DC11,
        0xE3A0_0403,
        0xE880_2000,
        0xE590_1000,
        0xE321_F011,
        0xE3A0_DC22,
        0xE321_F0D3,
        0xE8C0_2000,
        0xE590_2000,
        0xE321_F012,
        0xE3A0_DC33,
        0xE321_F0D3,
        0xE1A0_300D,
        0xE321_F010,
        0xE1A0_400D,
        0xE328_F102,
        0xE10F_5000,
        0xE321_F0D3,
        0xE10F_6000,
        0xEF00_0000,
        0xE1A0_700D,
        0xE1B0_F00E,
    ];
    let mut bus = rom(&words);
    let mut cpu = Cpu::new(0x0800_0000);

    // SVC sp was stored through the plain STM.
    run(&mut cpu, &mut bus, 5);
    assert_eq!(reg(&cpu, 1), 0x1100);

    // FIQ has its own SP; the S-bit STM stores the USER sp instead.
    run(&mut cpu, &mut bus, 5);
    assert_eq!(reg(&cpu, 2), 0);

    // IRQ banks SP too; returning to SVC restores 0x1100.
    run(&mut cpu, &mut bus, 4);
    assert_eq!(reg(&cpu, 3), 0x1100);

    // Entering user mode: SP reads as the user bank (0).
    run(&mut cpu, &mut bus, 2);
    assert_eq!(reg(&cpu, 4), 0);

    // Flag writes work in user mode; control writes do not.
    run(&mut cpu, &mut bus, 2);
    assert_eq!(reg(&cpu, 5), 0x8000_0010);
    run(&mut cpu, &mut bus, 2);
    assert_eq!(reg(&cpu, 6), 0x8000_0010);
    assert_eq!(cpu.mode(), Mode::User);

    // SWI enters supervisor; SPSR preserved the user CPSR.
    run(&mut cpu, &mut bus, 1);
    assert_eq!(cpu.pc(), 8);
    assert_eq!(cpu.mode(), Mode::Supervisor);
    assert_eq!(cpu.spsr_for_test(), Some(0x8000_0010));
    assert_eq!(reg(&cpu, 14), 0x0800_0000 + (20 * 4) as u32 + 4);

    // movs pc, lr restores the user CPSR from SPSR.
    cpu.set_pc(0x0800_0000 + 22 * 4);
    run(&mut cpu, &mut bus, 1);
    assert_eq!(cpu.mode(), Mode::User);
    assert_eq!(cpu.cpsr(), 0x8000_0010);
    assert_eq!(cpu.pc(), 0x0800_0000 + (21 * 4) as u32);
    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 7), 0);
}

#[test]
fn swi_entry_state() {
    let mut bus = rom(&[0xEF12_3456]);
    let mut cpu = Cpu::new(0x0800_0000);
    cpu.set_cpsr_raw(0x0000_0053); // SVC, IRQ masked, FIQ clear
    run(&mut cpu, &mut bus, 1);
    assert_eq!(cpu.pc(), 8);
    assert_eq!(cpu.mode(), Mode::Supervisor);
    assert_eq!(cpu.spsr_for_test(), Some(0x53));
    assert_eq!(reg(&cpu, 14), 0x0800_0004);
    assert_eq!(cpu.cpsr() & 0x80, 0x80); // I set on entry
}

#[test]
fn undefined_instruction_exception() {
    let mut bus = rom(&[0xE7F0_00F0]); // udf
    let mut cpu = Cpu::new(0x0800_0000);
    run(&mut cpu, &mut bus, 1);
    assert_eq!(cpu.pc(), 4);
    assert_eq!(cpu.mode(), Mode::Undefined);
    assert_eq!(cpu.spsr_for_test(), Some(0xD3));
    assert_eq!(reg(&cpu, 14), 0x0800_0004);
}

#[test]
fn irq_entry_and_return() {
    let mut bus = rom(&[0xE3A0_0001, 0xE25E_F004]);
    let mut cpu = Cpu::new(0x0800_0000);
    cpu.set_cpsr_raw(0x0000_0013); // SVC with I clear
    cpu.set_irq_line(true);
    run(&mut cpu, &mut bus, 1);
    assert_eq!(cpu.pc(), 0x18);
    assert_eq!(cpu.mode(), Mode::Irq);
    assert_eq!(cpu.spsr_for_test(), Some(0x13));
    assert_eq!(reg(&cpu, 14), 0x0800_0004);
    assert_eq!(cpu.cpsr() & 0x80, 0x80);

    // subs pc, lr, #4 re-executes the interrupted instruction.
    cpu.set_irq_line(false);
    cpu.set_pc(0x0800_0004);
    run(&mut cpu, &mut bus, 1);
    assert_eq!(cpu.pc(), 0x0800_0000);
    assert_eq!(cpu.mode(), Mode::Supervisor);
    assert_eq!(cpu.cpsr(), 0x13);
    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 0), 1);
}

#[test]
fn bx_sets_and_clears_thumb_state() {
    // mov r0,#0x08000000; orr r0,r0,#0x21; bx r0
    let mut bus = rom(&[0xE3A0_0302, 0xE380_0021, 0xE12F_FF10]);
    let mut cpu = Cpu::new(0x0800_0000);
    run(&mut cpu, &mut bus, 3);
    assert_eq!(cpu.pc(), 0x0800_0020);
    assert_eq!(cpu.cpsr() & 0x20, 0x20); // T set, but no THUMB fetch yet

    // bx to an even address returns to ARM state and keeps executing.
    let mut words = [0u32; 16];
    words[0] = 0xE3A0_0302; // mov r0, #0x08000000
    words[1] = 0xE380_0030; // orr r0, r0, #0x30
    words[2] = 0xE12F_FF10; // bx r0
    words[12] = 0xE3A0_1077; // mov r1, #0x77  (at 0x08000030)
    let mut bus = rom(&words);
    let mut cpu = Cpu::new(0x0800_0000);
    run(&mut cpu, &mut bus, 4);
    assert_eq!(cpu.pc(), 0x0800_0034);
    assert_eq!(cpu.cpsr() & 0x20, 0);
    assert_eq!(reg(&cpu, 1), 0x77);
}

#[test]
fn branch_and_link() {
    // b +8; mov r0,#0xEE; bl +8; mov pc,lr; mov r0,#0x12; mov pc,lr
    let words = [
        0xEA00_0000,
        0xE3A0_00EE,
        0xEB00_0000,
        0xE1A0_F00E,
        0xE3A0_0012,
        0xE1A0_F00E,
    ];
    let mut bus = rom(&words);
    let mut cpu = Cpu::new(0x0800_0000);

    run(&mut cpu, &mut bus, 1);
    assert_eq!(cpu.pc(), 0x0800_0008); // skipped the 0xEE
    assert_eq!(reg(&cpu, 0), 0);

    run(&mut cpu, &mut bus, 1);
    assert_eq!(cpu.pc(), 0x0800_0010);
    assert_eq!(reg(&cpu, 14), 0x0800_000C);

    run(&mut cpu, &mut bus, 2);
    assert_eq!(reg(&cpu, 0), 0x12);

    run(&mut cpu, &mut bus, 1);
    assert_eq!(cpu.pc(), 0x0800_000C); // mov pc, lr returned to the call site
}

#[test]
fn condition_field_sweep() {
    // MOV<cond> r0, #1 executed under favorable and unfavorable flags.
    let cases: [(Condition, u32, bool); 16] = [
        (Condition::Eq, 0x4000_0000, true), // Z
        (Condition::Eq, 0, false),
        (Condition::Ne, 0, true),
        (Condition::Ne, 0x4000_0000, false),
        (Condition::Cs, 0x2000_0000, true), // C
        (Condition::Cs, 0, false),
        (Condition::Cc, 0, true),
        (Condition::Cc, 0x2000_0000, false),
        (Condition::Mi, 0x8000_0000, true), // N
        (Condition::Mi, 0, false),
        (Condition::Pl, 0, true),
        (Condition::Pl, 0x8000_0000, false),
        (Condition::Vs, 0x1000_0000, true), // V
        (Condition::Vs, 0, false),
        (Condition::Vc, 0, true),
        (Condition::Vc, 0x1000_0000, false),
    ];
    for (cond, cpsr_flags, expect) in cases {
        let mut bus = rom(&[((u32::from(cond.bits()) << 28) | 0x03A0_0001)]);
        let mut cpu = Cpu::new(0x0800_0000);
        cpu.set_cpsr_raw(cpsr_flags | 0x13);
        run(&mut cpu, &mut bus, 1);
        assert_eq!(
            reg(&cpu, 0),
            u32::from(expect),
            "{cond:?} with {cpsr_flags:#010x}"
        );
    }

    // Compound conditions.
    let cases: [(Condition, u32, bool); 17] = [
        (Condition::Hi, 0x2000_0000, true),  // C and !Z
        (Condition::Hi, 0x6000_0000, false), // C and Z
        (Condition::Ls, 0x4000_0000, true),  // Z or !C
        (Condition::Ls, 0, true),            // C clear
        (Condition::Ls, 0x2000_0000, false), // C set, Z clear
        (Condition::Ge, 0x9000_0000, true),  // N == V
        (Condition::Ge, 0x8000_0000, false), // N != V
        (Condition::Lt, 0x8000_0000, true),  // N != V
        (Condition::Lt, 0x9000_0000, false),
        (Condition::Gt, 0, true), // !Z and N == V
        (Condition::Gt, 0x4000_0000, false),
        (Condition::Le, 0x4000_0000, true), // Z or N != V
        (Condition::Le, 0, false),
        (Condition::Le, 0x8000_0000, true), // N != V
        (Condition::Al, 0xF000_0000, true),
        (Condition::Ge, 0, true),           // N == V == 0
        (Condition::Lt, 0x1000_0000, true), // V set, N clear
    ];
    for (cond, cpsr_flags, expect) in cases {
        let mut bus = rom(&[((u32::from(cond.bits()) << 28) | 0x03A0_0001)]);
        let mut cpu = Cpu::new(0x0800_0000);
        cpu.set_cpsr_raw(cpsr_flags | 0x13);
        run(&mut cpu, &mut bus, 1);
        assert_eq!(
            reg(&cpu, 0),
            u32::from(expect),
            "{cond:?} with {cpsr_flags:#010x}"
        );
    }
}

#[test]
fn ldm_exception_return() {
    // msr CPSR_c,#0xD3; mov sp,#0x03000000; add sp,sp,#0x100; swi 0;
    // mov r0,#0xEE; b .; ldm sp!,{r0,pc}^; nop; mov r7,#0x77; nop
    let words = [
        0xE321_F0D3,
        0xE3A0_D403,
        0xE28D_DC01,
        0xEF00_0000,
        0xE3A0_00EE,
        0xEAFF_FFFE,
        0xE8FD_8001,
        0xE1A0_0000,
        0xE3A0_7077,
        0xE1A0_0000,
    ];
    let mut bus = rom(&words);
    let mut cpu = Cpu::new(0x0800_0000);
    run(&mut cpu, &mut bus, 4);
    assert_eq!(cpu.pc(), 8);
    assert_eq!(cpu.mode(), Mode::Supervisor);

    // Stack image for the exception return: r0 and pc.
    bus.write_u32(0x0300_0100, 0x0800_0024);
    bus.write_u32(0x0300_0104, 0x0800_0020);
    cpu.set_pc(0x0800_0018);
    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 0), 0x0800_0024);
    assert_eq!(reg(&cpu, 13), 0x0300_0108);
    assert_eq!(cpu.pc(), 0x0800_0020);
    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 7), 0x77);
    // The 0xEE after the SWI never executed.
    assert_eq!(reg(&cpu, 0), 0x0800_0024);
}

#[test]
fn cycles_accumulate_per_step() {
    let mut bus = rom(&[0xE3A0_0001, 0xE3A0_1002, 0xE080_0001, 0xE1A0_0000]);
    let mut cpu = Cpu::new(0x0800_0000);
    let mut total = 0;
    for _ in 0..4 {
        let c = cpu.step(&mut bus);
        assert!(c > 0);
        total += c;
    }
    assert_eq!(cpu.cycles(), u64::from(total));
}

#[test]
fn block_transfer_and_halfword_quirks() {
    // stmia r0!,{r0,r1}; ldmia r0!,{r0,r5}; stmia r1!,{} (empty rlist,
    // hand-encoded by clearing the verified stmia r1!,{pc} list bits);
    // ldrh r2,[r1,#3]; ldrsh r2,[r1,#3]
    let words = [
        0xE8A0_0003,
        0xE8B0_0021,
        0xE8A1_0000,
        0xE1D1_20B3,
        0xE1D1_20F3,
    ];
    let mut bus = rom(&words);
    let mut cpu = Cpu::new(0x0800_0000);
    let area = 0x0300_0100u32;

    // STM with the base as the first list entry stores the OLD base
    // (ARMv4 de-facto behavior, GBATEK "Strange Effects on Invalid
    // Rlist's").
    cpu.set_reg_for_test(0, area);
    cpu.set_reg_for_test(1, 0xBB);
    run(&mut cpu, &mut bus, 1);
    assert_eq!(bus.read_u32(area), area);
    assert_eq!(bus.read_u32(area + 4), 0xBB);
    assert_eq!(reg(&cpu, 0), area + 8);

    // LDM with the base in the list performs no writeback (ARMv4): r0
    // keeps the loaded value, not base + 8.
    cpu.set_reg_for_test(0, area);
    bus.write_u32(area, 0x1234);
    bus.write_u32(area + 4, 0x5678);
    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 0), 0x1234);
    assert_eq!(reg(&cpu, 5), 0x5678);

    // Empty rlist (ARMv4): R15 is stored and the base moves by 0x40.
    // The store happens at instruction index 2, so R15 = 0x08000008+12.
    cpu.set_reg_for_test(1, area);
    run(&mut cpu, &mut bus, 1);
    assert_eq!(bus.read_u32(area), 0x0800_0000 + 2 * 4 + 12);
    assert_eq!(reg(&cpu, 1), area + 0x40);

    // Odd-address LDRH reads the aligned halfword rotated right by 8.
    cpu.set_reg_for_test(1, area);
    bus.write_u16(area + 2, 0xABCD);
    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 2), 0xCD00_00AB);

    // Odd-address LDRSH degenerates to LDRSB on ARM7.
    run(&mut cpu, &mut bus, 1);
    assert_eq!(reg(&cpu, 2), 0xFFFF_FFAB);
}
