//! Byte-level code/data flag map over canonical ROM addresses.
//!
//! Mirrors the reference tool's RomFlags: every image byte carries a set
//! of [`Flags`] describing what the analysis believes lives there, and
//! every query takes a GBA address which is canonicalized (mirror bands
//! stripped) into an image offset first. The map serializes to JSON as
//! run-length-coded flag values so analyses can be cached.

use rom::{Image, ImageKind};
use serde::{Deserialize, Serialize};

/// Cart execution base (GBATEK, "Memory Map"; the runtime's boot base).
const CART_BASE: u32 = 0x0800_0000;

/// Multiboot execution base in EWRAM (GBATEK, "Multiboot").
const MULTIBOOT_BASE: u32 = 0x0200_0000;

/// Processor state a seeded entry point executes in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum State {
    /// ARM state (4-byte instructions).
    Arm,
    /// THUMB state (2-byte instructions).
    Thumb,
}

/// One seeded entry point: where traversal starts and in which state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seed {
    /// Canonical address of the first instruction.
    pub addr: u32,
    /// State the code at `addr` runs in.
    pub state: State,
}

/// Address canonicalization rules for an image kind.
///
/// The top four address bits are unused (GBATEK, "Unused Memory Area");
/// cartridge ROM mirrors live at `0x0A000000..0x0DFFFFFF`, and EWRAM
/// mirrors every 256 KiB through `0x02FFFFFF`. Canonicalization strips
/// those bands so both a mirror and its target land on the same flag-map
/// byte (matching the bus mirroring).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Canonical {
    base: u32,
    kind: Kind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Cart,
    Multiboot,
}

impl Canonical {
    /// Canonicalization rules for an image kind.
    #[must_use]
    pub fn for_kind(kind: ImageKind) -> Self {
        match kind {
            ImageKind::Cart => Self {
                base: CART_BASE,
                kind: Kind::Cart,
            },
            ImageKind::Multiboot => Self {
                base: MULTIBOOT_BASE,
                kind: Kind::Multiboot,
            },
        }
    }

    /// The execution base address: `0x08000000` for carts, `0x02000000`
    /// for multiboot images.
    #[must_use]
    pub fn base(self) -> u32 {
        self.base
    }

    /// Canonicalizes a GBA address, or `None` when it names a region the
    /// image cannot back (different region, or a non-mirrored address).
    #[must_use]
    pub fn canonicalize(self, addr: u32) -> Option<u32> {
        match self.kind {
            Kind::Cart => {
                let stripped = addr & 0x0FFF_FFFF;
                (0x0800_0000..0x0E00_0000)
                    .contains(&stripped)
                    .then_some(stripped)
            }
            Kind::Multiboot => ((addr & 0xFF00_0000) == 0x0200_0000).then_some(addr & 0x0203_FFFF),
        }
    }

    /// Image offset of a GBA address, or `None` when uncanonicalizable.
    #[must_use]
    pub fn offset(self, addr: u32) -> Option<usize> {
        self.canonicalize(addr)
            .filter(|canonical| *canonical >= self.base)
            .map(|canonical| (canonical - self.base) as usize)
    }
}

/// Per-byte flag bits over one image byte.
///
/// The bit values are part of the JSON cache format; [`Flags::from_bits`]
/// accepts any `u8` so older caches keep loading.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Flags(u8);

impl Flags {
    /// No flags: unanalyzed byte.
    pub const EMPTY: Self = Self(0);
    /// Byte is part of an executed instruction.
    pub const EXEC: Self = Self(1 << 0);
    /// Byte is part of a decoded instruction word.
    pub const WORD: Self = Self(1 << 1);
    /// Byte is part of a literal-pool word (PC-relative load target).
    pub const LITERAL: Self = Self(1 << 2);
    /// Byte is data.
    pub const DATA: Self = Self(1 << 3);
    /// Byte starts a function.
    pub const FUNC_START: Self = Self(1 << 4);

    /// Builds flags from raw cache bits.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    /// Raw bits for the JSON cache.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Whether every bit of `other` is set.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether no bit is set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for Flags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for Flags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl std::ops::BitAnd for Flags {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

/// A maximal run of image bytes carrying equal flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    /// Canonical address of the first byte.
    pub addr: u32,
    /// Run length in bytes (always nonzero).
    pub len: usize,
    /// Flags shared by every byte in the run.
    pub flags: Flags,
}

