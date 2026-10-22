//! Jump-table (switch idiom) detection over a traversed flag map.
//!
//! Armv4T has no indexed-indirect-branch instruction, so compilers
//! express switches as a table of words plus a register-indirect jump.
//! The documented idioms (GBATEK compiler-output
//! examples) are:
//!
//! - ARM absolute: `ldr rB, [pc, ...]` pulls the table base from a
//!   literal pool, `ldr rX, [rB, rY, lsl #2]` picks an entry, and
//!   `mov pc, rX` / `bx rX` jumps to it.
//! - THUMB inline: `ldr rX, [pc, rY]` (format 7 with the PC as base)
//!   reads an entry from a table sitting just after the code, then
//!   `mov pc, rX` (a leading `lsl rY, rZ, #2` scales the index and
//!   does not change detection).
//! - THUMB relative: `ldr rT, [pc, ...]` pulls an entry of byte
//!   offsets, `add rT, pc` makes it absolute, `mov pc, rT`.
//!
//! Detection is a bounded backward window per indirect-jump site: the
//! twelve instructions before the site are re-decoded and evaluated
//! forward with a tiny abstract domain (literal-pool word, absolute
//! table base, pool word plus PC anchor). Any branch or other
//! register-indirect jump inside the window breaks the linear
//! dataflow, so such sites are skipped and stay unresolved. Resolved
//! tables are reported with their entry targets; [`mark_tables`] flags
//! the table words as `WORD`, and the pipeline feeds the targets back
//! into the traversal as branch targets.
//!
//! Config hints (`arm_table` / `thumb_table` lines) bypass detection
//! and declare a table outright, forcing the transfer bit per state.

use decode::{
    ArmInstr, DataOp, Register, RegisterShift, ShiftAmount, ShiftKind, ShifterOperand, ThumbHiOp,
    ThumbImmOp, ThumbInstr, ThumbMemOp, TransferOffset, decode_arm, decode_thumb,
};
use rom::Image;

use crate::config::TableHint;
use crate::flags::{Canonical, FlagMap, Flags, State};

/// One detected or hinted jump table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JumpTable {
    /// Canonical address of the indirect jump that consumes the table.
    /// Hinted tables carry the table address instead.
    pub site: u32,
    /// Processor state of the indirect jump.
    pub state: State,
    /// Canonical address of the first table entry.
    pub base: u32,
    /// Number of entry words.
    pub entries: u32,
    /// Entry targets with their entry states, in table order.
    pub targets: Vec<(u32, State)>,
}

/// How far back the dataflow window reaches, in instructions.
const WINDOW: u32 = 12;

/// Largest table the detector reports.
const MAX_ENTRIES: u32 = 256;

/// Abstract value of one register inside a site's window.
#[derive(Clone, Copy, Debug)]
enum Source {
    /// Not defined in the window, or defined by an unmodeled instruction.
    Bot,
    /// The register holds the word stored at this pool address; the
    /// value is resolved only when a consumer needs it.
    PoolWord { addr: u32 },
    /// The register holds an absolute table base address.
    Base { addr: u32 },
    /// The register holds `word at addr + anchor`: a relative table
    /// entry made absolute by `add ..., pc`.
    Relative { addr: u32, anchor: u32 },
}

/// Finds jump tables: hinted tables first, then the idiom scan over
/// EXEC-marked runs in both states. Sorted by site address.
#[must_use]
pub fn find_tables(image: &Image, map: &FlagMap, hints: &[TableHint]) -> Vec<JumpTable> {
    let mut tables: Vec<(u32, JumpTable)> = Vec::new();
    for hint in hints {
        if let Some(table) = hint_table(image, hint) {
            tables.push((table.site, table));
        }
    }
    let bytes = image.bytes();
    let canon = map.canonical();
    let mut sites: Vec<(u32, State)> = Vec::new();
    for run in map.runs() {
        if !run.flags.contains(Flags::EXEC) {
            continue;
        }
        let (start, end) = aligned_run(run, canon, 4);
        let mut addr = start;
        while addr + 4 <= end {
            if let Some(word) = read_word(bytes, canon, addr)
                && is_arm_indirect_jump(decode_arm(word))
            {
                sites.push((addr, State::Arm));
            }
            addr += 4;
        }
        let (start, end) = aligned_run(run, canon, 2);
        let mut addr = start;
        while addr + 2 <= end {
            if let Some(half) = read_half(bytes, canon, addr)
                && is_thumb_indirect_jump(decode_thumb(half, 0))
            {
                sites.push((addr, State::Thumb));
            }
            addr += 2;
        }
    }
    sites.sort_unstable();
    sites.dedup();
    for (site, state) in sites {
        if tables.iter().any(|(seen, _)| *seen == site) {
            continue;
        }
        if let Some(table) = analyze_site(image, map, site, state) {
            tables.push((site, table));
        }
    }
    tables.sort_by_key(|(site, _)| *site);
    tables.into_iter().map(|(_, table)| table).collect()
}

