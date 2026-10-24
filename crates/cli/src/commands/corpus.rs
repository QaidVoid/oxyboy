//! The `corpus` subcommand: determinism checks and frame-time budgets
//! over the regression corpus.
//!
//! The manifest (`corpus.json`) lists every entry with its frame count,
//! per-frame wall-time budget, and the expected final-state hash. Each
//! entry runs twice through the pure interpreter; the two state hashes
//! must agree (determinism) and match the pinned hash (regression).
//! An optional homebrew directory contributes determinism-only entries.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rom::Image;
use serde::{Deserialize, Serialize};

use super::diff::{boot, snapshot};

/// Frames run for homebrew entries, which carry no per-entry frames.
const HOMEBREW_FRAMES: u32 = 60;

/// Per-frame budget for homebrew entries, in milliseconds.
const HOMEBREW_BUDGET_MS: u64 = 300;

/// Arguments of `oxyboy corpus`.
#[derive(Debug, clap::Args)]
pub struct CorpusArgs {
    /// Path to the corpus manifest.
    #[arg(long, value_name = "FILE", default_value = "corpus.json")]
    pub manifest: PathBuf,

    /// Re-pin each entry's expected-state hash from this run.
    #[arg(long, default_value_t = false)]
    pub update: bool,
}

/// The corpus manifest: pinned entries plus an optional homebrew dir.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Directory of user-supplied homebrew ROMs; every `.gba` inside
    /// gets a determinism-only check with the default budget.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homebrew_dir: Option<PathBuf>,
    /// The curated entries.
    #[serde(default)]
    pub entries: Vec<Entry>,
}

/// One corpus entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    /// Short name used in reports.
    pub name: String,
    /// ROM path, relative to the manifest's directory.
    pub rom: PathBuf,
    /// Frames to run before hashing the state.
    pub frames: u32,
    /// Per-frame wall-time budget in milliseconds.
    pub budget_ms: u64,
    /// Expected final-state hash; `None` pins nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
}

/// One unit of work: an entry to check.
struct Job {
    name: String,
    rom: PathBuf,
    frames: u32,
    budget_ms: u64,
    pinned: Option<String>,
}

/// Runs `oxyboy corpus` and returns the process exit code: 0 when
/// every checked entry is deterministic, on-budget, and on-hash; 1 on
/// any failure, naming the entries.
pub fn run(args: CorpusArgs) -> i32 {
    let text = match fs::read_to_string(&args.manifest) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("oxyboy corpus: {}: {e}", args.manifest.display());
            return 1;
        }
    };
    let mut manifest: Manifest = match serde_json::from_str(&text) {
        Ok(manifest) => manifest,
        Err(e) => {
            eprintln!("oxyboy corpus: {}: {e}", args.manifest.display());
            return 1;
        }
    };
    let base = args
        .manifest
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let mut jobs: Vec<Job> = manifest
        .entries
        .iter()
        .map(|entry| Job {
            name: entry.name.clone(),
            rom: base.join(&entry.rom),
            frames: entry.frames,
            budget_ms: entry.budget_ms,
            pinned: entry.hash.clone(),
        })
        .collect();
    if let Some(dir) = &manifest.homebrew_dir {
        let dir = base.join(dir);
        match homebrew_roms(&dir) {
            Ok(roms) => {
                for rom in roms {
                    let name = rom
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("homebrew")
                        .to_string();
                    jobs.push(Job {
                        name,
                        rom,
                        frames: HOMEBREW_FRAMES,
                        budget_ms: HOMEBREW_BUDGET_MS,
                        pinned: None,
                    });
                }
            }
            Err(e) => eprintln!("oxyboy corpus: homebrew dir {}: {e}", dir.display()),
        }
    }
    let mut failures: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for job in &jobs {
        if !job.rom.is_file() {
            skipped.push(job.name.clone());
            continue;
        }
        let outcome = check_entry(job);
        match outcome {
            Ok(result) => {
                println!(
                    "{}: hash={} ms/frame={:.1} budget={}",
                    job.name, result.hash, result.ms_per_frame, job.budget_ms
                );
                if result.ms_per_frame > job.budget_ms as f64 {
                    failures.push(format!(
                        "{}: ms/frame {:.1} exceeds budget {}",
                        job.name, result.ms_per_frame, job.budget_ms
                    ));
                }
                if let Some(pinned) = &job.pinned
                    && pinned != &result.hash
                {
                    failures.push(format!(
                        "{}: state hash {} differs from pinned {pinned}",
                        job.name, result.hash
                    ));
                }
                if !args.update {
                    continue;
                }
                if let Some(entry) = manifest
                    .entries
                    .iter_mut()
                    .find(|entry| entry.name == job.name)
                {
                    entry.hash = Some(result.hash);
                }
            }
            Err(e) => failures.push(format!("{}: {e}", job.name)),
        }
    }
    if args.update {
        let json = match serde_json::to_string_pretty(&manifest) {
            Ok(json) => json,
            Err(e) => {
                eprintln!("oxyboy corpus: manifest serialize: {e}");
                return 1;
            }
        };
        if let Err(e) = fs::write(&args.manifest, json + "\n") {
            eprintln!("oxyboy corpus: {}: {e}", args.manifest.display());
            return 1;
        }
        println!("manifest updated: {}", args.manifest.display());
    }
    if !skipped.is_empty() {
        println!("skipped (ROM missing): {}", skipped.join(", "));
    }
    if !failures.is_empty() {
        eprintln!("oxyboy corpus: {} failure(s):", failures.len());
        for failure in &failures {
            eprintln!("  {failure}");
        }
        return 1;
    }
    println!("corpus: {} checked, all green", jobs.len() - skipped.len());
    0
}

