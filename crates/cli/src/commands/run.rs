//! The `run` subcommand: execute a ROM in the GBA runtime.
//!
//! Images are validated through the `rom` loader first, then handed to
//! the runtime. Headless mode runs a fixed number of frames and can dump
//! the final frame as a PNG; this is the mode tests and scripting use.
//! Without `--headless` a minifb window presents each rendered frame
//! until the window closes or Escape is pressed (manual verification
//! only; not test-gated).
//!
//! The machine boots directly at 0x08000000 with no BIOS reset, the
//! documented boot decision. Standard crt0 places
//! its entry branch there, so header-carrying ROMs run unmodified.
//! Multiboot-classified images boot the way the BIOS would: the bytes
//! land in EWRAM at 0x02000000 and the header entry branch runs from
//! there.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use rom::{Image, ImageKind};
use runtime::ppu::{CYCLES_PER_LINE, LINES_PER_FRAME, VISIBLE_LINES, WIDTH};
use runtime::save::Save;
use runtime::system::System;
use trace::Writer;

use crate::RomArgs;

/// Cart execution base (no BIOS reset; see module docs).
const CART_BASE: u32 = 0x0800_0000;

/// Arguments of `oxyboy run`.
#[derive(Debug, clap::Args)]
pub struct RunArgs {
    #[command(flatten)]
    pub rom: RomArgs,

    /// Run without a window for exactly `--frames` frames.
    #[arg(long)]
    pub headless: bool,

    /// Number of frames to run in headless mode.
    #[arg(long, value_name = "N", default_value_t = 1)]
    pub frames: u32,

    /// Write the final rendered frame to this PNG path.
    #[arg(long, value_name = "FILE")]
    pub dump_frame: Option<std::path::PathBuf>,

    /// Directory for save files. The default is the user data dir
    /// (never inside the repository).
    #[arg(long, value_name = "DIR")]
    pub save_dir: Option<PathBuf>,

    /// Path to a JSON ROM database; an entry's `save_type` overrides
    /// the backup medium auto-detection.
    #[arg(long, value_name = "FILE", env = "OXYBOY_ROM_DB")]
    pub db: Option<PathBuf>,

    /// Write an execution trace (`.oxytrace`) of the headless run to
    /// this file. Traces are only meaningful in `--headless` mode:
    /// windowed runs depend on wall-clock input.
    #[arg(long, value_name = "FILE")]
    pub trace: Option<PathBuf>,

    /// Log every IO window access of the headless run to this CSV
    /// file (`pc,addr,rw,width,value`). Requires `--headless`.
    #[arg(long, value_name = "FILE")]
    pub io_log: Option<PathBuf>,

    /// First IO address to log, inclusive (default: window start).
    #[arg(long, value_name = "ADDR", requires = "io_log", value_parser = crate::commands::parse_addr)]
    pub io_from: Option<u32>,

    /// Last IO address to log, inclusive (default: window end).
    #[arg(long, value_name = "ADDR", requires = "io_log", value_parser = crate::commands::parse_addr)]
    pub io_to: Option<u32>,
}

/// Runs `oxyboy run` and returns the process exit code.
pub fn run(args: RunArgs) -> i32 {
    let image = match Image::from_file(&args.rom.rom) {
        Ok(image) => image,
        Err(e) => {
            eprintln!("oxyboy run: {}: {e}", args.rom.rom.display());
            return 1;
        }
    };
    let mut system = match image.kind() {
        ImageKind::Multiboot => System::with_multiboot(image.bytes().to_vec()),
        ImageKind::Cart => System::new(image.bytes().to_vec(), CART_BASE),
    };
    let save_file = save_path(&args, &image);
    if let Some(Err(e)) = save_file.as_deref().map(|path| {
        path.parent()
            .map(std::fs::create_dir_all)
            .unwrap_or_else(|| Ok(()))
    }) {
        eprintln!("oxyboy run: {e}");
        return 1;
    }
    if let Some(path) = &save_file {
        apply_save_override(&mut system, &args, &image);
        if path.exists() {
            match Save::from_file(path) {
                Ok(loaded) => *system.bus_mut().save_mut() = loaded,
                Err(e) => eprintln!("oxyboy run: {}: {e}", path.display()),
            }
        }
    }
    if args.headless {
        if let Some(path) = &args.trace {
            if let Err(code) = run_traced(&mut system, path, args.frames) {
                eprintln!("oxyboy run: {code}");
                return 1;
            }
        } else if let Some(path) = &args.io_log {
            let range = args.io_from.unwrap_or(0x0400_0000)..=args.io_to.unwrap_or(0x0400_03FF);
            if let Err(e) = run_io_logged(&mut system, path, range, args.frames) {
                eprintln!("oxyboy run: {e}");
                return 1;
            }
        } else {
            for _ in 0..args.frames {
                system.step_frame();
            }
        }
    } else if args.trace.is_some() || args.io_log.is_some() {
        eprintln!("oxyboy run: --trace and --io-log require --headless");
        return 1;
    } else if let Err(code) = run_windowed(&mut system) {
        eprintln!("oxyboy run: {code}");
        return 1;
    }
    if let Some(Err(e)) = save_file
        .as_deref()
        .map(|path| system.bus().save().to_file(path))
    {
        eprintln!("oxyboy run: {e}");
        return 1;
    }
    if let Some(Err(e)) = args
        .dump_frame
        .as_deref()
        .map(|path| dump_png(path, system.framebuffer()))
    {
        eprintln!("oxyboy run: {e}");
        return 1;
    }
    0
}

