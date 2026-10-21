//! oxyboy command line interface.

fn main() {}

#[cfg(test)]
mod tests {
    #[test]
    fn workspace_wiring() {
        assert_eq!(env!("CARGO_PKG_NAME"), "oxyboy");
    }
}
