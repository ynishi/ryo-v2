//! The search over refactorings: a tree of states, the static metrics that rank them before any
//! check, the pruning that decides which ones are checked, and the final ranking by reward.
//!
//! Pure: no file is read and no clock is asked. The tree merges nodes that reach the same
//! files, so a state is explored once however many paths lead to it.
//!
//! # Contract
//! - C1 [invariant]: no two nodes of a [`SearchTree`] have the same [`StateFp`], and node 0 is
//!   the root: depth 0, no blueprint, no parent.
//!   tested: domain::search::tests::same_state_fp_is_one_node

use super::judge::{Noul, Prior, ProposalId};
use super::{Blueprint, DecisionBudget, Edit, FileRole, Fingerprint, Ident, RelPath, TextRange};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU16;

/// A node of a [`SearchTree`], by position in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(u32);

impl NodeId {
    /// The position in the tree: 0 is the root.
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "n{}", self.0)
    }
}

/// The state of the files a path of edits leads to, as a fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StateFp(Fingerprint);

impl StateFp {
    /// The state `edits` lead to.
    ///
    /// # Contract
    /// - C1: the edits are taken by path, the last edit of a path wins and the order of the
    ///   input does not matter; `of(&[])` is the state of the unchanged base.
    ///   tested: domain::search::tests::state_fp_ignores_edit_order
    pub fn of(edits: &[Edit]) -> StateFp {
        let mut last: BTreeMap<&RelPath, &Edit> = BTreeMap::new();
        for e in edits {
            last.insert(&e.path, e);
        }
        let mut bytes = Vec::new();
        for (path, e) in last {
            bytes.extend_from_slice(path.as_str().as_bytes());
            bytes.push(0x00);
            match &e.new_text {
                Some(text) => {
                    bytes.push(0x01);
                    bytes
                        .extend_from_slice(&Fingerprint::of(text.as_bytes()).value().to_le_bytes());
                }
                None => bytes.push(0x02),
            }
        }
        StateFp(Fingerprint::of(&bytes))
    }

    /// The fingerprint behind the state.
    pub fn fingerprint(self) -> Fingerprint {
        self.0
    }
}

/// Puts the edits of a child on top of the edits of its parent.
///
/// # Contract
/// - C1: for each path the child's edit replaces the parent's; a write whose text equals
///   what `base` gives for the path is dropped, so a file back at its base text leaves the
///   state; the result is sorted by path.
///   tested: domain::search::tests::compose_drops_writes_equal_to_base
pub fn compose(
    parent: &[Edit],
    child: &[Edit],
    base: &dyn Fn(&RelPath) -> Option<String>,
) -> Vec<Edit> {
    let mut by_path: BTreeMap<RelPath, Option<String>> = BTreeMap::new();
    for e in parent.iter().chain(child) {
        by_path.insert(e.path.clone(), e.new_text.clone());
    }
    by_path
        .into_iter()
        .filter(|(path, text)| text.is_none() || *text != base(path))
        .map(|(path, new_text)| Edit { path, new_text })
        .collect()
}

/// Where a change is wanted: a file and, if known, a range of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    path: RelPath,
    range: Option<TextRange>,
}

impl Target {
    /// A target in `path`, at `range` if given.
    pub fn new(path: RelPath, range: Option<TextRange>) -> Target {
        Target { path, range }
    }

    /// The file.
    pub fn path(&self) -> &RelPath {
        &self.path
    }

    /// The range in the file, if known.
    pub fn range(&self) -> Option<TextRange> {
        self.range
    }
}

/// A change a later edit might make, tried on a candidate to see how easily it extends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeProbe {
    /// Add a variant to the enum `ty`.
    AddVariant {
        /// The enum.
        ty: Ident,
    },
    /// Add a field to the struct `ty`.
    AddField {
        /// The struct.
        ty: Ident,
    },
    /// Add a method to the type `ty`.
    AddMethod {
        /// The type.
        ty: Ident,
    },
}

/// The change in how many errors a clean-code check reports. Higher is better.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CleanDelta(pub i32);

/// The change in how many probes still apply cleanly. Higher is better.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExtDelta(pub i32);

/// What is known of a candidate's edits without checking them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct StaticMetrics {
    /// Files the edits touch.
    pub touched_files: u16,
    /// Crates the edits touch.
    pub touched_crates: u8,
    /// Bytes the edits write.
    pub edit_bytes: u32,
    /// The change in clean-code errors.
    pub clean: CleanDelta,
    /// The change in extensibility.
    pub extensibility: ExtDelta,
}

/// What a checked path of candidates earned, summed over the path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reward {
    /// Whether the last check passed.
    pub passed: bool,
    /// The summed clean-code change.
    pub clean: i32,
    /// The summed extensibility change.
    pub extensibility: i32,
    /// The summed bytes written.
    pub edit_bytes: u32,
}

