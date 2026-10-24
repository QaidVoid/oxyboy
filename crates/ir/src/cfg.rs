//! Control-flow-graph construction and dominance analysis over lifted
//! functions.
//!
//! The IR already carries basic blocks with explicit terminators, so
//! this module derives the analysis views the structurer
//! needs: predecessor/successor adjacency over the blocks reachable
//! from the entry, the immediate-dominator tree (Cooper-Harvey-
//! Kennedy), dominance frontiers, and the natural-loop forest from
//! back edges. Dumps in text and dot form exist for debugging.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use crate::{BlockId, Function, Terminator};

/// The control-flow graph of one function: adjacency over reachable
/// blocks plus the entry block (by IR layout, block 0).
pub struct Cfg {
    /// The entry block.
    pub entry: BlockId,
    /// Successors per block index.
    pub succs: Vec<Vec<BlockId>>,
    /// Predecessors per block index.
    pub preds: Vec<Vec<BlockId>>,
    /// Whether each block is reachable from the entry.
    reachable: Vec<bool>,
}

/// One natural loop: its header plus the blocks that reach the back
/// edge without passing through the header. Sorted by block index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loop {
    /// The loop header: the unique target of a back edge that
    /// dominates the edge's source.
    pub header: BlockId,
    /// The header and every block that can reach the back edge
    /// without going through the header.
    pub body: Vec<BlockId>,
}

impl Cfg {
    /// Builds the CFG of `function` from its terminators.
    pub fn build(function: &Function) -> Cfg {
        let count = function.blocks.len();
        let mut succs = vec![Vec::new(); count];
        let mut preds = vec![Vec::new(); count];
        for (index, block) in function.blocks.iter().enumerate() {
            let from = BlockId(index as u32);
            let targets: Vec<BlockId> = match &block.term {
                Terminator::Branch { target, .. } => vec![*target],
                Terminator::BranchCond {
                    then_target,
                    else_target,
                    ..
                } => vec![*then_target, *else_target],
                Terminator::Call {
                    cont: Some((cont, _)),
                    ..
                } => vec![*cont],
                _ => Vec::new(),
            };
            succs[index] = targets.clone();
            for target in targets {
                preds[target.0 as usize].push(from);
            }
        }
        let mut cfg = Cfg {
            entry: BlockId(0),
            succs,
            preds,
            reachable: vec![false; count],
        };
        cfg.compute_reachability();
        cfg
    }

    fn compute_reachability(&mut self) {
        let mut stack = vec![self.entry];
        self.reachable[self.entry.0 as usize] = true;
        while let Some(block) = stack.pop() {
            for succ in &self.succs[block.0 as usize] {
                if !self.reachable[succ.0 as usize] {
                    self.reachable[succ.0 as usize] = true;
                    stack.push(*succ);
                }
            }
        }
    }

    /// `true` when control can reach the block from the entry.
    pub fn is_reachable(&self, block: BlockId) -> bool {
        self.reachable[block.0 as usize]
    }

    /// The immediate dominator of every reachable block (`None` for
    /// the entry); unreachable blocks are `None` as well.
    pub fn idoms(&self) -> Vec<Option<BlockId>> {
        // Cooper, Harvey, Kennedy: A Simple, Fast Dominance Algorithm.
        let count = self.succs.len();
        let mut idoms: Vec<Option<BlockId>> = vec![None; count];
        // A dense postorder numbering with the entry last gives the
        // reverse-postorder walk the algorithm wants.
        let mut order: Vec<BlockId> = Vec::new();
        let mut visited = vec![false; count];
        self.postorder(self.entry, &mut visited, &mut order);
        order.reverse();
        let mut po_number = vec![usize::MAX; count];
        for (position, block) in order.iter().enumerate() {
            po_number[block.0 as usize] = position;
        }
        let idom_of = |idoms: &[Option<BlockId>], block: BlockId| {
            idoms[block.0 as usize].map(|b| po_number[b.0 as usize])
        };
        let mut changed = true;
        while changed {
            changed = false;
            for block in &order {
                if *block == self.entry {
                    continue;
                }
                let mut new_idom: Option<BlockId> = None;
                for pred in &self.preds[block.0 as usize] {
                    if !self.reachable[pred.0 as usize]
                        || (idoms[pred.0 as usize].is_none() && *pred != self.entry)
                    {
                        continue;
                    }
                    new_idom = Some(match new_idom {
                        None => *pred,
                        Some(existing) => self.intersect(existing, *pred, &idoms, &po_number),
                    });
                }
                if idom_of(&idoms, *block) != new_idom.map(|b| po_number[b.0 as usize]) {
                    idoms[block.0 as usize] = new_idom;
                    changed = true;
                }
            }
        }
        idoms
    }