/// Runs headless for `frames` frames, recording every instruction to
/// the trace file at `path`. The step budget is the same cycle count
/// plain headless mode charges (`frames * CYCLES_PER_FRAME`), spent
/// through `System::step_recorded` one instruction at a time.
///
/// # Errors
///
/// Returns a printable message when the trace file cannot be written.
/// Runs headless with the IO log armed, writing one CSV row per
/// logged access inside `range`; the `pc` column is the instruction
/// that performed (or contained) the access.
fn run_io_logged(
    system: &mut System,
    path: &Path,
    range: std::ops::RangeInclusive<u32>,
    frames: u32,
) -> Result<(), String> {
    let mut rows = String::from("pc,addr,rw,width,value\n");
    system.bus_mut().set_io_log(true);
    for _ in 0..frames {
        while !system.take_frame_ended() {
            let pc = system.cpu.pc();
            system.step();
            for access in system.bus_mut().take_io_log() {
                if range.contains(&access.addr) {
                    rows.push_str(&format!(
                        "0x{pc:08x},0x{:08x},{},{},0x{:x}\n",
                        access.addr,
                        if access.read { "r" } else { "w" },
                        access.width,
                        access.value,
                    ));
                }
            }
        }
    }
    std::fs::write(path, rows).map_err(|e| format!("{}: {e}", path.display()))
}

fn run_traced(system: &mut System, path: &Path, frames: u32) -> Result<(), String> {
    let file = File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut writer =
        Writer::new(BufWriter::new(file)).map_err(|e| format!("{}: {e}", path.display()))?;
    let budget = u64::from(frames)
        * u64::from(CYCLES_PER_LINE)
        * u64::try_from(LINES_PER_FRAME).unwrap_or(0);
    let mut spent = 0u64;
    while spent < budget {
        let (cycles, record) = system.step_recorded();
        writer
            .write(&record)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        spent += u64::from(cycles);
    }
    writer
        .finish()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(())
}

/// Presents frames in a window until it closes or Escape is pressed.
///
/// # Errors
///
/// Returns a printable message when the window cannot open or a
/// buffer presentation fails.
fn run_windowed(system: &mut System) -> Result<(), String> {
    use minifb::{Key, Window, WindowOptions};

    let mut window = Window::new("oxyboy", WIDTH, VISIBLE_LINES, WindowOptions::default())
        .map_err(|e| format!("cannot open window: {e}"))?;
    let mut buffer = vec![0u32; WIDTH * VISIBLE_LINES];
    while window.is_open() && !window.is_key_down(Key::Escape) {
        system.step_frame();
        for (dst, px) in buffer
            .iter_mut()
            .zip(system.framebuffer().as_chunks::<4>().0)
        {
            *dst = u32::from_be_bytes([0xFF, px[0], px[1], px[2]]);
        }
        window
            .update_with_buffer(&buffer, WIDTH, VISIBLE_LINES)
            .map_err(|e| format!("present failed: {e}"))?;
    }
    Ok(())
}

/// Resolves the save file path: `<save-dir>/<game code>.sav`.
///
/// With `--save-dir` the directory is used as given; otherwise the
/// default is `$XDG_DATA_HOME/oxyboy/saves` or
/// `$HOME/.local/share/oxyboy/saves`. Returns `None` when persistence
/// is impossible (no directory resolvable or no game code in the
/// header).
fn save_path(args: &RunArgs, image: &Image) -> Option<PathBuf> {
    let dir = args.save_dir.clone().map_or_else(default_save_dir, Some)?;
    let code = image.header().game_code_str()?;
    Some(dir.join(format!("{code}.sav")))
}

/// The user data directory for saves, or `None` when neither
/// `XDG_DATA_HOME` nor `HOME` is set.
fn default_save_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share"))
        })?;
    Some(base.join("oxyboy").join("saves"))
}

/// Replaces the auto-detected medium when the ROM database entry for
/// this image carries a `save_type` override.
fn apply_save_override(system: &mut System, args: &RunArgs, image: &Image) {
    let Some(path) = &args.db else {
        return;
    };
    let db = match rom::Database::from_file(path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("oxyboy run: {}: {e}", path.display());
            return;
        }
    };
    let hashes = rom::Hashes::compute(image.bytes());
    let Some(entry) = db.find(&hashes) else {
        return;
    };
    let Some(name) = entry.save_type.as_deref() else {
        return;
    };
    match runtime::save::SaveKind::from_name(name) {
        Some(kind) => *system.bus_mut().save_mut() = Save::new(kind),
        None => eprintln!("oxyboy run: unknown save_type {name:?} in {path:?}"),
    }
}

/// Encodes the framebuffer as an RGBA8888 PNG.
///
/// # Errors
///
/// Propagates file and encoder failures.
fn dump_png(path: &Path, frame: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let file = File::create(path)?;
    let writer = BufWriter::new(file);
    let mut encoder = png::Encoder::new(writer, WIDTH as u32, VISIBLE_LINES as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut png_writer = encoder.write_header()?;
    png_writer.write_image_data(frame)?;
    png_writer.finish()?;
    Ok(())
}
