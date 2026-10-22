//! Function discovery over a traversed flag map.
//!
//! Candidate functions come from two evidence sources. Every BL target
//! the global traversal recorded is a candidate outright; `push {.., lr}`
//! prologue sites (ARM `stmdb sp!, {.., lr}` per the ARM ARM "Block Data
//! Transfer", THUMB `PUSH {.., lr}`) become candidates only once at
//! least `min_calls` BLs target them and at least `min_length`
//! consecutive instructions decode from the site. Every candidate then
//! earns its place by exit behavior: a function-body walk scores the
//! ratio of conventional returns against undefined encodings, unproven
//! branch targets, and out-of-bounds paths, and accepts the candidate at
//! `min_exit_ratio` or better.
//!
//! Provided functions (the seeds, including any config entries) are
//! accepted up front and analyzed first; BL targets seen inside accepted
//! bodies feed back into the candidate queue until it reaches a fixpoint.
//!
//! Deviation from the reference analyzer: candidate sites come from
//! branch-evidence traversal (EXEC-marked runs and recorded calls)
//! rather than a whole-ROM linear scan, which two-state decoding cannot
//! do soundly.

use std::collections::{BTreeSet, HashMap, HashSet};

use decode::{ArmInstr, Condition, Register, ThumbHiOp, ThumbInstr, decode_arm, decode_thumb};
use rom::Image;

use crate::flags::{Canonical, FlagMap, Flags, Run, Seed, State};
use crate::traverse::{Call, Stop, StopReason, WalkOptions, walk};

/// Acceptance gates for a candidate function.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Thresholds {
    /// BL calls a `push {.., lr}` site needs before it is considered.
    pub min_calls: u32,
    /// Consecutive decodable instructions a `push {.., lr}` site needs.
    pub min_length: u32,
    /// Exit-behavior ratio (returns over all path ends) a candidate
    /// must reach.
    pub min_exit_ratio: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            min_calls: 2,
            min_length: 3,
            min_exit_ratio: 2.0 / 3.0,
        }
    }
}

/// One accepted or rejected function location.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Function {
    /// Canonical address of the first instruction.
    pub addr: u32,
    /// Execution state of the body.
    pub state: State,
}

/// Result of a discovery run.
#[derive(Debug, Default)]
pub struct Discovery {
    /// Provided and accepted functions, sorted by address.
    pub functions: Vec<Function>,
    /// Candidates that failed the gates, sorted by address.
    pub rejected: Vec<Function>,
    /// Every BL the global traversal recorded, in discovery order.
    pub calls: Vec<Call>,
}

/// Runs discovery: a marking global traversal from `paths`, candidate
/// collection, and gated function-body scoring. `provided` functions
/// (config entries and the seeds) are accepted up front and analyzed
/// first; their BL targets join the candidate queue until it reaches a
/// fixpoint. Callers pass `provided` inside `paths` so provided code is
/// marked. Duplicate seeds (the header entry re-listed by a reloaded
/// config, say) are collapsed so BL calls are recorded once.
pub fn discover(
    image: &Image,
    map: &mut FlagMap,
    provided: &[Seed],
    paths: &[Seed],
    thresholds: Thresholds,
) -> Discovery {
    let provided = dedup_seeds(provided);
    let paths = dedup_seeds(paths);
    let traversal = walk(image, map, &paths, WalkOptions::global());

    let mut call_counts: HashMap<(u32, State), u32> = HashMap::new();
    for call in &traversal.calls {
        *call_counts.entry((call.target, call.state)).or_insert(0) += 1;
    }

    let mut candidates: BTreeSet<(u32, State)> = BTreeSet::new();
    for call in &traversal.calls {
        candidates.insert((call.target, call.state));
    }
    for (site, state) in push_return_sites(image, map) {
        let calls = call_counts.get(&(site, state)).copied().unwrap_or(0);
        if calls >= thresholds.min_calls
            && decodable_length(image.bytes(), map.canonical(), site, state)
                >= thresholds.min_length
        {
            candidates.insert((site, state));
        }
    }
    let known: HashSet<(u32, State)> = provided.iter().map(|s| (s.addr, s.state)).collect();
    let mut functions: Vec<Function> = provided
        .iter()
        .map(|s| Function {
            addr: s.addr,
            state: s.state,
        })
        .collect();
    let mut rejected: Vec<Function> = Vec::new();
    let mut queue: Vec<(u32, State)> = candidates.into_iter().collect();
    let mut scored = known;
    while let Some((addr, state)) = queue.pop() {
        if !scored.insert((addr, state)) {
            continue;
        }
        let (body_calls, stops) = body_walk(image, map, addr, state);
        let returns = stops
            .iter()
            .filter(|stop| stop.reason == StopReason::Return)
            .count();
        if !stops.is_empty() && returns as f64 / stops.len() as f64 >= thresholds.min_exit_ratio {
            functions.push(Function { addr, state });
            map.mark(addr, Flags::FUNC_START);
            for call in body_calls {
                if !scored.contains(&(call.target, call.state)) {
                    queue.push((call.target, call.state));
                }
            }
        } else {
            rejected.push(Function { addr, state });
        }
    }
    functions.sort_by_key(|function| (function.addr, function.state));
    functions.dedup();
    rejected.sort_by_key(|function| (function.addr, function.state));
    Discovery {
        functions,
        rejected,
        calls: traversal.calls,
    }
}