    fn postorder(&self, block: BlockId, visited: &mut [bool], out: &mut Vec<BlockId>) {
        visited[block.0 as usize] = true;
        for succ in &self.succs[block.0 as usize] {
            if !visited[succ.0 as usize] {
                self.postorder(*succ, visited, out);
            }
        }
        out.push(block);
    }

    /// The closest common dominator of `a` and `b`, walking the
    /// half-finished idom tree by postorder number.
    fn intersect(
        &self,
        mut a: BlockId,
        mut b: BlockId,
        idoms: &[Option<BlockId>],
        po_number: &[usize],
    ) -> BlockId {
        while a != b {
            while po_number[a.0 as usize] > po_number[b.0 as usize] {
                a = idoms[a.0 as usize].expect("intersect before idom assigned");
            }
            while po_number[b.0 as usize] > po_number[a.0 as usize] {
                b = idoms[b.0 as usize].expect("intersect before idom assigned");
            }
        }
        a
    }

    /// The dominance frontier of every block: the blocks dominated by
    /// the block that also have a predecessor it does not dominate.
    pub fn dominance_frontiers(&self) -> Vec<Vec<BlockId>> {
        let idoms = self.idoms();
        let mut frontiers: Vec<BTreeSet<u32>> = vec![BTreeSet::new(); self.succs.len()];
        for (index, block) in self.preds.iter().enumerate() {
            let target = BlockId(index as u32);
            if !self.reachable[index] || block.len() < 2 {
                continue;
            }
            for pred in block {
                if !self.reachable[pred.0 as usize] {
                    continue;
                }
                let mut runner = *pred;
                while runner != idoms[index].unwrap_or(self.entry) && runner != self.entry {
                    frontiers[runner.0 as usize].insert(target.0);
                    match idoms[runner.0 as usize] {
                        Some(up) => runner = up,
                        None => break,
                    }
                }
            }
        }
        frontiers
            .into_iter()
            .map(|set| set.into_iter().map(BlockId).collect())
            .collect()
    }

    /// The natural loops from back edges (an edge to a dominating
    /// header), sorted by header index.
    pub fn loops(&self) -> Vec<Loop> {
        let idoms = self.idoms();
        let dominates = |dominator: BlockId, block: BlockId| {
            let mut walker = block;
            loop {
                if walker == dominator {
                    return true;
                }
                match idoms[walker.0 as usize] {
                    Some(up) => walker = up,
                    None => return false,
                }
            }
        };
        let mut headers: BTreeSet<BlockId> = BTreeSet::new();
        for (index, preds) in self.preds.iter().enumerate() {
            let target = BlockId(index as u32);
            if !self.reachable[index] {
                continue;
            }
            for pred in preds {
                if self.reachable[pred.0 as usize] && dominates(target, *pred) {
                    headers.insert(target);
                }
            }
        }
        headers
            .into_iter()
            .map(|header| {
                // The classic natural-loop walk: start from the back
                // edges into the header, then everything that reaches
                // them without passing the header.
                let mut body: BTreeSet<u32> = BTreeSet::from([header.0]);
                let mut stack: Vec<BlockId> = Vec::new();
                for pred in &self.preds[header.0 as usize] {
                    if self.reachable[pred.0 as usize]
                        && dominates(header, *pred)
                        && body.insert(pred.0)
                    {
                        stack.push(*pred);
                    }
                }
                while let Some(block) = stack.pop() {
                    for pred in &self.preds[block.0 as usize] {
                        if self.reachable[pred.0 as usize] && body.insert(pred.0) {
                            stack.push(*pred);
                        }
                    }
                }
                Loop {
                    header,
                    body: body.into_iter().map(BlockId).collect(),
                }
            })
            .collect()
    }

    /// The innermost loop containing each block, keyed by header
    /// (`None` outside every loop).
    pub fn loop_forest(&self) -> Vec<Option<BlockId>> {
        let loops = self.loops();
        let mut forest = vec![None; self.succs.len()];
        // Walk bodies largest-first so the innermost loop wins.
        let mut ordered: Vec<&Loop> = loops.iter().collect();
        ordered.sort_by_key(|loop_| std::cmp::Reverse(loop_.body.len()));
        for loop_ in ordered {
            for block in &loop_.body {
                forest[block.0 as usize] = Some(loop_.header);
            }
        }
        forest
    }
}

