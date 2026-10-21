//! The `info` subcommand: header fields, hashes, and identification.

use std::path::{Path, PathBuf};

use rom::{Database, DatabaseError, Hashes, Image, ImageKind};

use crate::RomArgs;

/// Runs `oxyboy info` and returns the process exit code.
///
/// Identification uses the optional JSON database at `db_path` (the
/// `--db` flag or `OXYBOY_ROM_DB`). An unknown image and a missing
/// database are both reported on stdout with exit code 0; only unreadable
/// input fails.
pub fn run(args: RomArgs, db_path: Option<PathBuf>) -> i32 {
    let image = match Image::from_file(&args.rom) {
        Ok(image) => image,
        Err(e) => {
            eprintln!("oxyboy info: {e}");
            return 1;
        }
    };
    let hashes = Hashes::compute(image.bytes());
    let identification = match identify(db_path, &hashes) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("oxyboy info: {e}");
            return 1;
        }
    };
    print_report(&args.rom, &image, &hashes, &identification);
    0
}

/// Resolves the identification line.
///
/// # Errors
///
/// Propagates database load and validation failures.
fn identify(db_path: Option<PathBuf>, hashes: &Hashes) -> Result<String, DatabaseError> {
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

/// Prints the full report in a stable, golden-testable format.
fn print_report(path: &Path, image: &Image, hashes: &Hashes, identification: &str) {
    let header = image.header();
    let kind = match image.kind() {
        ImageKind::Cart => "cart",
        ImageKind::Multiboot => "multiboot",
    };
    let size_note = if image.size_is_multiple_of_4() {
        "multiple of 4"
    } else {
        "not a multiple of 4"
    };
    println!("file: {}", path.display());
    println!("size: {} bytes ({size_note})", image.bytes().len());
    println!("kind: {kind}");
    println!("entry point: 0x{:08X}", header.entry_word);
    println!("title: {}", field(header.title_str()));
    println!("game code: {}", field(header.game_code_str()));
    println!("maker code: {}", field(header.maker_code_str()));
    println!("unit code: 0x{:02X}", header.unit_code);
    println!("device type: 0x{:02X}", header.device_type);
    println!("version: 0x{:02X}", header.version);
    if header.checksum_valid() {
        println!("checksum: valid (0x{:02X})", header.complement_checksum);
    } else {
        println!(
            "checksum: INVALID (stored 0x{:02X}, expected 0x{:02X})",
            header.complement_checksum,
            header.expected_checksum()
        );
    }
    println!("md5: {}", hashes.md5_hex());
    println!("sha1: {}", hashes.sha1_hex());
    println!("identification: {identification}");
}

/// Renders an optional ASCII field with a placeholder for non-UTF-8.
fn field(value: Option<&str>) -> &str {
    value.unwrap_or("(invalid UTF-8)")
}