/// Serialized form of a [`FlagMap`]: run-length-coded flag values.
#[derive(Serialize, Deserialize)]
struct FlagMapData {
    version: u32,
    base: u32,
    size: u32,
    /// Consecutive `(run_length, flag_bits)` pairs summing to `size`.
    runs: Vec<(u32, u8)>,
}

/// Serialization and loading failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The cache names a base this version does not know.
    #[error("unknown flag-map base {0:#010x}")]
    UnknownBase(u32),
    /// The cache format is newer than this build.
    #[error("flag-map version {0} is not supported")]
    Version(u32),
    /// The encoded runs do not cover exactly `size` bytes.
    #[error("flag-map runs cover {sum} bytes but the size field says {size}")]
    RunSum {
        /// Sum of the encoded run lengths.
        sum: u64,
        /// The declared image size.
        size: u32,
    },
    /// The cache is not valid JSON or has the wrong shape.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Byte-level flag map over one image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlagMap {
    canon: Canonical,
    bytes: Vec<Flags>,
}

impl FlagMap {
    /// Builds an empty map with `size` bytes canonicalized by `canon`.
    #[must_use]
    pub fn new(size: usize, canon: Canonical) -> Self {
        Self {
            canon,
            bytes: vec![Flags::EMPTY; size],
        }
    }

    /// Builds an empty map sized to `image` with its kind's rules.
    #[must_use]
    pub fn from_image(image: &Image) -> Self {
        Self::new(image.bytes().len(), Canonical::for_kind(image.kind()))
    }

    /// The map's canonicalization rules.
    #[must_use]
    pub fn canonical(&self) -> Canonical {
        self.canon
    }

    /// Number of bytes covered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether the map covers no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Flags at a GBA address; uncanonicalizable addresses and offsets
    /// beyond the image read empty.
    #[must_use]
    pub fn get(&self, addr: u32) -> Flags {
        match self.canon.offset(addr) {
            Some(offset) if offset < self.bytes.len() => self.bytes[offset],
            _ => Flags::EMPTY,
        }
    }

    /// Replaces the flags at a GBA address; out-of-image writes are
    /// ignored so traversal can mark speculatively.
    pub fn set(&mut self, addr: u32, flags: Flags) {
        if let Some(offset) = self.canon.offset(addr).filter(|&o| o < self.bytes.len()) {
            self.bytes[offset] = flags;
        }
    }

    /// ORs flags into a GBA address.
    pub fn mark(&mut self, addr: u32, flags: Flags) {
        if let Some(offset) = self.canon.offset(addr).filter(|&o| o < self.bytes.len()) {
            self.bytes[offset] |= flags;
        }
    }

    /// ORs flags into every address in `start..end` (GBA addresses,
    /// canonicalized per byte).
    pub fn mark_range(&mut self, start: u32, end: u32, flags: Flags) {
        for addr in start..end {
            self.mark(addr, flags);
        }
    }

    /// Seeds the map from the image header and user entries, marking
    /// [`Flags::FUNC_START`] at each entry.
    ///
    /// The header entry is the cart/multiboot base in ARM state (GBATEK,
    /// "The Cartridge Header": the entry word is ARM code the machine
    /// executes first). User seeds whose addresses cannot be
    /// canonicalized are dropped. Returns the seeds in traversal order.
    pub fn seed(&mut self, user: &[Seed]) -> Vec<Seed> {
        let mut seeds = vec![Seed {
            addr: self.canon.base,
            state: State::Arm,
        }];
        for seed in user {
            if self.canon.canonicalize(seed.addr).is_some() {
                seeds.push(*seed);
            }
        }
        for seed in &seeds {
            self.mark(seed.addr, Flags::FUNC_START);
        }
        seeds
    }

