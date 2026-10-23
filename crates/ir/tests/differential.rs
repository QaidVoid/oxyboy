//! Differential verification: randomized single instructions
//! executed on the interpreter and evaluated through their lifted IR
//! must agree on the full machine state delta (registers, flags, and
//! RAM).
//!
//! Exception-producing and control-flow classes (branches, SWI,
//! undefined encodings, PSR transfers, pc-target writes) are outside
//! the twenty-slot IR model; the generator rejects them by decoding
//! each candidate and matching it against the included classes.

use analysis::{FlagMap, Flags};
use decode::{ArmInstr, Register, ThumbInstr, decode_arm, decode_thumb};
use ir::eval::Machine;
use ir::lift::{FunctionRegion, lift_function};
use runtime::bus::Bus;
use runtime::cpu::Cpu;

/// Deterministic xorshift; a fixed seed keeps failures reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn word(&mut self) -> u32 {
        (self.next() >> 16) as u32
    }

    fn below(&mut self, n: u32) -> u32 {
        self.word() % n
    }

    fn bit(&mut self) -> bool {
        self.word() & 1 == 1
    }
}

/// Where the instruction under test sits in the image (past header).
const CODE_OFF: u32 = 0x100;
/// Image size: big enough for pc-relative literal loads to stay inside.
const IMAGE_SIZE: usize = 4096;
/// How much EWRAM the harness pre-fills with random data.
const EWRAM_PREFILL: usize = 0x2000;

/// Runs one randomized case: lift the code, run it on both the
/// interpreter and the evaluator, compare full state.
fn run_case(state: analysis::State, code: &[u8], rng: &mut Rng) {
    let mut bytes = vec![0u8; IMAGE_SIZE];
    bytes[CODE_OFF as usize..CODE_OFF as usize + code.len()].copy_from_slice(code);

    // Shared random machine state: 15 registers, 4 flags, an EWRAM
    // pre-fill. Both sides see exactly these values.
    let mut regs = [0u32; 15];
    for r in &mut regs {
        *r = rng.word();
    }
    let flags = [rng.bit(), rng.bit(), rng.bit(), rng.bit()];
    let mut ewram = vec![0u8; EWRAM_PREFILL];
    for chunk in ewram.as_chunks_mut::<4>().0 {
        chunk.copy_from_slice(&rng.word().to_le_bytes());
    }

    // Lifted side.
    let image = rom::Image::from_bytes(bytes.clone()).expect("parses");
    let mut map = FlagMap::from_image(&image);
    let base = map.canonical().base();
    let start = base + CODE_OFF;
    let len = code.len() as u32;
    map.mark_range(start, start + len, Flags::EXEC);
    map.set(start, Flags::EXEC | Flags::FUNC_START);
    let function = lift_function(
        &image,
        &map,
        FunctionRegion {
            start,
            end: start + len,
            state,
        },
    );
    let block = &function.blocks[0];
    assert!(!block.insts.is_empty(), "no ops lifted");
    let ir::Terminator::Return { state: end_state } = &block.term else {
        panic!("expected a returning block, got {:?}", block.term);
    };
    let mut machine = Machine::new(256, bytes.clone());
    machine.bind_params(&block.params);
    for (i, r) in regs.iter().enumerate() {
        machine.set_slot(i, *r);
    }
    // The pc slot is never read by the included classes.
    machine.set_slot(15, 0);
    for (f, v) in flags.iter().enumerate() {
        machine.set_slot(16 + f, u32::from(*v));
    }
    machine.ewram_mut()[..EWRAM_PREFILL].copy_from_slice(&ewram);
    machine.eval(&block.insts, end_state);

    // Interpreter side.
    let mut bus = Bus::new(bytes);
    for (i, chunk) in ewram.as_chunks::<4>().0.iter().enumerate() {
        let word = u32::from_le_bytes(*chunk);
        bus.write_u32(0x0200_0000 + i as u32 * 4, word);
    }
    let mut cpu = Cpu::new(start);
    let t_bit = matches!(state, analysis::State::Thumb);
    let mut cpsr = 0x0000_00D3 | u32::from(t_bit) << 5;
    for (f, v) in flags.iter().enumerate() {
        cpsr |= u32::from(*v) << (31 - f);
    }
    cpu.set_cpsr_raw(cpsr);
    for (i, r) in regs.iter().enumerate() {
        cpu.set_reg_for_test(i as u8, *r);
    }
    cpu.step(&mut bus);

    // Compare: registers r0..r14, flags, and both RAM blocks.
    let mut fail = String::new();
    for i in 0..15 {
        let (a, b) = (machine.slot(i), cpu.reg_for_test(i as u8));
        if a != b {
            fail.push_str(&format!("\n  r{i}: ir={a:#010x} cpu={b:#010x}"));
        }
    }
    let cpsr = cpu.cpsr();
    for (f, bit) in [(16usize, 31u32), (17, 30), (18, 29), (19, 28)] {
        let (a, b) = (machine.slot(f), (cpsr >> bit) & 1);
        if a != b {
            let name = ["n", "z", "c", "v"][f - 16];
            fail.push_str(&format!("\n  {name}: ir={a} cpu={b}"));
        }
    }
    if machine.ewram() != bus.ewram() {
        fail.push_str("\n  ewram: diverged");
    }
    if machine.iwram() != bus.iwram() {
        fail.push_str("\n  iwram: diverged");
    }
    if !fail.is_empty() {
        let code_hex: String = code
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        let ir_text = ir::print::fmt_function(&function);
        let seeds: String = regs
            .iter()
            .enumerate()
            .map(|(i, r)| format!("r{i}={r:#010x}"))
            .chain(flags.iter().enumerate().map(|(i, f)| format!("f{i}={f}")))
            .collect::<Vec<_>>()
            .join(" ");
        panic!("differential mismatch for {code_hex}:{fail}\nseeds: {seeds}\n{ir_text}");
    }
}

