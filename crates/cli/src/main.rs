//! oxyboy command line interface.

use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand};

mod commands;

/// GBA analyzer, decompiler, and runtime.
#[derive(Debug, Parser)]
#[command(name = "oxyboy", version, propagate_version = true)]
struct Cli {
    /// Subcommand to run.
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print header fields, hashes, and identification for a ROM.
    Info {
        #[command(flatten)]
        rom: RomArgs,
        /// Path to a JSON ROM database used for identification.
        #[arg(long, value_name = "FILE", env = "OXYBOY_ROM_DB")]
        db: Option<PathBuf>,
    },
    /// Emit matching, re-assemblable assembly from a ROM.
    Disasm(RomArgs),
    /// Recover code and data layout and function boundaries.
    Analyze(RomArgs),
    /// Lift a ROM into the intermediate representation.
    Lift(commands::lift::LiftArgs),
    /// Statically recompile a ROM to host code.
    Recomp(RomArgs),
    /// Execute a ROM in the GBA runtime.
    Run(commands::run::RunArgs),
    /// Compare two execution traces and report the first divergence.
    TraceDiff(commands::trace_diff::TraceDiffArgs),
}

/// Arguments shared by every subcommand.
#[derive(Debug, Args)]
struct RomArgs {
    /// Path to the .gba ROM image.
    #[arg(short, long, value_name = "FILE")]
    rom: PathBuf,

    /// Increase logging verbosity; repeat for more detail.
    #[arg(short, long, action = ArgAction::Count)]
    verbose: u8,
}

fn main() {
    let cli = Cli::parse();
    let code = match cli.command {
        Command::Info { rom, db } => commands::info::run(rom, db),
        Command::Disasm(_) => not_implemented("disasm"),
        Command::Analyze(_) => not_implemented("analyze"),
        Command::Lift(args) => commands::lift::run(args),
        Command::Recomp(_) => not_implemented("recomp"),
        Command::Run(args) => commands::run::run(args),
        Command::TraceDiff(args) => commands::trace_diff::run(args),
    };
    std::process::exit(code);
}

fn not_implemented(name: &str) -> i32 {
    eprintln!("oxyboy {name}: not implemented yet");
    1
}
