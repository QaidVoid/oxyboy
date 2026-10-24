//! The `diff` subcommand: run one ROM through the interpreter and the
//! static-recompilation hybrid, frame by frame, and compare the full
//! machine state.
//!
//! Both paths start from identical boots (the same multiboot/cart
//! dispatch as `run`). The hybrid's translated regions come from the
//! analysis pipeline over the image: the entry seed, function
//! discovery, and one lifted region per accepted function (each region
//! ends where the next function starts or at the image end). After
//! every frame the runner compares registers, CPSR, EWRAM, IWRAM,
//! palette, VRAM, the IO image, save media, and the rendered
//! framebuffer; the first diverging frame ends the run.
//!
//! On a mismatch the runner re-executes both paths up to the diverging
//! frame with execution recording and runs the trace diff over
//! the two traces. The hybrid trace carries gaps at native chunks (the
//! compiled code records nothing), so the reported first difference
//! names either the interpreted instruction where the paths split or,
//! when the split sits inside a native span, the chunk entry that
//! precedes the divergence. Everything lands in a case directory
//! (default `target/diff-cases/<rom stem>`): the report, both traces,
//! and both frames as PNGs.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use analysis::discover::Thresholds;
use analysis::flags::{FlagMap, Seed};
use analysis::{State, analyze};
use ir::lift::FunctionRegion;
use rom::{Image, ImageKind};
use runtime::ppu::{VISIBLE_LINES, WIDTH};
use runtime::recomp::{Driver, TranslatedFn};
use runtime::system::System;
use trace::{Reader, diff};

use crate::RomArgs;

/// Cart execution base (see the `run` subcommand; no BIOS reset).
const CART_BASE: u32 = 0x0800_0000;

/// Arguments of `oxyboy diff`.
#[derive(Debug, clap::Args)]
pub struct DiffArgs {
    #[command(flatten)]
    pub rom: RomArgs,

    /// Frames to run before comparing the final state.
    #[arg(long, value_name = "N", default_value_t = 60)]
    pub frames: u32,

    /// Directory for mismatch case artifacts. The default lives under
    /// `target/` so case files never enter version control.
    #[arg(long, value_name = "DIR", default_value = "target/diff-cases")]
    pub case_dir: PathBuf,

    /// Compile scratch tag; defaults to the ROM file stem.
    #[arg(long, value_name = "TAG")]
    pub tag: Option<String>,
}

/// One translated region candidate: the function start from discovery
/// plus the exclusive end adopted from the next function start.
struct Region {
    start: u32,
    end: u32,
    state: State,
}

/// Runs the analysis over `image` and turns the accepted functions
/// into lifted, compiled translated regions.
///
/// # Errors
///
/// Returns a printable message when no host C compiler exists or the
/// module fails to compile.
pub fn build_hybrid(image: &Image, tag: &str) -> Result<Driver, String> {
    let mut map = FlagMap::from_image(image);
    let base = map.canonical().base();
    let entry = Seed {
        addr: base,
        state: State::Arm,
    };
    let seeds = [entry];
    let analysis = analyze(image, &mut map, &seeds, &seeds, Thresholds::default(), &[]);
    let mut starts: Vec<_> = analysis
        .discovery
        .functions
        .iter()
        .map(|f| (f.addr, f.state))
        .collect();
    starts.sort();
    starts.dedup();
    if starts.is_empty() {
        return Err("analysis found no functions to translate".to_string());
    }
    let image_end = base + image.bytes().len() as u32;
    let mut regions = Vec::new();
    for (index, &(start, state)) in starts.iter().enumerate() {
        let end = starts
            .get(index + 1)
            .map(|&(next, _)| next)
            .unwrap_or(image_end);
        if end <= start {
            continue;
        }
        regions.push(Region { start, end, state });
    }
    let mut entries = Vec::new();
    let mut functions = Vec::new();
    for region in &regions {
        let mut lifted = ir::lift::lift_function(
            image,
            &map,
            FunctionRegion {
                start: region.start,
                end: region.end,
                state: region.state,
            },
        );
        ir::opt::optimize(&mut lifted);
        entries.push(TranslatedFn {
            start: region.start,
            end: region.end,
            thumb: region.state == State::Thumb,
        });
        functions.push(lifted);
    }
    if entries.is_empty() {
        return Err("analysis produced no translatable regions".to_string());
    }
    let refs: Vec<(u32, &ir::Function)> = functions
        .iter()
        .enumerate()
        .map(|(i, f)| (entries[i].start, f))
        .collect();
    let source = recomp::emit_module(&refs);
    let module = recomp::Module::build(&source.c_source, tag)?;
    Ok(Driver::new(module, entries))
}

