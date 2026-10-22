//! Matching assembly emission from analyzed GBA images.
//!
//! [`emit`] turns one analyzed image (bytes plus [`FlagMap`]) into GNU
//! as-compatible assembly that re-assembles byte-identically: code runs
//! through the shared decode-to-text renderers, data runs through
//! `.word`/`.byte` directives, and functions, internal branch targets,
//! and referenced data carry labels. Every image byte is emitted exactly
//! once in address order, so the assembler's location counter tracks the
//! image layout without any alignment directives.
//!
//! The byte-identical invariant holds by construction only for
//! encodings the renderers emit faithfully. Encodings flagged
//! unpredictable render as their nearest real syntax (see
//! `decode::text`), which assembles differently; handling those is
//! the fallback work, and the corpus harness pins the invariant.

use std::collections::BTreeMap;

use analysis::{Canonical, FlagMap, Flags};
use decode::{ArmInstr, Condition, ThumbInstr, arm_text, decode_arm, decode_thumb, thumb_text};
use rom::Image;

/// A safety fallback the emitter took at one address because the
/// analysis could not guarantee a byte-identical rendering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Warning {
    /// Canonical address of the offending site.
    pub addr: u32,
    /// Which fallback fired.
    pub fallback: Fallback,
    /// Full human-readable message, naming the address.
    pub message: String,
}

/// The kinds of fallback, each with a stable greppable tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fallback {
    /// Branch whose target carries no label (or no static target at
    /// all); raw bytes are emitted so the encoding stays exact.
    BranchWithoutLabel,
    /// An instruction span contains a label or is truncated by the
    /// flag map; the safe prefix is emitted as raw bytes.
    InstructionOverlapsLabel,
    /// FUNC_START with THUMB code at a non-halfword-aligned address.
    /// Emission continues unchanged because no emitted directive needs
    /// alignment, but bit-0 references to the function cannot be
    /// proven, so data words keep their literals.
    MisalignedThumbFunction,
    /// Encoding architecturally UNPREDICTABLE on the ARM7TDMI; the
    /// renderer's nearest-real-syntax text would assemble to a
    /// different encoding, so raw bytes are emitted.
    UnpredictableEncoding,
    /// Word with the THUMB bit-0 pattern pointing at executed THUMB
    /// code that carries no label; the literal is kept because a
    /// symbolic reference would not be provably safe.
    WordReferenceNotProvable,
}

impl Fallback {
    /// Stable greppable tag used in warnings and assembly comments.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Self::BranchWithoutLabel => "branch-without-label",
            Self::InstructionOverlapsLabel => "instruction-overlaps-label",
            Self::MisalignedThumbFunction => "misaligned-thumb-function",
            Self::UnpredictableEncoding => "unpredictable-encoding",
            Self::WordReferenceNotProvable => "word-reference-not-provable",
        }
    }
}

/// Emits assembly reproducing `image` byte-for-byte, driven by `map`,
/// returning the assembly and every fallback warning raised on the way.
///
/// Code flags (EXEC) decide instruction bytes, the THUMB flag picks the
/// assembler dialect, and everything else is emitted as data. Labels:
/// `sub_XXXXXXXX` at FUNC_START bytes, `loc_XXXXXXXX` at branch targets
/// that land on executed code, `_*XXXXXXXX` at referenced data
/// (literal pools).
///
/// Whenever rendering an address faithfully would depend on analysis
/// being right, the emitter falls back to a form that cannot break the
/// byte-identical invariant (raw bytes, or a literal instead of a
/// symbolic reference) and records a [`Warning`] naming the address;
/// the same warnings appear as `@ WARNING:` comments in the assembly.
#[must_use]
pub fn emit_with_warnings(image: &Image, map: &FlagMap) -> (String, Vec<Warning>) {
    let bytes = image.bytes();
    let canon = map.canonical();
    let flags = flag_bytes(map, bytes.len());

    let labels = collect_labels(bytes, &flags, canon.base());
    let mut warnings = Vec::new();
    let mut out = String::new();
    out.push_str(".syntax unified\n");
    out.push_str(".cpu arm7tdmi\n");
    out.push_str(".section .text, \"ax\"\n");
    let mut dialect = Dialect::Arm;
    out.push_str(".arm\n");

    let mut index = 0usize;
    while index < bytes.len() {
        let addr = canon.base() + index as u32;
        if let Some(label) = labels.get(&addr) {
            out.push_str(&format!("{label}:\n"));
        }
        let run = Flags::from_bits(flags[index]);
        if run.contains(Flags::EXEC) {
            warn_misaligned_thumb_function(addr, run, &mut warnings);
            let warned = warnings.len();
            let (step, line) = if run.contains(Flags::THUMB) {
                if dialect != Dialect::Thumb {
                    out.push_str(".thumb\n");
                    dialect = Dialect::Thumb;
                }
                emit_thumb(bytes, &flags, index, addr, &labels, &mut warnings)
            } else {
                if dialect != Dialect::Arm {
                    out.push_str(".arm\n");
                    dialect = Dialect::Arm;
                }
                emit_arm(bytes, &flags, index, addr, &labels, &mut warnings)
            };
            for warning in &warnings[warned..] {
                out.push_str(&format!("@ WARNING: {}\n", warning.message));
            }
            out.push_str(&line);
            out.push('\n');
            index += step;
        } else {
            index += emit_data(
                bytes,
                &flags,
                canon,
                index,
                &labels,
                &mut warnings,
                &mut out,
            );
        }
    }
    (out, warnings)
}