/// Renders the dominator tree, frontiers, and loop forest as text.
pub fn dump_text(cfg: &Cfg) -> String {
    let idoms = cfg.idoms();
    let frontiers = cfg.dominance_frontiers();
    let loops = cfg.loops();
    let forest = cfg.loop_forest();
    let mut out = String::new();
    let _ = writeln!(out, "entry: b{}", cfg.entry.0);
    let _ = writeln!(out, "dominator tree:");
    for (index, idom) in idoms.iter().enumerate() {
        if !cfg.is_reachable(BlockId(index as u32)) {
            let _ = writeln!(out, "  b{index}: unreachable");
        } else {
            let name = idom.map_or("entry".to_string(), |b| format!("b{}", b.0));
            let _ = writeln!(out, "  b{index}: idom {name}");
        }
    }
    let _ = writeln!(out, "dominance frontiers:");
    for (index, frontier) in frontiers.iter().enumerate() {
        let list = frontier
            .iter()
            .map(|b| format!("b{}", b.0))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(out, "  b{index}: [{list}]");
    }
    let _ = writeln!(out, "loops:");
    for loop_ in &loops {
        let body = loop_
            .body
            .iter()
            .map(|b| format!("b{}", b.0))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(out, "  header b{}: [{body}]", loop_.header.0);
    }
    let _ = writeln!(out, "innermost loops:");
    for (index, header) in forest.iter().enumerate() {
        let name = header.map_or("-".to_string(), |b| format!("b{}", b.0));
        let _ = writeln!(out, "  b{index}: {name}");
    }
    out
}