impl Reward {
    /// The reward of the path whose steps have `path` metrics.
    ///
    /// # Contract
    /// - C1: `clean`, `extensibility` and `edit_bytes` are the sums over `path`, saturating
    ///   instead of overflowing; `passed` is as given.
    ///   tested: domain::search::tests::reward_sums_the_path
    pub fn along(path: &[StaticMetrics], passed: bool) -> Reward {
        path.iter().fold(
            Reward {
                passed,
                clean: 0,
                extensibility: 0,
                edit_bytes: 0,
            },
            |acc, m| Reward {
                passed,
                clean: acc.clean.saturating_add(m.clean.0),
                extensibility: acc.extensibility.saturating_add(m.extensibility.0),
                edit_bytes: acc.edit_bytes.saturating_add(m.edit_bytes),
            },
        )
    }
}

/// How much each metric counts in a score.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Weights {
    /// Per point of clean-code change.
    pub clean: i64,
    /// Per point of extensibility change.
    pub extensibility: i64,
    /// Subtracted per 100 bytes written.
    pub per_100_bytes: i64,
    /// Subtracted per file touched.
    pub per_file: i64,
    /// Added per 100 thousandths of prior.
    pub prior_per_100: i64,
}

/// The weights used when none are given.
pub const DEFAULT_WEIGHTS: Weights = Weights {
    clean: 10,
    extensibility: 10,
    per_100_bytes: 1,
    per_file: 2,
    prior_per_100: 1,
};

fn weighted(clean: i32, extensibility: i32, edit_bytes: u32, w: &Weights) -> i64 {
    i64::from(clean)
        .saturating_mul(w.clean)
        .saturating_add(i64::from(extensibility).saturating_mul(w.extensibility))
        .saturating_sub(i64::from(edit_bytes / 100).saturating_mul(w.per_100_bytes))
}

/// How good a candidate looks before it is checked. Higher is better.
///
/// # Contract
/// - C1: the sum of `clean` times its weight and `extensibility` times its weight, less the
///   hundreds of `edit_bytes` times their weight and the touched files times theirs, plus the
///   hundreds of thousandths of `prior` times their weight; integer arithmetic throughout.
///   tested: domain::search::tests::score_weighs_metrics_and_prior
pub fn score(m: &StaticMetrics, prior: Prior, w: &Weights) -> i64 {
    weighted(m.clean.0, m.extensibility.0, m.edit_bytes, w)
        .saturating_sub(i64::from(m.touched_files).saturating_mul(w.per_file))
        .saturating_add(i64::from(prior.permille() / 100).saturating_mul(w.prior_per_100))
}

/// How good a checked path was. Higher is better.
///
/// # Contract
/// - C1: summed `clean` and `extensibility` points at their weights, less the hundreds of
///   summed `edit_bytes` at theirs; no prior and no file count enter it.
///   tested: domain::search::tests::score_weighs_metrics_and_prior
pub fn reward_score(r: &Reward, w: &Weights) -> i64 {
    weighted(r.clean, r.extensibility, r.edit_bytes, w)
}

/// How much search is allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchBudget {
    /// Candidates kept per depth.
    pub beam: u16,
    /// Candidates generated per depth at most.
    pub max_expand: u16,
    /// Nodes expanded per depth at most.
    pub beam_parents: u8,
    /// Depth.
    pub max_depth: u8,
    /// Milliseconds per candidate check.
    pub per_candidate_ms: u64,
    /// Milliseconds after which no new depth starts.
    pub soft_ms: u64,
    /// Milliseconds after which the search stops.
    pub hard_ms: u64,
    /// Every this many candidates dropped for a Noul is checked anyway.
    pub audit_every: NonZeroU16,
}

impl SearchBudget {
    /// The part of the budget a decision records.
    ///
    /// # Contract
    /// - C1: `beam` becomes `candidates_per_depth`; `max_depth` and `per_candidate_ms` are
    ///   copied.
    ///   tested: domain::search::tests::default_budget_matches_decision_budget
    pub fn decision_budget(&self) -> DecisionBudget {
        DecisionBudget {
            candidates_per_depth: u32::from(self.beam),
            max_depth: u32::from(self.max_depth),
            per_candidate_ms: self.per_candidate_ms,
        }
    }
}

/// A candidate with everything pruning looks at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scored {
    /// The proposal.
    pub proposal: ProposalId,
    /// The state its edits lead to.
    pub state: StateFp,
    /// Its static metrics.
    pub metrics: StaticMetrics,
    /// The judge's prior.
    pub prior: Prior,
    /// The flaw the judge found, if any.
    pub noul: Option<Noul>,
}

/// Why pruning dropped a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PruneStage {
    /// The judge found a flaw.
    Noul,
    /// The state was seen before.
    Duplicate,
    /// Another candidate is at least as good everywhere and better somewhere, and no room was left.
    Dominated,
    /// Not among the best `keep` by score.
    Rank,
}

impl std::fmt::Display for PruneStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PruneStage::Noul => "noul",
            PruneStage::Duplicate => "duplicate",
            PruneStage::Dominated => "dominated",
            PruneStage::Rank => "rank",
        })
    }
}

/// The result of pruning: indices into the candidates given.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Pruned {
    /// The candidates to check, best first.
    pub kept: Vec<usize>,
    /// Candidates dropped for a Noul that are checked anyway, as an audit of the judge.
    pub audit: Vec<usize>,
    /// The candidates dropped and why, by index.
    pub dropped: Vec<(usize, PruneStage)>,
}