/// Emits assembly, dropping the fallback warnings; see
/// [`emit_with_warnings`].
#[must_use]
pub fn emit(image: &Image, map: &FlagMap) -> String {
    emit_with_warnings(image, map).0
}

/// Warns once per THUMB function label that is not halfword-aligned.
fn warn_misaligned_thumb_function(addr: u32, run: Flags, warnings: &mut Vec<Warning>) {
    if run.contains(Flags::FUNC_START)
        && run.contains(Flags::THUMB)
        && addr % 2 == 1
        && !warnings.iter().any(|warning| warning.addr == addr)
    {
        warnings.push(Warning {
            addr,
            fallback: Fallback::MisalignedThumbFunction,
            message: format!(
                "misaligned-thumb-function at 0x{addr:08x}: THUMB function \
                 start is not halfword-aligned; bit-0 references to it \
                 cannot be proven"
            ),
        });
    }
}

/// Which assembler dialect is active in the emitted stream.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dialect {
    Arm,
    Thumb,
}

/// Flattens the map's runs into one flag byte per image byte.
fn flag_bytes(map: &FlagMap, len: usize) -> Vec<u8> {
    let base = map.canonical().base() as usize;
    let mut flags = vec![0u8; len];
    for run in map.runs() {
        let Some(start) = run.addr.checked_sub(base as u32) else {
            continue;
        };
        let start = start as usize;
        let end = (start + run.len).min(len);
        if start < end {
            flags[start..end].fill(run.flags.bits());
        }
    }
    flags
}

/// Collects labels: functions at FUNC_START bytes, `loc_` at static
/// branch targets that land on executed code inside the image, and
/// `_*` at literal-pool starts.
fn collect_labels(bytes: &[u8], flags: &[u8], base: u32) -> BTreeMap<u32, String> {
    let mut labels = BTreeMap::new();
    for (index, &bits) in flags.iter().enumerate() {
        let run = Flags::from_bits(bits);
        if run.contains(Flags::FUNC_START) {
            labels.insert(
                base + index as u32,
                format!("sub_{:08x}", base + index as u32),
            );
        } else if run.contains(Flags::LITERAL)
            && (index == 0 || !Flags::from_bits(flags[index - 1]).contains(Flags::LITERAL))
        {
            labels.insert(base + index as u32, format!("_{:08x}", base + index as u32));
        }
    }
    let mut index = 0usize;
    while index < bytes.len() {
        let run = Flags::from_bits(flags[index]);
        if !run.contains(Flags::EXEC) {
            index += 1;
            continue;
        }
        if run.contains(Flags::THUMB) {
            index += record_thumb_branch(bytes, flags, base, index, &mut labels);
        } else {
            index += record_arm_branch(bytes, flags, base, index, &mut labels);
        }
    }
    labels
}

