//! Whole-image hashing for identification.

/// MD5 and SHA-1 digests of an image.
///
/// Both hashes are required to match a database entry; see
/// [`crate::db::Database::find`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hashes {
    /// 16-byte MD5 digest.
    pub md5: [u8; 16],
    /// 20-byte SHA-1 digest.
    pub sha1: [u8; 20],
}

impl Hashes {
    /// Computes both digests over `data`.
    #[must_use]
    pub fn compute(data: &[u8]) -> Self {
        use md5::Digest;
        let md5 = md5::Md5::digest(data);
        let sha1 = sha1::Sha1::digest(data);
        let mut md5_out = [0u8; 16];
        md5_out.copy_from_slice(&md5);
        let mut sha1_out = [0u8; 20];
        sha1_out.copy_from_slice(&sha1);
        Self {
            md5: md5_out,
            sha1: sha1_out,
        }
    }

    /// MD5 digest as lowercase hex.
    #[must_use]
    pub fn md5_hex(&self) -> String {
        to_hex(&self.md5)
    }

    /// SHA-1 digest as lowercase hex.
    #[must_use]
    pub fn sha1_hex(&self) -> String {
        to_hex(&self.sha1)
    }
}

/// Lowercase hex encoding.
fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::Hashes;

    #[test]
    fn matches_known_digests() {
        let hashes = Hashes::compute(b"abc");
        assert_eq!(hashes.md5_hex(), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            hashes.sha1_hex(),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        let empty = Hashes::compute(b"");
        assert_eq!(empty.md5_hex(), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(empty.sha1_hex(), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }
}
