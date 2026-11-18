//! The `run` subcommand: execute a ROM in the GBA runtime.
//!
//! Images are validated through the `rom` loader first, then handed to
//! the runtime. Headless mode runs a fixed number of frames and can dump
//! the final frame as a PNG; this is the mode tests and scripting use.
//! Without `--headless` a minifb window presents each rendered frame
//! at 60 fps until the window closes (manual verification only; not
//! test-gated).
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
use runtime::recomp::Driver;
use runtime::save::{Save, SaveKind};
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

    /// Execute translated regions natively (compiled from the lifted
    /// C), interpreting everything the IR cannot model. Requires a
    /// host C compiler.
    #[arg(long)]
    pub hybrid: bool,

    /// Analysis config seeding the hybrid translation: config-declared
    /// functions are translated even when discovery never walks a call
    /// to them. Requires `--hybrid`.
    #[arg(long, value_name = "FILE", requires = "hybrid")]
    pub config: Option<PathBuf>,

    /// Window upscale factor (nearest-neighbor, 1 to 8).
    #[arg(long, value_name = "N", default_value_t = 3, value_parser = clap::value_parser!(u32).range(1..=8))]
    pub scale: u32,

    /// Number of frames to run in headless mode.
    #[arg(long, value_name = "N", default_value_t = 1)]
    pub frames: u32,

    /// Write the final rendered frame to this PNG path.
    #[arg(long, value_name = "FILE")]
    pub dump_frame: Option<std::path::PathBuf>,

    /// Directory for save files. Windowed runs default to
    /// $XDG_DATA_HOME/oxyboy/saves; headless runs save only with this
    /// flag.
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
    if args.hybrid && (args.trace.is_some() || args.io_log.is_some()) {
        eprintln!(
            "oxyboy run: --trace and --io-log require the interpreter (native chunks record nothing)"
        );
        return 1;
    }
    apply_save_override(&mut system, &args, &image);
    // A cart without backup media has nothing to persist.
    let mut save_file =
        save_path(&args, &image).filter(|_| system.bus().save().kind() != SaveKind::None);
    if let Some(Err(e)) = save_file.as_deref().map(|path| {
        path.parent()
            .map(std::fs::create_dir_all)
            .unwrap_or_else(|| Ok(()))
    }) {
        eprintln!("oxyboy run: {e}");
        return 1;
    }
    if let Some(path) = save_file.clone()
        && path.exists()
    {
        match Save::from_file(&path) {
            Ok(loaded) => *system.bus_mut().save_mut() = loaded,
            Err(e) => {
                // Never overwrite a save this run could not read.
                eprintln!(
                    "oxyboy run: {}: {e}; saving is disabled for this run",
                    path.display()
                );
                save_file = None;
            }
        }
    }
    let mut driver = if args.hybrid {
        let config = match &args.config {
            Some(path) => match analysis::config::Config::load(path) {
                Ok(config) => Some(config),
                Err(e) => {
                    eprintln!("oxyboy run: {}: {e}", path.display());
                    return 1;
                }
            },
            None => None,
        };
        match super::diff::build_hybrid(&image, "run", config.as_ref()) {
            Ok(driver) => {
                println!("hybrid: {} regions translated", driver.translated_count());
                Some(driver)
            }
            Err(e) => {
                eprintln!("oxyboy run: {e}");
                return 1;
            }
        }
    } else {
        None
    };
    if args.headless {
        if args.hybrid {
            for _ in 0..args.frames {
                if let Err(e) = driver
                    .as_mut()
                    .expect("hybrid driver")
                    .run_frame(&mut system)
                {
                    eprintln!("oxyboy run: hybrid: {e}");
                    return 1;
                }
            }
        } else if let Some(path) = &args.trace {
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
    } else if let Err(code) = run_windowed(&mut system, args.scale, driver.as_mut()) {
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

/// Runs headless with the IO log armed, writing one CSV row per
/// logged access inside `range`; the `pc` column is the instruction
/// that performed (or contained) the access.
///
/// # Errors
///
/// Returns a printable message when the log file cannot be written.
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

/// Runs headless for `frames` frames, recording every instruction to
/// the trace file at `path`. The step budget is the same cycle count
/// plain headless mode charges (`frames * CYCLES_PER_FRAME`), spent
/// through `System::step_recorded` one instruction at a time.
///
/// # Errors
///
/// Returns a printable message when the trace file cannot be written.
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

/// Presents frames in a window at 60 fps until it closes.
///
/// The frame is scaled by whole-pixel replication and the host keyboard
/// drives the keypad.
///
/// # Errors
///
/// Returns a printable message when the window cannot open or a
/// buffer presentation fails.
fn run_windowed(
    system: &mut System,
    scale: u32,
    mut driver: Option<&mut Driver>,
) -> Result<(), String> {
    use minifb::{Window, WindowOptions};

    let scale = scale as usize;
    let width = WIDTH * scale;
    let height = VISIBLE_LINES * scale;
    println!(
        "controls: arrows d-pad, X A (confirm), Z B (back), Enter start, Backspace select, A L, S R; close the window to quit"
    );
    let mut window = Window::new("oxyboy", width, height, WindowOptions::default())
        .map_err(|e| format!("cannot open window: {e}"))?;
    window.set_target_fps(60);
    let mut audio = match super::audio::start() {
        Ok(output) => Some(output),
        Err(e) => {
            eprintln!("oxyboy run: audio disabled ({e})");
            None
        }
    };
    while window.is_open() {
        apply_keypad(system, &window);
        if let Some(d) = driver.as_deref_mut() {
            d.run_frame(system).map_err(|e| format!("hybrid: {e}"))?;
        } else {
            system.step_frame();
        }
        let samples = system.take_audio();
        if let Some(output) = &mut audio {
            output.push(&samples);
        }
        let buffer = scale_frame(system.framebuffer(), scale);
        window
            .update_with_buffer(&buffer, width, height)
            .map_err(|e| format!("present failed: {e}"))?;
    }
    Ok(())
}

/// Expands a framebuffer into a `scale`-times larger 0xRGB pixel buffer
/// by nearest-neighbor replication.
fn scale_frame(frame: &[u8], scale: usize) -> Vec<u32> {
    let row = WIDTH * scale;
    let mut buffer = vec![0u32; row * VISIBLE_LINES * scale];
    let pixels = frame.as_chunks::<4>().0;
    for y in 0..VISIBLE_LINES {
        let base = y * scale * row;
        let src_row = &pixels[y * WIDTH..(y + 1) * WIDTH];
        for (x, px) in src_row.iter().enumerate() {
            let word = u32::from_be_bytes([0xFF, px[0], px[1], px[2]]);
            let start = base + x * scale;
            buffer[start..start + scale].fill(word);
        }
        for dup in 1..scale {
            buffer.copy_within(base..base + row, base + dup * row);
        }
    }
    buffer
}

/// Translates held host keys into the keypad image on the bus.
fn apply_keypad(system: &mut System, window: &minifb::Window) {
    use minifb::Key;
    use runtime::input::Key as Button;

    const MAP: [(Key, Button); 10] = [
        (Key::X, Button::A),
        (Key::Z, Button::B),
        (Key::Enter, Button::Start),
        (Key::Backspace, Button::Select),
        (Key::Up, Button::Up),
        (Key::Down, Button::Down),
        (Key::Left, Button::Left),
        (Key::Right, Button::Right),
        (Key::A, Button::L),
        (Key::S, Button::R),
    ];
    for (host, button) in MAP {
        if window.is_key_down(host) {
            system.bus_mut().press(button);
        } else {
            system.bus_mut().release(button);
        }
    }
}

/// Resolves the save file path: `<save-dir>/<game code>.sav`.
///
/// With `--save-dir` the directory is used as given. Windowed runs
/// otherwise default to `$XDG_DATA_HOME/oxyboy/saves` or
/// `$HOME/.local/share/oxyboy/saves`; headless runs (tests, traces,
/// automation) persist only with `--save-dir`, so they never read or
/// overwrite a player's save. Returns `None` when there is nowhere to
/// persist or the game code is not a safe file name.
fn save_path(args: &RunArgs, image: &Image) -> Option<PathBuf> {
    let dir = match &args.save_dir {
        Some(dir) => dir.clone(),
        None if args.headless => return None,
        None => default_save_dir()?,
    };
    let code = image
        .header()
        .game_code_str()
        .filter(|code| !code.is_empty() && code.bytes().all(|b| b.is_ascii_alphanumeric()))?;
    Some(dir.join(format!("{code}.sav")))
}

/// The user data directory for saves, or `None` when neither an
/// absolute `XDG_DATA_HOME` nor `HOME` is set.
fn default_save_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
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
    match SaveKind::from_name(name) {
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
#[cfg(test)]
mod tests {
    use super::{VISIBLE_LINES, WIDTH, scale_frame};

    /// A frame whose pixel (x, y) encodes as 0x00RRGGBB from its coordinates.
    fn gradient_frame() -> Vec<u8> {
        let mut frame = vec![0u8; WIDTH * VISIBLE_LINES * 4];
        for (i, px) in frame.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (x, y) = (i % WIDTH, i / WIDTH);
            *px = [0x00, (x % 256) as u8, (y % 256) as u8, 0x00];
        }
        frame
    }

    #[test]
    fn scale_one_is_the_identity() {
        let frame = gradient_frame();
        let out = scale_frame(&frame, 1);
        assert_eq!(out.len(), WIDTH * VISIBLE_LINES);
        for (i, word) in out.iter().enumerate() {
            let (x, y) = (i % WIDTH, i / WIDTH);
            assert_eq!(
                *word,
                0xFF00_0000 | u32::from((x % 256) as u8) << 8 | u32::from((y % 256) as u8)
            );
        }
    }

    #[test]
    fn scale_three_replicates_blocks_and_rows() {
        let frame = gradient_frame();
        let out = scale_frame(&frame, 3);
        let row = WIDTH * 3;
        assert_eq!(out.len(), row * VISIBLE_LINES * 3);
        let at = |x: usize, y: usize| out[y * row + x];
        for (x, y) in [(0usize, 0usize), (6, 3), (239 * 3, 159 * 3)] {
            let expected =
                0xFF00_0000 | u32::from((x / 3 % 256) as u8) << 8 | u32::from((y / 3) as u8);
            assert_eq!(at(x, y), expected, "block ({x},{y})");
            assert_eq!(at(x + 2, y + 2), expected, "block tail ({x},{y})");
        }
        for dup in 1..3 {
            assert_eq!(out[..row], out[dup * row..(dup + 1) * row], "row {dup}");
        }
    }
}