/// Records the branch target of one ARM instruction, returning its size.
fn record_arm_branch(
    bytes: &[u8],
    flags: &[u8],
    base: u32,
    index: usize,
    labels: &mut BTreeMap<u32, String>,
) -> usize {
    if index + 4 <= bytes.len()
        && let ArmInstr::Branch { link: _, offset } = decode_arm(word(bytes, index))
    {
        let target = (base + index as u32)
            .wrapping_add(8)
            .wrapping_add(offset as u32);
        note_target(flags, base, target, labels);
    }
    4
}

/// Records the branch target of one THUMB instruction, returning its
/// size (4 for BL pairs, else 2).
fn record_thumb_branch(
    bytes: &[u8],
    flags: &[u8],
    base: u32,
    index: usize,
    labels: &mut BTreeMap<u32, String>,
) -> usize {
    let first = half(bytes, index);
    let next = half(bytes, index + 2);
    let (size, target) = match decode_thumb(first, next) {
        ThumbInstr::ConditionalBranch { offset, .. }
        | ThumbInstr::UnconditionalBranch { offset } => (
            2,
            (base + index as u32)
                .wrapping_add(4)
                .wrapping_add((i32::from(offset) * 2) as u32),
        ),
        ThumbInstr::LongBranchWithLink { offset } => (
            4,
            (base + index as u32)
                .wrapping_add(4)
                .wrapping_add((offset * 2) as u32),
        ),
        _ => (2, 0),
    };
    if target != 0 {
        note_target(flags, base, target, labels);
    }
    size
}

/// Labels `target` when it is executed code inside the image.
fn note_target(flags: &[u8], base: u32, target: u32, labels: &mut BTreeMap<u32, String>) {
    let offset = match target.checked_sub(base) {
        Some(offset) if (offset as usize) < flags.len() => offset as usize,
        _ => return,
    };
    if Flags::from_bits(flags[offset]).contains(Flags::EXEC) && !labels.contains_key(&target) {
        labels.insert(target, format!("loc_{target:08x}"));
    }
}

/// Emits one ARM instruction, returning its byte size. Falls back to
/// raw bytes when the span is not a clean 4-byte instruction or when
/// the encoding has no faithful assembly form.
fn emit_arm(
    bytes: &[u8],
    flags: &[u8],
    index: usize,
    addr: u32,
    labels: &BTreeMap<u32, String>,
    warnings: &mut Vec<Warning>,
) -> (usize, String) {
    if !span_ok(flags, index, 4, bytes.len(), false) || label_inside(labels, addr, 4) {
        return demote(bytes, flags, labels, index, 4, warnings, addr);
    }
    let word = word(bytes, index);
    let instr = decode_arm(word);
    if let ArmInstr::Branch { link, offset } = instr {
        let target = addr.wrapping_add(8).wrapping_add(offset as u32);
        if let Some(label) = labels.get(&target) {
            let cond = cond_suffix(word);
            return (
                4,
                format!("{}{} {}", if link { "bl" } else { "b" }, cond, label),
            );
        }
        warnings.push(Warning {
            addr,
            fallback: Fallback::BranchWithoutLabel,
            message: format!(
                "branch-without-label at 0x{addr:08x}: target 0x{target:08x} \
                 has no label; emitting raw bytes"
            ),
        });
        return (4, raw_bytes(bytes, index, 4));
    }
    if instr.is_unpredictable() {
        warnings.push(Warning {
            addr,
            fallback: Fallback::UnpredictableEncoding,
            message: format!(
                "unpredictable-encoding at 0x{addr:08x}: word 0x{word:08X} \
                 is UNPREDICTABLE on the ARM7TDMI; emitting raw bytes"
            ),
        });
        return (4, raw_bytes(bytes, index, 4));
    }
    (4, arm_text(word))
}

