//! Code and data discovery over decoded GBA images.

pub mod flags;
pub mod traverse;

pub use flags::{Canonical, Error, FlagMap, Flags, Run, Seed, State};
pub use traverse::{Stop, StopReason, Traversal, traverse};

#[cfg(test)]
mod tests {
    #[test]
    fn workspace_wiring() {
        assert_eq!(env!("CARGO_PKG_NAME"), "analysis");
    }
}
