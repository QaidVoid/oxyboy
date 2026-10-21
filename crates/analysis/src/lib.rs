//! Code and data discovery over decoded GBA images.

#[cfg(test)]
mod tests {
    #[test]
    fn workspace_wiring() {
        assert_eq!(env!("CARGO_PKG_NAME"), "analysis");
    }
}
