//! Golden-file assertions with opt-in regeneration.
//!
//! Golden files hold the expected text output of a test. They are committed
//! next to the test that owns them (usually `tests/golden/`) and are
//! regenerated with `OXYBOY_UPDATE_GOLDEN=1 cargo test`, never edited by
//! hand.

use std::fs;
use std::path::Path;

/// Reports whether golden files should be regenerated instead of compared.
///
/// Enabled by setting the `OXYBOY_UPDATE_GOLDEN` environment variable to
/// `1`.
#[must_use]
pub fn update_golden() -> bool {
    matches!(std::env::var("OXYBOY_UPDATE_GOLDEN").as_deref(), Ok("1"))
}

/// Asserts that `actual` matches the golden file at `path`.
///
/// With `OXYBOY_UPDATE_GOLDEN=1` the file is created or overwritten with
/// `actual` (creating parent directories) and the check passes.
///
/// # Panics
///
/// Panics when the golden file cannot be read, does not exist, or its
/// contents differ from `actual`. The panic message explains how to
/// regenerate the file.
///
/// # Examples
///
/// ```no_run
/// use std::path::Path;
///
/// let path =
///     Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/demo.txt");
/// testutil::golden::assert_matches(&path, "expected output");
/// ```
pub fn assert_matches(path: impl AsRef<Path>, actual: &str) {
    let path = path.as_ref();
    if update_golden() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap_or_else(|e| {
                panic!("failed to create golden dir {}: {e}", parent.display())
            });
        }
        fs::write(path, actual)
            .unwrap_or_else(|e| panic!("failed to write golden file {}: {e}", path.display()));
        return;
    }
    let expected = fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "failed to read golden file {}: {e}\nRun with OXYBOY_UPDATE_GOLDEN=1 to create it",
            path.display()
        )
    });
    if expected != actual {
        panic!(
            "golden mismatch in {}\n\
             --- expected ---\n\
             {expected}\n\
             --- actual ---\n\
             {actual}\n\
             Run with OXYBOY_UPDATE_GOLDEN=1 to regenerate",
            path.display()
        );
    }
}