/// Flags every table's entry words as `WORD`.
pub fn mark_tables(map: &mut FlagMap, tables: &[JumpTable]) {
    for table in tables {
        let end = table.base + table.entries * 4;
        map.mark_range(table.base, end, Flags::WORD);
    }
}

/// Instruction-aligned `[start, end)` bounds for scanning a run: runs
/// split at byte granularity (an interior FUNC_START mark, say), so
/// scanning must stay aligned to the image base in `step` units.
fn aligned_run(run: crate::flags::Run, canon: Canonical, step: u32) -> (u32, u32) {
    let misalign = (run.addr - canon.base()) % step;
    let start = run.addr + (step - misalign) % step;
    (start, run.addr + run.len as u32)
}

/// Expands a config hint into a table, forcing the transfer bit per the
/// declared state.
fn hint_table(image: &Image, hint: &TableHint) -> Option<JumpTable> {
    let bytes = image.bytes();
    let canon = Canonical::for_kind(image.kind());
    let mut targets = Vec::with_capacity(hint.entries as usize);
    for index in 0..hint.entries {
        let word = read_word(bytes, canon, hint.addr + index * 4)?;
        let target = match hint.state {
            State::Arm => word & !1,
            State::Thumb => word | 1,
        };
        canon.canonicalize(target)?;
        targets.push((target, hint.state));
    }
    Some(JumpTable {
        site: hint.addr,
        state: hint.state,
        base: hint.addr,
        entries: hint.entries,
        targets,
    })
}

/// Runs the bounded backward window over one indirect-jump site.
fn analyze_site(image: &Image, map: &FlagMap, site: u32, state: State) -> Option<JumpTable> {
    let bytes = image.bytes();
    let canon = map.canonical();
    let step = match state {
        State::Arm => 4,
        State::Thumb => 2,
    };
    // Walk back over EXEC-marked bytes to find the window start.
    let mut start = site;
    let mut count = 0;
    while count < WINDOW
        && start >= canon.base() + step
        && map.get(start - step).contains(Flags::EXEC)
    {
        start -= step;
        count += 1;
    }
    let mut sources = [Source::Bot; 16];
    let mut addr = start;
    while addr < site {
        match state {
            State::Arm => {
                let word = read_word(bytes, canon, addr)?;
                let instr = decode_arm(word);
                if breaks_arm_window(&instr) {
                    return None;
                }
                eval_arm(addr, instr, &mut sources, bytes, canon);
                addr += 4;
            }
            State::Thumb => {
                let half = read_half(bytes, canon, addr)?;
                let next = read_half(bytes, canon, addr.wrapping_add(2)).unwrap_or(0);
                let instr = decode_thumb(half, next);
                if breaks_thumb_window(&instr) {
                    return None;
                }
                eval_thumb(addr, instr, &mut sources, bytes, canon);
                addr += if matches!(instr, ThumbInstr::LongBranchWithLink { .. }) {
                    4
                } else {
                    2
                };
            }
        }
    }
    // The site itself selects the table from its source register.
    let (source, via_bx) = match state {
        State::Arm => {
            let word = read_word(bytes, canon, site)?;
            match decode_arm(word) {
                ArmInstr::DataProcessing {
                    opcode: DataOp::Mov,
                    rd: Register::Pc,
                    operand2: ShifterOperand::Register { rm, .. },
                    ..
                } => (sources[rm.bits() as usize], false),
                ArmInstr::BranchExchange { rn } => (sources[rn.bits() as usize], true),
                _ => return None,
            }
        }
        State::Thumb => {
            let half = read_half(bytes, canon, site)?;
            let next = read_half(bytes, canon, site.wrapping_add(2)).unwrap_or(0);
            match decode_thumb(half, next) {
                ThumbInstr::HighRegisterOperation {
                    op: ThumbHiOp::Mov,
                    rd: Register::Pc,
                    rs,
                } => (sources[rs.bits() as usize], false),
                ThumbInstr::HighRegisterOperation {
                    op: ThumbHiOp::Bx,
                    rs,
                    ..
                } => (sources[rs.bits() as usize], true),
                _ => return None,
            }
        }
    };
    match source {
        Source::Base { addr } => absolute_table(bytes, canon, site, state, addr, via_bx),
        Source::Relative { addr, anchor } => relative_table(bytes, canon, site, addr, anchor),
        _ => None,
    }
}

