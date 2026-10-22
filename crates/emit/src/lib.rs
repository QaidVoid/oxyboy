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

use analysis::{FlagMap, Flags};
use decode::{ArmInstr, Condition, ThumbInstr, arm_text, decode_arm, decode_thumb, thumb_text};
use rom::Image;

/// Emits assembly reproducing `image` byte-for-byte, driven by `map`.
///
/// Code flags (EXEC) decide instruction bytes, the THUMB flag picks the
/// assembler dialect, and everything else is emitted as data. Labels:
/// `sub_XXXXXXXX` at FUNC_START bytes, `loc_XXXXXXXX` at branch targets
/// that land on executed code, `_*XXXXXXXX` at referenced data
/// (literal pools).
#[must_use]
pub fn emit(image: &Image, map: &FlagMap) -> String {
    let bytes = image.bytes();
    let canon = map.canonical();
    let flags = flag_bytes(map, bytes.len());

    let labels = collect_labels(bytes, &flags, canon.base());
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
            let (step, line) = if run.contains(Flags::THUMB) {
                if dialect != Dialect::Thumb {
                    out.push_str(".thumb\n");
                    dialect = Dialect::Thumb;
                }
                emit_thumb(bytes, index, addr, &labels)
            } else {
                if dialect != Dialect::Arm {
                    out.push_str(".arm\n");
                    dialect = Dialect::Arm;
                }
                emit_arm(bytes, index, addr, &labels)
            };
            out.push_str(&line);
            out.push('\n');
            index += step;
        } else {
            index += emit_data(bytes, &flags, index, &mut out);
        }
    }
    out
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

/// Emits one ARM instruction, returning its byte size.
fn emit_arm(
    bytes: &[u8],
    index: usize,
    addr: u32,
    labels: &BTreeMap<u32, String>,
) -> (usize, String) {
    let word = word(bytes, index);
    if let ArmInstr::Branch { link, offset } = decode_arm(word) {
        let target = addr.wrapping_add(8).wrapping_add(offset as u32);
        if let Some(label) = labels.get(&target) {
            let cond = cond_suffix(word);
            return (
                4,
                format!("{}{} {}", if link { "bl" } else { "b" }, cond, label),
            );
        }
    }
    (4, arm_text(word))
}

/// Emits one THUMB instruction (BL consumes its second halfword),
/// returning its byte size.
fn emit_thumb(
    bytes: &[u8],
    index: usize,
    addr: u32,
    labels: &BTreeMap<u32, String>,
) -> (usize, String) {
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
        _ => None,
    };
    if let Some((size, target)) = target
        && let Some(label) = labels.get(&target)
    {
        let text = match instr {
            ThumbInstr::ConditionalBranch { cond, .. } => {
                format!("b{} {}", cond.mnemonic(), label)
            }
            ThumbInstr::UnconditionalBranch { .. } => format!("b {label}"),
            _ => format!("bl {label}"),
        };
        return (size, text);
    }
    if matches!(instr, ThumbInstr::LongBranchWithLink { .. }) {
        (4, thumb_text(first, next))
    } else {
        (2, thumb_text(first, 0))
    }
}

/// Emits data bytes from `index` on: aligned WORD runs as `.word`,
/// everything else grouped into `.byte` lines. Returns bytes consumed.
/// Byte runs stop at executed bytes (instructions render their own
/// text) and at literal-pool starts (labeled and emitted as words).
fn emit_data(bytes: &[u8], flags: &[u8], index: usize, out: &mut String) -> usize {
    if index + 4 <= bytes.len()
        && index.is_multiple_of(4)
        && (index..index + 4).all(|i| Flags::from_bits(flags[i]).contains(Flags::WORD))
    {
        let value = u32::from_le_bytes(bytes[index..index + 4].try_into().expect("in-bounds"));
        out.push_str(&format!(".word 0x{value:08X}\n"));
        return 4;
    }
    let start = index;
    let mut end = index;
    while end < bytes.len() && end - start < 12 && !boundary(flags[end]) {
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

/// True when a data byte run must stop before this byte.
fn boundary(bits: u8) -> bool {
    let run = Flags::from_bits(bits);
    run.contains(Flags::EXEC) || run.contains(Flags::LITERAL)
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
    /// at 0xD8, and unmarked (data) filler elsewhere.
    fn fixture() -> (Image, FlagMap) {
        let mut bytes = vec![0u8; 256];
        bytes[0x00..0x04].copy_from_slice(&0xEA00_002Eu32.to_le_bytes());
        bytes[0x04..0xC0].fill(0x5A);
        bytes[0xC0..0xC4].copy_from_slice(&0xE59F_0010u32.to_le_bytes());
        bytes[0xC4..0xC8].copy_from_slice(&0xE12F_FF10u32.to_le_bytes());
        bytes[0xC8..0xD6].copy_from_slice(&[
            0x04, 0x49, 0x03, 0x22, 0x0A, 0x80, 0x04, 0x4B, 0x04, 0x48, 0x18, 0x60, 0xFE, 0xE7,
        ]);
        bytes[0xD8..0xDC].copy_from_slice(&0x0800_00C9u32.to_le_bytes());
        let image = Image::from_bytes(bytes).expect("fixture parses");
        let mut map = FlagMap::from_image(&image);
        let base = map.canonical().base();
        map.set(base, Flags::EXEC | Flags::WORD | Flags::FUNC_START);
        map.mark_range(base + 0xC0, base + 0xC8, Flags::EXEC | Flags::WORD);
        map.mark_range(base + 0xD8, base + 0xDC, Flags::WORD | Flags::LITERAL);
        (image, map)
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
}
