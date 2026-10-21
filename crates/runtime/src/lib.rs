//! GBA runtime: bus, CPU interpreter, hardware devices, and the
//! recompilation host.

#[cfg(test)]
mod tests {
    #[test]
    fn workspace_wiring() {
        assert_eq!(env!("CARGO_PKG_NAME"), "runtime");
    }
}
