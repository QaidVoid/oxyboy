//! Subcommand implementations.

pub mod analyze;
pub mod corpus;
pub mod debug;
pub mod diff;
pub mod disasm;
pub mod info;
pub mod lift;
pub mod recomp;
pub mod run;
pub mod suite;
pub mod trace_diff;

/// Parses an address literal in hex (`0x` prefix) or decimal.
pub fn parse_addr(s: &str) -> Result<u32, String> {
    let s = s.trim();
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u32::from_str_radix(hex, 16).map_err(|e| format!("invalid address: {e}")),
        None => s
            .parse::<u32>()
            .map_err(|e| format!("invalid address: {e}")),
    }
}
