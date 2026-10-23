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

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use rom::Image;
use runtime::ppu::{VISIBLE_LINES, WIDTH};
use runtime::save::Save;
use runtime::system::System;

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
    let mut system = System::new(image.bytes().to_vec(), CART_BASE);
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
        for _ in 0..args.frames {
            system.step_frame();
        }
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