/// Evaluates one ARM window instruction against the abstract sources.
fn eval_arm(
    addr: u32,
    instr: ArmInstr,
    sources: &mut [Source; 16],
    bytes: &[u8],
    canon: Canonical,
) {
    match instr {
        ArmInstr::LoadStore {
            load: true,
            pre_index: true,
            rn: Register::Pc,
            rd,
            offset: TransferOffset::Immediate(imm),
            up,
            ..
        } => {
            let target = addr.wrapping_add(8).wrapping_add_signed(if up {
                imm as i32
            } else {
                -(imm as i32)
            });
            sources[rd.bits() as usize] = Source::PoolWord { addr: target };
        }
        ArmInstr::LoadStore {
            load: true,
            rd,
            rn,
            offset:
                TransferOffset::Register {
                    rm: _,
                    shift:
                        RegisterShift {
                            kind: ShiftKind::Lsl,
                            amount: ShiftAmount::Immediate(2),
                        },
                },
            ..
        } => {
            sources[rd.bits() as usize] = match sources[rn.bits() as usize] {
                Source::PoolWord { addr: pool } => match read_word(bytes, canon, pool) {
                    Some(word) if canon.canonicalize(word).is_some() => Source::Base { addr: word },
                    _ => Source::Bot,
                },
                _ => Source::Bot,
            };
        }
        ArmInstr::DataProcessing { rd, .. } | ArmInstr::LoadStore { rd, .. } => {
            sources[rd.bits() as usize] = Source::Bot;
        }
        _ => {}
    }
}

/// Evaluates one THUMB window instruction against the abstract sources.
fn eval_thumb(
    addr: u32,
    instr: ThumbInstr,
    sources: &mut [Source; 16],
    bytes: &[u8],
    canon: Canonical,
) {
    match instr {
        ThumbInstr::PcRelativeLoad { rd, offset } => {
            let target = (addr.wrapping_add(4) & !3) + u32::from(offset) * 4;
            sources[rd.bits() as usize] = Source::PoolWord { addr: target };
        }
        ThumbInstr::LoadStoreRegisterOffset {
            kind: ThumbMemOp::Ldr,
            rd,
            rb,
            ro: _,
        } => {
            sources[rd.bits() as usize] = if rb == Register::Pc {
                Source::Base {
                    addr: addr.wrapping_add(4) & !3,
                }
            } else {
                match sources[rb.bits() as usize] {
                    Source::PoolWord { addr: pool } => match read_word(bytes, canon, pool) {
                        Some(word) if canon.canonicalize(word).is_some() => {
                            Source::Base { addr: word }
                        }
                        _ => Source::Bot,
                    },
                    _ => Source::Bot,
                }
            };
        }
        ThumbInstr::LoadAddress {
            to_sp: false,
            rd,
            offset,
        } => {
            sources[rd.bits() as usize] = Source::Base {
                addr: (addr.wrapping_add(4) & !3) + u32::from(offset) * 4,
            };
        }
        ThumbInstr::HighRegisterOperation {
            op: ThumbHiOp::Add,
            rd,
            rs: Register::Pc,
        } => {
            // `add rd, pc`: rd = rd + (addr + 4).
            sources[rd.bits() as usize] = match sources[rd.bits() as usize] {
                Source::PoolWord { addr: pool } => Source::Relative {
                    addr: pool,
                    anchor: addr.wrapping_add(4),
                },
                _ => Source::Bot,
            };
        }
        ThumbInstr::ImmediateOperation {
            op: ThumbImmOp::Mov,
            rd,
            ..
        }
        | ThumbInstr::ShiftedImmediate { rd, .. }
        | ThumbInstr::AddSubtract { rd, .. }
        | ThumbInstr::AluOperation { rd, .. }
        | ThumbInstr::LoadAddress { rd, .. }
        | ThumbInstr::LoadStoreRegisterOffset { rd, .. }
        | ThumbInstr::LoadStoreSignedRegisterOffset { rd, .. } => {
            sources[rd.bits() as usize] = Source::Bot;
        }
        _ => {}
    }
}