/// Scores one candidate with a non-marking function-body walk.
fn body_walk(image: &Image, map: &mut FlagMap, addr: u32, state: State) -> (Vec<Call>, Vec<Stop>) {
    let seeds = [Seed { addr, state }];
    let out = walk(image, map, &seeds, WalkOptions::function_body());
    (out.calls, out.stops)
}

/// Collapses duplicate seeds, keeping the first occurrence.
fn dedup_seeds(seeds: &[Seed]) -> Vec<Seed> {
    let mut unique: Vec<Seed> = Vec::with_capacity(seeds.len());
    for seed in seeds {
        if !unique
            .iter()
            .any(|known| known.addr == seed.addr && known.state == seed.state)
        {
            unique.push(*seed);
        }
    }
    unique
}

/// Finds `push {.., lr}` prologue sites inside EXEC-marked runs, scanned
/// in both states; the call-evidence gate in [`discover`] filters sites
/// scanned in the state callers never use.
fn push_return_sites(image: &Image, map: &FlagMap) -> Vec<(u32, State)> {
    let bytes = image.bytes();
    let canon = map.canonical();
    let mut sites = Vec::new();
    for run in map.runs() {
        if !run.flags.contains(Flags::EXEC) {
            continue;
        }
        let (start, end) = aligned_run(run, canon, 4);
        let mut word_addr = start;
        while word_addr + 4 <= end {
            if let Some(word) = read_word(bytes, canon, word_addr)
                && let ArmInstr::BlockTransfer {
                    load: false,
                    writeback: true,
                    rn: Register::Sp,
                    registers,
                    ..
                } = decode_arm(word)
                && registers.contains(Register::Lr)
            {
                sites.push((word_addr, State::Arm));
            }
            word_addr += 4;
        }
        let (start, end) = aligned_run(run, canon, 2);
        let mut half_addr = start;
        while half_addr + 2 <= end {
            if let Some(half) = read_half(bytes, canon, half_addr)
                && let ThumbInstr::PushPop {
                    pop: false,
                    include_pc_or_lr: true,
                    ..
                } = decode_thumb(half, 0)
            {
                sites.push((half_addr, State::Thumb));
            }
            half_addr += 2;
        }
    }
    sites
}

/// Instruction-aligned `[start, end)` bounds for scanning a run: runs
/// split at byte granularity (an interior FUNC_START mark, say), so
/// scanning must stay aligned to the image base in `step` units.
fn aligned_run(run: Run, canon: Canonical, step: u32) -> (u32, u32) {
    let misalign = (run.addr - canon.base()) % step;
    let start = run.addr + (step - misalign) % step;
    (start, run.addr + run.len as u32)
}

/// Counts consecutive decodable instructions from `addr`, stopping at
/// the first undefined encoding or the instruction that ends the
/// function's linear body (a return, an indirect transfer, or an
/// unconditional branch out).
fn decodable_length(bytes: &[u8], canon: Canonical, addr: u32, state: State) -> u32 {
    match state {
        State::Arm => arm_length(bytes, canon, addr),
        State::Thumb => thumb_length(bytes, canon, addr),
    }
}

fn arm_length(bytes: &[u8], canon: Canonical, mut addr: u32) -> u32 {
    let mut count = 0;
    while let Some(word) = read_word(bytes, canon, addr) {
        if matches!(decode_arm(word), ArmInstr::Undefined { .. }) {
            break;
        }
        count += 1;
        let cond = Condition::from_bits((word >> 28) as u8);
        let exit = match decode_arm(word) {
            ArmInstr::Branch { link: false, .. } => cond == Some(Condition::Al),
            ArmInstr::BranchExchange { .. } => true,
            ArmInstr::DataProcessing {
                rd: Register::Pc, ..
            } => true,
            ArmInstr::BlockTransfer {
                load: true,
                registers,
                ..
            } => registers.contains(Register::Pc),
            _ => false,
        };
        if exit {
            break;
        }
        addr = addr.wrapping_add(4);
    }
    count
}

fn thumb_length(bytes: &[u8], canon: Canonical, mut addr: u32) -> u32 {
    let mut count = 0;
    while let Some(half) = read_half(bytes, canon, addr) {
        let next = read_half(bytes, canon, addr.wrapping_add(2)).unwrap_or(0);
        let instr = decode_thumb(half, next);
        if matches!(instr, ThumbInstr::Undefined { .. }) {
            break;
        }
        count += 1;
        let exit = match instr {
            ThumbInstr::UnconditionalBranch { .. }
            | ThumbInstr::LongBranchWithLinkFromLr { .. }
            | ThumbInstr::PushPop {
                pop: true,
                include_pc_or_lr: true,
                ..
            }
            | ThumbInstr::HighRegisterOperation {
                op: ThumbHiOp::Bx, ..
            }
            | ThumbInstr::HighRegisterOperation {
                op: ThumbHiOp::Mov,
                rd: Register::Pc,
                ..
            } => true,
            ThumbInstr::LoadStoreMultiple {
                load: true,
                registers,
                ..
            } => registers & (1 << Register::Pc.bits()) != 0,
            _ => false,
        };
        if exit {
            break;
        }
        addr += if matches!(instr, ThumbInstr::LongBranchWithLink { .. }) {
            4
        } else {
            2
        };
    }
    count
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
