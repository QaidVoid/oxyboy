//! The `lift` subcommand: lift analyzed functions into IR.
//!
//! The pipeline mirrors the IR test harness: image, flag map, entry-seeded
//! analysis, function regions, then lifting. Each function runs the
//! optimization pipeline; by default only the final IR prints,
//! `--print-after-all` adds a dump after every pass.

use analysis::{FlagMap, Thresholds, analyze};
use ir::opt;
use ir::print::fmt_function;

use crate::RomArgs;

/// Runs `oxyboy lift` and returns the process exit code.
pub fn run(args: LiftArgs) -> i32 {
    let image = match rom::Image::from_file(&args.rom.rom) {
        Ok(image) => image,
        Err(e) => {
            eprintln!("oxyboy lift: {}: {e}", args.rom.rom.display());
            return 1;
        }
    };
    let mut map = FlagMap::from_image(&image);
    let seeds = map.seed(&[]);
    let _analysis = analyze(&image, &mut map, &seeds, &seeds, Thresholds::default(), &[]);
    let mut printed = 0;
    for region in ir::lift::function_regions(&image, &map) {
        if let Some(addr) = args.function
            && region.start != addr
        {
            continue;
        }
        let mut function = ir::lift::lift_function(&image, &map, region);
        if args.print_after_all {
            optimize_with_dumps(&mut function);
        } else {
            opt::optimize(&mut function);
            println!("{}", fmt_function(&function));
        }
        printed += 1;
    }
    if printed == 0 {
        eprintln!("oxyboy lift: no matching function");
        return 1;
    }
    0
}

/// Prints the IR after each pipeline stage, pass name first.
fn optimize_with_dumps(function: &mut ir::Function) {
    opt::optimize_with_dumps(function, &mut |name, f| {
        println!("=== {name} ===");
        println!("{}", fmt_function(f));
    });
}

/// Arguments of `oxyboy lift`.
#[derive(Debug, clap::Args)]
pub struct LiftArgs {
    #[command(flatten)]
    pub rom: RomArgs,

    /// Lift only the function starting at this address (hex or
    /// decimal, e.g. 0x08000100).
    #[arg(long, value_name = "ADDR", value_parser = parse_addr)]
    pub function: Option<u32>,

    /// Print the IR after every pass, not just the final form.
    #[arg(long)]
    pub print_after_all: bool,
}

/// Parses an address literal in hex (`0x` prefix) or decimal.
fn parse_addr(s: &str) -> Result<u32, String> {
    let s = s.trim();
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u32::from_str_radix(hex, 16).map_err(|e| format!("invalid address: {e}")),
        None => s
            .parse::<u32>()
            .map_err(|e| format!("invalid address: {e}")),
    }
}