/// Emits one THUMB instruction (BL consumes its second halfword),
/// returning its byte size. Falls back like [`emit_arm`].
fn emit_thumb(
    bytes: &[u8],
    flags: &[u8],
    index: usize,
    addr: u32,
    labels: &BTreeMap<u32, String>,
    warnings: &mut Vec<Warning>,
) -> (usize, String) {
    if !span_ok(flags, index, 2, bytes.len(), true) || label_inside(labels, addr, 2) {
        return demote(bytes, flags, labels, index, 2, warnings, addr);
    }
    let first = half(bytes, index);
    let next = half(bytes, index + 2);
    let instr = decode_thumb(first, next);
    let target = match instr {
        ThumbInstr::ConditionalBranch { offset, .. }
        | ThumbInstr::UnconditionalBranch { offset } => Some((
            2,
            addr.wrapping_add(4)
                .wrapping_add((i32::from(offset) * 2) as u32),
        )),
        ThumbInstr::LongBranchWithLink { offset } => {
            Some((4, addr.wrapping_add(4).wrapping_add((offset * 2) as u32)))
        }
        // `bl lr+N` pseudo-syntax (GBATEK THUMB.19): no static target
        // and no assembler accepts the text, so raw bytes.
        ThumbInstr::LongBranchWithLinkFromLr { .. } => {
            warnings.push(Warning {
                addr,
                fallback: Fallback::BranchWithoutLabel,
                message: format!(
                    "branch-without-label at 0x{addr:08x}: BL from LR has \
                     no static target; emitting raw bytes"
                ),
            });
            return (4, raw_bytes(bytes, index, 4));
        }
        _ => None,
    };
    if let Some((size, target)) = target {
        let size_ok = span_ok(flags, index, size, bytes.len(), true);
        if !size_ok || label_inside(labels, addr, size as u32) {
            return demote(bytes, flags, labels, index, size, warnings, addr);
        }
        if let Some(label) = labels.get(&target) {
            let text = match instr {
                ThumbInstr::ConditionalBranch { cond, .. } => {
                    format!("b{} {}", cond.mnemonic(), label)
                }
                ThumbInstr::UnconditionalBranch { .. } => format!("b {label}"),
                _ => format!("bl {label}"),
            };
            return (size, text);
        }
        warnings.push(Warning {
            addr,
            fallback: Fallback::BranchWithoutLabel,
            message: format!(
                "branch-without-label at 0x{addr:08x}: target \
                 0x{target:08x} has no label; emitting raw bytes"
            ),
        });
        return (size, raw_bytes(bytes, index, size));
    }
    if instr.is_unpredictable() {
        warnings.push(Warning {
            addr,
            fallback: Fallback::UnpredictableEncoding,
            message: format!(
                "unpredictable-encoding at 0x{addr:08x}: halfword \
                 0x{first:04X} is UNPREDICTABLE on the ARM7TDMI; emitting \
                 raw bytes"
            ),
        });
        return (2, raw_bytes(bytes, index, 2));
    }
    if matches!(instr, ThumbInstr::LongBranchWithLink { .. }) {
        (4, thumb_text(first, next))
    } else {
        (2, thumb_text(first, 0))
    }
}

/// True when `size` bytes from `index` are all executed in the same
/// state and fit in the image.
fn span_ok(flags: &[u8], index: usize, size: usize, len: usize, thumb: bool) -> bool {
    if index + size > len {
        return false;
    }
    (index..index + size).all(|i| {
        let run = Flags::from_bits(flags[i]);
        run.contains(Flags::EXEC) && run.contains(Flags::THUMB) == thumb
    })
}

/// True when a label exists strictly inside the span at `addr`.
fn label_inside(labels: &BTreeMap<u32, String>, addr: u32, size: u32) -> bool {
    byte_offset_to_next_label(labels, addr, size).is_some()
}

/// Byte offset of the first label strictly inside `size` bytes at
/// `addr`, or `None`. Overflow far past the address space yields `None`:
/// no label can exist there.
fn byte_offset_to_next_label(
    labels: &BTreeMap<u32, String>,
    addr: u32,
    size: u32,
) -> Option<usize> {
    let start = addr.checked_add(1)?;
    let end = addr.checked_add(size)?;
    if start >= end {
        return None;
    }
    labels
        .range(start..end)
        .next()
        .map(|(target, _)| (*target - addr) as usize)
}

/// Emits up to `max` bytes from `index` as raw bytes and records the
/// overlap warning. The run stops at the first label strictly inside
/// the span (so the label still prints) and at non-executed bytes.
/// Always emits at least one byte so the walk makes progress. Returns
/// bytes consumed and the raw-bytes line.
fn demote(
    bytes: &[u8],
    flags: &[u8],
    labels: &BTreeMap<u32, String>,
    index: usize,
    max: usize,
    warnings: &mut Vec<Warning>,
    addr: u32,
) -> (usize, String) {
    warnings.push(Warning {
        addr,
        fallback: Fallback::InstructionOverlapsLabel,
        message: format!(
            "instruction-overlaps-label at 0x{addr:08x}: instruction span \
             contains a label or is truncated; emitting raw bytes"
        ),
    });
    let limit = bytes.len().min(index + max);
    let stop = byte_offset_to_next_label(labels, addr, max as u32)
        .unwrap_or(max)
        .min(max);
    let mut end = index + 1;
    while end < limit && end - index < stop && Flags::from_bits(flags[end]).contains(Flags::EXEC) {
        end += 1;
    }
    let step = end - index;
    (step, raw_bytes(bytes, index, step))
}

