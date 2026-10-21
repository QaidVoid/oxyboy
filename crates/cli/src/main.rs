//! oxyboy command line interface.

use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand};

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
    Info(RomArgs),
    /// Emit matching, re-assemblable assembly from a ROM.
    Disasm(RomArgs),
    /// Recover code and data layout and function boundaries.
    Analyze(RomArgs),
    /// Lift a ROM into the intermediate representation.
    Lift(RomArgs),
    /// Statically recompile a ROM to host code.
    Recomp(RomArgs),
    /// Execute a ROM in the GBA runtime.
    Run(RomArgs),
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
    let name = match cli.command {
        Command::Info(_) => "info",
        Command::Disasm(_) => "disasm",
        Command::Analyze(_) => "analyze",
        Command::Lift(_) => "lift",
        Command::Recomp(_) => "recomp",
        Command::Run(_) => "run",
    };
    eprintln!("oxyboy {name}: not implemented yet");
    std::process::exit(1);
}
