//! The `trace-diff` subcommand: compare two `.oxytrace` files and
//! report the first diverging instruction.

use std::path::PathBuf;

use clap::Args;
use trace::{Diff, Reader};

/// Arguments of `oxyboy trace-diff`.
#[derive(Debug, Args)]
pub struct TraceDiffArgs {
    /// The reference trace.
    #[arg(value_name = "A")]
    pub a: PathBuf,

    /// The trace to compare against it.
    #[arg(value_name = "B")]
    pub b: PathBuf,
}

/// Runs `oxyboy trace-diff` and returns the process exit code: 0 when
/// the traces are identical, 1 when they differ or cannot be read.
pub fn run(args: TraceDiffArgs) -> i32 {
    let open =
        |path: &std::path::Path| -> Result<Reader<std::io::BufReader<std::fs::File>>, String> {
            let file = std::fs::File::open(path)
                .map_err(|e| format!("oxyboy trace-diff: {}: {e}", path.display()))?;
            Reader::new(std::io::BufReader::new(file))
                .map_err(|e| format!("oxyboy trace-diff: {}: {e}", path.display()))
        };
    let a = match open(&args.a) {
        Ok(reader) => reader,
        Err(msg) => {
            eprintln!("{msg}");
            return 1;
        }
    };
    let b = match open(&args.b) {
        Ok(reader) => reader,
        Err(msg) => {
            eprintln!("{msg}");
            return 1;
        }
    };
    match trace::diff(a, b) {
        Ok(Diff::Identical) => {
            println!("traces are identical");
            0
        }
        Ok(Diff::Diverged {
            index,
            first,
            second,
        }) => {
            println!(
                "first divergence at instruction {index}: pc {:#010X} vs {:#010X}",
                first.pc, second.pc
            );
            if first.flags != second.flags {
                println!("  state differs: {:?} vs {:?}", first.flags, second.flags);
            }
            if first.reg_delta != second.reg_delta {
                println!("  registers: {first:?} vs {second:?}");
            }
            if first.writes != second.writes {
                println!("  writes: {:?} vs {:?}", first.writes, second.writes);
            }
            1
        }
        Ok(Diff::LengthMismatch {
            index,
            second_longer,
        }) => {
            if second_longer {
                println!("traces agree through instruction {index}; the second is longer");
            } else {
                println!("traces agree through instruction {index}; the first is longer");
            }
            1
        }
        Err(e) => {
            eprintln!("oxyboy trace-diff: {e}");
            1
        }
    }
}
