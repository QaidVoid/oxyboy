//! Matching assembly emission from analyzed GBA images.

#[cfg(test)]
mod tests {
    #[test]
    fn workspace_wiring() {
        assert_eq!(env!("CARGO_PKG_NAME"), "emit");
    }
}