/// One comparable slice of machine state.
pub(crate) struct Slice {
    pub(crate) name: &'static str,
    pub(crate) bytes: Vec<u8>,
}

/// Captures the full comparable state of `system`: registers first,
/// then every memory image and the framebuffer.
pub(crate) fn snapshot(system: &System) -> Vec<Slice> {
    let mut regs = Vec::new();
    for i in 0..16u8 {
        regs.extend_from_slice(&system.cpu.reg_for_test(i).to_le_bytes());
    }
    regs.extend_from_slice(&system.cpu.cpsr().to_le_bytes());
    regs.extend_from_slice(&system.cpu.pc().to_le_bytes());
    let bus = system.bus();
    let mut slices = vec![Slice {
        name: "registers",
        bytes: regs,
    }];
    for (name, bytes) in [
        ("ewram", bus.ewram()),
        ("iwram", bus.iwram()),
        ("palette", bus.palette()),
        ("vram", bus.vram()),
        ("io", bus.io_image()),
    ] {
        slices.push(Slice {
            name,
            bytes: bytes.to_vec(),
        });
    }
    slices.push(Slice {
        name: "save",
        bytes: bus.save().to_bytes(),
    });
    slices.push(Slice {
        name: "framebuffer",
        bytes: system.framebuffer().to_vec(),
    });
    slices
}

/// The first difference between two snapshots: `None` when equal,
/// otherwise the slice name and the first differing offset with both
/// byte values.
fn first_difference(a: &[Slice], b: &[Slice]) -> Option<String> {
    for (sa, sb) in a.iter().zip(b.iter()) {
        if sa.bytes.len() != sb.bytes.len() {
            return Some(format!(
                "{}: length {} vs {}",
                sa.name,
                sa.bytes.len(),
                sb.bytes.len()
            ));
        }
        if let Some(offset) = sa
            .bytes
            .iter()
            .zip(sb.bytes.iter())
            .position(|(x, y)| x != y)
        {
            return Some(format!(
                "{}: offset {:#x}: {:02x} vs {:02x}",
                sa.name, offset, sa.bytes[offset], sb.bytes[offset]
            ));
        }
    }
    None
}

/// Runs `oxyboy diff` and returns the process exit code: 0 on match,
/// 1 on mismatch or infrastructure failure.
pub fn run(args: DiffArgs) -> i32 {
    let image = match Image::from_file(&args.rom.rom) {
        Ok(image) => image,
        Err(e) => {
            eprintln!("oxyboy diff: {}: {e}", args.rom.rom.display());
            return 1;
        }
    };
    let tag = args.tag.clone().unwrap_or_else(|| {
        args.rom
            .rom
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("diff")
            .to_string()
    });
    let mut driver = match build_hybrid(&image, &tag) {
        Ok(driver) => driver,
        Err(e) => {
            eprintln!("oxyboy diff: hybrid build failed: {e}");
            return 1;
        }
    };
    let mut interpreted = boot(&image);
    let mut hybrid = boot(&image);
    println!("hybrid: {} regions translated", driver.translated_count());
    let mut diverged: Option<(u32, String)> = None;
    for frame in 0..args.frames {
        interpreted.step_frame();
        if let Err(e) = driver.run_frame(&mut hybrid) {
            diverged = Some((frame, format!("hybrid native call failed: {e}")));
            break;
        }
        let a = snapshot(&interpreted);
        let b = snapshot(&hybrid);
        if let Some(d) = first_difference(&a, &b) {
            diverged = Some((frame, d));
            break;
        }
    }
    match diverged {
        None => {
            println!("match");
            0
        }
        Some((frame, detail)) => {
            println!("MISMATCH at frame {frame}: {detail}");
            let case = write_case_dir(&args.case_dir, &tag, &image, frame, &detail, args.frames);
            match case {
                Ok(path) => println!("case directory: {}", path.display()),
                Err(e) => eprintln!("oxyboy diff: case directory failed: {e}"),
            }
            1
        }
    }
}

/// One full machine-state checkpoint: the pc after the unit plus the
/// register file and raw CPSR.
type Checkpoint = [u32; 18];

const CKPT_PC: usize = 16;
const CKPT_CPSR: usize = 17;

