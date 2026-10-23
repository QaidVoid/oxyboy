//! The `suite-test` subcommand: run one external conformance-suite ROM
//! headless and decode its on-screen verdict.
//!
//! The jsmolka/gba-tests suites draw their verdict in the suite font
//! (lib/glyphs.asm, one 8x8 1-bpp glyph per ASCII char 32..127, two
//! `dw` words per glyph, LSB-first rows) into bitmap video memory:
//! "All tests passed" on success, "Failed test NNN" at the first
//! failing test, then park. This command runs `--frames` frames,
//! reconstructs the glyph table from the fetched glyphs.asm, decodes
//! every 8x8 screen cell, and prints exactly one verdict token on
//! stdout: `pass`, `fail <test-number>`, or `timeout`. Diagnostics go
//! to stderr; the validating script aggregates the tokens into the
//! per-category pass-rate report.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rom::{Image, ImageKind};
use runtime::ppu::{VISIBLE_LINES, WIDTH};
use runtime::system::System;

use crate::RomArgs;

/// Cart execution base (see the `run` subcommand; no BIOS reset).
const CART_BASE: u32 = 0x0800_0000;

/// Success message drawn by `m_test_eval` (lib/macros.inc).
const PASS_MESSAGE: &str = "All tests passed";

/// Prefix of the failure message, before the test number digits.
const FAIL_PREFIX: &str = "Failed test";

/// Arguments of `oxyboy suite-test`.
#[derive(Debug, clap::Args)]
pub struct SuiteTestArgs {
    #[command(flatten)]
    pub rom: RomArgs,

    /// Path to the suite's glyphs.asm font-table source.
    #[arg(long, value_name = "FILE")]
    pub glyphs: PathBuf,

    /// Frames to run before decoding the verdict.
    #[arg(long, value_name = "N", default_value_t = 300)]
    pub frames: u32,
}

/// Runs `oxyboy suite-test` and returns the process exit code.
pub fn run(args: SuiteTestArgs) -> i32 {
    let image = match Image::from_file(&args.rom.rom) {
        Ok(image) => image,
        Err(e) => {
            eprintln!("oxyboy suite-test: {}: {e}", args.rom.rom.display());
            return 1;
        }
    };
    let mut system = match image.kind() {
        ImageKind::Multiboot => System::with_multiboot(image.bytes().to_vec()),
        ImageKind::Cart => System::new(image.bytes().to_vec(), CART_BASE),
    };
    for _ in 0..args.frames {
        system.step_frame();
    }
    let table = match read_glyph_table(&args.glyphs) {
        Ok(table) => table,
        Err(e) => {
            eprintln!("oxyboy suite-test: {}: {e}", args.glyphs.display());
            return 1;
        }
    };
    let text = decode_text(system.framebuffer(), &table);
    eprintln!("oxyboy suite-test: decoded screen text: {text:?}");
    match verdict(&text) {
        Verdict::Pass => println!("pass"),
        Verdict::Fail(number) => println!("fail {number}"),
        Verdict::Timeout => println!("timeout"),
    }
    0
}

/// The verdict a decoded screen text represents.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The suite reached `eval` with a zero test register.
    Pass,
    /// The suite bailed at the numbered test.
    Fail(u32),
    /// No recognizable verdict text after the frame budget.
    Timeout,
}

/// Classifies decoded screen text into a [`Verdict`].
#[must_use]
pub fn verdict(text: &str) -> Verdict {
    if text.contains(PASS_MESSAGE) {
        return Verdict::Pass;
    }
    if let Some(rest) = text.split_once(FAIL_PREFIX).map(|(_, r)| r) {
        let digits: String = rest
            .chars()
            .skip_while(|c: &char| !c.is_ascii_digit())
            .take_while(|c: &char| c.is_ascii_digit())
            .collect();
        if let Ok(number) = digits.parse::<u32>() {
            return Verdict::Fail(number);
        }
    }
    Verdict::Timeout
}

