//! Function configuration files: text declarations of known functions,
//! loadable as analysis input and writable as `--config-out` output.
//!
//! Grammar, one function per line; blank lines and `#` comments are
//! ignored:
//!
//! ```text
//! arm_func <addr> [name]
//! thumb_func <addr> [name]
//! <module>.s [arm|thumb] <addr> [name]
//! arm_table <addr> <entries>
//! thumb_table <addr> <entries>
//! ```
//!
//! `<addr>` is canonical `0x`-prefixed hexadecimal. `arm_func` and
//! `thumb_func` name the execution state directly. A `<module>.s` line
//! additionally places the function into an output module (consumed by
//! module splitting) and takes an optional state token that
//! defaults to ARM, so the plain `<module>.s <addr> [name]` spelling
//! parses too. `arm_table` / `thumb_table` declare jump-table hints:
//! `entries` words at `<addr>` are one table whose targets are entered
//! in the declared state, forcing the transfer bit accordingly.
//! Writing always produces the canonical spelling, including the
//! explicit state token on module lines, so `parse(to_string())`
//! round trips exactly.

use std::fmt;
use std::path::Path;

use crate::flags::{Seed, State};

/// Configuration parse or I/O failures, carrying the offending line.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The line is not a valid directive.
    #[error(
        "line {line}: expected `arm_func|thumb_func <addr> [name]`, `<module>.s [arm|thumb] <addr> [name]`, a comment, or a blank line"
    )]
    Syntax {
        /// 1-based line number.
        line: usize,
    },
    /// The address field is not `0x`-prefixed hexadecimal in `u32`
    /// range.
    #[error("line {line}: `{text}` is not a 0x-prefixed hex address")]
    Address {
        /// 1-based line number.
        line: usize,
        /// The offending token.
        text: String,
    },
    /// A name or module stem is not a valid assembler identifier.
    #[error("line {line}: `{text}` is not a valid assembler identifier")]
    Name {
        /// 1-based line number.
        line: usize,
        /// The offending token.
        text: String,
    },
    /// Reading or writing the file failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// One declared function: where it lives, in which state, what it is
/// called, and which output module it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionSpec {
    /// Canonical address of the first instruction.
    pub addr: u32,
    /// Execution state of the function body.
    pub state: State,
    /// Optional symbol name.
    pub name: Option<String>,
    /// Optional output module stem (`<module>.s`).
    pub module: Option<String>,
}

/// One declared jump table: `entries` words at `addr`, targets entered
/// in `state`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TableHint {
    /// Canonical address of the first table entry.
    pub addr: u32,
    /// Number of entry words.
    pub entries: u32,
    /// State the targets are entered in; the transfer bit is forced
    /// accordingly (ARM clears it, THUMB sets it).
    pub state: State,
}

impl FunctionSpec {
    /// A function with no name and no module.
    #[must_use]
    pub fn bare(addr: u32, state: State) -> Self {
        Self {
            addr,
            state,
            name: None,
            module: None,
        }
    }

    /// The traversal seed for this function.
    #[must_use]
    pub fn to_seed(&self) -> Seed {
        Seed {
            addr: self.addr,
            state: self.state,
        }
    }
}

/// A parsed config file: declared functions and table hints in file
/// order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    /// Every declared function, in declaration order.
    pub functions: Vec<FunctionSpec>,
    /// Every declared jump-table hint, in declaration order.
    pub tables: Vec<TableHint>,
}

impl Config {
    /// Parses config text.
    ///
    /// # Errors
    ///
    /// Fails with a [`ConfigError`] naming the first offending line.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let mut functions = Vec::new();
        let mut tables = Vec::new();
        for (index, raw) in text.lines().enumerate() {
            let line = index + 1;
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            match parse_line(trimmed, line)? {
                Line::Function(spec) => functions.push(spec),
                Line::Hint(hint) => tables.push(hint),
            }
        }
        Ok(Self { functions, tables })
    }

    /// Loads and parses a config file.
    ///
    /// # Errors
    ///
    /// Propagates I/O and parse failures.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)?;
        Self::parse(&text)
    }

    /// Writes the canonical config text to a file.
    ///
    /// # Errors
    ///
    /// Propagates I/O failures.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        std::fs::write(path, self.to_string())?;
        Ok(())
    }
}

/// One parsed line: a function declaration or a table hint.
enum Line {
    Function(FunctionSpec),
    Hint(TableHint),
}