fn cpu_checkpoint(system: &System) -> Checkpoint {
    let mut ckpt = [0u32; 18];
    for (i, slot) in ckpt.iter_mut().take(16).enumerate() {
        *slot = system.cpu.reg_for_test(i as u8);
    }
    ckpt[CKPT_PC] = system.cpu.pc();
    ckpt[CKPT_CPSR] = system.cpu.cpsr();
    ckpt
}

const REG_NAMES: [&str; 16] = [
    "r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10", "r11", "r12", "sp", "lr",
    "pc",
];

/// Renders one checkpoint's registers as `name=value` pairs.
fn fmt_checkpoint(c: &Checkpoint) -> String {
    let regs = REG_NAMES
        .iter()
        .enumerate()
        .map(|(i, name)| format!("{name}={:08x}", c[i]))
        .collect::<Vec<_>>()
        .join(" ");
    format!("{regs} cpsr={:08x}", c[CKPT_CPSR])
}

/// Localizes the first real state divergence between the interpreter
/// and the hybrid up to `frame`.
///
/// The raw trace diff cannot see inside native chunks, so this walks
/// the hybrid unit by unit and checks each post-unit state against the
/// interpreter's first visit to the same pc: a pc the interpreter
/// never reaches, or the same pc with different registers, is the
/// divergence. Returns the report text.
///
/// # Errors
///
/// Propagates native-call failures from the hybrid driver.
fn meeting_point(image: &Image, tag: &str, frame: u32) -> Result<String, String> {
    let mut driver = build_hybrid(image, tag)?;
    let mut interpreted = boot(image);
    let mut hybrid = boot(image);
    let mut interp_log: Vec<Checkpoint> = Vec::new();
    let mut hybrid_units: Vec<(bool, u32, Checkpoint)> = Vec::new();
    for _ in 0..=frame {
        while !interpreted.frame_ended() {
            interpreted.step_recorded();
            interp_log.push(cpu_checkpoint(&interpreted));
        }
        interpreted.step_frame();
        while !hybrid.frame_ended() {
            let entry = hybrid.cpu.pc();
            let (cycles, record) = driver.step_recorded(&mut hybrid)?;
            hybrid_units.push((record.is_none(), entry, cpu_checkpoint(&hybrid)));
            let _ = cycles;
        }
        hybrid.step_frame();
    }
    let mut report = format!(
        "meeting-point analysis to frame {frame}: {} interpreter steps, {} hybrid units\n",
        interp_log.len(),
        hybrid_units.len()
    );
    let mut cursor = 0usize;
    for (index, &(native, entry, ref unit)) in hybrid_units.iter().enumerate() {
        let mut at = None;
        for (j, ckpt) in interp_log.iter().enumerate().skip(cursor) {
            if ckpt[CKPT_PC] == unit[CKPT_PC] {
                at = Some(j);
                break;
            }
        }
        let kind = if native {
            format!("native chunk entered at {entry:#010x}")
        } else {
            "interpreted step".to_string()
        };
        let Some(j) = at else {
            report.push_str(&format!(
                "first divergence: hybrid unit #{index} ({kind}) resumed at pc {:#010x}, which the interpreter never reaches in the window\n  hybrid: {}\n",
                unit[CKPT_PC],
                fmt_checkpoint(unit)
            ));
            return Ok(report);
        };
        let same = interp_log[j]
            .iter()
            .zip(unit.iter())
            .position(|(a, b)| a != b);
        if let Some(slot) = same {
            let name = REG_NAMES.get(slot).copied().unwrap_or("cpsr");
            report.push_str(&format!(
                "first divergence: after hybrid unit #{index} ({kind}) the states differ at pc {:#010x}\n  interpreter (after {} steps): {}\n  hybrid:                       {}\n  first differing slot: {name} ({:08x} vs {:08x})\n",
                unit[CKPT_PC],
                j + 1,
                fmt_checkpoint(&interp_log[j]),
                fmt_checkpoint(unit),
                interp_log[j][slot],
                unit[slot]
            ));
            return Ok(report);
        }
        cursor = j + 1;
    }
    report.push_str("no state divergence found up to the last hybrid unit\n");
    Ok(report)
}

/// Boots a system for `image` with the shared multiboot/cart dispatch.
pub(crate) fn boot(image: &Image) -> System {
    match image.kind() {
        ImageKind::Multiboot => System::with_multiboot(image.bytes().to_vec()),
        ImageKind::Cart => System::new(image.bytes().to_vec(), CART_BASE),
    }
}

