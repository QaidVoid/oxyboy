//! The `analyze` subcommand: code/data layout and function discovery.

use std::path::PathBuf;

use analysis::{FlagMap, Thresholds, analyze};
use rom::Image;
use serde::Serialize;

use crate::RomArgs;

/// Runs `oxyboy analyze` and returns the process exit code.
pub fn run(args: AnalyzeArgs) -> i32 {
    let image = match Image::from_file(&args.rom.rom) {
        Ok(image) => image,
        Err(e) => {
            eprintln!("oxyboy analyze: {}: {e}", args.rom.rom.display());
            return 1;
        }
    };
    let config = match &args.config {
        Some(path) => match analysis::config::Config::load(path) {
            Ok(config) => config,
            Err(e) => {
                eprintln!("oxyboy analyze: {}: {e}", path.display());
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
        Thresholds {
            min_calls: args.min_calls,
            min_length: args.min_length,
            min_exit_ratio: args.min_exit_ratio,
        },
        &config.tables,
    );
    let report = Report::new(&analysis);
    if args.json {
        let text = match serde_json::to_string_pretty(&report) {
            Ok(text) => text,
            Err(e) => {
                eprintln!("oxyboy analyze: {e}");
                return 1;
            }
        };
        println!("{text}");
    } else {
        let text = render_text(&report);
        if let Some(out) = &args.out {
            if let Err(e) = std::fs::write(out, &text) {
                eprintln!("oxyboy analyze: {}: {e}", out.display());
                return 1;
            }
        } else {
            print!("{text}");
        }
    }
    if let Some(out) = &args.config_out {
        let discovered = analysis::config::Config {
            functions: analysis
                .discovery
                .functions
                .iter()
                .map(|f| analysis::config::FunctionSpec::bare(f.addr, f.state))
                .collect(),
            tables: analysis
                .tables
                .iter()
                .map(|t| analysis::config::TableHint {
                    addr: t.site,
                    entries: t.entries,
                    state: t.state,
                })
                .collect(),
        };
        if let Err(e) = discovered.save(out) {
            eprintln!("oxyboy analyze: {}: {e}", out.display());
            return 1;
        }
    }
    0
}

/// The machine-readable analysis summary. Field names are stable;
/// addresses render as `0x`-prefixed lowercase hex strings.
#[derive(Debug, Serialize)]
pub struct Report {
    /// Accepted functions, ascending by address.
    pub functions: Vec<Entry>,
    /// Candidates that failed the gates, ascending by address.
    pub rejected: Vec<Entry>,
    /// Resolved jump tables, in site order.
    pub tables: Vec<Table>,
    /// Software interrupts the traversal passed, in site order.
    pub swi_calls: Vec<SwiEntry>,
    /// Copy-then-jump notes into EWRAM, in site order.
    pub copy_hints: Vec<CopyHintEntry>,
    /// Indirect jumps no table explains, in site order.
    pub unresolved: Vec<Entry>,
}

/// One discovered or rejected function site.
#[derive(Debug, Serialize)]
pub struct Entry {
    /// Canonical address, `0x`-prefixed hex.
    pub addr: String,
    /// Execution state of the body.
    pub state: String,
}

/// One recorded software interrupt.
#[derive(Debug, Serialize)]
pub struct SwiEntry {
    /// Address of the SWI instruction, `0x`-prefixed hex.
    pub site: String,
    /// BIOS call number.
    pub number: u8,
    /// Processor state at the instruction.
    pub state: String,
}

/// One copy-then-jump note.
#[derive(Debug, Serialize)]
pub struct CopyHintEntry {
    /// Address of the jump into the copied region, `0x`-prefixed hex.
    pub site: String,
    /// Source address passed to the copy call, `0x`-prefixed hex.
    pub source: String,
    /// EWRAM destination of the copy, `0x`-prefixed hex.
    pub dest: String,
    /// Copy size in bytes.
    pub bytes: u32,
}

/// One resolved jump table.
#[derive(Debug, Serialize)]
pub struct Table {
    /// Address of the consuming indirect jump, `0x`-prefixed hex.
    pub site: String,
    /// Processor state of the jump.
    pub state: String,
    /// First table entry address, `0x`-prefixed hex.
    pub base: String,
    /// Number of entry words.
    pub entries: u32,
}

fn report_state(state: analysis::State) -> &'static str {
    match state {
        analysis::State::Arm => "arm",
        analysis::State::Thumb => "thumb",
    }
}

impl Report {
    fn new(analysis: &analysis::Analysis) -> Self {
        let entry = |f: &analysis::Function| Entry {
            addr: format!("0x{:08x}", f.addr),
            state: report_state(f.state).to_owned(),
        };
        Report {
            functions: analysis.discovery.functions.iter().map(entry).collect(),
            rejected: analysis.discovery.rejected.iter().map(entry).collect(),
            tables: analysis
                .tables
                .iter()
                .map(|t| Table {
                    site: format!("0x{:08x}", t.site),
                    state: report_state(t.state).to_owned(),
                    base: format!("0x{:08x}", t.base),
                    entries: t.entries,
                })
                .collect(),
            swi_calls: analysis
                .swi_calls
                .iter()
                .map(|c| SwiEntry {
                    site: format!("0x{:08x}", c.site),
                    number: c.number,
                    state: report_state(c.state).to_owned(),
                })
                .collect(),
            copy_hints: analysis
                .copy_hints
                .iter()
                .map(|h| CopyHintEntry {
                    site: format!("0x{:08x}", h.site),
                    source: format!("0x{:08x}", h.source),
                    dest: format!("0x{:08x}", h.dest),
                    bytes: h.bytes,
                })
                .collect(),
            unresolved: analysis
                .unresolved
                .iter()
                .map(|s| Entry {
                    addr: format!("0x{:08x}", s.addr),
                    state: report_state(s.state).to_owned(),
                })
                .collect(),
        }
    }
}

/// The text report: stable, golden-testable lines.
fn render_text(report: &Report) -> String {
    let mut out = String::new();
    let line = |out: &mut String, text: String| {
        out.push_str(&text);
        out.push('\n');
    };
    line(&mut out, format!("functions: {}", report.functions.len()));
    for f in &report.functions {
        line(&mut out, format!("  {} {}", f.addr, f.state));
    }
    line(&mut out, format!("rejected: {}", report.rejected.len()));
    for f in &report.rejected {
        line(&mut out, format!("  {} {}", f.addr, f.state));
    }
    line(&mut out, format!("tables: {}", report.tables.len()));
    for t in &report.tables {
        line(
            &mut out,
            format!(
                "  {} {} base {} entries {}",
                t.site, t.state, t.base, t.entries
            ),
        );
    }
    line(&mut out, format!("swi calls: {}", report.swi_calls.len()));
    for c in &report.swi_calls {
        line(
            &mut out,
            format!("  {} {} swi {:#04x}", c.site, c.state, c.number),
        );
    }
    line(&mut out, format!("copy hints: {}", report.copy_hints.len()));
    for h in &report.copy_hints {
        line(
            &mut out,
            format!(
                "  {} from {} to {} ({} bytes)",
                h.site, h.source, h.dest, h.bytes
            ),
        );
    }
    line(
        &mut out,
        format!("unresolved indirect jumps: {}", report.unresolved.len()),
    );
    for s in &report.unresolved {
        line(&mut out, format!("  {} {}", s.addr, s.state));
    }
    out
}

/// Arguments of `oxyboy analyze`.
#[derive(Debug, clap::Args)]
pub struct AnalyzeArgs {
    #[command(flatten)]
    pub rom: RomArgs,

    /// Analysis config with known functions and table hints.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Write every discovered function and table as a config file,
    /// ready to annotate with names for `lift --config`.
    #[arg(long, value_name = "FILE")]
    pub config_out: Option<PathBuf>,

    /// Print the summary as JSON instead of text.
    #[arg(long)]
    pub json: bool,

    /// BL calls a `push {.., lr}` site needs before it is considered.
    #[arg(long, value_name = "N", default_value_t = 2)]
    pub min_calls: u32,

    /// Consecutive decodable instructions a candidate needs.
    #[arg(long, value_name = "N", default_value_t = 3)]
    pub min_length: u32,

    /// Exit-behavior ratio a candidate must reach (0..1].
    #[arg(long, value_name = "RATIO", default_value_t = 2.0 / 3.0)]
    pub min_exit_ratio: f64,

    /// Write the text report to this file instead of stdout.
    #[arg(long, value_name = "FILE")]
    pub out: Option<PathBuf>,
}