    /// Maximal runs of equal flags, in address order. Unmarked bytes
    /// form [`Flags::EMPTY`] runs so a disassembler can emit raw data
    /// blocks for them.
    pub fn runs(&self) -> impl Iterator<Item = Run> + '_ {
        let mut next = 0usize;
        std::iter::from_fn(move || {
            if next >= self.bytes.len() {
                return None;
            }
            let start = next;
            let flags = self.bytes[start];
            while next < self.bytes.len() && self.bytes[next] == flags {
                next += 1;
            }
            Some(Run {
                addr: self.canon.base + start as u32,
                len: next - start,
                flags,
            })
        })
    }

    /// Serializes to JSON for caching. The format is run-length coded:
    /// `{"version":1,"base":..,"size":..,"runs":[[len,bits],..]}`.
    ///
    /// # Errors
    ///
    /// Propagates JSON encoding failures.
    pub fn to_json(&self) -> Result<String, Error> {
        let mut runs = Vec::new();
        for run in self.runs() {
            runs.push((run.len as u32, run.flags.bits()));
        }
        let data = FlagMapData {
            version: 1,
            base: self.canon.base,
            size: self.bytes.len() as u32,
            runs,
        };
        Ok(serde_json::to_string(&data)?)
    }

    /// Restores a map from [`FlagMap::to_json`] output.
    ///
    /// # Errors
    ///
    /// Fails on malformed JSON, unknown bases or versions, and runs that
    /// do not cover exactly the declared size.
    pub fn from_json(json: &str) -> Result<Self, Error> {
        let data: FlagMapData = serde_json::from_str(json)?;
        if data.version != 1 {
            return Err(Error::Version(data.version));
        }
        let canon = match data.base {
            CART_BASE => Canonical {
                base: CART_BASE,
                kind: Kind::Cart,
            },
            MULTIBOOT_BASE => Canonical {
                base: MULTIBOOT_BASE,
                kind: Kind::Multiboot,
            },
            other => return Err(Error::UnknownBase(other)),
        };
        let mut bytes = Vec::with_capacity(data.size as usize);
        let mut sum = 0u64;
        for (len, bits) in data.runs {
            sum += u64::from(len);
            bytes.extend(std::iter::repeat_n(Flags::from_bits(bits), len as usize));
        }
        if sum != u64::from(data.size) {
            return Err(Error::RunSum {
                sum,
                size: data.size,
            });
        }
        Ok(Self { canon, bytes })
    }
}

#[cfg(test)]
mod tests {
    use super::{Canonical, FlagMap, Flags, Run, Seed, State};
    use rom::ImageKind;

    #[test]
    fn cart_canonicalization_strips_mirrors() {
        let cart = Canonical::for_kind(ImageKind::Cart);
        assert_eq!(cart.base(), 0x0800_0000);
        assert_eq!(cart.canonicalize(0x0800_0000), Some(0x0800_0000));
        // Mirror bands 0x0A..0x0D and unused top bits.
        assert_eq!(cart.canonicalize(0x0A00_0004), Some(0x0A00_0004));
        assert_eq!(cart.canonicalize(0x1A00_0004), Some(0x0A00_0004));
        // SRAM region and foreign regions do not map into the image.
        assert_eq!(cart.canonicalize(0x0E00_0000), None);
        assert_eq!(cart.canonicalize(0x0000_0100), None);
        assert_eq!(cart.canonicalize(0x0300_0000), None);
    }

    #[test]
    fn multiboot_canonicalization_strips_ewram_mirrors() {
        let mb = Canonical::for_kind(ImageKind::Multiboot);
        assert_eq!(mb.base(), 0x0200_0000);
        assert_eq!(mb.canonicalize(0x0200_0000), Some(0x0200_0000));
        assert_eq!(mb.canonicalize(0x0204_0010), Some(0x0200_0010));
        assert_eq!(mb.canonicalize(0x0300_0000), None);
        assert_eq!(mb.canonicalize(0x0800_0000), None);
    }

    #[test]
    fn offset_maps_into_the_image() {
        let cart = Canonical::for_kind(ImageKind::Cart);
        assert_eq!(cart.offset(0x0800_00C0), Some(0xC0));
        // Mirror offset beyond the image size still canonicalizes; bounds
        // are the map's job.
        assert_eq!(cart.offset(0x0A00_0000), Some(0x0200_0000));
        assert_eq!(cart.offset(0x0700_0000), None);
    }

    #[test]
    fn flags_behave_as_a_bitfield() {
        let both = Flags::EXEC | Flags::FUNC_START;
        assert!(both.contains(Flags::EXEC));
        assert!(both.contains(Flags::FUNC_START));
        assert!(!both.contains(Flags::WORD));
        assert!(Flags::EXEC.contains(Flags::EXEC));
        assert!(Flags::EMPTY.is_empty());
        assert_eq!(both & Flags::EXEC, Flags::EXEC);
        let mut flags = Flags::WORD;
        flags |= Flags::LITERAL;
        assert_eq!(flags, Flags(0x06));
    }