/// Re-runs both paths to `frame` with execution recording, diffs the
/// traces, and writes the case directory. Returns its path.
///
/// # Errors
///
/// Returns a printable message when the directory or any artifact
/// cannot be written.
fn write_case_dir(
    case_dir: &Path,
    tag: &str,
    image: &Image,
    frame: u32,
    detail: &str,
    frames: u32,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(case_dir).map_err(|e| format!("{}: {e}", case_dir.display()))?;
    let case_dir = case_dir.join(tag);
    std::fs::create_dir_all(&case_dir).map_err(|e| format!("{}: {e}", case_dir.display()))?;
    let report = format!(
        "rom: {}\nframes requested: {frames}\ndiverged at frame: {frame}\nfirst difference: {detail}\n",
        tag
    );
    std::fs::write(case_dir.join("report.txt"), report).map_err(|e| format!("report.txt: {e}"))?;
    let rerun = traced_rerun(image, tag, frame)?;
    std::fs::write(case_dir.join("interpreted.oxytrace"), &rerun.interp_trace)
        .map_err(|e| format!("interpreted.oxytrace: {e}"))?;
    std::fs::write(case_dir.join("hybrid.oxytrace"), &rerun.hybrid_trace)
        .map_err(|e| format!("hybrid.oxytrace: {e}"))?;
    let mut trace_report = String::new();
    match diff(
        Reader::new(&rerun.interp_trace[..]).map_err(|e| format!("trace read: {e}"))?,
        Reader::new(&rerun.hybrid_trace[..]).map_err(|e| format!("trace read: {e}"))?,
    ) {
        Ok(trace::Diff::Identical) => {
            trace_report.push_str("trace diff: identical (divergence is state-only)\n");
        }
        Ok(trace::Diff::Diverged {
            index,
            first,
            second,
        }) => {
            trace_report.push_str(&format!(
                "trace diff: first diverging record #{index}\n  interpreter: pc={:#010x} flags={:#04x} regs={:?}\n  hybrid:      pc={:#010x} flags={:#04x} regs={:?}\n",
                first.pc, first.flags,
                first.reg_delta.iter().map(|r| (r.index, r.value)).collect::<Vec<_>>(),
                second.pc, second.flags,
                second.reg_delta.iter().map(|r| (r.index, r.value)).collect::<Vec<_>>(),
            ));
        }
        Ok(trace::Diff::LengthMismatch {
            index,
            second_longer,
        }) => {
            trace_report.push_str(&format!(
                "trace diff: length mismatch at record #{index} (hybrid longer: {second_longer})\n"
            ));
        }
        Err(e) => trace_report.push_str(&format!("trace diff failed: {e}\n")),
    }
    if !rerun.chunk_entries.is_empty() {
        trace_report.push_str("native chunk entries during the traced rerun:\n");
        for entry in &rerun.chunk_entries {
            trace_report.push_str(&format!("  {entry:#010x}\n"));
        }
    }
    std::fs::write(case_dir.join("trace-diff.txt"), &trace_report)
        .map_err(|e| format!("trace-diff.txt: {e}"))?;
    match meeting_point(image, tag, frame) {
        Ok(report) => {
            std::fs::write(case_dir.join("divergence.txt"), &report)
                .map_err(|e| format!("divergence.txt: {e}"))?;
        }
        Err(e) => eprintln!("oxyboy diff: divergence analysis failed: {e}"),
    }
    dump_frame_png(
        &case_dir.join("interpreted.png"),
        &rerun_frame(image, frame, false),
    )?;
    dump_frame_png(
        &case_dir.join("hybrid.png"),
        &rerun_frame(image, frame, true),
    )?;
    Ok(case_dir.to_path_buf())
}

/// The rerun artifacts for one case: both trace files and the native
/// chunk entry PCs the hybrid hit.
struct TracedRerun {
    interp_trace: Vec<u8>,
    hybrid_trace: Vec<u8>,
    chunk_entries: Vec<u32>,
}