/// Generates a random ARM word decoding to an included class.
fn random_arm_word(rng: &mut Rng) -> Option<u32> {
    for _ in 0..64 {
        let word = match rng.below(7) {
            0..=1 => random_data_processing(rng),
            2 => random_multiply(rng),
            3 => random_multiply_long(rng),
            4 => random_swap(rng),
            5 => random_load_store(rng),
            _ => random_transfer_group(rng),
        };
        if let Some(w) = word
            && arm_class_included(&decode_arm(w))
        {
            return Some(w);
        }
    }
    None
}

fn arm_class_included(instr: &ArmInstr) -> bool {
    match instr {
        ArmInstr::DataProcessing { rd, .. } => *rd != Register::Pc,
        ArmInstr::Multiply { rd, .. } => *rd != Register::Pc,
        ArmInstr::MultiplyLong { rd_hi, rd_lo, .. } => {
            *rd_hi != Register::Pc && *rd_lo != Register::Pc && rd_hi != rd_lo
        }
        ArmInstr::Swap { rd, .. } => *rd != Register::Pc,
        ArmInstr::LoadStore { rd, .. } => *rd != Register::Pc,
        ArmInstr::LoadStoreHalfwordSigned { rd, .. } => *rd != Register::Pc,
        ArmInstr::BlockTransfer {
            psr_and_force_user,
            registers,
            ..
        } => !psr_and_force_user && !registers.contains(Register::Pc),
        _ => false,
    }
}

fn random_data_processing(rng: &mut Rng) -> Option<u32> {
    let cond = rng.below(15) << 28;
    let opcode = rng.below(16) << 21;
    // TST/TEQ/CMP/CMN with S=0 are the PSR encodings; force S there.
    let s = if (0x0100_0000..0x0180_0000).contains(&opcode) {
        1 << 20
    } else {
        u32::from(rng.bit()) << 20
    };
    let rn = rng.below(16) << 16;
    let rd = rng.below(15) << 12;
    let operand2 = if rng.bit() {
        // Immediate: rotate and 8-bit value.
        1 << 25 | rng.below(16) << 8 | rng.below(256)
    } else if rng.bit() {
        // Register with immediate shift.
        rng.below(16) | rng.below(31) << 7 | rng.below(4) << 5
    } else {
        // Register with register-specified shift.
        rng.below(16) | rng.below(16) << 8 | 1 << 4
    };
    Some(cond | 0x0200_0000 | opcode | s | rn | rd | operand2)
}

fn random_multiply(rng: &mut Rng) -> Option<u32> {
    let cond = rng.below(15) << 28;
    let a = u32::from(rng.bit()) << 21;
    let s = u32::from(rng.bit()) << 20;
    let rd = rng.below(15) << 16;
    let rn = rng.below(16) << 12;
    let rs = rng.below(16) << 8;
    let rm = rng.below(16);
    Some(cond | a | s | rd | rn | rs | rm | 0x90)
}