/// Whether `b` is at least as good as `a` on every static metric and better on one. The prior
/// never enters it.
fn dominates(b: &StaticMetrics, a: &StaticMetrics) -> bool {
    b.clean >= a.clean
        && b.extensibility >= a.extensibility
        && b.edit_bytes <= a.edit_bytes
        && b.touched_files <= a.touched_files
        && (b.clean > a.clean
            || b.extensibility > a.extensibility
            || b.edit_bytes < a.edit_bytes
            || b.touched_files < a.touched_files)
}

/// Chooses which candidates are checked.
///
/// # Contract
/// - C1: a candidate with a Noul is dropped as `Noul`; of those, the 0-based k-th in input
///   order is also listed in `audit` when `k % audit_every == 0`. Then a state in `seen`, or
///   the same as that of an earlier surviving candidate, is dropped as `Duplicate`.
///   tested: domain::search::tests::prune_drops_noul_then_duplicates_then_ranks_dominated_last
///   tested: domain::search::tests::prune_audits_every_nth_noul
/// - C2: the rest are ordered by (dominated by another of the rest, last), then score
///   descending, then input index; the first `keep` are `kept` in that order and the others
///   are dropped as `Dominated` if dominated, else as `Rank`. The prior never decides
///   dominance.
///   tested: domain::search::tests::prune_drops_noul_then_duplicates_then_ranks_dominated_last
///   tested: domain::search::tests::prune_keeps_top_k_by_score
/// - C3: every input index is in exactly one of `kept` and `dropped`, and `dropped` is in input
///   order.
///   tested: domain::search::tests::prune_accounts_for_every_candidate
pub fn prune(
    cands: &[Scored],
    seen: &BTreeSet<StateFp>,
    keep: usize,
    audit_every: NonZeroU16,
    w: &Weights,
) -> Pruned {
    let mut out = Pruned::default();
    let mut noul_count = 0usize;
    let mut taken: BTreeSet<StateFp> = BTreeSet::new();
    let mut rest: Vec<usize> = Vec::new();
    for (i, c) in cands.iter().enumerate() {
        if c.noul.is_some() {
            out.dropped.push((i, PruneStage::Noul));
            if noul_count.is_multiple_of(usize::from(audit_every.get())) {
                out.audit.push(i);
            }
            noul_count += 1;
        } else if seen.contains(&c.state) || !taken.insert(c.state) {
            out.dropped.push((i, PruneStage::Duplicate));
        } else {
            rest.push(i);
        }
    }
    let dominated: BTreeSet<usize> = rest
        .iter()
        .copied()
        .filter(|&a| {
            rest.iter()
                .any(|&b| b != a && dominates(&cands[b].metrics, &cands[a].metrics))
        })
        .collect();
    rest.sort_by_key(|&i| {
        (
            dominated.contains(&i),
            Reverse(score(&cands[i].metrics, cands[i].prior, w)),
            i,
        )
    });
    for (rank, i) in rest.into_iter().enumerate() {
        if rank < keep {
            out.kept.push(i);
        } else if dominated.contains(&i) {
            out.dropped.push((i, PruneStage::Dominated));
        } else {
            out.dropped.push((i, PruneStage::Rank));
        }
    }
    out.dropped.sort_by_key(|(i, _)| *i);
    out
}

/// Orders the verified nodes of a finished search, best first.
///
/// # Contract
/// - C1: only nodes whose reward `passed` are listed, by `reward_score` descending, then
///   depth ascending, then node index ascending; the prior takes no part.
///   tested: domain::search::tests::rank_final_orders_by_reward_then_depth
pub fn rank_final(verified: &[(NodeId, u8, Reward)], w: &Weights) -> Vec<NodeId> {
    let mut passed: Vec<&(NodeId, u8, Reward)> =
        verified.iter().filter(|(_, _, r)| r.passed).collect();
    passed.sort_by_key(|(id, depth, r)| (Reverse(reward_score(r, w)), *depth, id.index()));
    passed.into_iter().map(|(id, _, _)| *id).collect()
}

/// One state reached in the search, and the path of edits that reached it first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchNode {
    id: NodeId,
    parent: Option<NodeId>,
    blueprint: Option<Blueprint>,
    depth: u8,
    state: StateFp,
    edits: Vec<Edit>,
    roles: BTreeSet<FileRole>,
    metrics: StaticMetrics,
    prior: Prior,
    reward: Option<Reward>,
}

impl SearchNode {
    /// The id.
    pub fn id(&self) -> NodeId {
        self.id
    }

    /// The node it was expanded from; `None` for the root.
    pub fn parent(&self) -> Option<NodeId> {
        self.parent
    }

    /// The Blueprint that led here; `None` for the root.
    pub fn blueprint(&self) -> Option<&Blueprint> {
        self.blueprint.as_ref()
    }

    /// Steps from the root.
    pub fn depth(&self) -> u8 {
        self.depth
    }

    /// The state it stands for.
    pub fn state(&self) -> StateFp {
        self.state
    }

    /// The whole-file edits from the base to this state.
    pub fn edits(&self) -> &[Edit] {
        &self.edits
    }