/// Parses one non-comment line.
fn parse_line(trimmed: &str, line: usize) -> Result<Line, ConfigError> {
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    if matches!(tokens[0], "arm_table" | "thumb_table") {
        return parse_table_hint(&tokens, line).map(Line::Hint);
    }
    parse_function(&tokens, line).map(Line::Function)
}

/// Parses an `arm_table`/`thumb_table` line.
fn parse_table_hint(tokens: &[&str], line: usize) -> Result<TableHint, ConfigError> {
    let state = if tokens[0] == "arm_table" {
        State::Arm
    } else {
        State::Thumb
    };
    if tokens.len() != 3 {
        return Err(ConfigError::Syntax { line });
    }
    let addr = parse_addr(tokens[1]).ok_or(ConfigError::Address {
        line,
        text: tokens[1].to_string(),
    })?;
    let entries = tokens[2]
        .parse::<u32>()
        .map_err(|_| ConfigError::Syntax { line })?;
    Ok(TableHint {
        addr,
        entries,
        state,
    })
}

/// Parses one function declaration line.
fn parse_function(tokens: &[&str], line: usize) -> Result<FunctionSpec, ConfigError> {
    let (mut cursor, state, module) = match tokens[0] {
        "arm_func" => (1usize, State::Arm, None),
        "thumb_func" => (1, State::Thumb, None),
        head if head.ends_with(".s") => {
            let stem = &head[..head.len() - 2];
            if !is_identifier(stem) {
                return Err(ConfigError::Name {
                    line,
                    text: stem.to_string(),
                });
            }
            let mut cursor = 1;
            let mut state = State::Arm;
            if matches!(tokens.get(cursor), Some(&"arm") | Some(&"thumb")) {
                state = if tokens[cursor] == "arm" {
                    State::Arm
                } else {
                    State::Thumb
                };
                cursor += 1;
            }
            (cursor, state, Some(stem.to_string()))
        }
        _ => return Err(ConfigError::Syntax { line }),
    };
    let Some(addr_token) = tokens.get(cursor) else {
        return Err(ConfigError::Syntax { line });
    };
    let addr = parse_addr(addr_token).ok_or(ConfigError::Address {
        line,
        text: (*addr_token).to_string(),
    })?;
    cursor += 1;
    // Any whitespace-free token is accepted as a name here; characters
    // illegal in assembler identifiers are sanitized when the name is
    // applied to emitted labels, with a warning.
    let name = tokens.get(cursor).map(|name| (*name).to_string());
    if tokens.len() > cursor + 1 {
        return Err(ConfigError::Syntax { line });
    }
    Ok(FunctionSpec {
        addr,
        state,
        name,
        module,
    })
}

/// Parses a `0x`-prefixed hexadecimal `u32` address.
fn parse_addr(token: &str) -> Option<u32> {
    let digits = token
        .strip_prefix("0x")
        .or_else(|| token.strip_prefix("0X"))?;
    u32::from_str_radix(digits, 16).ok()
}

/// Whether `token` is a valid assembler identifier: ASCII alphabetic or
/// `_` first, then alphanumeric or `_`.
fn is_identifier(token: &str) -> bool {
    let mut chars = token.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|rest| rest.is_ascii_alphanumeric() || rest == '_')
}

/// Rewrites `raw` into a valid assembler identifier: every character
/// outside `[A-Za-z0-9_]` becomes `_`, and a leading digit gains a `_`
/// prefix. The result is never empty.
#[must_use]
pub fn sanitize_identifier(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| if is_identifier_char(c) { c } else { '_' })
        .collect();
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// Whether `c` may appear in an assembler identifier.
fn is_identifier_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

impl fmt::Display for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for spec in &self.functions {
            match &spec.module {
                None => {
                    let keyword = match spec.state {
                        State::Arm => "arm_func",
                        State::Thumb => "thumb_func",
                    };
                    write!(f, "{keyword} {:#010x}", spec.addr)?;
                }
                Some(module) => {
                    let state = match spec.state {
                        State::Arm => "arm",
                        State::Thumb => "thumb",
                    };
                    write!(f, "{module}.s {state} {:#010x}", spec.addr)?;
                }
            }
            if let Some(name) = &spec.name {
                write!(f, " {name}")?;
            }
            writeln!(f)?;
        }
        for hint in &self.tables {
            let keyword = match hint.state {
                State::Arm => "arm_table",
                State::Thumb => "thumb_table",
            };
            writeln!(f, "{keyword} {:#010x} {}", hint.addr, hint.entries)?;
        }
        Ok(())
    }
}
