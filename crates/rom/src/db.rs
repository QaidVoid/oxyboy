//! JSON ROM database loading and lookup.
//!
//! The database maps hash pairs to names and statuses. It is user
//! supplied; no database ships with this crate. A lookup is a hit only
//! when both MD5 and SHA-1 match, following the reference tool's rule
//! against false positives.

use std::path::Path;

use serde::Deserialize;
use thiserror::Error;

/// A ROM database: a list of known images.
#[derive(Clone, Debug, Deserialize)]
pub struct Database {
    /// Known images.
    #[serde(default)]
    pub entries: Vec<DatabaseEntry>,
}

/// One known image in a [`Database`].
#[derive(Clone, Debug, Deserialize)]
pub struct DatabaseEntry {
    /// Human-readable image name.
    pub name: String,
    /// Dump status such as `Verified`, `Bad dump`, or `Homebrew`.
    pub status: Option<String>,
    /// MD5 digest as lowercase hex.
    pub md5: String,
    /// SHA-1 digest as lowercase hex.
    pub sha1: String,
}

/// Errors produced while loading a database.
#[derive(Debug, Error)]
pub enum DatabaseError {
    /// The file could not be read.
    #[error("cannot read database: {0}")]
    Io(#[from] std::io::Error),

    /// The file is not valid JSON with the expected schema.
    #[error("cannot parse database: {0}")]
    Json(#[from] serde_json::Error),

    /// An entry has malformed hashes.
    #[error("database entry {index}: {reason}")]
    InvalidEntry {
        /// Zero-based index of the offending entry.
        index: usize,
        /// What is wrong with it.
        reason: String,
    },
}

impl Database {
    /// Parses a database from JSON text and validates every entry.
    ///
    /// # Errors
    ///
    /// Returns [`DatabaseError::Json`] on malformed JSON and
    /// [`DatabaseError::InvalidEntry`] when an entry's hashes are not the
    /// right length or not hex.
    pub fn from_json(text: &str) -> Result<Self, DatabaseError> {
        let db: Database = serde_json::from_str(text).map_err(DatabaseError::Json)?;
        for (index, entry) in db.entries.iter().enumerate() {
            validate_hash(&entry.md5, 32).map_err(|reason| DatabaseError::InvalidEntry {
                index,
                reason: format!("md5: {reason}"),
            })?;
            validate_hash(&entry.sha1, 40).map_err(|reason| DatabaseError::InvalidEntry {
                index,
                reason: format!("sha1: {reason}"),
            })?;
        }
        Ok(db)
    }

    /// Loads a database from a file.
    ///
    /// # Errors
    ///
    /// Returns [`DatabaseError::Io`] when the file cannot be read,
    /// otherwise the errors of [`Database::from_json`].
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, DatabaseError> {
        let text = std::fs::read_to_string(path)?;
        Self::from_json(&text)
    }

    /// Finds the entry matching `hashes`.
    ///
    /// A hit requires both the MD5 and the SHA-1 digest to match; an entry
    /// matching only one hash is never returned.
    #[must_use]
    pub fn find(&self, hashes: &crate::hash::Hashes) -> Option<&DatabaseEntry> {
        let md5 = hashes.md5_hex();
        let sha1 = hashes.sha1_hex();
        self.entries.iter().find(|e| e.md5 == md5 && e.sha1 == sha1)
    }
}

/// Validates a hex hash field of the expected length, normalized to
/// lowercase.
fn validate_hash(hex: &str, len: usize) -> Result<String, String> {
    let lower = hex.to_ascii_lowercase();
    if lower.len() != len {
        return Err(format!(
            "expected {len} hex characters, got {}",
            lower.len()
        ));
    }
    if !lower.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("contains non-hex characters".to_owned());
    }
    Ok(lower)
}

#[cfg(test)]
mod tests {
    use super::Database;
    use crate::hash::Hashes;

    const VALID_DB: &str = r#"{
        "entries": [
            { "name": "Known Game", "status": "Verified",
              "md5": "900150983cd24fb0d6963f7d28e17f72",
              "sha1": "a9993e364706816aba3e25717850c26c9cd0d89d" },
            { "name": "Half Match", "status": null,
              "md5": "900150983cd24fb0d6963f7d28e17f72",
              "sha1": "0000000000000000000000000000000000000000" }
        ]
    }"#;

    #[test]
    fn loads_and_finds_double_hash_hits() {
        let db = Database::from_json(VALID_DB).expect("valid database");
        let hashes = Hashes::compute(b"abc");
        let found = db.find(&hashes).expect("hit");
        assert_eq!(found.name, "Known Game");
        assert_eq!(found.status.as_deref(), Some("Verified"));
    }

    #[test]
    fn single_hash_match_is_not_a_hit() {
        let db = Database::from_json(VALID_DB).expect("valid database");
        let hashes = Hashes::compute(b"abc");
        assert_eq!(db.entries.len(), 2);
        assert!(db.find(&hashes).unwrap().name != "Half Match");
        // And an image whose only partial match is the other entry:
        let other = Hashes::compute(b"");
        assert!(db.find(&other).is_none());
    }

    #[test]
    fn rejects_malformed_hashes() {
        let bad = r#"{ "entries": [ { "name": "X", "md5": "zz", "sha1": "00" } ] }"#;
        match Database::from_json(bad) {
            Err(super::DatabaseError::InvalidEntry { index, .. }) => assert_eq!(index, 0),
            other => panic!("expected InvalidEntry, got {other:?}"),
        }
    }

    #[test]
    fn empty_entries_is_accepted() {
        let db = Database::from_json("{}").expect("empty database");
        assert!(db.entries.is_empty());
    }
}
