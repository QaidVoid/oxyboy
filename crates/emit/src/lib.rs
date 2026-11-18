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

use std::collections::{BTreeMap, BTreeSet};

use analysis::config::sanitize_identifier;
use analysis::{Canonical, Config, FlagMap, Flags};
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
    /// A config name was not a valid assembler identifier and was
    /// rewritten (e.g. `my-func` to `my_func`); the config keeps the
    /// original spelling.
    NameSanitized,
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
            Self::NameSanitized => "name-sanitized",
        }
    }
}

/// Applies config names over the generated label map: a named config
/// function renames (or creates) the label at its canonical address.
/// Names that are not valid assembler identifiers are sanitized, with a
/// [`Fallback::NameSanitized`] warning per changed name.
fn apply_config_names(
    labels: &mut BTreeMap<u32, String>,
    config: &Config,
    canon: Canonical,
    image_len: usize,
    warnings: &mut Vec<Warning>,
) {
    let end = canon.base() + image_len as u32;
    for function in &config.functions {
        let Some(raw) = function.name.as_deref() else {
            continue;
        };
        let Some(addr) = canon.canonicalize(function.addr) else {
            continue;
        };
        if addr < canon.base() || addr >= end {
            continue;
        }
        let name = sanitize_identifier(raw);
        if name != raw {
            warnings.push(Warning {
                addr,
                fallback: Fallback::NameSanitized,
                message: format!(
                    "name-sanitized at 0x{addr:08x}: '{raw}' is not a valid \
                     assembler identifier; using '{name}'"
                ),
            });
        }
        labels.insert(addr, name);
    }
}

/// Emits assembly reproducing `image` byte-for-byte, driven by `map`,
/// returning the assembly and every fallback warning raised on the way.
///
/// Code flags (EXEC) decide instruction bytes, the THUMB flag picks the
/// assembler dialect, and everything else is emitted as data. Labels:
/// `sub_XXXXXXXX` at FUNC_START bytes, `loc_XXXXXXXX` at branch targets
/// that land on executed code, `_*XXXXXXXX` at referenced data
/// (literal pools). Named config functions rename or create the label
/// at their address; names that are not valid assembler identifiers
/// are sanitized, with a [`Fallback::NameSanitized`] warning.
///
/// Whenever rendering an address faithfully would depend on analysis
/// being right, the emitter falls back to a form that cannot break the
/// byte-identical invariant (raw bytes, or a literal instead of a
/// symbolic reference) and records a [`Warning`] naming the address;
/// the same warnings appear as `@ WARNING:` comments in the assembly.
#[must_use]
pub fn emit_with_warnings(image: &Image, map: &FlagMap, config: &Config) -> (String, Vec<Warning>) {
    let bytes = image.bytes();
    let canon = map.canonical();
    let flags = flag_bytes(map, bytes.len());
    let mut labels = collect_labels(bytes, &flags, canon.base());
    let mut warnings = Vec::new();
    apply_config_names(&mut labels, config, canon, bytes.len(), &mut warnings);
    let mut refs = BTreeSet::new();
    let mut ctx = Ctx {
        bytes,
        flags: &flags,
        canon,
        labels: &labels,
        warnings: &mut warnings,
        refs: &mut refs,
        limit: bytes.len(),
    };
    let mut out = String::from(".syntax unified\n.cpu arm7tdmi\n.section .text, \"ax\"\n.arm\n");
    let end = canon.base() + bytes.len() as u32;
    out.push_str(&emit_span_body(&mut ctx, canon.base(), end));
    (out, warnings)
}

/// Emits assembly, dropping the fallback warnings; see
/// [`emit_with_warnings`].
#[must_use]
pub fn emit(image: &Image, map: &FlagMap, config: &Config) -> String {
    emit_with_warnings(image, map, config).0
}

/// Shared state for one emission pass over a byte range.
struct Ctx<'a> {
    bytes: &'a [u8],
    flags: &'a [u8],
    canon: Canonical,
    labels: &'a BTreeMap<u32, String>,
    warnings: &'a mut Vec<Warning>,
    /// Addresses of labels referenced while emitting this span.
    refs: &'a mut BTreeSet<u32>,
    /// Exclusive byte offset the pass must not size instructions or
    /// data runs past (the span end, clamped to the image).
    limit: usize,
}