    /// The roles of every file edited along the way.
    pub fn roles(&self) -> &BTreeSet<FileRole> {
        &self.roles
    }

    /// The static metrics of the step into this node.
    pub fn metrics(&self) -> &StaticMetrics {
        &self.metrics
    }

    /// The judge's prior for the step into this node.
    pub fn prior(&self) -> Prior {
        self.prior
    }

    /// The reward, once the node has been checked.
    pub fn reward(&self) -> Option<&Reward> {
        self.reward.as_ref()
    }
}

/// What a new node is made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewNode {
    /// The node it is expanded from.
    pub parent: NodeId,
    /// The Blueprint of the step.
    pub blueprint: Blueprint,
    /// The whole-file edits from the base to the new state.
    pub edits: Vec<Edit>,
    /// The roles of every file edited along the way.
    pub roles: BTreeSet<FileRole>,
    /// The static metrics of the step.
    pub metrics: StaticMetrics,
    /// The judge's prior for the step.
    pub prior: Prior,
}

/// What inserting a node did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inserted {
    /// A new node was added.
    Added(NodeId),
    /// The state was already in the tree; nothing was added.
    Merged(NodeId),
}

/// The states reached so far, each once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchTree {
    nodes: Vec<SearchNode>,
    by_state: BTreeMap<StateFp, NodeId>,
}

impl SearchTree {
    /// A tree whose root stands for the state `start` leads to.
    ///
    /// # Contract
    /// - C1: the root is node 0 at depth 0, with no parent and no blueprint, `edits` are
    ///   `start` and the state is that of `start`; it has no reward.
    ///   tested: domain::search::tests::same_state_fp_is_one_node
    pub fn new(start: Vec<Edit>) -> SearchTree {
        let state = StateFp::of(&start);
        let root = NodeId(0);
        let node = SearchNode {
            id: root,
            parent: None,
            blueprint: None,
            depth: 0,
            state,
            edits: start,
            roles: BTreeSet::new(),
            metrics: StaticMetrics::default(),
            prior: Prior::NEUTRAL,
            reward: None,
        };
        SearchTree {
            nodes: vec![node],
            by_state: BTreeMap::from([(state, root)]),
        }
    }

    /// The root.
    pub fn root(&self) -> NodeId {
        NodeId(0)
    }

    /// The node `id`, or `None` for an id that is not in this tree.
    pub fn node(&self, id: NodeId) -> Option<&SearchNode> {
        self.nodes.get(id.index())
    }

    /// Adds a node unless its state is already in the tree.
    ///
    /// # Contract
    /// - C1: the new node's depth is its parent's plus one; a state already in the tree gives
    ///   `Merged` with the node that has it and adds nothing, otherwise `Added`; a parent that
    ///   is not in this tree is taken to be the root.
    ///   tested: domain::search::tests::same_state_fp_is_one_node
    pub fn insert(&mut self, n: NewNode) -> Inserted {
        let state = StateFp::of(&n.edits);
        if let Some(&existing) = self.by_state.get(&state) {
            return Inserted::Merged(existing);
        }
        let parent = if self.node(n.parent).is_some() {
            n.parent
        } else {
            self.root()
        };
        let depth = self.nodes[parent.index()].depth.saturating_add(1);
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(SearchNode {
            id,
            parent: Some(parent),
            blueprint: Some(n.blueprint),
            depth,
            state,
            edits: n.edits,
            roles: n.roles,
            metrics: n.metrics,
            prior: n.prior,
            reward: None,
        });
        self.by_state.insert(state, id);
        Inserted::Added(id)
    }

    /// Records the reward of `id`. An id that is not in this tree is ignored.
    pub fn set_reward(&mut self, id: NodeId, r: Reward) {
        if let Some(node) = self.nodes.get_mut(id.index()) {
            node.reward = Some(r);
        }
    }

    /// The nodes from the root to `id`; empty for an id that is not in this tree.
    pub fn path(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut at = self.node(id);
        while let Some(node) = at {
            out.push(node.id);
            at = node.parent.and_then(|p| self.node(p));
        }
        out.reverse();
        out
    }

    /// Whether some node has state `s`.
    pub fn has_state(&self, s: StateFp) -> bool {
        self.by_state.contains_key(&s)
    }

    /// The states of all nodes.
    pub fn states(&self) -> BTreeSet<StateFp> {
        self.by_state.keys().copied().collect()
    }