/// Runs the interpreter side to `frame` with recording, the hybrid
/// side likewise (native chunks record nothing), and returns both
/// trace bytes plus the native chunk entry PCs the hybrid entered.
fn traced_rerun(image: &Image, tag: &str, frame: u32) -> Result<TracedRerun, String> {
    let mut driver = build_hybrid(image, tag)?;
    let mut interpreted = boot(image);
    let mut hybrid = boot(image);
    let mut interp_trace = Vec::new();
    let mut hybrid_trace = Vec::new();
    let mut chunks = Vec::new();
    let mut writer_a = trace::Writer::new(&mut interp_trace).map_err(|e| format!("{e}"))?;
    let mut writer_b = trace::Writer::new(&mut hybrid_trace).map_err(|e| format!("{e}"))?;
    // Inclusive of the diverging frame: the divergence happens during
    // it, so the traces must cover it.
    for _ in 0..=frame {
        while !interpreted.frame_ended() {
            let (_, record) = interpreted.step_recorded();
            writer_a.write(&record).map_err(|e| format!("{e}"))?;
        }
        interpreted.step_frame();
        while !hybrid.frame_ended() {
            let (_, record) = driver.step_recorded(&mut hybrid)?;
            match record {
                Some(record) => writer_b.write(&record).map_err(|e| format!("{e}"))?,
                None => chunks.push(hybrid.cpu.pc()),
            }
        }
        hybrid.step_frame();
    }
    writer_a.finish().map_err(|e| format!("{e}"))?;
    writer_b.finish().map_err(|e| format!("{e}"))?;
    Ok(TracedRerun {
        interp_trace,
        hybrid_trace,
        chunk_entries: chunks,
    })
}

/// Re-runs only the interpreter or only the hybrid to `frame` and
/// captures the rendered frame (for the case-directory PNGs).
fn rerun_frame(image: &Image, frame: u32, hybrid_path: bool) -> Vec<u8> {
    let Some(mut driver) = hybrid_path
        .then(|| build_hybrid(image, "diff-frame").ok())
        .flatten()
    else {
        let mut system = boot(image);
        for _ in 0..frame {
            system.step_frame();
        }
        system.step_frame();
        return system.framebuffer().to_vec();
    };
    let mut system = boot(image);
    for _ in 0..frame {
        if driver.run_frame(&mut system).is_err() {
            break;
        }
    }
    system.framebuffer().to_vec()
}

/// Encodes one RGBA frame as a PNG file.
///
/// # Errors
///
/// Propagates file and encoder failures.
fn dump_frame_png(path: &Path, frame: &[u8]) -> Result<(), String> {
    let file = File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), WIDTH as u32, VISIBLE_LINES as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    writer
        .write_image_data(frame)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    writer
        .finish()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slice(name: &'static str, bytes: &[u8]) -> Slice {
        Slice {
            name,
            bytes: bytes.to_vec(),
        }
    }

    #[test]
    fn first_difference_equal_slices_is_none() {
        let a = vec![slice("registers", &[1, 2, 3]), slice("vram", &[9])];
        let b = vec![slice("registers", &[1, 2, 3]), slice("vram", &[9])];
        assert_eq!(first_difference(&a, &b), None);
    }

    #[test]
    fn first_difference_reports_name_offset_and_values() {
        let a = vec![slice("registers", &[1, 2, 3]), slice("vram", &[0xAA, 0xBB])];
        let b = vec![slice("registers", &[1, 2, 3]), slice("vram", &[0xAA, 0x00])];
        assert_eq!(
            first_difference(&a, &b),
            Some("vram: offset 0x1: bb vs 00".to_string())
        );
    }

    #[test]
    fn first_difference_catches_length_drift() {
        let a = vec![slice("save", &[1, 2])];
        let b = vec![slice("save", &[1])];
        assert_eq!(
            first_difference(&a, &b),
            Some("save: length 2 vs 1".to_string())
        );
    }

    #[test]
    fn first_difference_names_the_earliest_slice() {
        let a = vec![slice("ewram", &[1]), slice("iwram", &[1])];
        let b = vec![slice("ewram", &[2]), slice("iwram", &[2])];
        assert_eq!(
            first_difference(&a, &b),
            Some("ewram: offset 0x0: 01 vs 02".to_string())
        );
    }

    #[test]
    fn cpu_checkpoint_carries_regs_pc_and_cpsr() {
        let mut system = System::new(vec![0xFF; 0x100], 0x0800_0000);
        system.cpu.set_reg_for_test(3, 0xDEAD_BEEF);
        let ckpt = cpu_checkpoint(&system);
        assert_eq!(ckpt[3], 0xDEAD_BEEF);
        assert_eq!(ckpt[CKPT_PC], system.cpu.pc());
        assert_eq!(ckpt[CKPT_CPSR], system.cpu.cpsr());
    }
}