impl Ctx<'_> {
    /// The label at `target`, recording the reference.
    fn label(&mut self, target: u32) -> Option<&String> {
        if let Some(name) = self.labels.get(&target) {
            self.refs.insert(target);
            Some(name)
        } else {
            None
        }
    }

    /// Records one fallback warning.
    fn warn(&mut self, addr: u32, fallback: Fallback, message: String) {
        self.warnings.push(Warning {
            addr,
            fallback,
            message,
        });
    }
}

/// Emits the bytes of `[start, end)` (canonical addresses). Dialect
/// switches start from ARM, so every span is a valid file body. The
/// pass reads at most `end` bytes: an instruction whose span would
/// cross `end` demotes to raw bytes like any truncated span.
fn emit_span_body(ctx: &mut Ctx<'_>, start: u32, end: u32) -> String {
    let base = ctx.canon.base();
    ctx.limit = ((end - base) as usize).min(ctx.bytes.len());
    let mut out = String::new();
    let mut dialect = Dialect::Arm;
    let mut index = (start - base) as usize;
    while index < ctx.limit {
        let addr = base + index as u32;
        if let Some(label) = ctx.labels.get(&addr) {
            out.push_str(&format!("{label}:\n"));
        }
        let run = Flags::from_bits(ctx.flags[index]);
        if run.contains(Flags::EXEC) {
            warn_misaligned_thumb_function(addr, run, ctx.warnings);
            let warned = ctx.warnings.len();
            let (step, line) = if run.contains(Flags::THUMB) {
                if dialect != Dialect::Thumb {
                    out.push_str(".thumb\n");
                    dialect = Dialect::Thumb;
                }
                emit_thumb(ctx, index, addr)
            } else {
                if dialect != Dialect::Arm {
                    out.push_str(".arm\n");
                    dialect = Dialect::Arm;
                }
                emit_arm(ctx, index, addr)
            };
            for warning in &ctx.warnings[warned..] {
                out.push_str(&format!("@ WARNING: {}\n", warning.message));
            }
            out.push_str(&line);
            out.push('\n');
            index += step;
        } else {
            index += emit_data(ctx, index, &mut out);
        }
    }
    out
}

/// A module split plan: contiguous address spans, one output file per
/// stem. Built from config `<module>.s` claims; regions no module
/// claims belong to the `default` stem.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModulePlan {
    spans: Vec<(u32, String)>,
}

/// Stem of the catch-all module for unclaimed regions.
const DEFAULT_STEM: &str = "default";

impl ModulePlan {
    /// Builds a plan from config module claims. Every claimed address
    /// opens a span owned by its module. Boundaries snap up to
    /// 4-aligned addresses (claims inside one word collapse) so every
    /// module section's size is a multiple of the assembler's
    /// alignment for its contents, which is what keeps the exact
    /// placement padding-free.
    #[must_use]
    pub fn from_config(config: &Config, image: &Image) -> Self {
        let canon = Canonical::for_kind(image.kind());
        let base = canon.base();
        let end = base + image.bytes().len() as u32;
        let mut boundaries: BTreeMap<u32, String> = BTreeMap::new();
        for function in &config.functions {
            let Some(stem) = function.module.as_deref() else {
                continue;
            };
            let Some(addr) = canon.canonicalize(function.addr) else {
                continue;
            };
            let snapped = addr.next_multiple_of(4);
            if snapped > base && snapped < end {
                boundaries.insert(snapped, stem.to_string());
            }
        }
        let mut spans: Vec<(u32, String)> = Vec::new();
        match boundaries.first_key_value() {
            Some((&first, _)) if first == base => {}
            _ => spans.push((base, DEFAULT_STEM.to_string())),
        }
        spans.extend(boundaries.iter().map(|(&addr, stem)| (addr, stem.clone())));
        spans.dedup_by(|a, b| a.1 == b.1);
        Self { spans }
    }

    /// The spans in address order as `(start, stem)`.
    #[must_use]
    pub fn spans(&self) -> &[(u32, String)] {
        &self.spans
    }
}

