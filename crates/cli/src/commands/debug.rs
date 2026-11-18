//! The `debug` subcommand: a breakpoint/step loop over the
//! interpreter.
//!
//! Commands arrive from `--script FILE` (one command per line, `#`
//! comments) or stdin, so sessions are reproducible in tests:
//!
//! - `break ADDR`: stop when the PC reaches `ADDR`
//! - `run [FRAMES]`: execute until a breakpoint, a halt, or the
//!   frame budget (default `--frames`)
//! - `step [N]`: execute `N` instructions (default 1)
//! - `regs`: dump r0-r14, the PC, and the CPSR
//! - `peek ADDR [1|2|4]`: read memory through the real bus
//! - `quit`: leave the loop
//!
//! Peeks and steps use the live machine: reads can observe live
//! counters and feeds, exactly like guest execution.

use std::io::BufRead;
use std::path::PathBuf;

use rom::{Image, ImageKind};
use runtime::system::System;

use crate::RomArgs;

const CART_BASE: u32 = 0x0800_0000;

/// Runs `oxyboy debug` and returns the process exit code.
pub fn run(args: DebugArgs) -> i32 {
    let image = match Image::from_file(&args.rom.rom) {
        Ok(image) => image,
        Err(e) => {
            eprintln!("oxyboy debug: {}: {e}", args.rom.rom.display());
            return 1;
        }
    };
    let mut system = match image.kind() {
        ImageKind::Multiboot => System::with_multiboot(image.bytes().to_vec()),
        ImageKind::Cart => System::new(image.bytes().to_vec(), CART_BASE),
    };
    let mut breakpoints: Vec<u32> = Vec::new();
    let stdin = std::io::stdin();
    let lines: Box<dyn BufRead> = match &args.script {
        Some(path) => match std::fs::File::open(path) {
            Ok(file) => Box::new(std::io::BufReader::new(file)),
            Err(e) => {
                eprintln!("oxyboy debug: {}: {e}", path.display());
                return 1;
            }
        },
        None => Box::new(stdin.lock()),
    };
    for line in lines.lines().map_while(Result::ok) {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        println!("> {line}");
        if execute(&mut system, &mut breakpoints, line, args.frames).is_quit() {
            break;
        }
    }
    0
}

/// What one command did with the session.
enum Effect {
    Continue,
    Quit,
}

impl Effect {
    fn is_quit(&self) -> bool {
        matches!(self, Effect::Quit)
    }
}

/// Executes one command line, printing its output.
fn execute(
    system: &mut System,
    breakpoints: &mut Vec<u32>,
    line: &str,
    default_frames: u32,
) -> Effect {
    let mut parts = line.split_whitespace();
    match (parts.next(), parts.next(), parts.next()) {
        (Some("break"), Some(addr), None) => {
            match crate::commands::parse_addr(addr) {
                Ok(addr) => {
                    breakpoints.push(addr);
                    let index = breakpoints.len() - 1;
                    println!("breakpoint {index} at 0x{addr:08x}");
                }
                Err(e) => println!("error: {e}"),
            }
            Effect::Continue
        }
        (Some("run"), frames, None) => {
            let budget = frames
                .map(crate::commands::parse_addr)
                .unwrap_or(Ok(default_frames));
            match budget {
                Ok(budget) => run_until_stop(system, breakpoints, budget),
                Err(e) => println!("error: {e}"),
            }
            Effect::Continue
        }
        (Some("step"), count, None) => {
            let count = count.map(crate::commands::parse_addr).unwrap_or(Ok(1));
            match count {
                Ok(count) => {
                    for _ in 0..count {
                        system.take_frame_ended();
                        system.step();
                    }
                    println!("pc 0x{:08x}", system.cpu.pc());
                }
                Err(e) => println!("error: {e}"),
            }
            Effect::Continue
        }
        (Some("regs"), None, None) => {
            for index in 0..15u8 {
                println!("r{index:<2}=0x{:08x}", system.cpu.reg_for_test(index));
            }
            let cpsr = system.cpu.cpsr();
            println!("pc =0x{:08x}", system.cpu.pc());
            println!(
                "cpsr=0x{:08x} {}",
                cpsr,
                if cpsr & 0x20 != 0 { "thumb" } else { "arm" }
            );
            Effect::Continue
        }
        (Some("peek"), Some(addr), width) => {
            let parsed = crate::commands::parse_addr(addr);
            let width = width.map_or(Ok(4), crate::commands::parse_addr);
            match (parsed, width) {
                (Ok(addr), Ok(width)) => match width {
                    1 => {
                        let value = system.bus_mut().read_u8(addr);
                        println!("0x{addr:08x}: 0x{value:02x}");
                    }
                    2 => {
                        let value = system.bus_mut().read_u16(addr);
                        println!("0x{addr:08x}: 0x{value:04x}");
                    }
                    4 => {
                        let value = system.bus_mut().read_u32(addr);
                        println!("0x{addr:08x}: 0x{value:08x}");
                    }
                    other => {
                        println!("error: width must be 1, 2, or 4 (got {other})");
                    }
                },
                (Ok(_), Err(e)) | (Err(e), Ok(_)) => println!("error: {e}"),
                (Err(e), Err(_)) => println!("error: {e}"),
            }
            Effect::Continue
        }
        (Some("quit"), None, None) | (Some("exit"), None, None) => Effect::Quit,
        (Some(other), ..) => {
            println!("unknown command: {other} (break, run, step, regs, peek, quit)");
            Effect::Continue
        }
        _ => Effect::Continue,
    }
}

/// Steps until a breakpoint is reached, the CPU enters a halt, or
/// the frame budget runs out. It always steps first, so a `run` from a
/// breakpoint or from inside a halt makes progress.
fn run_until_stop(system: &mut System, breakpoints: &[u32], frames: u32) {
    let mut frames_done = 0u32;
    while frames_done < frames {
        let was_halted = system.cpu.is_halted();
        system.step();
        if system.take_frame_ended() {
            frames_done += 1;
        }
        let pc = system.cpu.pc();
        if system.cpu.is_halted() {
            if !was_halted {
                println!("halted at 0x{pc:08x}");
                return;
            }
        } else if breakpoints.contains(&pc) {
            println!("break at 0x{pc:08x}");
            return;
        }
    }
    println!("ran {frames} frames");
}

/// Arguments of `oxyboy debug`.
#[derive(Debug, clap::Args)]
pub struct DebugArgs {
    #[command(flatten)]
    pub rom: RomArgs,

    /// Command file: one command per line, `#` comments. Reads stdin
    /// when omitted.
    #[arg(long, value_name = "FILE")]
    pub script: Option<PathBuf>,

    /// Frame budget of each `run` command without an explicit count.
    #[arg(long, value_name = "N", default_value_t = 60)]
    pub frames: u32,
}