/// Renders the CFG as a dot graph (dominance edges omitted).
pub fn dump_dot(cfg: &Cfg) -> String {
    let mut out = String::from("digraph cfg {\n");
    let _ = writeln!(out, "  entry [shape=point];");
    let _ = writeln!(out, "  entry -> b{};", cfg.entry.0);
    for (index, succs) in cfg.succs.iter().enumerate() {
        for succ in succs {
            let _ = writeln!(out, "  b{index} -> b{};", succ.0);
        }
    }
    out.push_str("}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Block, BlockId, Value};

    /// A block with no instructions and a plain branch.
    fn branch(addr: u32, targets: &[BlockId]) -> Block {
        let term = match targets {
            [] => Terminator::Unreachable,
            [one] => Terminator::Branch {
                target: *one,
                args: Vec::new(),
            },
            [then, otherwise] => Terminator::BranchCond {
                cond: crate::Condition::Eq,
                then_target: *then,
                then_args: Vec::new(),
                else_target: *otherwise,
                else_args: Vec::new(),
            },
            _ => panic!("at most two targets"),
        };
        Block {
            addr,
            params: Vec::new(),
            insts: Vec::new(),
            term,
        }
    }

    fn ret(addr: u32) -> Block {
        Block {
            addr,
            params: Vec::new(),
            insts: Vec::new(),
            term: Terminator::Return {
                state: Vec::<Value>::new(),
                switches_state: false,
            },
        }
    }

    fn function(blocks: Vec<Block>) -> Function {
        Function {
            blocks,
            name: String::new(),
        }
    }

    #[test]
    fn straight_line_dominators() {
        let f = function(vec![
            branch(0, &[BlockId(1)]),
            branch(1, &[BlockId(2)]),
            ret(2),
        ]);
        let cfg = Cfg::build(&f);
        let idoms = cfg.idoms();
        assert_eq!(idoms[1], Some(BlockId(0)));
        assert_eq!(idoms[2], Some(BlockId(1)));
        assert_eq!(cfg.idoms()[0], None);
        assert!(cfg.loops().is_empty());
        let frontiers = cfg.dominance_frontiers();
        assert!(frontiers.iter().all(|f| f.is_empty()));
    }

    #[test]
    fn diamond_join_is_dominated_by_the_fork() {
        let f = function(vec![
            branch(0, &[BlockId(1), BlockId(2)]),
            branch(1, &[BlockId(3)]),
            branch(2, &[BlockId(3)]),
            ret(3),
        ]);
        let cfg = Cfg::build(&f);
        let idoms = cfg.idoms();
        assert_eq!(idoms[3], Some(BlockId(0)));
        let frontiers = cfg.dominance_frontiers();
        assert_eq!(frontiers[1], vec![BlockId(3)]);
        assert_eq!(frontiers[2], vec![BlockId(3)]);
        assert!(frontiers[0].is_empty());
        assert!(frontiers[3].is_empty());
    }

    #[test]
    fn while_loop_has_header_body_and_frontier() {
        // 0 -> 1 -> 2; 2 -> 1 (back edge), 2 -> 3.
        let f = function(vec![
            branch(0, &[BlockId(1)]),
            branch(1, &[BlockId(2)]),
            branch(2, &[BlockId(1), BlockId(3)]),
            ret(3),
        ]);
        let cfg = Cfg::build(&f);
        let idoms = cfg.idoms();
        assert_eq!(idoms[1], Some(BlockId(0)));
        assert_eq!(idoms[2], Some(BlockId(1)));
        assert_eq!(idoms[3], Some(BlockId(2)));
        let loops = cfg.loops();
        assert_eq!(loops.len(), 1);
        assert_eq!(loops[0].header, BlockId(1));
        assert_eq!(loops[0].body, vec![BlockId(1), BlockId(2)]);
        let forest = cfg.loop_forest();
        assert_eq!(forest[1], Some(BlockId(1)));
        assert_eq!(forest[2], Some(BlockId(1)));
        assert_eq!(forest[3], None);
        let frontiers = cfg.dominance_frontiers();
        assert_eq!(frontiers[2], vec![BlockId(1)]);
        assert_eq!(frontiers[1], vec![BlockId(1)]);
    }

    #[test]
    fn nested_loops_form_a_forest() {
        // 0 -> 1 -> 2 -> 3; 3 -> (2 | 5); 5 -> (1 | 4) with 4 the
        // shared exit: two back edges into dominating headers.
        let f = function(vec![
            branch(0, &[BlockId(1)]),
            branch(1, &[BlockId(2)]),
            branch(2, &[BlockId(3)]),
            branch(3, &[BlockId(2), BlockId(5)]),
            ret(4),
            branch(5, &[BlockId(1), BlockId(4)]),
        ]);
        let cfg = Cfg::build(&f);
        let loops = cfg.loops();
        assert_eq!(loops.len(), 2);
        assert_eq!(loops[0].header, BlockId(1));
        assert_eq!(
            loops[0].body,
            vec![BlockId(1), BlockId(2), BlockId(3), BlockId(5)]
        );
        assert_eq!(loops[1].header, BlockId(2));
        assert_eq!(loops[1].body, vec![BlockId(2), BlockId(3)]);
        let forest = cfg.loop_forest();
        assert_eq!(forest[1], Some(BlockId(1)));
        assert_eq!(forest[2], Some(BlockId(2)));
        assert_eq!(forest[3], Some(BlockId(2)));
        assert_eq!(forest[5], Some(BlockId(1)));
        assert_eq!(forest[4], None);
    }

    #[test]
    fn unreachable_blocks_do_not_join_dominance() {
        // Block 1 is never reachable; block 2 is a second join point.
        let f = function(vec![
            branch(0, &[BlockId(2)]),
            ret(1),
            branch(2, &[BlockId(3), BlockId(4)]),
            ret(3),
            ret(4),
        ]);
        let cfg = Cfg::build(&f);
        assert!(!cfg.is_reachable(BlockId(1)));
        let idoms = cfg.idoms();
        assert_eq!(idoms[1], None);
        assert_eq!(idoms[3], Some(BlockId(2)));
        assert_eq!(idoms[4], Some(BlockId(2)));
        let frontiers = cfg.dominance_frontiers();
        assert!(frontiers[1].is_empty());
    }

    #[test]
    fn call_with_cont_is_an_edge() {
        let f = function(vec![
            Block {
                addr: 0,
                params: Vec::new(),
                insts: Vec::new(),
                term: Terminator::Call {
                    target: Value::Const(0x100),
                    cont: Some((BlockId(1), Vec::new())),
                    state: Vec::new(),
                },
            },
            ret(1),
        ]);
        let cfg = Cfg::build(&f);
        assert_eq!(cfg.succs[0], vec![BlockId(1)]);
        assert_eq!(cfg.idoms()[1], Some(BlockId(0)));
    }

    #[test]
    fn text_dump_names_entry_and_loops() {
        // 0 -> 1 -> 2; 2 -> 1, 2 -> 3.
        let f = function(vec![
            branch(0, &[BlockId(1)]),
            branch(1, &[BlockId(2)]),
            branch(2, &[BlockId(1), BlockId(3)]),
            ret(3),
        ]);
        let cfg = Cfg::build(&f);
        let text = dump_text(&cfg);
        assert!(text.contains("entry: b0"));
        assert!(text.contains("header b1: [b1, b2]"));
        let dot = dump_dot(&cfg);
        assert!(dot.contains("digraph cfg"));
        assert!(dot.contains("b2 -> b1;"));
    }
}
