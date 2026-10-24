//! The `disasm` subcommand: matching, re-assemblable assembly.

use std::path::PathBuf;

use analysis::{FlagMap, Thresholds, analyze};
use rom::Image;

use crate::RomArgs;

/// Runs `oxyboy disasm` and returns the process exit code.
pub fn run(args: DisasmArgs) -> i32 {
    let image = match Image::from_file(&args.rom.rom) {
        Ok(image) => image,
        Err(e) => {
            eprintln!("oxyboy disasm: {}: {e}", args.rom.rom.display());
            return 1;
        }
    };
    let config = match &args.config {
        Some(path) => match analysis::config::Config::load(path) {
            Ok(config) => config,
            Err(e) => {
                eprintln!("oxyboy disasm: {}: {e}", path.display());
                return 1;
            }
        },
        None => analysis::config::Config::default(),
    };
    let mut map = FlagMap::from_image(&image);
    let mut seeds = map.seed(&[]);
    for spec in &config.functions {
        seeds.push(spec.to_seed());
    }
    let _analysis = analyze(
        &image,
        &mut map,
        &seeds,
        &seeds,
        Thresholds::default(),
        &config.tables,
    );
    let text = emit::emit(&image, &map, &config);
    if let Some(out) = &args.out {
        if let Err(e) = std::fs::write(out, &text) {
            eprintln!("oxyboy disasm: {}: {e}", out.display());
            return 1;
        }
    } else {
        print!("{text}");
    }
    0
}

/// Arguments of `oxyboy disasm`.
#[derive(Debug, clap::Args)]
pub struct DisasmArgs {
    #[command(flatten)]
    pub rom: RomArgs,

    /// Analysis config naming functions and data for symbols.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Write the assembly to this file instead of stdout.
    #[arg(long, value_name = "FILE")]
    pub out: Option<PathBuf>,
}
