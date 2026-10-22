//! Locating assembler-built test ROMs.
//!
//! Larger test ROMs are assembled from checked-in sources under
//! `tests/rom-src/` by `scripts/build-test-roms.sh` into
//! `target/test-roms/`. That script needs `arm-none-eabi-binutils`, so
//! machines without the toolchain never have the images: tests locate
//! them through [`locate`] or [`require`] and skip loudly instead of
//! failing. A directory of prebuilt images can be supplied through the
//! `OXYBOY_TEST_ROMS` environment variable.

use std::env;
use std::path::{Path, PathBuf};

/// Resolves a built test ROM by name (without extension).
///
/// Search order: the `OXYBOY_TEST_ROMS` directory, then every workspace
/// `target/test-roms/` directory found by walking up from this crate's
/// manifest path. When `OXYBOY_TEST_ROMS` is set it is authoritative: a
/// missing image is an error naming the variable rather than a silent
/// fallthrough.
///
/// # Errors
///
/// Returns a printable reason when the image cannot be found; the
/// message always names the remedy (run the build script or fix the
/// variable).
pub fn locate(name: &str) -> Result<PathBuf, String> {
    let file = format!("{name}.gba");
    if let Ok(dir) = env::var("OXYBOY_TEST_ROMS") {
        let path = Path::new(&dir).join(&file);
        return if path.is_file() {
            Ok(path)
        } else {
            Err(format!(
                "OXYBOY_TEST_ROMS is set to {dir}, but {file} is not in it; \
                 run scripts/build-test-roms.sh or fix the variable"
            ))
        };
    }
    for root in candidate_roots() {
        let path = root.join("target").join("test-roms").join(&file);
        if path.is_file() {
            return Ok(path);
        }
    }
    Err(format!(
        "{file} not found: run scripts/build-test-roms.sh (requires \
         arm-none-eabi-binutils) or point OXYBOY_TEST_ROMS at a directory \
         of built test ROMs"
    ))
}

/// Resolves a built test ROM, turning a miss into a loud skip.
///
/// Prints the reason to stderr and returns `None` so a test can bail
/// with an early `let Some(rom) = ... else { return; }`.
pub fn require(name: &str) -> Option<PathBuf> {
    match locate(name) {
        Ok(path) => Some(path),
        Err(reason) => {
            eprintln!("skipping test: {reason}");
            None
        }
    }
}

/// Directories whose `target/test-roms/` may hold built images.
fn candidate_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(dir) = env::var("CARGO_TARGET_DIR") {
        roots.push(PathBuf::from(dir));
    }
    // The manifest path of the crate that compiled this helper is an
    // ancestor walk away from the workspace root.
    if let Ok(manifest) = env::var("CARGO_MANIFEST_DIR") {
        let mut dir = PathBuf::from(manifest);
        loop {
            roots.push(dir.clone());
            if !dir.pop() {
                break;
            }
        }
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::{candidate_roots, locate, require};
    use std::fs;

    /// Points `OXYBOY_TEST_ROMS` at a fresh temp dir. The mutation is
    /// `unsafe` under edition 2024 and confined to this test; sibling
    /// tests only read the variable and tolerate any value.
    unsafe fn set_roms_dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("oxyboy-testrom-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        // SAFETY: single-threaded env mutation confined to this test.
        unsafe { std::env::set_var("OXYBOY_TEST_ROMS", &dir) };
        dir
    }

    #[test]
    fn resolves_from_oxyboy_test_roms_then_reports_missing() {
        let dir = unsafe { set_roms_dir("locate") };
        fs::write(dir.join("probe.gba"), b"MZ").expect("write probe");

        let found = locate("probe").expect("probe must resolve");
        assert_eq!(found, dir.join("probe.gba"));

        let missing = locate("no-such-rom-anywhere").expect_err("must miss");
        assert!(missing.contains("OXYBOY_TEST_ROMS"), "{missing}");

        unsafe { std::env::remove_var("OXYBOY_TEST_ROMS") };
        let unset = locate("no-such-rom-anywhere").expect_err("must miss");
        assert!(unset.contains("build-test-roms.sh"), "{unset}");
        assert!(require("no-such-rom-anywhere").is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn require_skips_or_finds_built_roms() {
        // When the build script has run on this machine, the workspace
        // target dir holds mode3.gba; when it has not, require skips.
        if let Some(path) = require("mode3") {
            assert!(path.is_file(), "{path:?} is not a file");
        }
    }

    #[test]
    fn candidate_roots_include_a_workspace_root() {
        assert!(
            candidate_roots()
                .iter()
                .any(|r| r.join("Cargo.toml").is_file()),
            "no workspace root among {:?}",
            candidate_roots()
        );
    }
}
