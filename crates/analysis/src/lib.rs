//! Code and data discovery over decoded GBA images.

pub mod config;
pub mod discover;
pub mod flags;
pub mod traverse;

pub use config::{Config, ConfigError, FunctionSpec};
pub use discover::{Discovery, Function, Thresholds, discover};
pub use flags::{Canonical, Error, FlagMap, Flags, Run, Seed, State};
pub use traverse::{Call, Stop, StopReason, Traversal, WalkOptions, traverse, walk};

#[cfg(test)]
mod tests {
    #[test]
    fn workspace_wiring() {
        assert_eq!(env!("CARGO_PKG_NAME"), "analysis");
    }
}
