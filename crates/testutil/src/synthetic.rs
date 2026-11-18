//! In-memory synthetic `.gba` images for tests.
//!
//! The builder emits a header that passes complement-checksum validation so
//! loader tests can start from a known-valid image. The Nintendo logo region
//! is filled with a fixed pattern instead of the real logo bitmap, which we
//! do not distribute.

/// Size of the GBA cartridge header in bytes (GBATEK, "The Cartridge
/// Header").
const HEADER_LEN: usize = 0x100;

/// Fill byte for the logo region at `0x004..0x0A0`. Synthetic stand-in for
/// the copyrighted Nintendo logo bitmap.
const LOGO_FILL: u8 = 0xA5;

/// Builder for in-memory synthetic GBA cartridge images.
///
/// # Examples
///
/// ```
/// use testutil::synthetic::RomBuilder;
///
/// let rom = RomBuilder::new().title("DEMO").payload(&[1, 2, 3]).build();
/// assert_eq!(rom[0x0B2], 0x96);
/// assert_eq!(&rom[0x100..0x103], &[1, 2, 3]);
/// ```
#[derive(Clone, Debug)]
pub struct RomBuilder {
    title: [u8; 12],
    game_code: [u8; 4],
    maker_code: [u8; 2],
    entry: u32,
    payload: Vec<u8>,
    min_size: usize,
}

impl RomBuilder {
    /// Creates a builder with the defaults: title `OXYBOY`, game code
    /// `OTST`, maker code `00`, entry point `0x08000000`, empty payload,
    /// and a 512-byte image.
    #[must_use]
    pub fn new() -> Self {
        Self {
            title: fit_into::<12>("OXYBOY"),
            game_code: fit_into::<4>("OTST"),
            maker_code: fit_into::<2>("00"),
            entry: 0x0800_0000,
            payload: Vec::new(),
            min_size: 512,
        }
    }

    /// Sets the 12-byte game title at `0x0A0`. Input is truncated to 12
    /// bytes and NUL padded.
    #[must_use]
    pub fn title(mut self, title: &str) -> Self {
        self.title = fit_into::<12>(title);
        self
    }

    /// Sets the 4-byte game code at `0x0AC`. Input is truncated to 4 bytes
    /// and NUL padded.
    #[must_use]
    pub fn game_code(mut self, game_code: &str) -> Self {
        self.game_code = fit_into::<4>(game_code);
        self
    }

    /// Sets the 2-byte maker code at `0x0B0`. Input is truncated to 2 bytes
    /// and NUL padded.
    #[must_use]
    pub fn maker_code(mut self, maker_code: &str) -> Self {
        self.maker_code = fit_into::<2>(maker_code);
        self
    }

    /// Sets the entry-point word stored at `0x000`. On a real cartridge
    /// this holds an ARM branch to the entry address; tests store the raw
    /// value.
    #[must_use]
    pub fn entry(mut self, entry: u32) -> Self {
        self.entry = entry;
        self
    }

    /// Replaces the payload placed at `0x100`.
    #[must_use]
    pub fn payload(mut self, payload: &[u8]) -> Self {
        self.payload = payload.to_vec();
        self
    }

    /// Grows the final image to at least `size` bytes with zero padding.
    #[must_use]
    pub fn min_size(mut self, size: usize) -> Self {
        self.min_size = size;
        self
    }

    /// Builds the image and fixes up the complement checksum at `0x0BD`,
    /// computed as `-(sum(header[0x0A0..0x0BD]) + 0x19) mod 256` (GBATEK,
    /// "The Cartridge Header").
    #[must_use]
    pub fn build(self) -> Vec<u8> {
        let size = HEADER_LEN
            .max(self.min_size)
            .max(HEADER_LEN + self.payload.len());
        let mut image = vec![0u8; size];
        image[0x000..0x004].copy_from_slice(&self.entry.to_le_bytes());
        image[0x004..0x0A0].fill(LOGO_FILL);
        image[0x0A0..0x0AC].copy_from_slice(&self.title);
        image[0x0AC..0x0B0].copy_from_slice(&self.game_code);
        image[0x0B0..0x0B2].copy_from_slice(&self.maker_code);
        // 0x0B2 holds the fixed unit code required by GBATEK.
        image[0x0B2] = 0x96;
        image[0x0BD] = complement_checksum(&image);
        image[HEADER_LEN..HEADER_LEN + self.payload.len()].copy_from_slice(&self.payload);
        image
    }
}

impl Default for RomBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Computes the header complement checksum over bytes `0x0A0..0x0BD`.
fn complement_checksum(header: &[u8]) -> u8 {
    let sum: u32 = header[0x0A0..0x0BD].iter().map(|&b| u32::from(b)).sum();
    (0u32.wrapping_sub(sum).wrapping_sub(0x19) & 0xFF) as u8
}

/// Copies `s` into a fixed-width buffer, truncating and NUL padding.
fn fit_into<const N: usize>(s: &str) -> [u8; N] {
    let mut out = [0u8; N];
    for (out_byte, src_byte) in out.iter_mut().zip(s.bytes()) {
        *out_byte = src_byte;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{RomBuilder, complement_checksum};

    #[test]
    fn checksum_matches_the_bios_formula() {
        let mut header = vec![0u8; 0x100];
        header[0x0A0..0x0AC].copy_from_slice(b"CHECKSUMTEST");
        header[0x0B2] = 0x96;
        let expected = header[0x0A0..0x0BD]
            .iter()
            .fold(0u8, |chk, &b| chk.wrapping_sub(b))
            .wrapping_sub(0x19);
        assert_eq!(complement_checksum(&header), expected);
    }

    #[test]
    fn defaults_build_512_byte_image() {
        let rom = RomBuilder::new().build();
        assert_eq!(rom.len(), 512);
        assert_eq!(rom[0x0B2], 0x96);
        assert!(rom[0x0BE..0x100].iter().all(|&b| b == 0));
    }
}
