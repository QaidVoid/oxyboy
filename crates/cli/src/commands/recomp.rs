//! The `recomp` subcommand: static recompilation to host C.

use std::path::PathBuf;

use analysis::{FlagMap, Thresholds, analyze};
use ir::opt;
use rom::Image;
use serde::Serialize;

use crate::RomArgs;

/// Runs `oxyboy recomp` and returns the process exit code.
pub fn run(args: RecompArgs) -> i32 {
    let image = match Image::from_file(&args.rom.rom) {
        Ok(image) => image,
        Err(e) => {
            eprintln!("oxyboy recomp: {}: {e}", args.rom.rom.display());
            return 1;
        }
    };
    let config = match &args.config {
        Some(path) => match analysis::config::Config::load(path) {
            Ok(config) => config,
            Err(e) => {
                eprintln!("oxyboy recomp: {}: {e}", path.display());
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
    let analysis = analyze(
        &image,
        &mut map,
        &seeds,
        &seeds,
        Thresholds::default(),
        &config.tables,
    );
    let mut functions: Vec<(u32, ir::Function)> = Vec::new();
    for region in ir::lift::function_regions(&image, &map) {
        let mut function = ir::lift::lift_function(&image, &map, region, &analysis.tables);
        opt::optimize(&mut function);
        if let Some(name) = config
            .functions
            .iter()
            .find(|spec| spec.addr == region.start)
            .and_then(|spec| spec.name.as_ref())
        {
            function.name = analysis::config::sanitize_identifier(name);
        }
        functions.push((region.start, function));
    }
    if functions.is_empty() {
        eprintln!("oxyboy recomp: no functions discovered");
        return 1;
    }
    let refs: Vec<(u32, &ir::Function)> = functions.iter().map(|(addr, f)| (*addr, f)).collect();
    let module = recomp::emit_module(&refs);
    let source = if args.json {
        match serde_json::to_string_pretty(&ModuleReport {
            entries: module
                .entries
                .iter()
                .map(|addr| format!("0x{addr:08x}"))
                .collect(),
            symbols: module
                .entries
                .iter()
                .map(|addr| recomp::symbol(*addr))
                .collect(),
            c_bytes: module.c_source.len(),
        }) {
            Ok(text) => format!("{text}\n"),
            Err(e) => {
                eprintln!("oxyboy recomp: {e}");
                return 1;
            }
        }
    } else {
        module.c_source
    };
    if let Some(out) = &args.out {
        if let Err(e) = std::fs::write(out, &source) {
            eprintln!("oxyboy recomp: {}: {e}", out.display());
            return 1;
        }
    } else {
        print!("{source}");
    }
    0
}

/// The machine-readable `recomp --json` summary.
#[derive(Debug, Serialize)]
pub struct ModuleReport {
    /// Translated entry addresses, ascending, `0x`-prefixed hex.
    pub entries: Vec<String>,
    /// Host C symbols for the entries, same order.
    pub symbols: Vec<String>,
    /// Size of the emitted C source in bytes.
    pub c_bytes: usize,
}

/// Arguments of `oxyboy recomp`.
#[derive(Debug, clap::Args)]
pub struct RecompArgs {
    #[command(flatten)]
    pub rom: RomArgs,

    /// Analysis config with known functions and table hints.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Write the C source to this file instead of stdout.
    #[arg(long, value_name = "FILE")]
    pub out: Option<PathBuf>,

    /// Print a JSON summary (entries and symbols) instead of the C
    /// source.
    #[arg(long)]
    pub json: bool,
}