fn random_multiply_long(rng: &mut Rng) -> Option<u32> {
    let cond = rng.below(15) << 28;
    let u = u32::from(rng.bit()) << 22;
    let a = u32::from(rng.bit()) << 21;
    let s = u32::from(rng.bit()) << 20;
    let rd_hi = rng.below(15) << 16;
    let mut rd_lo = rng.below(15) << 12;
    if rd_hi == rd_lo {
        rd_lo = (rd_lo + (1 << 12)) & 0xE000;
    }
    Some(cond | u | a | s | rd_hi | rd_lo | rng.below(16) << 8 | rng.below(16) | 0x90)
}

fn random_swap(rng: &mut Rng) -> Option<u32> {
    let cond = rng.below(15) << 28;
    let b = u32::from(rng.bit()) << 22;
    let rd = rng.below(15) << 12;
    let rn = rng.below(16) << 16;
    let rm = rng.below(16);
    Some(cond | b | rd | rn | rm | 0x100_0090)
}

fn random_load_store(rng: &mut Rng) -> Option<u32> {
    let cond = rng.below(15) << 28;
    let p = u32::from(rng.bit()) << 24;
    let u = u32::from(rng.bit()) << 23;
    let b = u32::from(rng.bit()) << 22;
    let w = u32::from(rng.bit()) << 21;
    let l = u32::from(rng.bit()) << 20;
    let rn = rng.below(16) << 16;
    let rd = rng.below(15) << 12;
    let offset = if rng.bit() {
        rng.below(0x1000)
    } else {
        rng.below(16) | rng.below(31) << 7 | rng.below(4) << 5
    };
    Some(cond | p | u | b | w | l | rn | rd | offset)
}

fn random_transfer_group(rng: &mut Rng) -> Option<u32> {
    match rng.below(3) {
        0 => random_load_store(rng),
        1 => {
            // Halfword/signed transfers (bit 25 clear, bit 22 selects
            // the immediate offset form).
            let cond = rng.below(15) << 28;
            let p = u32::from(rng.bit()) << 24;
            let u = u32::from(rng.bit()) << 23;
            let imm = u32::from(rng.bit()) << 22;
            let w = u32::from(rng.bit()) << 21;
            let l = u32::from(rng.bit()) << 20;
            let rn = rng.below(16) << 16;
            let rd = rng.below(15) << 12;
            let kind = rng.below(4) << 5;
            let offset = if imm == 0 {
                rng.below(16)
            } else {
                rng.below(16) << 4 | rng.below(16)
            };
            Some(cond | p | u | imm | w | l | rn | rd | kind | offset | 0x90)
        }
        // Block transfer: cond 100 P U S W L Rn list. The class filter
        // drops psr_and_force_user and pc-in-list encodings.
        _ => {
            let cond = rng.below(15) << 28;
            let p = u32::from(rng.bit()) << 24;
            let u = u32::from(rng.bit()) << 23;
            let s = u32::from(rng.bit()) << 22;
            let w = u32::from(rng.bit()) << 21;
            let l = u32::from(rng.bit()) << 20;
            let rn = rng.below(16) << 16;
            let list = rng.word() & 0xFFFF;
            Some(cond | 0x0800_0000 | p | u | s | w | l | rn | list)
        }
    }
}