    /// How many nodes there are, the root included.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether there are no nodes. Never true: the root is always there.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        BaseFp, BlueprintId, BlueprintParts, Command, DEFAULT_BUDGET, DecisionRecord, ReadRange,
        Splice,
    };

    fn rel(s: &str) -> RelPath {
        RelPath::parse(s).unwrap()
    }
    fn tr(a: u32, b: u32) -> TextRange {
        TextRange::new(a, b).unwrap()
    }
    fn w(path: &str, text: &str) -> Edit {
        Edit::write(rel(path), text)
    }
    fn bp() -> Blueprint {
        let read = ReadRange::capture(rel("a.rs"), "let a = a;", tr(0, 10)).unwrap();
        Blueprint::new(BlueprintParts {
            id: BlueprintId::parse("01890a5d-ac96-774b-bcce-b302099a8057").unwrap(),
            command: Command::RenameLocal {
                from: Ident::parse("a").unwrap(),
                to: Ident::parse("b").unwrap(),
            },
            splices: vec![Splice::replace(rel("a.rs"), tr(4, 5), "b").unwrap()],
            reads: vec![read],
            bases: vec![BaseFp::capture(rel("a.rs"), "let a = a;")],
            spec_refs: vec![],
            record: DecisionRecord::new("agent", "because").unwrap(),
            builds_on: None,
        })
        .unwrap()
    }
    fn new_node(parent: NodeId, edits: Vec<Edit>) -> NewNode {
        NewNode {
            parent,
            blueprint: bp(),
            edits,
            roles: BTreeSet::new(),
            metrics: StaticMetrics::default(),
            prior: Prior::NEUTRAL,
        }
    }
    fn added(i: Inserted) -> NodeId {
        match i {
            Inserted::Added(id) => id,
            Inserted::Merged(id) => panic!("merged into {id}"),
        }
    }
    fn st(n: u8) -> StateFp {
        StateFp::of(&[w("a.rs", &n.to_string())])
    }
    fn pid(n: u8) -> ProposalId {
        ProposalId::parse(&Fingerprint::of(&[n]).to_string()).unwrap()
    }
    fn metrics(clean: i32, ext: i32, bytes: u32, files: u16) -> StaticMetrics {
        StaticMetrics {
            touched_files: files,
            touched_crates: 1,
            edit_bytes: bytes,
            clean: CleanDelta(clean),
            extensibility: ExtDelta(ext),
        }
    }
    fn cand(state: u8, m: StaticMetrics, prior: u16, noul: bool) -> Scored {
        Scored {
            proposal: pid(state),
            state: st(state),
            metrics: m,
            prior: Prior::new(prior).unwrap(),
            noul: noul.then(|| {
                Noul::new(
                    crate::domain::judge::Criterion::NamingMismatch,
                    rel("a.rs"),
                    tr(0, 1),
                )
            }),
        }
    }
    fn every(n: u16) -> NonZeroU16 {
        NonZeroU16::new(n).unwrap()
    }

    #[test]
    fn state_fp_ignores_edit_order() {
        let a = w("a.rs", "one");
        let b = w("b.rs", "two");
        assert_eq!(
            StateFp::of(&[a.clone(), b.clone()]),
            StateFp::of(&[b.clone(), a.clone()])
        );
        // the last edit of a path wins
        assert_eq!(
            StateFp::of(&[w("a.rs", "old"), b.clone(), a.clone()]),
            StateFp::of(&[a.clone(), b.clone()])
        );
        let alone = StateFp::of(std::slice::from_ref(&a));
        assert_ne!(alone, StateFp::of(&[w("a.rs", "uno")]));
        assert_ne!(alone, StateFp::of(&[w("c.rs", "one")]));
        // a delete is a state of its own, apart from an empty file and from the base
        let del = StateFp::of(&[Edit::delete(rel("a.rs"))]);
        assert_ne!(del, StateFp::of(&[w("a.rs", "")]));
        assert_ne!(del, StateFp::of(&[]));
        assert_eq!(StateFp::of(&[]).fingerprint(), Fingerprint::of(&[]));
    }

    #[test]
    fn compose_drops_writes_equal_to_base() {
        let base = |p: &RelPath| match p.as_str() {
            "a.rs" => Some("base a".to_owned()),
            "b.rs" => Some("base b".to_owned()),
            _ => None,
        };
        // the child overrides the parent per path, and the result is sorted by path
        let out = compose(
            &[w("b.rs", "p b"), w("a.rs", "p a")],
            &[w("a.rs", "c a"), w("c.rs", "c c")],
            &base,
        );
        assert_eq!(
            out,
            vec![w("a.rs", "c a"), w("b.rs", "p b"), w("c.rs", "c c")]
        );
        // a write back to the base text leaves the state
        let out = compose(&[w("a.rs", "p a")], &[w("a.rs", "base a")], &base);
        assert!(out.is_empty());
        assert_eq!(StateFp::of(&out), StateFp::of(&[]));
        // a delete stays, and so does a write of a file with no base
        let out = compose(&[], &[Edit::delete(rel("b.rs")), w("new.rs", "x")], &base);
        assert_eq!(out, vec![Edit::delete(rel("b.rs")), w("new.rs", "x")]);
        assert!(compose(&[], &[], &base).is_empty());
    }

    #[test]
    fn same_state_fp_is_one_node() {
        let mut t = SearchTree::new(vec![]);
        let root = t.root();
        assert_eq!((root.index(), root.to_string()), (0, "n0".to_owned()));
        let r = t.node(root).unwrap();
        assert_eq!((r.depth(), r.parent()), (0, None));
        assert!(r.blueprint().is_none() && r.reward().is_none());
        assert_eq!(r.state(), StateFp::of(&[]));
        assert_eq!(t.len(), 1);
        assert!(!t.is_empty());

        let a = added(t.insert(new_node(root, vec![w("a.rs", "x")])));
        let b = added(t.insert(new_node(a, vec![w("a.rs", "x"), w("b.rs", "y")])));
        assert_eq!((a.index(), b.index()), (1, 2));
        assert_eq!(t.node(b).unwrap().depth(), 2);
        assert_eq!(t.node(b).unwrap().parent(), Some(a));
        assert_eq!(t.node(b).unwrap().edits().len(), 2);
        assert!(t.node(b).unwrap().blueprint().is_some());
        // the same files reached by another path, in another order, are the same node
        assert_eq!(
            t.insert(new_node(root, vec![w("b.rs", "y"), w("a.rs", "x")])),
            Inserted::Merged(b)
        );
        assert_eq!(
            t.insert(new_node(b, vec![w("a.rs", "x")])),
            Inserted::Merged(a)
        );
        // back at the base is the root
        assert_eq!(t.insert(new_node(b, vec![])), Inserted::Merged(root));
        assert_eq!(t.len(), 3);
        assert!(t.has_state(StateFp::of(&[w("a.rs", "x")])));
        assert!(!t.has_state(StateFp::of(&[w("a.rs", "z")])));
        assert_eq!(t.states().len(), 3);
        // an id of another tree is not a node of this one, and a parent of another tree is the root
        let mut other = SearchTree::new(vec![]);
        let mut last = other.root();
        for i in 0..5u8 {
            last = added(other.insert(new_node(last, vec![w("z.rs", &i.to_string())])));
        }
        assert!(t.node(last).is_none());
        assert!(t.path(last).is_empty());
        t.set_reward(last, Reward::along(&[], true));
        let c = added(t.insert(new_node(last, vec![w("c.rs", "c")])));
        assert_eq!(
            (t.node(c).unwrap().parent(), t.node(c).unwrap().depth()),
            (Some(root), 1)
        );
    }

    #[test]
    fn search_tree_paths_and_rewards() {
        let mut t = SearchTree::new(vec![w("s.rs", "start")]);
        let root = t.root();
        let a = added(t.insert(new_node(root, vec![w("s.rs", "a")])));
        let b = added(t.insert(new_node(a, vec![w("s.rs", "b")])));
        assert_eq!(t.path(root), vec![root]);
        assert_eq!(t.path(b), vec![root, a, b]);
        let r = Reward::along(&[metrics(1, 2, 300, 1)], true);
        t.set_reward(b, r);
        assert_eq!(t.node(b).unwrap().reward(), Some(&r));
        assert!(t.node(a).unwrap().reward().is_none());
        let n = t.node(a).unwrap();
        assert_eq!(n.id(), a);
        assert_eq!(n.metrics(), &StaticMetrics::default());
        assert_eq!(n.prior(), Prior::NEUTRAL);
        assert!(n.roles().is_empty());
        assert_eq!(n.state(), StateFp::of(&[w("s.rs", "a")]));
    }

    #[test]
    fn reward_sums_the_path() {
        let path = [metrics(2, -1, 120, 1), metrics(-5, 4, 80, 2)];
        let r = Reward::along(&path, true);
        assert_eq!(
            r,
            Reward {
                passed: true,
                clean: -3,
                extensibility: 3,
                edit_bytes: 200
            }
        );
        assert!(!Reward::along(&path, false).passed);
        assert_eq!(
            Reward::along(&[], false),
            Reward {
                passed: false,
                clean: 0,
                extensibility: 0,
                edit_bytes: 0
            }
        );
        let big = metrics(i32::MAX, i32::MIN, u32::MAX, 1);
        let r = Reward::along(&[big, big], true);
        assert_eq!(
            (r.clean, r.extensibility, r.edit_bytes),
            (i32::MAX, i32::MIN, u32::MAX)
        );
    }

    #[test]
    fn score_weighs_metrics_and_prior() {
        let m = metrics(3, 2, 250, 2);
        // 3*10 + 2*10 - 2*1 - 2*2 + (700/100)*1
        assert_eq!(
            score(&m, Prior::new(700).unwrap(), &DEFAULT_WEIGHTS),
            30 + 20 - 2 - 4 + 7
        );
        assert_eq!(score(&m, Prior::new(799).unwrap(), &DEFAULT_WEIGHTS), 51);
        let r = Reward::along(&[m], true);
        // 3*10 + 2*10 - 2*1: no files, no prior
        assert_eq!(reward_score(&r, &DEFAULT_WEIGHTS), 48);
        let heavy = Weights {
            clean: 1,
            extensibility: 0,
            per_100_bytes: 0,
            per_file: 100,
            prior_per_100: 1000,
        };
        assert_eq!(score(&m, Prior::new(100).unwrap(), &heavy), 3 - 200 + 1000);
        assert_eq!(
            score(&StaticMetrics::default(), Prior::NEUTRAL, &DEFAULT_WEIGHTS),
            5
        );
        // weights that would overflow saturate instead
        let huge = Weights {
            clean: i64::MAX,
            ..DEFAULT_WEIGHTS
        };
        assert_eq!(
            reward_score(&Reward::along(&[metrics(9, 0, 0, 0)], true), &huge),
            i64::MAX
        );
    }

    #[test]
    fn prune_drops_noul_then_duplicates_then_ranks_dominated_last() {
        let cands = [
            cand(10, metrics(2, 0, 100, 1), 0, false),    // 0: good
            cand(11, metrics(9, 9, 0, 0), 1000, true),    // 1: flawed, however good
            cand(10, metrics(5, 5, 0, 0), 1000, false),   // 2: same state as 0
            cand(12, metrics(1, 1, 0, 0), 1000, false),   // 3: state already seen
            cand(13, metrics(1, 0, 200, 1), 1000, false), // 4: dominated by 0
            cand(14, metrics(0, 5, 100, 1), 500, false),  // 5: trades clean for extensibility
        ];
        let seen = BTreeSet::from([st(12)]);
        // the prior weighs so much that candidate 4 would win on score alone
        let heavy = Weights {
            prior_per_100: 100,
            ..DEFAULT_WEIGHTS
        };
        let p = prune(&cands, &seen, 10, every(1), &heavy);
        assert_eq!(p.kept, vec![5, 0, 4]);
        assert_eq!(
            p.dropped,
            vec![
                (1, PruneStage::Noul),
                (2, PruneStage::Duplicate),
                (3, PruneStage::Duplicate)
            ]
        );
        assert_eq!(p.audit, vec![1]);
        // with room for two, the dominated one is what goes, and it is called so
        let p = prune(&cands, &seen, 2, every(1), &heavy);
        assert_eq!(p.kept, vec![5, 0]);
        assert_eq!(p.dropped.last(), Some(&(4, PruneStage::Dominated)));
        // with room for one, the other good one is cut by rank
        let p = prune(&cands, &seen, 1, every(1), &heavy);
        assert_eq!(p.kept, vec![5]);
        assert_eq!(
            p.dropped,
            vec![
                (0, PruneStage::Rank),
                (1, PruneStage::Noul),
                (2, PruneStage::Duplicate),
                (3, PruneStage::Duplicate),
                (4, PruneStage::Dominated)
            ]
        );
        // equal metrics dominate nothing, and the prior alone never makes one candidate dominate
        let twins = [
            cand(1, metrics(1, 1, 100, 1), 0, false),
            cand(2, metrics(1, 1, 100, 1), 1000, false),
        ];
        let p = prune(&twins, &BTreeSet::new(), 1, every(1), &DEFAULT_WEIGHTS);
        assert_eq!(p.kept, vec![1]);
        assert_eq!(p.dropped, vec![(0, PruneStage::Rank)]);
        for (stage, name) in [
            (PruneStage::Noul, "noul"),
            (PruneStage::Duplicate, "duplicate"),
            (PruneStage::Dominated, "dominated"),
            (PruneStage::Rank, "rank"),
        ] {
            assert_eq!(stage.to_string(), name);
        }
    }

    #[test]
    fn prune_keeps_top_k_by_score() {
        // none dominates another: each is best at something
        let cands = [
            cand(1, metrics(3, 0, 0, 1), 500, false), // 30 - 2 + 5 = 33
            cand(2, metrics(0, 3, 0, 1), 500, false), // 33: ties with 0, the lower index first
            cand(3, metrics(1, 1, 0, 1), 500, false), // 23
            cand(4, metrics(0, 0, 0, 0), 500, false), // 5, but dominates nothing and is dominated by none
        ];
        let p = prune(&cands, &BTreeSet::new(), 2, every(5), &DEFAULT_WEIGHTS);
        assert_eq!(p.kept, vec![0, 1]);
        assert_eq!(
            p.dropped,
            vec![(2, PruneStage::Rank), (3, PruneStage::Rank)]
        );
        // the prior breaks a tie of metrics
        let same = metrics(1, 1, 100, 1);
        let cands = [
            cand(1, same, 300, false),
            cand(2, same, 900, false),
            cand(3, same, 600, false),
        ];
        let p = prune(&cands, &BTreeSet::new(), 2, every(5), &DEFAULT_WEIGHTS);
        assert_eq!(p.kept, vec![1, 2]);
        assert_eq!(p.dropped, vec![(0, PruneStage::Rank)]);
        // keep 0 keeps nothing
        let p = prune(&cands, &BTreeSet::new(), 0, every(5), &DEFAULT_WEIGHTS);
        assert!(p.kept.is_empty() && p.dropped.len() == 3);
        assert_eq!(
            prune(&[], &BTreeSet::new(), 3, every(1), &DEFAULT_WEIGHTS),
            Pruned::default()
        );
    }

    #[test]
    fn prune_audits_every_nth_noul() {
        let m = metrics(0, 0, 0, 0);
        let cands: Vec<Scored> = (0..7).map(|i| cand(i, m, 500, true)).collect();
        let p = prune(&cands, &BTreeSet::new(), 5, every(3), &DEFAULT_WEIGHTS);
        assert_eq!(p.audit, vec![0, 3, 6]);
        assert!(p.kept.is_empty());
        assert!(p.dropped.iter().all(|(_, s)| *s == PruneStage::Noul));
        let p = prune(&cands, &BTreeSet::new(), 5, every(1), &DEFAULT_WEIGHTS);
        assert_eq!(p.audit, (0..7).collect::<Vec<_>>());
        // only candidates with a Noul are counted
        let mixed = [
            cand(0, m, 500, true),
            cand(1, m, 500, false),
            cand(2, m, 500, true),
            cand(3, m, 500, true),
            cand(4, m, 500, false),
            cand(5, m, 500, true),
        ];
        let p = prune(&mixed, &BTreeSet::new(), 5, every(2), &DEFAULT_WEIGHTS);
        assert_eq!(p.audit, vec![0, 3]);
        assert_eq!(p.kept.len(), 2);
    }

    #[test]
    fn prune_accounts_for_every_candidate() {
        let mut x: u32 = 12345;
        let mut next = move |n: u32| {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (x >> 16) % n
        };
        let cands: Vec<Scored> = (0..40)
            .map(|_| {
                let m = metrics(
                    next(4) as i32 - 1,
                    next(4) as i32 - 1,
                    next(4) * 100,
                    next(3) as u16,
                );
                cand(next(12) as u8, m, (next(11) * 100) as u16, next(4) == 0)
            })
            .collect();
        let seen = BTreeSet::from([st(0), st(5)]);
        for keep in 0..12 {
            let p = prune(&cands, &seen, keep, every(3), &DEFAULT_WEIGHTS);
            let mut all: Vec<usize> = p.kept.clone();
            all.extend(p.dropped.iter().map(|(i, _)| *i));
            all.sort_unstable();
            assert_eq!(all, (0..cands.len()).collect::<Vec<_>>(), "keep {keep}");
            assert!(p.kept.len() <= keep);
            assert!(p.dropped.windows(2).all(|w| w[0].0 < w[1].0));
            for i in &p.audit {
                assert!(p.dropped.contains(&(*i, PruneStage::Noul)));
            }
            let states: BTreeSet<StateFp> = p.kept.iter().map(|&i| cands[i].state).collect();
            assert_eq!(states.len(), p.kept.len(), "kept states are distinct");
            assert!(states.is_disjoint(&seen));
            assert!(p.kept.iter().all(|&i| cands[i].noul.is_none()));
        }
    }

    #[test]
    fn rank_final_orders_by_reward_then_depth() {
        let mut t = SearchTree::new(vec![]);
        let root = t.root();
        let n1 = added(t.insert(new_node(root, vec![w("a.rs", "1")])));
        let n2 = added(t.insert(new_node(n1, vec![w("a.rs", "2")])));
        let n3 = added(t.insert(new_node(root, vec![w("a.rs", "3")])));
        let n4 = added(t.insert(new_node(root, vec![w("a.rs", "4")])));
        let n5 = added(t.insert(new_node(root, vec![w("a.rs", "5")])));
        let rw = |clean, passed| Reward {
            passed,
            clean,
            extensibility: 0,
            edit_bytes: 0,
        };
        let verified = [
            (n1, 1, rw(2, true)),
            (n2, 2, rw(2, true)),  // ties n1 on reward, deeper
            (n3, 1, rw(9, false)), // better but failed: not listed
            (n4, 1, rw(5, true)),
            (n5, 1, rw(2, true)), // ties n1 on reward and depth: the later node
        ];
        assert_eq!(
            rank_final(&verified, &DEFAULT_WEIGHTS),
            vec![n4, n1, n5, n2]
        );
        assert!(rank_final(&[], &DEFAULT_WEIGHTS).is_empty());
        assert!(rank_final(&[(n3, 1, rw(9, false))], &DEFAULT_WEIGHTS).is_empty());
        // fewer bytes written is better at equal points
        let lean = Reward {
            edit_bytes: 0,
            ..rw(1, true)
        };
        let fat = Reward {
            edit_bytes: 500,
            ..rw(1, true)
        };
        assert_eq!(
            rank_final(&[(n1, 1, fat), (n2, 1, lean)], &DEFAULT_WEIGHTS),
            vec![n2, n1]
        );
    }

    #[test]
    fn default_budget_matches_decision_budget() {
        let b = SearchBudget {
            beam: 20,
            max_expand: 60,
            beam_parents: 4,
            max_depth: 3,
            per_candidate_ms: 6_000,
            soft_ms: 240_000,
            hard_ms: 360_000,
            audit_every: every(10),
        };
        assert_eq!(b.decision_budget(), DEFAULT_BUDGET);
        let d = DEFAULT_BUDGET;
        assert_eq!(
            u64::from(d.candidates_per_depth) * u64::from(d.max_depth) * d.per_candidate_ms,
            360_000
        );
        assert_eq!(DEFAULT_WEIGHTS.clean, 10);
    }

    #[test]
    fn targets_and_probes_carry_what_they_were_given() {
        let t = Target::new(rel("a.rs"), Some(tr(1, 2)));
        assert_eq!((t.path(), t.range()), (&rel("a.rs"), Some(tr(1, 2))));
        assert_eq!(Target::new(rel("b.rs"), None).range(), None);
        let p = ChangeProbe::AddVariant {
            ty: Ident::parse("Kind").unwrap(),
        };
        assert_ne!(
            p,
            ChangeProbe::AddField {
                ty: Ident::parse("Kind").unwrap()
            }
        );
        assert!(CleanDelta(1) > CleanDelta::default() && ExtDelta(-1) < ExtDelta::default());
    }
}