/// The per-module emission: one assembly file per module stem plus a
/// GNU ld script placing each module section at its exact image
/// address, so linking the per-module objects reproduces the image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleEmission {
    /// Assembly files keyed by module stem.
    pub files: BTreeMap<String, String>,
    /// GNU ld script placing every module section.
    pub script: String,
    /// Fallback warnings across all modules.
    pub warnings: Vec<Warning>,
}

/// Emits one assembly file per [`ModulePlan`] stem and the matching
/// linker script.
///
/// Every label referenced from a module other than its own is declared
/// `.global` in the defining module's file: without the declaration the
/// link fails with an undefined reference instead of silently
/// misplacing bytes. Disjoint spans of one stem share the file under
/// differently numbered sections.
#[must_use]
pub fn emit_modules(
    image: &Image,
    map: &FlagMap,
    plan: &ModulePlan,
    config: &Config,
) -> ModuleEmission {
    let bytes = image.bytes();
    let canon = map.canonical();
    let flags = flag_bytes(map, bytes.len());
    let mut labels = collect_labels(bytes, &flags, canon.base());
    let mut warnings = Vec::new();
    apply_config_names(&mut labels, config, canon, bytes.len(), &mut warnings);
    let end = canon.base() + bytes.len() as u32;

    // One section per span; a stem with disjoint spans gets numbered
    // sections so every output section name stays unique.
    let mut sections: Vec<(String, String, u32)> = Vec::new();
    let mut used: BTreeMap<String, u32> = BTreeMap::new();
    for &(start, ref stem) in plan.spans() {
        let count = used.entry(stem.clone()).or_default();
        let section = if *count == 0 {
            format!(".text.{stem}")
        } else {
            format!(".text.{stem}.{count}")
        };
        *count += 1;
        sections.push((stem.clone(), section, start));
    }

    let mut pass: Vec<(String, String, String, BTreeSet<u32>)> = Vec::new();
    for i in 0..sections.len() {
        let span_end = sections.get(i + 1).map_or(end, |next| next.2);
        let mut refs = BTreeSet::new();
        let mut ctx = Ctx {
            bytes,
            flags: &flags,
            canon,
            labels: &labels,
            warnings: &mut warnings,
            refs: &mut refs,
            limit: bytes.len(),
        };
        let body = emit_span_body(&mut ctx, sections[i].2, span_end);
        pass.push((sections[i].0.clone(), sections[i].1.clone(), body, refs));
    }

    // The module that defines each label.
    let mut definer: BTreeMap<u32, &str> = BTreeMap::new();
    for (i, &(ref stem, _, start)) in sections.iter().enumerate() {
        let span_end = sections.get(i + 1).map_or(end, |next| next.2);
        for label in labels.range(start..span_end).map(|(&addr, _)| addr) {
            definer.insert(label, stem);
        }
    }

    // Globals: referenced from a foreign module, declared in the
    // defining module's file.
    let mut globals: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (stem, _, _, refs) in &pass {
        for &addr in refs {
            if let Some(owner) = definer.get(&addr)
                && *owner != stem
            {
                globals
                    .entry((*owner).to_string())
                    .or_default()
                    .insert(labels[&addr].clone());
            }
        }
    }

    let mut files: BTreeMap<String, String> = BTreeMap::new();
    let mut script = String::from("/* Generated by oxyboy: module placement. */\nSECTIONS\n{\n");
    for (stem, section, body, _) in pass {
        let file = files.entry(stem.clone()).or_default();
        if file.is_empty() {
            file.push_str(".syntax unified\n.cpu arm7tdmi\n");
            if let Some(names) = globals.get(&stem) {
                for name in names {
                    file.push_str(&format!(".global {name}\n"));
                }
            }
        }
        file.push_str(&format!(".section {section}, \"ax\"\n.arm\n"));
        file.push_str(&body);
        let start = sections
            .iter()
            .find(|(_, s, _)| s == &section)
            .map_or(0, |&(_, _, start)| start);
        script.push_str(&format!(
            "    {section} 0x{start:08x} : {{ *({section}) }}\n"
        ));
    }
    script.push_str("}\n");

    ModuleEmission {
        files,
        script,
        warnings,
    }
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
fn emit_arm(ctx: &mut Ctx<'_>, index: usize, addr: u32) -> (usize, String) {
    if !span_ok(ctx, index, 4, false) || label_inside(ctx.labels, addr, 4) {
        return demote(ctx, index, 4, addr);
    }
    let word = word(ctx.bytes, index);
    let instr = decode_arm(word);
    if let ArmInstr::Branch { link, offset } = instr {
        let target = addr.wrapping_add(8).wrapping_add(offset as u32);
        if let Some(label) = ctx.label(target) {
            let cond = cond_suffix(word);
            return (
                4,
                format!("{}{} {}", if link { "bl" } else { "b" }, cond, label),
            );
        }
        ctx.warn(
            addr,
            Fallback::BranchWithoutLabel,
            format!(
                "branch-without-label at 0x{addr:08x}: target 0x{target:08x} \
                 has no label; emitting raw bytes"
            ),
        );
        return (4, raw_bytes(ctx.bytes, index, 4));
    }
    if instr.is_unpredictable() {
        ctx.warn(
            addr,
            Fallback::UnpredictableEncoding,
            format!(
                "unpredictable-encoding at 0x{addr:08x}: word 0x{word:08X} \
                 is UNPREDICTABLE on the ARM7TDMI; emitting raw bytes"
            ),
        );
        return (4, raw_bytes(ctx.bytes, index, 4));
    }
    (4, arm_text(word))
}
/// Emits one THUMB instruction (BL consumes its second halfword),
/// returning its byte size. Falls back like [`emit_arm`].
fn emit_thumb(ctx: &mut Ctx<'_>, index: usize, addr: u32) -> (usize, String) {
    if !span_ok(ctx, index, 2, true) || label_inside(ctx.labels, addr, 2) {
        return demote(ctx, index, 2, addr);
    }
    let first = half(ctx.bytes, index);
    let next = half(ctx.bytes, index + 2);
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
            ctx.warn(
                addr,
                Fallback::BranchWithoutLabel,
                format!(
                    "branch-without-label at 0x{addr:08x}: BL from LR has \
                     no static target; emitting raw bytes"
                ),
            );
            return (2, raw_bytes(ctx.bytes, index, 2));
        }
        _ => None,
    };
    if let Some((size, target)) = target {
        let size_ok = span_ok(ctx, index, size, true);
        if !size_ok || label_inside(ctx.labels, addr, size as u32) {
            return demote(ctx, index, size, addr);
        }
        if let Some(label) = ctx.label(target) {
            let text = match instr {
                ThumbInstr::ConditionalBranch { cond, .. } => {
                    format!("b{} {}", cond.mnemonic(), label)
                }
                ThumbInstr::UnconditionalBranch { .. } => format!("b {label}"),
                _ => format!("bl {label}"),
            };
            return (size, text);
        }
        ctx.warn(
            addr,
            Fallback::BranchWithoutLabel,
            format!(
                "branch-without-label at 0x{addr:08x}: target \
                 0x{target:08x} has no label; emitting raw bytes"
            ),
        );
        return (size, raw_bytes(ctx.bytes, index, size));
    }
    if instr.is_unpredictable() {
        ctx.warn(
            addr,
            Fallback::UnpredictableEncoding,
            format!(
                "unpredictable-encoding at 0x{addr:08x}: halfword \
                 0x{first:04X} is UNPREDICTABLE on the ARM7TDMI; emitting \
                 raw bytes"
            ),
        );
        return (2, raw_bytes(ctx.bytes, index, 2));
    }
    (2, thumb_text(first, 0))
}
/// True when `size` bytes from `index` are all executed in the same
/// state and fit before the pass limit.
fn span_ok(ctx: &Ctx<'_>, index: usize, size: usize, thumb: bool) -> bool {
    if index + size > ctx.limit {
        return false;
    }
    (index..index + size).all(|i| {
        let run = Flags::from_bits(ctx.flags[i]);
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
fn demote(ctx: &mut Ctx<'_>, index: usize, max: usize, addr: u32) -> (usize, String) {
    ctx.warn(
        addr,
        Fallback::InstructionOverlapsLabel,
        format!(
            "instruction-overlaps-label at 0x{addr:08x}: instruction span \
             contains a label or is truncated; emitting raw bytes"
        ),
    );
    let limit = ctx.limit.min(index + max);
    let stop = byte_offset_to_next_label(ctx.labels, addr, max as u32)
        .unwrap_or(max)
        .min(max);
    let mut end = index + 1;
    while end < limit
        && end - index < stop
        && Flags::from_bits(ctx.flags[end]).contains(Flags::EXEC)
    {
        end += 1;
    }
    let step = end - index;
    (step, raw_bytes(ctx.bytes, index, step))
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
/// text) and at labeled addresses (labels must print on their own
/// line). A word carrying the THUMB bit-0 pattern is emitted as a
/// symbolic `label+1` reference only when the target is provably a
/// labeled THUMB code address; otherwise the literal is kept.
fn emit_data(ctx: &mut Ctx<'_>, index: usize, out: &mut String) -> usize {
    let addr = ctx.canon.base() + index as u32;
    if index + 4 <= ctx.limit
        && index.is_multiple_of(4)
        && (index..index + 4).all(|i| {
            let run = Flags::from_bits(ctx.flags[i]);
            run.contains(Flags::WORD) && !run.contains(Flags::EXEC)
        })
        && !label_inside(ctx.labels, addr, 4)
    {
        let value = u32::from_le_bytes(ctx.bytes[index..index + 4].try_into().expect("in-bounds"));
        out.push_str(&word_line(ctx, index, value));
        return 4;
    }
    let start = index;
    let base = ctx.canon.base();
    // Always emit at least one byte: the start byte itself may carry a
    // boundary flag (e.g. a pool label inside a demoted region).
    let mut end = index + 1;
    while end < ctx.limit
        && end - start < 12
        && !Flags::from_bits(ctx.flags[end]).contains(Flags::EXEC)
        && !ctx.labels.contains_key(&(base + end as u32))
    {
        end += 1;
    }
    let list = ctx.bytes[start..end]
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
fn word_line(ctx: &mut Ctx<'_>, index: usize, value: u32) -> String {
    let addr = ctx.canon.base() + index as u32;
    if value & 1 == 1
        && let Some(target) = value.checked_sub(1)
        && let Some(offset) = ctx.canon.offset(target)
        && offset < ctx.bytes.len()
        && Flags::from_bits(ctx.flags[offset]).contains(Flags::EXEC | Flags::THUMB)
    {
        if let Some(label) = ctx.label(target) {
            return format!(".word {label}+1\n");
        }
        ctx.warn(
            addr,
            Fallback::WordReferenceNotProvable,
            format!(
                "word-reference-not-provable at 0x{addr:08x}: value \
                 0x{value:08X} points at unlabeled THUMB code; keeping the \
                 literal"
            ),
        );
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
    use analysis::{FunctionSpec, State};

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
        let asm = emit(&image, &map, &Config::default());
        assert!(asm.starts_with(".syntax unified\n.cpu arm7tdmi\n.section .text, \"ax\"\n.arm\n"));
    }

    #[test]
    fn labels_functions_internal_branches_and_pools() {
        let (image, map) = fixture();
        let asm = emit(&image, &map, &Config::default());
        let base = map.canonical().base();
        assert!(asm.contains(&format!("sub_{base:08x}:\n")));
        assert!(asm.contains(&format!("b loc_{:08x}\n", base + 0xC0)));
        assert!(asm.contains(&format!("loc_{:08x}:\n", base + 0xC0)));
        assert!(asm.contains(&format!("_{:08x}:\n.word 0x080000C9\n", base + 0xD8)));
    }

    #[test]
    fn instructions_never_bleed_into_byte_runs() {
        let (image, map) = fixture();
        let asm = emit(&image, &map, &Config::default());
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
        let asm = emit(&image, &map, &Config::default());
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
        let asm = emit(&image, &map, &Config::default());
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
        let (asm, warnings) = emit_with_warnings(&image, &map, &Config::default());
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
        let (asm, warnings) = emit_with_warnings(&image, &map, &Config::default());
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
        let (asm, warnings) = emit_with_warnings(&image, &map, &Config::default());
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
        let (asm, warnings) = emit_with_warnings(&image, &map, &Config::default());
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
        let (asm, warnings) = emit_with_warnings(&image, &map, &Config::default());
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
        let (asm, warnings) = emit_with_warnings(&image, &map, &Config::default());
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
        let (asm, warnings) = emit_with_warnings(&image, &map, &Config::default());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0].fallback, Fallback::WordReferenceNotProvable);
        assert_eq!(warnings[0].addr, base + 0xD8);
        assert!(asm.contains(".word 0x020000C9\n"), "{asm}");
    }

    #[test]
    fn word_reference_stays_literal_when_target_is_offbase() {
        let (image, map) = fixture();
        let (asm, warnings) = emit_with_warnings(&image, &map, &Config::default());
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
        let (asm, warnings) = emit_with_warnings(&image, &map, &Config::default());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0].fallback, Fallback::BranchWithoutLabel);
        assert_eq!(warnings[0].addr, base + 0xC8);
        assert!(asm.contains(".byte 0x00, 0xF8"), "{asm}");
        assert!(asm.contains("strh r0, [r0, #0]"), "{asm}");
        assert!(!asm.contains("bl lr"), "{asm}");
    }

    #[test]
    fn bl_from_lr_at_the_image_end_stays_in_bounds() {
        let mut bytes = vec![0u8; 256];
        bytes[0x00..0x04].copy_from_slice(&0xEA00_0030u32.to_le_bytes());
        bytes[0xFE..0x100].copy_from_slice(&0xF800u16.to_le_bytes());
        let image = Image::from_bytes(bytes).expect("fixture parses");
        let mut map = FlagMap::from_image(&image);
        let base = map.canonical().base();
        map.mark_range(base, base + 4, Flags::EXEC | Flags::WORD);
        map.mark_range(base + 0xFE, base + 0x100, Flags::EXEC | Flags::THUMB);
        let (asm, warnings) = emit_with_warnings(&image, &map, &Config::default());
        assert_eq!(warnings[0].fallback, Fallback::BranchWithoutLabel);
        assert!(asm.contains(".byte 0x00, 0xF8"), "{asm}");
    }

    #[test]
    fn warnings_appear_as_comments_in_the_assembly() {
        let (image, mut map) = fixture();
        let base = map.canonical().base();
        clear(&mut map, base, 0xC2, 0xC4);
        let (asm, warnings) = emit_with_warnings(&image, &map, &Config::default());
        assert_eq!(warnings.len(), 1);
        assert!(
            asm.contains(&format!("@ WARNING: {}\n", warnings[0].message)),
            "{asm}"
        );
    }

    /// One config function claimed into `module`.
    fn claimed(addr: u32, module: &str) -> FunctionSpec {
        let mut function = FunctionSpec::bare(addr, State::Arm);
        function.module = Some(module.to_string());
        function
    }

    #[test]
    fn module_plan_snaps_claims_and_merges_adjacent_stems() {
        let (image, _) = fixture();
        let base = map_base(&image);
        let config = Config {
            functions: vec![
                claimed(base + 0xC1, "a"),
                claimed(base + 0xC2, "a"),
                claimed(base + 0x88, "b"),
            ],
            tables: Vec::new(),
        };
        let plan = ModulePlan::from_config(&config, &image);
        assert_eq!(
            plan.spans(),
            &[
                (base, "default".to_string()),
                (base + 0x88, "b".to_string()),
                (base + 0xC4, "a".to_string()),
            ]
        );
    }

    #[test]
    fn module_plan_ignores_claims_outside_the_image() {
        let (image, _) = fixture();
        let base = map_base(&image);
        let len = image.bytes().len() as u32;
        let config = Config {
            functions: vec![
                claimed(0x0300_0000, "nowhere"),
                claimed(base + 0xC0 + len, "past_end"),
            ],
            tables: Vec::new(),
        };
        let plan = ModulePlan::from_config(&config, &image);
        assert_eq!(plan.spans(), &[(base, "default".to_string())]);
    }

    /// The canonical base of a built image.
    fn map_base(image: &Image) -> u32 {
        Canonical::for_kind(image.kind()).base()
    }

    #[test]
    fn modules_declare_globals_for_cross_module_references() {
        let (image, map) = fixture();
        let base = map.canonical().base();
        let config = Config {
            functions: vec![claimed(base + 0xC0, "code")],
            tables: Vec::new(),
        };
        let plan = ModulePlan::from_config(&config, &image);
        let emission = emit_modules(&image, &map, &plan, &config);
        assert_eq!(emission.files.len(), 2);
        let default_file = &emission.files["default"];
        let code_file = &emission.files["code"];
        assert!(
            default_file.contains(&format!("b loc_{:08x}\n", base + 0xC0)),
            "{default_file}"
        );
        assert!(
            code_file.contains(&format!(".global loc_{:08x}\n", base + 0xC0)),
            "{code_file}"
        );
        assert!(!default_file.contains(".global"), "{default_file}");
        assert!(
            emission
                .script
                .contains(&format!(".text.default 0x{base:08x}")),
            "{}",
            emission.script
        );
        assert!(
            emission
                .script
                .contains(&format!(".text.code 0x{:08x}", base + 0xC0)),
            "{}",
            emission.script
        );
    }

    #[test]
    fn modules_without_foreign_references_skip_globals() {
        let (image, map) = fixture();
        let base = map.canonical().base();
        let config = Config {
            functions: vec![claimed(base + 0xC4, "code")],
            tables: Vec::new(),
        };
        let plan = ModulePlan::from_config(&config, &image);
        let emission = emit_modules(&image, &map, &plan, &config);
        for (stem, file) in &emission.files {
            assert!(!file.contains(".global"), "{stem}: {file}");
        }
        // The pool label and word stay inside the code module.
        assert!(emission.files["code"].contains(".word 0x080000C9\n"));
    }

    #[test]
    fn config_names_override_generated_labels() {
        let (image, map) = fixture();
        let base = map.canonical().base();
        let config = Config {
            functions: vec![FunctionSpec {
                name: Some("entry_loop".to_string()),
                ..FunctionSpec::bare(base, State::Arm)
            }],
            tables: Vec::new(),
        };
        let asm = emit(&image, &map, &config);
        assert!(asm.contains("entry_loop:\n"), "{asm}");
        assert!(!asm.contains("sub_{base:08x}"), "{asm}");
    }

    #[test]
    fn config_names_create_labels_on_data() {
        let (image, map) = fixture();
        let base = map.canonical().base();
        // 0x30 is unmarked filler; the name alone must place a label.
        let config = Config {
            functions: vec![FunctionSpec {
                name: Some("blob".to_string()),
                ..FunctionSpec::bare(base + 0x30, State::Arm)
            }],
            tables: Vec::new(),
        };
        let asm = emit(&image, &map, &config);
        assert!(asm.contains("blob:\n"), "{asm}");
    }

    #[test]
    fn illegal_names_are_sanitized_with_a_warning() {
        let (image, map) = fixture();
        let base = map.canonical().base();
        let config = Config {
            functions: vec![FunctionSpec {
                name: Some("9my-func".to_string()),
                ..FunctionSpec::bare(base, State::Arm)
            }],
            tables: Vec::new(),
        };
        let (asm, warnings) = emit_with_warnings(&image, &map, &config);
        assert!(asm.contains("_9my_func:\n"), "{asm}");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0].fallback, Fallback::NameSanitized);
        assert_eq!(warnings[0].addr, base);
        assert!(warnings[0].message.contains("_9my_func"), "{warnings:?}");
    }

    #[test]
    fn modules_use_sanitized_config_names() {
        let (image, map) = fixture();
        let base = map.canonical().base();
        let mut function = claimed(base + 0xC0, "code");
        function.name = Some("my-func".to_string());
        let config = Config {
            functions: vec![function],
            tables: Vec::new(),
        };
        let plan = ModulePlan::from_config(&config, &image);
        let emission = emit_modules(&image, &map, &plan, &config);
        assert!(
            emission.files["code"].contains(".global my_func\n"),
            "{:#?}",
            emission.files
        );
        assert!(
            emission
                .warnings
                .iter()
                .any(|warning| warning.fallback == Fallback::NameSanitized),
            "{:?}",
            emission.warnings
        );
    }
}