/// Parses the glyph table from a glyphs.asm source text: every `dw`
/// line contributes comma-separated hex words, and consecutive word
/// pairs pack one 8x8 glyph (low word rows 0-3, high word rows 4-7).
///
/// # Errors
///
/// Returns a printable message when a `dw` operand is malformed or a
/// glyph pair is incomplete.
pub fn read_glyph_table(path: &Path) -> Result<Vec<u64>, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("cannot read glyph table: {e}"))?;
    let mut words = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("dw") else {
            continue;
        };
        for part in rest.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let hex = part
                .strip_prefix("0x")
                .or_else(|| part.strip_prefix("0X"))
                .ok_or_else(|| format!("malformed dw operand {part:?}"))?;
            let word = u64::from_str_radix(hex, 16)
                .map_err(|e| format!("malformed dw operand {part:?}: {e}"))?;
            words.push(u32::try_from(word).map_err(|_| format!("dw operand {part:?} overflows"))?);
        }
    }
    if words.len() % 2 != 0 {
        return Err("glyph table has an odd number of dw words".to_string());
    }
    Ok(words
        .chunks(2)
        .map(|pair| u64::from(pair[0]) | (u64::from(pair[1]) << 32))
        .collect())
}

/// Decodes the screen text through the glyph table and returns every
/// non-empty text line over all 64 (x, y) grid phases. The suite draws
/// its verdict at fixed pixel positions (x=56 or x=60, y=76) that are
/// not multiples of the 8-pixel glyph cell, so a single aligned grid
/// slices glyphs in half; sweeping the phase guarantees one grid puts
/// every glyph cell-aligned. Unmatched cells decode as `?`. A pixel is
/// ink when its red channel is dark: the suite draws black glyphs
/// (palette index 1) on white (palette index 0).
#[must_use]
pub fn decode_text(frame: &[u8], glyphs: &[u64]) -> String {
    let mut by_bits: HashMap<u64, char> = HashMap::new();
    for (index, bits) in glyphs.iter().enumerate() {
        let ch = char::from_u32(32 + index as u32).unwrap_or('?');
        by_bits.entry(*bits).or_insert(ch);
    }
    let cell = |x0: usize, y0: usize| -> char {
        let mut bits = 0u64;
        for y in 0..8usize {
            for x in 0..8usize {
                let offset = (y0 + y) * WIDTH + x0 + x;
                let Some(pixel) = frame.get(offset * 4) else {
                    continue;
                };
                if *pixel < 128 {
                    bits |= 1 << (8 * y + x);
                }
            }
        }
        by_bits.get(&bits).copied().unwrap_or('?')
    };
    let mut lines = Vec::new();
    for dy in 0..8usize {
        for dx in 0..8usize {
            for cy in 0..VISIBLE_LINES / 8 {
                let mut line = String::new();
                for cx in 0..WIDTH / 8 {
                    line.push(cell(cx * 8 + dx, cy * 8 + dy));
                }
                let trimmed = line.trim_end();
                if !trimmed.is_empty() {
                    lines.push(trimmed.to_string());
                }
            }
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Verdict, decode_text, read_glyph_table, verdict};

    /// Packs glyph rows (each a u8, MSB = leftmost pixel) into the
    /// two-word form the suite's font table uses.
    fn glyph(rows: [u8; 8]) -> u64 {
        let mut words = [0u32; 2];
        for (i, row) in rows.iter().enumerate() {
            words[i / 4] |= u32::from(*row) << (8 * (i % 4));
        }
        u64::from(words[0]) | (u64::from(words[1]) << 32)
    }

    /// A distinct all-rows pattern per glyph index, blank for the
    /// space glyph (index 0): every char decodes uniquely.
    fn test_font() -> Vec<u64> {
        (0..96).map(|i| glyph([i as u8; 8])).collect()
    }

    /// Draws `text` at cell (0, 0) onto a white frame.
    fn draw(text: &str, glyphs: &[u64]) -> Vec<u8> {
        let mut frame = vec![255u8; runtime::ppu::WIDTH * runtime::ppu::VISIBLE_LINES * 4];
        for (i, ch) in text.chars().enumerate() {
            let bits = glyphs[usize::from(ch as u8) - 32];
            for y in 0..8usize {
                for x in 0..8usize {
                    if bits & (1 << (8 * y + x)) != 0 {
                        let offset = y * runtime::ppu::WIDTH + i * 8 + x;
                        frame[offset * 4] = 0;
                        frame[offset * 4 + 1] = 0;
                        frame[offset * 4 + 2] = 0;
                    }
                }
            }
        }
        frame
    }

    #[test]
    fn rejects_missing_or_malformed_glyph_sources() {
        let missing = read_glyph_table(Path::new("fixtures/no-such-file"));
        assert!(missing.is_err());
        let dir =
            std::env::temp_dir().join(format!("oxyboy-suite-glyphs-reject-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("glyphs.asm");
        std::fs::write(
            &path,
            "align 4\nglyphs:\n  dw 0x18181818,0x00180018\n  dw 0x3636\n",
        )
        .expect("write");
        let malformed = read_glyph_table(&path);
        assert!(malformed.is_err(), "odd word count must be rejected");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn parses_dw_lines_into_glyphs() {
        let dir =
            std::env::temp_dir().join(format!("oxyboy-suite-glyphs-parse-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("glyphs.asm");
        std::fs::write(
            &path,
            "align 4\nglyphs:\n  dw 0x18181818,0x00180018\n  dw 0x0,0x3636\n",
        )
        .expect("write");
        let table = read_glyph_table(&path).expect("parses");
        assert_eq!(
            table,
            vec![
                u64::from(0x1818_1818u32) | (u64::from(0x0018_0018u32) << 32),
                u64::from(0x3636u32) << 32,
            ]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn decodes_drawn_text() {
        let glyphs = test_font();
        let frame = draw("All tests passed", &glyphs);
        assert_eq!(verdict(&decode_text(&frame, &glyphs)), Verdict::Pass);
        let frame = draw("Failed test 123", &glyphs);
        assert_eq!(verdict(&decode_text(&frame, &glyphs)), Verdict::Fail(123));
        let frame = draw("Failed test 224", &glyphs);
        assert_eq!(verdict(&decode_text(&frame, &glyphs)), Verdict::Fail(224));
    }

    /// The suite draws its verdict at pixel positions that are not
    /// multiples of the glyph cell (x=60, y=76); the phase sweep must
    /// still decode it.
    #[test]
    fn decodes_unaligned_text() {
        let glyphs = test_font();
        let mut frame = vec![255u8; runtime::ppu::WIDTH * runtime::ppu::VISIBLE_LINES * 4];
        let glyphs_by_char = |ch: char| glyphs[usize::from(ch as u8) - 32];
        for (i, ch) in "Failed test 7".chars().enumerate() {
            let bits = glyphs_by_char(ch);
            for y in 0..8usize {
                for x in 0..8usize {
                    if bits & (1 << (8 * y + x)) != 0 {
                        let offset = (76 + y) * runtime::ppu::WIDTH + 60 + i * 8 + x;
                        frame[offset * 4] = 0;
                        frame[offset * 4 + 1] = 0;
                        frame[offset * 4 + 2] = 0;
                    }
                }
            }
        }
        assert_eq!(verdict(&decode_text(&frame, &glyphs)), Verdict::Fail(7));
    }

    #[test]
    fn classifies_verdicts() {
        assert_eq!(verdict("All tests passed"), Verdict::Pass);
        assert_eq!(verdict("Failed test 7"), Verdict::Fail(7));
        assert_eq!(verdict("Failed test 157"), Verdict::Fail(157));
        assert_eq!(verdict("garbled ??"), Verdict::Timeout);
        assert_eq!(verdict(""), Verdict::Timeout);
    }
}