    #[test]
    fn set_get_and_mark_respect_image_bounds() {
        let mut map = FlagMap::new(0x100, Canonical::for_kind(ImageKind::Cart));
        map.set(0x0800_0010, Flags::EXEC);
        assert_eq!(map.get(0x0800_0010), Flags::EXEC);
        // A mirror offset lands past this small image, so it reads empty
        // and ignores writes even though it canonicalizes.
        assert_eq!(map.get(0x0A00_0010), Flags::EMPTY);
        map.set(0x0A00_0010, Flags::DATA);
        assert_eq!(map.get(0x0A00_0010), Flags::EMPTY);
        // Foreign regions do not canonicalize at all.
        assert_eq!(map.get(0x0E00_0000), Flags::EMPTY);
        map.set(0x0E00_0000, Flags::DATA);
        assert_eq!(map.get(0x0E00_0000), Flags::EMPTY);
    }

    #[test]
    fn mark_range_covers_end_exclusively() {
        let mut map = FlagMap::new(0x100, Canonical::for_kind(ImageKind::Cart));
        map.mark_range(0x0800_0004, 0x0800_0008, Flags::EXEC);
        assert_eq!(map.get(0x0800_0003), Flags::EMPTY);
        assert_eq!(map.get(0x0800_0004), Flags::EXEC);
        assert_eq!(map.get(0x0800_0007), Flags::EXEC);
        assert_eq!(map.get(0x0800_0008), Flags::EMPTY);
    }

    #[test]
    fn runs_group_equal_flag_bytes() {
        let mut map = FlagMap::new(8, Canonical::for_kind(ImageKind::Cart));
        map.mark_range(0x0800_0000, 0x0800_0002, Flags::EXEC);
        map.mark(0x0800_0005, Flags::DATA);
        let runs: Vec<Run> = map.runs().collect();
        assert_eq!(
            runs,
            vec![
                Run {
                    addr: 0x0800_0000,
                    len: 2,
                    flags: Flags::EXEC
                },
                Run {
                    addr: 0x0800_0002,
                    len: 3,
                    flags: Flags::EMPTY
                },
                Run {
                    addr: 0x0800_0005,
                    len: 1,
                    flags: Flags::DATA
                },
                Run {
                    addr: 0x0800_0006,
                    len: 2,
                    flags: Flags::EMPTY
                },
            ]
        );
    }

    #[test]
    fn seed_marks_header_entry_and_user_entries() {
        let mut map = FlagMap::new(0x100, Canonical::for_kind(ImageKind::Cart));
        let user = [
            Seed {
                addr: 0x0800_00C0,
                state: State::Thumb,
            },
            Seed {
                addr: 0x0300_0000,
                state: State::Arm,
            },
        ];
        let seeds = map.seed(&user);
        assert_eq!(
            seeds,
            vec![
                Seed {
                    addr: 0x0800_0000,
                    state: State::Arm
                },
                Seed {
                    addr: 0x0800_00C0,
                    state: State::Thumb
                },
            ]
        );
        assert!(map.get(0x0800_0000).contains(Flags::FUNC_START));
        assert!(map.get(0x0800_00C0).contains(Flags::FUNC_START));
        // The 0x03000000 seed names IWRAM, not the image, and is dropped.
        assert_eq!(map.get(0x0300_0000), Flags::EMPTY);
    }

    #[test]
    fn json_round_trip_preserves_the_map() {
        let mut map = FlagMap::new(64, Canonical::for_kind(ImageKind::Cart));
        map.mark_range(0x0800_0000, 0x0800_0004, Flags::EXEC | Flags::WORD);
        map.mark(0x0800_0000, Flags::FUNC_START);
        map.mark_range(0x0800_0010, 0x0800_0014, Flags::LITERAL);
        map.set(0x0800_0020, Flags::DATA);

        let json = map.to_json().expect("serialize");
        let restored = FlagMap::from_json(&json).expect("restore");
        assert_eq!(restored, map);
        let before: Vec<Run> = map.runs().collect();
        let after: Vec<Run> = restored.runs().collect();
        assert_eq!(before, after);
    }

    #[test]
    fn json_load_rejects_bad_caches() {
        assert!(FlagMap::from_json("not json").is_err());
        assert!(
            FlagMap::from_json(r#"{"version":2,"base":134217728,"size":0,"runs":[]}"#).is_err()
        );
        assert!(FlagMap::from_json(r#"{"version":1,"base":1,"size":0,"runs":[]}"#).is_err());
        let bad_sum = r#"{"version":1,"base":134217728,"size":8,"runs":[[3,0]]}"#;
        match FlagMap::from_json(bad_sum) {
            Err(super::Error::RunSum { sum, size }) => {
                assert_eq!((sum, size), (3, 8));
            }
            other => panic!("expected RunSum, got {other:?}"),
        }
    }
}
