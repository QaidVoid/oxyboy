//! Cartridge image loading, header parsing, and identification.
//!
//! Shared by the analyzer and the runtime. Depends on no other workspace
//! crate.

#[cfg(test)]
mod tests {
    #[test]
    fn workspace_wiring() {
        assert_eq!(env!("CARGO_PKG_NAME"), "rom");
    }
}
