//! The `lift` subcommand: lift analyzed functions into IR.
//!
//! The pipeline mirrors the IR test harness: image, flag map, entry-seeded
//! analysis, function regions, then lifting. Each function runs the
//! optimization pipeline; by default only the final IR prints,
//! `--print-after-all` adds a dump after every pass.

use analysis::{FlagMap, Seed, Thresholds, analyze};
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
    let (config, annotations) = match &args.config {
        Some(path) => match analysis::config::Config::load(path) {
            Ok(config) => {
                let annotations = ir::emit_c::Annotations::from_config(&config);
                (Some(config), annotations)
            }
            Err(e) => {
                eprintln!("oxyboy lift: {}: {e}", path.display());
                return 1;
            }
        },
        None => (None, ir::emit_c::Annotations::default()),
    };
    // Config-declared functions are user assertions: seed and mark
    // them so `function_regions` lifts them even when discovery never
    // walks a call to them.
    let config_seeds: Vec<Seed> = config.as_ref().map_or(Vec::new(), |c| {
        c.functions.iter().map(|f| f.to_seed()).collect()
    });
    let seeds = map.seed(&config_seeds);
    let tables = config.as_ref().map_or(Vec::new(), |c| c.tables.clone());
    let analysis = analyze(
        &image,
        &mut map,
        &seeds,
        &seeds,
        Thresholds::default(),
        &tables,
    );
    let mut addrs: Vec<u32> = Vec::new();
    let mut functions: Vec<ir::Function> = Vec::new();
    let mut dumped = String::new();
    for region in ir::lift::function_regions(&image, &map) {
        if let Some(addr) = args.function
            && region.start != addr
        {
            continue;
        }
        let mut function = ir::lift::lift_function(&image, &map, region, &analysis.tables);
        function.drop_checkpoints();
        if args.print_after_all {
            optimize_with_dumps(&mut function, &mut dumped);
        } else {
            opt::optimize(&mut function);
            dumped.push_str(&fmt_function(&function));
            dumped.push('\n');
        }
        if let Some(name) = annotations.names.get(&region.start) {
            function.name = name.clone();
        }
        if args.cfg || args.cfg_dot {
            let cfg = ir::cfg::Cfg::build(&function);
            if args.cfg {
                dumped.push_str(&ir::cfg::dump_text(&cfg));
            }
            if args.cfg_dot {
                dumped.push_str(&ir::cfg::dump_dot(&cfg));
            }
        }
        addrs.push(region.start);
        functions.push(function);
    }
    if addrs.is_empty() {
        eprintln!("oxyboy lift: no matching function");
        return 1;
    }
    if args.c {
        print!(
            "{}",
            ir::emit_c::emit_unit(&addrs, &functions, &annotations)
        );
    } else {
        print!("{dumped}");
    }
    0
}

/// Prints the IR after each pipeline stage, pass name first.
fn optimize_with_dumps(function: &mut ir::Function, out: &mut String) {
    opt::optimize_with_dumps(function, &mut |name, f| {
        out.push_str(&format!("=== {name} ===\n"));
        out.push_str(&fmt_function(f));
        out.push('\n');
    });
}

/// Arguments of `oxyboy lift`.
#[derive(Debug, clap::Args)]
pub struct LiftArgs {
    #[command(flatten)]
    pub rom: RomArgs,

    /// Lift only the function starting at this address (hex or
    /// decimal, e.g. 0x08000100).
    #[arg(long, value_name = "ADDR", value_parser = crate::commands::parse_addr)]
    pub function: Option<u32>,

    /// Print the IR after every pass, not just the final form.
    #[arg(long)]
    pub print_after_all: bool,

    /// Print the control-flow analysis (dominators, frontiers, loops)
    /// after the IR.
    #[arg(long)]
    pub cfg: bool,

    /// Print the control-flow graph in dot form.
    #[arg(long)]
    pub cfg_dot: bool,

    /// Emit the functions as one compilable C translation unit
    /// instead of the IR text.
    #[arg(long)]
    pub c: bool,

    /// Analysis config naming functions and modules; named functions
    /// keep their configured names in the C output.
    #[arg(long, value_name = "PATH")]
    pub config: Option<std::path::PathBuf>,
}
