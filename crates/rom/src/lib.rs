//! Cartridge image loading, header parsing, and identification.
//!
//! Shared by the analyzer and the runtime. Depends on no other workspace
//! crate.
//!
//! # Examples
//!
//! ```
//! use testutil::synthetic::RomBuilder;
//!
//! let bytes = RomBuilder::new().title("DEMO").payload(&[1, 2, 3]).build();
//! let image = rom::Image::from_bytes(bytes).unwrap();
//! assert_eq!(image.header().title_str(), Some("DEMO"));
//! assert_eq!(&image.bytes()[0x100..0x103], &[1, 2, 3]);
//! ```

pub mod db;
pub mod hash;
pub mod header;
pub mod image;

pub use db::{Database, DatabaseEntry, DatabaseError};
pub use hash::Hashes;
pub use header::Header;
pub use image::{Image, ImageKind, LoadError, MAX_ROM_SIZE};