/// The outcome of one entry's double run.
struct EntryResult {
    hash: String,
    ms_per_frame: f64,
}

/// Runs one entry twice and compares state hashes and timing.
///
/// # Errors
///
/// Returns a printable message when the ROM cannot be read or the two
/// runs disagree on the final state.
fn check_entry(job: &Job) -> Result<EntryResult, String> {
    let image = Image::from_file(&job.rom).map_err(|e| e.to_string())?;
    let mut total_ms = 0.0;
    let mut hashes: [String; 2] = [String::new(), String::new()];
    for hash in &mut hashes {
        let mut system = boot(&image);
        let start = Instant::now();
        for _ in 0..job.frames {
            system.step_frame();
        }
        total_ms += start.elapsed().as_secs_f64() * 1000.0;
        *hash = state_hash(&snapshot(&system));
    }
    if hashes[0] != hashes[1] {
        return Err(format!(
            "nondeterministic: run 1 hash {} != run 2 hash {}",
            hashes[0], hashes[1]
        ));
    }
    Ok(EntryResult {
        hash: hashes[0].clone(),
        ms_per_frame: total_ms / 2.0 / f64::from(job.frames),
    })
}

/// FNV-1a over every slice's name and bytes; the corpus state hash.
fn state_hash(slices: &[super::diff::Slice]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for slice in slices {
        for byte in slice
            .name
            .as_bytes()
            .iter()
            .copied()
            .chain(std::iter::once(0xffu8))
            .chain(slice.bytes.iter().copied())
        {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

/// Lists the `.gba` files in a homebrew directory, sorted by name.
///
/// # Errors
///
/// Returns a printable message when the directory cannot be read.
fn homebrew_roms(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut roms = Vec::new();
    let read = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for path in read {
        let path = path.map_err(|e| format!("{}: {e}", dir.display()))?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("gba") {
            roms.push(path);
        }
    }
    roms.sort();
    Ok(roms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_hash_is_stable_and_order_sensitive() {
        let mk = |name: &'static str, bytes: &[u8]| super::super::diff::Slice {
            name,
            bytes: bytes.to_vec(),
        };
        let a = vec![mk("registers", &[1, 2, 3]), mk("vram", &[9])];
        let b = vec![mk("registers", &[1, 2, 3]), mk("vram", &[9])];
        assert_eq!(state_hash(&a), state_hash(&b));
        let flipped = vec![mk("vram", &[9]), mk("registers", &[1, 2, 3])];
        assert_ne!(state_hash(&a), state_hash(&flipped));
        let one_byte = vec![mk("registers", &[4, 2, 3]), mk("vram", &[9])];
        assert_ne!(state_hash(&a), state_hash(&one_byte));
    }

    #[test]
    fn manifest_parses_optional_fields() {
        let text = r#"{
            "entries": [
                {"name": "mode3", "rom": "target/test-roms/mode3.gba",
                 "frames": 60, "budget_ms": 100,
                 "hash": "0123456789abcdef"},
                {"name": "loose", "rom": "roms/loose.gba",
                 "frames": 10, "budget_ms": 5}
            ]
        }"#;
        let manifest: Manifest = serde_json::from_str(text).expect("parse");
        assert_eq!(manifest.entries.len(), 2);
        assert_eq!(
            manifest.entries[0].hash.as_deref(),
            Some("0123456789abcdef")
        );
        assert_eq!(manifest.entries[1].hash, None);
        assert_eq!(manifest.homebrew_dir, None);
    }

    #[test]
    fn manifest_rejects_missing_fields() {
        let text = r#"{"entries": [{"name": "x", "rom": "x.gba"}]}"#;
        assert!(serde_json::from_str::<Manifest>(text).is_err());
    }

    #[test]
    fn homebrew_roms_lists_only_gba_sorted() {
        let dir = std::env::temp_dir().join(format!("oxyboy-corpus-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("mkdir");
        for name in ["b.gba", "a.gba", "c.txt"] {
            fs::write(dir.join(name), []).expect("write");
        }
        let roms = homebrew_roms(&dir).expect("list");
        let names: Vec<String> = roms
            .iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str().map(String::from)))
            .collect();
        assert_eq!(names, ["a.gba", "b.gba"]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn roundtrip_keeps_optional_hash_absent() {
        let manifest = Manifest {
            homebrew_dir: None,
            entries: vec![Entry {
                name: "x".into(),
                rom: "x.gba".into(),
                frames: 1,
                budget_ms: 1,
                hash: None,
            }],
        };
        let json = serde_json::to_string(&manifest).expect("serialize");
        assert!(!json.contains("hash"), "absent hash leaked: {json}");
    }
}