/// Resolves an absolute table: consecutive plausible entry words from
/// the base until the first one that cannot be a code address.
fn absolute_table(
    bytes: &[u8],
    canon: Canonical,
    site: u32,
    state: State,
    base: u32,
    via_bx: bool,
) -> Option<JumpTable> {
    let mut targets = Vec::new();
    for index in 0..MAX_ENTRIES {
        let Some(word) = read_word(bytes, canon, base.wrapping_add(index * 4)) else {
            break;
        };
        let Some(entry) = canon.canonicalize(word) else {
            break;
        };
        let (target, target_state) = if via_bx {
            (
                entry & !1,
                if entry & 1 == 1 {
                    State::Thumb
                } else {
                    State::Arm
                },
            )
        } else {
            (entry & !1, state)
        };
        targets.push((target, target_state));
    }
    finish_table(site, state, base, targets)
}

/// Resolves a relative table: entries are byte offsets added to the
/// `add ..., pc` anchor.
fn relative_table(
    bytes: &[u8],
    canon: Canonical,
    site: u32,
    addr: u32,
    anchor: u32,
) -> Option<JumpTable> {
    let mut targets = Vec::new();
    for index in 0..MAX_ENTRIES {
        let Some(word) = read_word(bytes, canon, addr.wrapping_add(index * 4)) else {
            break;
        };
        let Some(entry) = canon.canonicalize(anchor.wrapping_add(word)) else {
            break;
        };
        targets.push((entry, State::Thumb));
    }
    finish_table(site, State::Thumb, addr, targets)
}

fn finish_table(
    site: u32,
    state: State,
    base: u32,
    targets: Vec<(u32, State)>,
) -> Option<JumpTable> {
    if targets.is_empty() {
        return None;
    }
    Some(JumpTable {
        site,
        state,
        base,
        entries: targets.len() as u32,
        targets,
    })
}

/// Whether the ARM word is a register-indirect jump: `mov pc, rX` or
/// `bx rX`.
fn is_arm_indirect_jump(instr: ArmInstr) -> bool {
    matches!(
        instr,
        ArmInstr::DataProcessing {
            opcode: DataOp::Mov,
            rd: Register::Pc,
            operand2: ShifterOperand::Register { .. },
            ..
        } | ArmInstr::BranchExchange { .. }
    )
}

/// Whether the THUMB halfword pair is a register-indirect jump.
fn is_thumb_indirect_jump(instr: ThumbInstr) -> bool {
    matches!(
        instr,
        ThumbInstr::HighRegisterOperation {
            op: ThumbHiOp::Mov,
            rd: Register::Pc,
            ..
        } | ThumbInstr::HighRegisterOperation {
            op: ThumbHiOp::Bx,
            ..
        }
    )
}

/// Whether an ARM instruction in the window breaks the linear dataflow.
fn breaks_arm_window(instr: &ArmInstr) -> bool {
    matches!(
        instr,
        ArmInstr::Branch { .. } | ArmInstr::BranchExchange { .. }
    ) || matches!(
        instr,
        ArmInstr::DataProcessing {
            rd: Register::Pc,
            ..
        }
    )
}

/// Whether a THUMB instruction in the window breaks the linear dataflow.
fn breaks_thumb_window(instr: &ThumbInstr) -> bool {
    matches!(
        instr,
        ThumbInstr::ConditionalBranch { .. }
            | ThumbInstr::UnconditionalBranch { .. }
            | ThumbInstr::LongBranchWithLink { .. }
            | ThumbInstr::LongBranchWithLinkFromLr { .. }
            | ThumbInstr::HighRegisterOperation {
                op: ThumbHiOp::Mov | ThumbHiOp::Bx,
                ..
            }
    )
}

fn read_word(bytes: &[u8], canon: Canonical, addr: u32) -> Option<u32> {
    let offset = canon.offset(addr)?;
    let word: [u8; 4] = bytes.get(offset..offset + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(word))
}

fn read_half(bytes: &[u8], canon: Canonical, addr: u32) -> Option<u16> {
    let offset = canon.offset(addr)?;
    let half: [u8; 2] = bytes.get(offset..offset + 2)?.try_into().ok()?;
    Some(u16::from_le_bytes(half))
}
