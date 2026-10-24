//! The `info` subcommand: header fields, hashes, and identification.

use std::path::{Path, PathBuf};

use rom::{Database, DatabaseError, Hashes, Image, ImageKind};
use serde::Serialize;

use crate::RomArgs;

/// Runs `oxyboy info` and returns the process exit code.
///
/// Identification uses the optional JSON database at `db_path` (the
/// `--db` flag or `OXYBOY_ROM_DB`). An unknown image and a missing
/// database are both reported with exit code 0; only unreadable
/// input fails.
pub fn run(args: InfoArgs) -> i32 {
    let image = match Image::from_file(&args.rom.rom) {
        Ok(image) => image,
        Err(e) => {
            eprintln!("oxyboy info: {e}");
            return 1;
        }
    };
    let hashes = Hashes::compute(image.bytes());
    let identification = match identify(args.db.as_deref(), &hashes) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("oxyboy info: {e}");
            return 1;
        }
    };
    let report = Report::build(&args.rom.rom, &image, &hashes, &identification);
    if args.json {
        let text = match serde_json::to_string_pretty(&report) {
            Ok(text) => text,
            Err(e) => {
                eprintln!("oxyboy info: {e}");
                return 1;
            }
        };
        println!("{text}");
    } else {
        print!("{report}");
    }
    0
}

/// Resolves the identification line.
///
/// # Errors
///
/// Propagates database load and validation failures.
fn identify(db_path: Option<&Path>, hashes: &Hashes) -> Result<String, DatabaseError> {
    let Some(path) = db_path else {
        return Ok("no database supplied".to_owned());
    };
    let db = Database::from_file(path)?;
    Ok(match db.find(hashes) {
        Some(entry) => match &entry.status {
            Some(status) => format!("{} ({status})", entry.name),
            None => entry.name.clone(),
        },
        None => "unknown".to_owned(),
    })
}

/// The full report, rendered as stable text (the `Display` form) or
/// JSON with stable field names.
#[derive(Debug, Serialize)]
pub struct Report {
    /// Path as given on the command line.
    pub file: String,
    /// Image size in bytes.
    pub size: u64,
    /// Whether the size is a multiple of four.
    pub size_multiple_of_4: bool,
    /// `cart` or `multiboot`.
    pub kind: String,
    /// Header entry word, `0x`-prefixed upper-case hex.
    pub entry_point: String,
    /// Header title, or null when not valid UTF-8.
    pub title: Option<String>,
    /// Header game code, or null when not valid UTF-8.
    pub game_code: Option<String>,
    /// Header maker code, or null when not valid UTF-8.
    pub maker_code: Option<String>,
    /// Header unit code, `0x`-prefixed hex.
    pub unit_code: String,
    /// Header device type, `0x`-prefixed hex.
    pub device_type: String,
    /// Header version, `0x`-prefixed hex.
    pub version: String,
    /// Whether the header complement checksum matches.
    pub checksum_valid: bool,
    /// Stored complement checksum, `0x`-prefixed hex.
    pub checksum: String,
    /// Expected complement checksum, `0x`-prefixed hex.
    pub checksum_expected: String,
    /// MD5 over the whole image, lowercase hex.
    pub md5: String,
    /// SHA-1 over the whole image, lowercase hex.
    pub sha1: String,
    /// Identification line from the database, when one resolves.
    pub identification: String,
}

impl Report {
    fn build(path: &Path, image: &Image, hashes: &Hashes, identification: &str) -> Self {
        let header = image.header();
        Report {
            file: path.display().to_string(),
            size: image.bytes().len() as u64,
            size_multiple_of_4: image.size_is_multiple_of_4(),
            kind: match image.kind() {
                ImageKind::Cart => "cart".to_owned(),
                ImageKind::Multiboot => "multiboot".to_owned(),
            },
            entry_point: format!("0x{:08X}", header.entry_word),
            title: header.title_str().map(str::to_owned),
            game_code: header.game_code_str().map(str::to_owned),
            maker_code: header.maker_code_str().map(str::to_owned),
            unit_code: format!("0x{:02X}", header.unit_code),
            device_type: format!("0x{:02X}", header.device_type),
            version: format!("0x{:02X}", header.version),
            checksum_valid: header.checksum_valid(),
            checksum: format!("0x{:02X}", header.complement_checksum),
            checksum_expected: format!("0x{:02X}", header.expected_checksum()),
            md5: hashes.md5_hex(),
            sha1: hashes.sha1_hex(),
            identification: identification.to_owned(),
        }
    }
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "file: {}", self.file)?;
        writeln!(
            f,
            "size: {} bytes ({})",
            self.size,
            if self.size_multiple_of_4 {
                "multiple of 4"
            } else {
                "not a multiple of 4"
            }
        )?;
        writeln!(f, "kind: {}", self.kind)?;
        writeln!(f, "entry point: {}", self.entry_point)?;
        writeln!(f, "title: {}", field_str(&self.title))?;
        writeln!(f, "game code: {}", field_str(&self.game_code))?;
        writeln!(f, "maker code: {}", field_str(&self.maker_code))?;
        writeln!(f, "unit code: {}", self.unit_code)?;
        writeln!(f, "device type: {}", self.device_type)?;
        writeln!(f, "version: {}", self.version)?;
        if self.checksum_valid {
            writeln!(f, "checksum: valid ({})", self.checksum)?;
        } else {
            writeln!(
                f,
                "checksum: INVALID (stored {}, expected {})",
                self.checksum, self.checksum_expected
            )?;
        }
        writeln!(f, "md5: {}", self.md5)?;
        writeln!(f, "sha1: {}", self.sha1)?;
        writeln!(f, "identification: {}", self.identification)
    }
}

/// Renders an optional field with a placeholder for missing values.
fn field_str(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or("(invalid UTF-8)")
}

/// Arguments of `oxyboy info`.
#[derive(Debug, clap::Args)]
pub struct InfoArgs {
    #[command(flatten)]
    pub rom: RomArgs,

    /// Path to a JSON ROM database used for identification.
    #[arg(long, value_name = "FILE", env = "OXYBOY_ROM_DB")]
    pub db: Option<PathBuf>,

    /// Print the report as JSON instead of text.
    #[arg(long)]
    pub json: bool,
}