/// Generates a random THUMB halfword decoding to an included format
/// (the second halfword only matters for BL, which is excluded).
fn random_thumb_code(rng: &mut Rng) -> Option<[u8; 2]> {
    for _ in 0..64 {
        let half: u32 = match rng.below(13) {
            // Format 1: shift by immediate; kind 0..2 in bits 11..10.
            0 => rng.below(3) << 11 | rng.below(32) << 6 | rng.below(8) << 3 | rng.below(8),
            // Format 2: 00011 I op rn/imm3 rs rd.
            1 => {
                0x1800
                    | u32::from(rng.bit()) << 10
                    | u32::from(rng.bit()) << 9
                    | rng.below(8) << 6
                    | rng.below(8) << 3
                    | rng.below(8)
            }
            // Format 3: MOV/CMP/ADD/SUB immediate.
            2 => 0x2000 | rng.below(4) << 11 | rng.below(8) << 8 | rng.below(256),
            // Format 4: ALU operation.
            3 => 0x4000 | rng.below(16) << 6 | rng.below(8) << 3 | rng.below(8),
            // Format 5: hi register operation.
            4 => {
                0x4400
                    | rng.below(3) << 8
                    | u32::from(rng.bit()) << 7
                    | u32::from(rng.bit()) << 6
                    | rng.below(8) << 3
                    | rng.below(8)
            }
            // Format 6: PC-relative load.
            5 => 0x4800 | rng.below(8) << 8 | rng.below(256),
            // Format 7/8: register-offset and signed/halfword transfers.
            6 => {
                let signed = u32::from(rng.bit()) << 9;
                0x5000
                    | signed
                    | rng.below(4) << 10
                    | rng.below(8) << 6
                    | rng.below(8) << 3
                    | rng.below(8)
            }
            // Format 9: word/byte immediate offset.
            7 => {
                0x6000
                    | u32::from(rng.bit()) << 12
                    | u32::from(rng.bit()) << 11
                    | rng.below(32) << 6
                    | rng.below(8) << 3
                    | rng.below(8)
            }
            // Format 10: halfword immediate offset.
            8 => {
                0x8000
                    | u32::from(rng.bit()) << 11
                    | rng.below(32) << 6
                    | rng.below(8) << 3
                    | rng.below(8)
            }
            // Format 11: SP-relative stack access.
            9 => 0x9000 | u32::from(rng.bit()) << 11 | rng.below(8) << 8 | rng.below(256),
            // Format 12: load address from SP or PC.
            10 => 0xA000 | u32::from(rng.bit()) << 11 | rng.below(8) << 8 | rng.below(256),
            // Format 13: add/sub offset to SP itself.
            11 => 0xB000 | u32::from(rng.bit()) << 7 | u32::from(rng.bit()) << 7 | rng.below(128),
            // Formats 14/15: push/pop and LDMIA/STMIA.
            _ => {
                if rng.bit() {
                    let pop = rng.bit();
                    let lr_pc = u32::from(pop && rng.bit()) << 8;
                    0xB400 | u32::from(pop) << 11 | lr_pc | rng.below(256)
                } else {
                    0xC000 | u32::from(rng.bit()) << 11 | rng.below(8) << 8 | rng.below(256)
                }
            }
        };
        if thumb_format_included(&decode_thumb(half as u16, 0x0000)) {
            return Some((half as u16).to_le_bytes());
        }
    }
    None
}

fn thumb_format_included(instr: &ThumbInstr) -> bool {
    match instr {
        ThumbInstr::HighRegisterOperation { op, rd, .. } => {
            !matches!(op, decode::ThumbHiOp::Bx)
                && !(matches!(op, decode::ThumbHiOp::Add | decode::ThumbHiOp::Mov)
                    && *rd == Register::Pc)
        }
        ThumbInstr::PushPop {
            pop,
            include_pc_or_lr,
            registers,
        } => !(*pop && *include_pc_or_lr) && !(*registers == 0 && !*include_pc_or_lr),
        ThumbInstr::LoadStoreMultiple { registers, .. } => *registers != 0,
        ThumbInstr::ConditionalBranch { .. }
        | ThumbInstr::UnconditionalBranch { .. }
        | ThumbInstr::LongBranchWithLink { .. }
        | ThumbInstr::LongBranchWithLinkFromLr { .. }
        | ThumbInstr::SoftwareInterrupt { .. }
        | ThumbInstr::Undefined { .. } => false,
        _ => true,
    }
}

#[test]
fn arm_differential_10k() {
    let mut rng = Rng(0x5EED_0001);
    for case in 0..10_000 {
        let Some(word) = random_arm_word(&mut rng) else {
            continue;
        };
        let mut rng = Rng(rng.next());
        run_case(analysis::State::Arm, &word.to_le_bytes(), &mut rng);
        let _ = case;
    }
}

#[test]
fn thumb_differential_10k() {
    let mut rng = Rng(0x5EED_0002);
    for case in 0..10_000 {
        let Some(code) = random_thumb_code(&mut rng) else {
            continue;
        };
        let mut rng = Rng(rng.next());
        run_case(analysis::State::Thumb, &code, &mut rng);
        let _ = case;
    }
}
