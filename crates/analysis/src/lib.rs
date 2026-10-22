//! Code and data discovery over decoded GBA images.

pub mod flags;

pub use flags::{Canonical, Error, FlagMap, Flags, Run, Seed, State};

#[cfg(test)]
mod tests {
    #[test]
    fn workspace_wiring() {
        assert_eq!(env!("CARGO_PKG_NAME"), "analysis");
    }
}