/// Renders `size` bytes from `index`: `.word` when aligned and whole,
/// a `.byte` line otherwise.
fn raw_bytes(bytes: &[u8], index: usize, size: usize) -> String {
    if size == 4 && index.is_multiple_of(4) && index + 4 <= bytes.len() {
        let value = u32::from_le_bytes(bytes[index..index + 4].try_into().expect("in-bounds"));
        return format!(".word 0x{value:08X}");
    }
    let list = bytes[index..index + size]
        .iter()
        .map(|byte| format!("0x{byte:02X}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(".byte {list}")
}

/// Emits data bytes from `index` on: aligned WORD runs as `.word`,
/// everything else grouped into `.byte` lines. Returns bytes consumed.
/// Byte runs stop at executed bytes (instructions render their own
/// text) and at literal-pool starts (labeled and emitted as words).
/// A word carrying the THUMB bit-0 pattern is emitted as a symbolic
/// `label+1` reference only when the target is provably a labeled
/// THUMB code address; otherwise the literal is kept with a warning.
fn emit_data(
    bytes: &[u8],
    flags: &[u8],
    canon: Canonical,
    index: usize,
    labels: &BTreeMap<u32, String>,
    warnings: &mut Vec<Warning>,
    out: &mut String,
) -> usize {
    let addr = canon.base() + index as u32;
    if index + 4 <= bytes.len()
        && index.is_multiple_of(4)
        && (index..index + 4).all(|i| {
            let run = Flags::from_bits(flags[i]);
            run.contains(Flags::WORD) && !run.contains(Flags::EXEC)
        })
        && !label_inside(labels, addr, 4)
    {
        let value = u32::from_le_bytes(bytes[index..index + 4].try_into().expect("in-bounds"));
        out.push_str(&word_line(
            bytes, flags, canon, index, value, labels, warnings,
        ));
        return 4;
    }
    let start = index;
    let base = canon.base();
    // Always emit at least one byte: the start byte itself may carry a
    // boundary flag (e.g. a pool label inside a demoted region).
    let mut end = index + 1;
    while end < bytes.len()
        && end - start < 12
        && !Flags::from_bits(flags[end]).contains(Flags::EXEC)
        && !labels.contains_key(&(base + end as u32))
    {
        end += 1;
    }
    let list = bytes[start..end]
        .iter()
        .map(|byte| format!("0x{byte:02X}"))
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str(&format!(".byte {list}\n"));
    end - start
}

/// Renders one emitted word: `label+1` for provably safe THUMB
/// pointers, the literal otherwise (warning when the target
/// canonicalizes onto executed THUMB code that has no label).
fn word_line(
    bytes: &[u8],
    flags: &[u8],
    canon: Canonical,
    index: usize,
    value: u32,
    labels: &BTreeMap<u32, String>,
    warnings: &mut Vec<Warning>,
) -> String {
    let addr = canon.base() + index as u32;
    if value & 1 == 1
        && let Some(target) = value.checked_sub(1)
        && let Some(offset) = canon.offset(target)
        && offset < bytes.len()
        && Flags::from_bits(flags[offset]).contains(Flags::EXEC | Flags::THUMB)
    {
        if let Some(label) = labels.get(&target) {
            return format!(".word {label}+1\n");
        }
        warnings.push(Warning {
            addr,
            fallback: Fallback::WordReferenceNotProvable,
            message: format!(
                "word-reference-not-provable at 0x{addr:08x}: value \
                 0x{value:08X} points at unlabeled THUMB code; keeping the \
                 literal"
            ),
        });
    }
    format!(".word 0x{value:08X}\n")
}

/// Reads the little-endian word at an image offset.
fn word(bytes: &[u8], index: usize) -> u32 {
    let slice: [u8; 4] = bytes[index..index + 4].try_into().expect("in-bounds");
    u32::from_le_bytes(slice)
}

/// Reads the little-endian halfword at an image offset (0 past the end).
fn half(bytes: &[u8], index: usize) -> u16 {
    if index + 2 > bytes.len() {
        return 0;
    }
    let slice: [u8; 2] = bytes[index..index + 2].try_into().expect("in-bounds");
    u16::from_le_bytes(slice)
}

/// The condition mnemonic suffix of an ARM word, empty for `al`.
fn cond_suffix(word: u32) -> &'static str {
    match Condition::from_bits((word >> 28) as u8) {
        Some(Condition::Al) | None => "",
        Some(cond) => cond.mnemonic(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a 256-byte multiboot-shaped image and a hand-marked map:
    /// an entry branch to 0xC0, an ARM `ldr`/`bx` pair, a literal pool
    /// at 0xD8, and unmarked (data) filler elsewhere. `pool` is the
    /// pool word value.
    fn fixture_with_pool(pool: u32) -> (Image, FlagMap) {
        let mut bytes = vec![0u8; 256];
        bytes[0x00..0x04].copy_from_slice(&0xEA00_002Eu32.to_le_bytes());
        bytes[0x04..0xC0].fill(0x5A);
        bytes[0xC0..0xC4].copy_from_slice(&0xE59F_0010u32.to_le_bytes());
        bytes[0xC4..0xC8].copy_from_slice(&0xE12F_FF10u32.to_le_bytes());
        bytes[0xC8..0xD6].copy_from_slice(&[
            0x04, 0x49, 0x03, 0x22, 0x0A, 0x80, 0x04, 0x4B, 0x04, 0x48, 0x18, 0x60, 0xFE, 0xE7,
        ]);
        bytes[0xD8..0xDC].copy_from_slice(&pool.to_le_bytes());
        let image = Image::from_bytes(bytes).expect("fixture parses");
        let mut map = FlagMap::from_image(&image);
        let base = map.canonical().base();
        map.mark_range(base, base + 4, Flags::EXEC | Flags::WORD);
        map.set(base, Flags::EXEC | Flags::WORD | Flags::FUNC_START);
        map.mark_range(base + 0xC0, base + 0xC8, Flags::EXEC | Flags::WORD);
        map.mark_range(base + 0xD8, base + 0xDC, Flags::WORD | Flags::LITERAL);
        (image, map)
    }

    /// The stock fixture: pool word points at the cart base, which the
    /// multiboot canonicalization cannot resolve.
    pub(super) fn fixture() -> (Image, FlagMap) {
        fixture_with_pool(0x0800_00C9)
    }

    /// Clears flags on `start..end` (canonical offsets from `base`).
    fn clear(map: &mut FlagMap, base: u32, start: u32, end: u32) {
        for offset in start..end {
            map.set(base + offset, Flags::EMPTY);
        }
    }

    #[test]
    fn prelude_precedes_output() {
        let (image, map) = fixture();
        let asm = emit(&image, &map);
        assert!(asm.starts_with(".syntax unified\n.cpu arm7tdmi\n.section .text, \"ax\"\n.arm\n"));
    }

    #[test]
    fn labels_functions_internal_branches_and_pools() {
        let (image, map) = fixture();
        let asm = emit(&image, &map);
        let base = map.canonical().base();
        assert!(asm.contains(&format!("sub_{base:08x}:\n")));
        assert!(asm.contains(&format!("b loc_{:08x}\n", base + 0xC0)));
        assert!(asm.contains(&format!("loc_{:08x}:\n", base + 0xC0)));
        assert!(asm.contains(&format!("_{:08x}:\n.word 0x080000C9\n", base + 0xD8)));
    }

    #[test]
    fn instructions_never_bleed_into_byte_runs() {
        let (image, map) = fixture();
        let asm = emit(&image, &map);
        for line in asm.lines() {
            if line.starts_with(".byte") {
                assert!(
                    !line.contains("0x10, 0x00, 0x9F, 0xE5"),
                    "ldr bled in: {line}"
                );
                assert!(
                    !line.contains("0x10, 0xFF, 0x2F, 0xE1"),
                    "bx bled in: {line}"
                );
                assert!(
                    !line.contains("0xC9, 0x00, 0x00, 0x08"),
                    "pool bled in: {line}"
                );
            }
        }
    }

    #[test]
    fn data_runs_stop_at_every_boundary_kind() {
        let (image, map) = fixture();
        let asm = emit(&image, &map);
        let lines: Vec<&str> = asm.lines().collect();
        let after_branch = lines
            .iter()
            .position(|line| line.starts_with("b loc_"))
            .expect("branch emitted");
        assert!(
            lines[after_branch + 1].starts_with(".byte 0x5A"),
            "filler run must start right after the branch"
        );
        let last_filler = lines
            .iter()
            .rposition(|line| line.starts_with(".byte 0x5A"))
            .expect("filler emitted");
        let expected = format!("loc_{:08x}:", map.canonical().base() + 0xC0);
        assert_eq!(
            lines[last_filler + 1],
            expected,
            "filler run must stop exactly at the executed label"
        );
    }

    #[test]
    fn thumb_code_switches_dialect() {
        let (image, mut map) = fixture();
        let base = map.canonical().base();
        map.mark_range(base + 0xC8, base + 0xD6, Flags::EXEC | Flags::THUMB);
        let asm = emit(&image, &map);
        let thumb_at = asm.find(".thumb\n").expect("thumb dialect emitted");
        let code = &asm[thumb_at..];
        assert!(
            code.contains("ldr r1, [pc, #16]"),
            "thumb text decoded: {code}"
        );
        assert!(
            !asm[..thumb_at].contains("ldr r1, [pc, #16]"),
            "thumb text must not appear before the dialect switch"
        );
    }

    #[test]
    fn branch_without_label_demotes_to_raw_bytes() {
        let (image, mut map) = fixture();
        let base = map.canonical().base();
        clear(&mut map, base, 0xC0, 0xC8);
        let (asm, warnings) = emit_with_warnings(&image, &map);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0].fallback, Fallback::BranchWithoutLabel);
        assert_eq!(warnings[0].addr, base);
        assert!(
            warnings[0]
                .message
                .contains(&format!("target 0x{:08x}", base + 0xC0)),
            "{warnings:?}"
        );
        assert!(asm.contains(".word 0xEA00002E"), "{asm}");
        assert!(!asm.contains("loc_"), "{asm}");
        assert!(!asm.contains("b ."), "{asm}");
    }

    #[test]
    fn truncated_instruction_span_demotes_to_raw_bytes() {
        let (image, mut map) = fixture();
        let base = map.canonical().base();
        clear(&mut map, base, 0xC2, 0xC4);
        let (asm, warnings) = emit_with_warnings(&image, &map);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0].fallback, Fallback::InstructionOverlapsLabel);
        assert_eq!(warnings[0].addr, base + 0xC0);
        assert!(asm.contains(".byte 0x10, 0x00\n"), "{asm}");
        assert!(!asm.contains("ldr r0"), "{asm}");
    }

    #[test]
    fn label_inside_instruction_demotes_and_keeps_label() {
        let (image, mut map) = fixture();
        let base = map.canonical().base();
        // set(): the flattened map has the pool overwrite the EXEC
        // marks, and OR-ing would leave EXEC|WORD|LITERAL instead.
        map.set(base + 0xC2, Flags::WORD | Flags::LITERAL);
        map.set(base + 0xC3, Flags::WORD | Flags::LITERAL);
        let (asm, warnings) = emit_with_warnings(&image, &map);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0].fallback, Fallback::InstructionOverlapsLabel);
        assert_eq!(warnings[0].addr, base + 0xC0);
        assert!(
            asm.contains(&format!("_{:08x}:", base + 0xC2)),
            "pool label must still print: {asm}"
        );
        assert!(asm.contains(".byte 0x10, 0x00\n"), "{asm}");
        assert!(asm.contains(".byte 0x9F, 0xE5\n"), "{asm}");
    }

    #[test]
    fn truncated_mark_at_image_end_does_not_panic() {
        let (image, mut map) = fixture();
        let base = map.canonical().base();
        let len = image.bytes().len() as u32;
        map.mark_range(base + len - 2, base + len, Flags::EXEC | Flags::WORD);
        let (asm, warnings) = emit_with_warnings(&image, &map);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0].fallback, Fallback::InstructionOverlapsLabel);
        assert_eq!(warnings[0].addr, base + len - 2);
        assert!(asm.contains(".byte 0x00, 0x00\n"), "{asm}");
    }

    #[test]
    fn misaligned_thumb_function_warns_and_keeps_label() {
        let (image, mut map) = fixture();
        let base = map.canonical().base();
        map.mark_range(base + 0xC8, base + 0xD6, Flags::EXEC | Flags::THUMB);
        map.mark(base + 0xC9, Flags::FUNC_START);
        let (asm, warnings) = emit_with_warnings(&image, &map);
        assert!(
            warnings.iter().any(
                |warning| warning.fallback == Fallback::MisalignedThumbFunction
                    && warning.addr == base + 0xC9
            ),
            "{warnings:?}"
        );
        assert!(
            warnings.iter().any(
                |warning| warning.fallback == Fallback::InstructionOverlapsLabel
                    && warning.addr == base + 0xC8
            ),
            "the span holding the odd label must demote: {warnings:?}"
        );
        assert!(asm.contains(&format!("sub_{:08x}:", base + 0xC9)), "{asm}");
        assert!(
            !asm.contains(".byte 0x04, 0x49"),
            "label byte not swallowed: {asm}"
        );
    }

    #[test]
    fn word_reference_symbolic_only_when_labeled() {
        let (image, map) = fixture_with_pool(0x0200_00C9);
        let mut map = map;
        let base = map.canonical().base();
        map.mark_range(base + 0xC8, base + 0xD6, Flags::EXEC | Flags::THUMB);
        map.mark(base + 0xC8, Flags::FUNC_START);
        let (asm, warnings) = emit_with_warnings(&image, &map);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(
            asm.contains(&format!(".word sub_{:08x}+1\n", base + 0xC8)),
            "{asm}"
        );
    }

    #[test]
    fn word_reference_warns_when_thumb_code_has_no_label() {
        let (image, map) = fixture_with_pool(0x0200_00C9);
        let mut map = map;
        let base = map.canonical().base();
        map.mark_range(base + 0xC8, base + 0xD6, Flags::EXEC | Flags::THUMB);
        let (asm, warnings) = emit_with_warnings(&image, &map);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0].fallback, Fallback::WordReferenceNotProvable);
        assert_eq!(warnings[0].addr, base + 0xD8);
        assert!(asm.contains(".word 0x020000C9\n"), "{asm}");
    }

    #[test]
    fn word_reference_stays_literal_when_target_is_offbase() {
        let (image, map) = fixture();
        let (asm, warnings) = emit_with_warnings(&image, &map);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(asm.contains(".word 0x080000C9\n"), "{asm}");
    }

    #[test]
    fn bl_from_lr_demotes_to_raw_bytes() {
        let mut bytes = vec![0u8; 256];
        bytes[0x00..0x04].copy_from_slice(&0xEA00_0030u32.to_le_bytes());
        bytes[0xC8..0xCA].copy_from_slice(&0xF800u16.to_le_bytes());
        bytes[0xCA..0xCC].copy_from_slice(&0x8000u16.to_le_bytes());
        let image = Image::from_bytes(bytes).expect("fixture parses");
        let mut map = FlagMap::from_image(&image);
        let base = map.canonical().base();
        map.mark_range(base, base + 4, Flags::EXEC | Flags::WORD);
        map.mark_range(base + 0xC8, base + 0xCC, Flags::EXEC | Flags::THUMB);
        let (asm, warnings) = emit_with_warnings(&image, &map);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0].fallback, Fallback::BranchWithoutLabel);
        assert_eq!(warnings[0].addr, base + 0xC8);
        assert!(asm.contains(".word 0x8000F800"), "{asm}");
        assert!(!asm.contains("bl lr"), "{asm}");
    }

    #[test]
    fn warnings_appear_as_comments_in_the_assembly() {
        let (image, mut map) = fixture();
        let base = map.canonical().base();
        clear(&mut map, base, 0xC2, 0xC4);
        let (asm, warnings) = emit_with_warnings(&image, &map);
        assert_eq!(warnings.len(), 1);
        assert!(
            asm.contains(&format!("@ WARNING: {}\n", warnings[0].message)),
            "{asm}"
        );
    }
}
