//! The analysis pipeline: traversal, jump-table detection, and
//! function discovery over one flag map.
//!
//! The loop marks code, resolves switch idioms (and hinted tables),
//! feeds the resolved targets back as traversal paths, and repeats
//! until no new targets appear (bounded, so a pathological image
//! terminates). Discovery then runs over the final map. Indirect jumps
//! no idiom or hint explains stay in [`Analysis::unresolved`] as
//! `UnknownTarget` stops for the hybrid fallback.

use rom::Image;

use crate::config::TableHint;
use crate::discover::{Discovery, Thresholds, discover};
use crate::flags::{FlagMap, Seed};
use crate::switch::{JumpTable, find_tables, mark_tables};
use crate::traverse::{Stop, StopReason, walk};

/// Full analysis result over one image.
#[derive(Debug, Default)]
pub struct Analysis {
    /// Function discovery over the final map.
    pub discovery: Discovery,
    /// Every resolved jump table, in site order.
    pub tables: Vec<JumpTable>,
    /// `UnknownTarget` stops at indirect jumps no table explains.
    pub unresolved: Vec<Stop>,
}

/// Bound on the mark-and-rescan fixpoint loop.
const MAX_PASSES: usize = 16;

/// Runs the full pipeline over `image`, mutating `map` with marks.
///
/// `provided` seeds are function entries known up front (config);
/// `paths` seeds everything the traversal should start from (the entry
/// plus `provided`).
pub fn analyze(
    image: &Image,
    map: &mut FlagMap,
    provided: &[Seed],
    paths: &[Seed],
    thresholds: Thresholds,
    hints: &[TableHint],
) -> Analysis {
    let mut paths = paths.to_vec();
    let mut tables: Vec<JumpTable> = Vec::new();
    let mut unresolved = Vec::new();
    for _ in 0..MAX_PASSES {
        let traversal = walk(image, map, &paths, crate::traverse::WalkOptions::global());
        unresolved = traversal
            .stops
            .iter()
            .copied()
            .filter(|stop| stop.reason == StopReason::UnknownTarget)
            .collect();
        let found = find_tables(image, map, hints);
        let mut fresh = Vec::new();
        for table in &found {
            for (target, state) in &table.targets {
                let seed = Seed {
                    addr: *target,
                    state: *state,
                };
                if !paths
                    .iter()
                    .any(|known| known.addr == seed.addr && known.state == seed.state)
                {
                    fresh.push(seed);
                }
            }
        }
        tables = found;
        if fresh.is_empty() {
            break;
        }
        mark_tables(map, &tables);
        unresolved.retain(|stop| {
            !tables
                .iter()
                .any(|table| table.site == stop.addr && table.state == stop.state)
        });
        paths.extend(fresh);
    }
    mark_tables(map, &tables);
    let discovery = discover(image, map, provided, &paths, thresholds);
    unresolved.retain(|stop| {
        !tables
            .iter()
            .any(|table| table.site == stop.addr && table.state == stop.state)
    });
    Analysis {
        discovery,
        tables,
        unresolved,
    }
}
