//! The use case: the best-first beam search of one decision.
//!
//! Children of a state are proposed by an [`Expander`], run through the stages of
//! [`crate::app::apply`] against the text of their parent, assessed from the texts alone, judged
//! once per depth and pruned to a beam; only the beam reaches the analyzer. The search ends with
//! a [`Resolution`], the ranking of every verified node and a trace of every child.
//!
//! Children are written on the search's stacked fork on top of their parent (C12 of
//! [`search_decision`]), and the expander reads that fork.
//!
//! The numbers of the budget are those of the decisions in force in `SEARCH_LEDGER.md`: D1 (time),
//! D2 (revert audit), D3 (memory per session) and D4 (the triggers that call for perf work).

use crate::app::apply::Acc;
use crate::app::ports::{
    Assessor, CheckError, CheckReach, Clock, ExpandError, Expander, Judge, JudgeError, RevertAudit,
    Stacked,
};
use crate::domain::search::{prune, rank_final, repath, reward_score, score};
use crate::domain::stage::{On, Refused, Staged, stage, view_of};
use crate::domain::{
    Blocking, Blueprint, BlueprintId, Candidate, ChangeProbe, CommandKind, Edit, Evidence, Finding,
    Infeasible, Inserted, JudgeRequest, NearMiss, NewNode, NodeId, NonEmpty, Noul, Prior, Proposal,
    ProposalId, ProviderTag, PruneStage, RelPath, Resolution, Reward, Scope, ScopedFindings,
    Scored, SearchBudget, SearchTree, Stage, StateFp, StaticMetrics, Target, Verdict, Weights,
};
use std::collections::BTreeMap;
use std::num::NonZeroU16;

/// The budget used when none is given: beam 20, 200 children per depth from the best 4 parents,
/// depth 3, 6 s per check, 300 s soft and 360 s hard per decision, one audited flaw in ten.
///
/// # Contract
/// - C1: the numbers are those of decision D1 in SEARCH_LEDGER.md, and `decision_budget()` of it is
///   `DEFAULT_BUDGET`.
///   tested: app::search::tests::default_search_budget_is_the_decided_numbers
pub const DEFAULT_SEARCH_BUDGET: SearchBudget = SearchBudget {
    beam: 20,
    max_expand: 200,
    beam_parents: 4,
    max_depth: 3,
    per_candidate_ms: 6_000,
    soft_ms: 300_000,
    hard_ms: 360_000,
    audit_every: NonZeroU16::MIN.saturating_add(9),
};

/// Decision D2 (SEARCH_LEDGER.md): the revert is asserted by a diagnostic re-check on every 50th check.
pub const ASSERT_REVERT_EVERY: u32 = 50;
/// Decision D1 (SEARCH_LEDGER.md): expansion and assessment of one depth take at most this long. Recorded, not enforced.
pub const EXPAND_DEPTH_MS: u64 = 10_000;
/// Decision D1 (SEARCH_LEDGER.md): the one judge call of a depth takes at most this long. Recorded, not enforced.
pub const JUDGE_DEPTH_MS: u64 = 30_000;
/// Decision D3 (SEARCH_LEDGER.md): memory of one analyzer session, in GB.
pub const SESSION_RAM_GB: u64 = 6;
/// Decision D3 (SEARCH_LEDGER.md): memory left to everything that is not a session, in GB.
pub const RESERVED_RAM_GB: u64 = 8;
/// Decision D4 (SEARCH_LEDGER.md), trigger 1: a decision whose median time passes this calls for a redesign.
pub const TRIGGER_DECISION_MEDIAN_MS: u64 = 300_000;
/// Decision D4 (SEARCH_LEDGER.md), trigger 2: a warm check at the affected scope whose median time passes this.
pub const TRIGGER_CHECK_MEDIAN_MS: u64 = 5_000;
/// Decision D4 (SEARCH_LEDGER.md), trigger 3: resident memory in kB, read after [`TRIGGER_AFTER_CHECKS`] checks.
pub const TRIGGER_RSS_KB: u64 = 8 * 1024 * 1024;
/// Decision D4 (SEARCH_LEDGER.md), trigger 3: check time relative to the first checks, in thousandths (x1.5).
pub const TRIGGER_CHECK_GROWTH_PERMILLE: u64 = 1_500;
/// Decision D4 (SEARCH_LEDGER.md), trigger 3: how many checks pass before memory and growth are looked at.
pub const TRIGGER_AFTER_CHECKS: u32 = 1_000;
/// Decision D4 (SEARCH_LEDGER.md), trigger 4: one run of the recall harness takes longer than this.
pub const TRIGGER_HARNESS_MS: u64 = 600_000;

/// How many sessions fit in `ram_gb` of memory.
///
/// # Contract
/// - C1: `max(1, (ram_gb - RESERVED_RAM_GB) / SESSION_RAM_GB)` with the subtraction saturating
///   at zero, so 30 GB gives 3 and no machine gives fewer than 1.
///   tested: app::search::tests::max_sessions_follows_decision_d3
pub fn max_sessions(ram_gb: u64) -> u64 {
    (ram_gb.saturating_sub(RESERVED_RAM_GB) / SESSION_RAM_GB).max(1)
}

/// What one search is given.
#[derive(Debug, Clone)]
pub struct SearchInput<'a> {
    /// The edits the search starts from, over the session base. Usually empty.
    pub start: &'a [Edit],
    /// Blueprints the Main AI proposes for the first depth, tried before any expanded child.
    pub roots: &'a [Blueprint],
    /// Where the expander may look for changes.
    pub targets: &'a [Target],
    /// Shapes the assessor measures extensibility against.
    pub probes: &'a [ChangeProbe],
    /// The scope every check runs at.
    pub scope: Scope,
    /// The limits.
    pub budget: SearchBudget,
    /// The weights of the score.
    pub weights: Weights,
    /// Whether the trace keeps the edits of every child that passed the stages.
    pub keep_trace_edits: bool,
}

/// What became of one child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fate {
    /// A stage refused it before a verdict: one before the check, or the check itself when it
    /// did not finish and the session survived (`Stage::Check`).
    Blocked(Stage),
    /// It was dropped without a check.
    Pruned(PruneStage),
    /// The time ran out: its check was cut off, or the decision's budget ended before its turn.
    TimedOut,
    /// It was checked and introduced errors.
    Failed {
        /// New errors plus unmapped ones.
        new_errors: usize,
    },
    /// It was checked and introduced no error.
    Passed,
}

/// One child of one depth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceEntry {
    /// The depth, from 1.
    pub depth: u8,
    /// The node it was proposed from.
    pub parent: NodeId,
    /// Its Blueprint.
    pub blueprint: BlueprintId,
    /// Its proposal, the identity a judge sees.
    pub proposal: ProposalId,
    /// The kind of the command it calls.
    pub command: CommandKind,
    /// The state it leads to; `None` when a stage refused it.
    pub state: Option<StateFp>,
    /// Its score for pruning; `None` when a stage refused it.
    pub score: Option<i64>,
    /// Its place in the beam, from 0; `None` when it was not kept.
    pub rank: Option<u16>,
    /// Whether it was checked only as an audit of a flaw the judge found.
    pub audited: bool,
    /// Whether the Main AI proposed it.
    pub root: bool,
    /// What became of it.
    pub fate: Fate,
    /// The node it became; `None` when it was never checked.
    pub node: Option<NodeId>,
    /// The edits of its state, kept only on request.
    pub edits: Option<Vec<Edit>>,
}

/// Counts and times of one depth. Times are differences of the clock, in ms.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DepthStats {
    /// The depth, from 1.
    pub depth: u8,
    /// Parents expanded.
    pub parents: u16,
    /// Children proposed, roots included.
    pub generated: u32,
    /// Children that passed the stages before the check.
    pub after_stages: u32,
    /// Children the prune kept.
    pub kept: u16,
    /// Flawed children checked anyway as an audit.
    pub audited: u16,
    /// Checks that ran.
    pub checked: u16,
    /// Checks that passed.
    pub passed: u16,
    /// Children with the fate of a time out, cut off or not reached.
    pub timed_out: u16,
    /// Time in the expansions: the expander, and the writes and pops that bring the search's
    /// fork to each parent's path before it is expanded.
    pub expand_ms: u64,
    /// Time in the stages and the assessor.
    pub assess_ms: u64,
    /// Time in the judge.
    pub judge_ms: u64,
    /// Time in the checks: each child's write, diagnosis and pop, the writes and pops that bring
    /// the search's fork to its parent's path, and the one diagnosis of the start state.
    pub check_ms: u64,
    /// Level writes on the search's fork in this depth: the parents' paths and the children.
    pub writes: u32,
}

/// A node that passed, as the search ranks it.
#[derive(Debug, Clone, PartialEq)]
pub struct RankedNode {
    /// The node.
    pub node: NodeId,
    /// The Blueprints from the first depth to this node.
    pub path: Vec<BlueprintId>,
    /// The depth.
    pub depth: u8,
    /// The reward along the path.
    pub reward: Reward,
    /// The reward scored with the weights of the search.
    pub reward_score: i64,
    /// The prior the judge gave it, for display only.
    pub prior: Prior,
}

/// What a search leaves.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchReport {
    /// The outcome.
    pub resolution: Resolution,
    /// Every node that passed, best first.
    pub ranking: Vec<RankedNode>,
    /// Every child of every depth, in the order they were proposed.
    pub trace: Vec<TraceEntry>,
    /// One entry per depth that ran.
    pub depths: Vec<DepthStats>,
    /// The provider of each judge call that answered.
    pub judge_providers: Vec<ProviderTag>,
    /// Each judge call that failed; the search went on with neutral priors.
    pub judge_errors: Vec<JudgeError>,
    /// Each expansion that failed; the search went on without that parent.
    pub expand_errors: Vec<ExpandError>,
    /// Whether the hard limit ended the search.
    pub budget_exhausted: bool,
    /// Whether the soft limit kept a further depth from starting.
    pub soft_stopped: bool,
    /// Time of the whole search.
    pub elapsed_ms: u64,
}

/// Why a search could not finish.
#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    /// A check failed as a process, not as a verdict, with a failure that reaches the session
    /// (`CheckReach::Session`), and the search cannot go on.
    #[error("check of blueprint {blueprint} failed: {source}")]
    Check {
        /// The Blueprint being checked.
        blueprint: BlueprintId,
        /// The failure.
        #[source]
        source: CheckError,
    },
    /// The expander lost its session.
    #[error("expansion failed: {0}")]
    Expand(#[source] ExpandError),
    /// The search's stacked fork failed outside the level of a child: it could not be opened or
    /// released, or a level of the start state or of a parent's path could not be written,
    /// diagnosed or popped; the session is held or poisoned.
    #[error("the search's fork failed: {0}")]
    Fork(#[source] CheckError),
    /// The session did not return to its baseline after the checks.
    #[error("revert audit failed: {0}")]
    RevertAudit(#[source] CheckError),
    /// The first depth had nothing to try.
    #[error("no candidate: no root and the expander produced nothing")]
    NoCandidates,
    /// A root names a Blueprint it builds on; roots are staged on the start state.
    #[error("root {blueprint} builds on {parent}; a search root cannot build on another blueprint")]
    ChainedRoot {
        /// The root.
        blueprint: BlueprintId,
        /// The Blueprint it builds on.
        parent: BlueprintId,
    },
}

/// A child before the stages.
struct Child {
    parent: NodeId,
    bp: Blueprint,
    root: bool,
}

/// A child that passed the stages, waiting for the prune and the check.
struct Pending {
    trace: usize,
    parent: NodeId,
    staged: Staged,
    metrics: StaticMetrics,
    prior: Prior,
    noul: Option<Noul>,
    root: bool,
    state: StateFp,
    proposal: ProposalId,
}

#[derive(Default)]
struct Run {
    trace: Vec<TraceEntry>,
    acc: Acc,
    blocking: Vec<Blocking>,
    near_miss: Vec<NearMiss>,
    depths: Vec<DepthStats>,
    judge_providers: Vec<ProviderTag>,
    judge_errors: Vec<JudgeError>,
    expand_errors: Vec<ExpandError>,
    verdicts: BTreeMap<NodeId, Verdict>,
    budget_exhausted: bool,
    soft_stopped: bool,
}

fn sat16(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

fn sat32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// What a step on the search's fork reads of the search.
struct Ctx<'a> {
    tree: &'a SearchTree,
    staged_of: &'a BTreeMap<NodeId, Staged>,
    start: &'a [Edit],
    scope: Scope,
    baseline: &'a [Finding],
}

/// What became of the check of one child on the search's fork.
enum Judged {
    /// Its level was diagnosed: the verdict, and the findings it was made from.
    Verdict(Verdict, ScopedFindings),
    /// Its own write, diagnosis, files or pop failed.
    Failed(CheckError),
    /// A level it would be written on failed with reach `Blueprint`; it was never written.
    Refused(CheckError),
    /// Its parent has no staged path. Parents are passing nodes, each with its staged path.
    Skipped,
}

/// The search's one stacked fork (C12, C13): the nodes whose levels it holds, and the findings of
/// the state of each node whose verdict was formed on it.
struct Walk<'v, V: Stacked> {
    v: &'v V,
    fork: V::Fork,
    /// The nodes whose levels the fork holds, bottom first.
    held: Vec<NodeId>,
    /// Whether the start state is a level: only when it has edits.
    start_level: bool,
    /// The root's findings (its one diagnosis, or none for an empty start) and those of every
    /// passing node, as its verdict was made from them. After a depth only those of its passing
    /// nodes are kept: no other node is a parent again.
    findings: BTreeMap<NodeId, ScopedFindings>,
    /// The failure of reach `Blueprint` of a level of a node's path in this depth: every child of
    /// the node is blocked with it.
    refused: BTreeMap<NodeId, CheckError>,
}

impl<'v, V: Stacked> Walk<'v, V> {
    /// Opens the fork. An empty start is no level and has no findings.
    fn open(
        v: &'v V,
        s: &mut V::Session,
        root: NodeId,
        start: &[Edit],
    ) -> Result<Self, CheckError> {
        let fork = v.fork(s)?;
        let mut findings = BTreeMap::new();
        if start.is_empty() {
            findings.insert(root, ScopedFindings::default());
        }
        Ok(Walk {
            v,
            fork,
            held: Vec::new(),
            start_level: !start.is_empty(),
            findings,
            refused: BTreeMap::new(),
        })
    }

    /// The nodes whose levels make the state of `node`, bottom first: the root when the start
    /// state has edits, then each node of its path from depth 1.
    fn levels_of(&self, tree: &SearchTree, node: NodeId) -> Vec<NodeId> {
        let mut path = tree.path(node);
        if !self.start_level {
            path.retain(|n| *n != tree.root());
        }
        path
    }

    /// Brings the fork to the levels of `node`'s path: pops to the longest common prefix of the
    /// levels it holds and that path, then writes the rest (`repath`). `false` when a node of the
    /// path has no staged path; a refused write leaves the fork at the levels written so far.
    fn to(
        &mut self,
        s: &mut V::Session,
        cx: &Ctx<'_>,
        node: NodeId,
        writes: &mut u32,
    ) -> Result<bool, CheckError> {
        let path = self.levels_of(cx.tree, node);
        let mut own: Vec<&[Edit]> = Vec::with_capacity(path.len());
        for n in &path {
            if *n == cx.tree.root() {
                own.push(cx.start);
            } else {
                match cx.staged_of.get(n) {
                    Some(staged) => own.push(staged.own()),
                    None => return Ok(false),
                }
            }
        }
        let (pops, rest) = repath(&self.held, &path);
        for _ in 0..pops {
            // A pop that fails has removed the level all the same (`Stacked::pop` C2).
            self.held.pop();
            self.v.pop(s, &mut self.fork)?;
        }
        let from = path.len() - rest.len();
        for (n, edits) in rest.iter().zip(&own[from..]) {
            *writes = writes.saturating_add(1);
            self.v.write(s, &mut self.fork, edits)?;
            self.held.push(*n);
        }
        Ok(true)
    }

    /// A failure of a level of `node`'s path: of reach `Blueprint`, every child of `node` in this
    /// depth is refused with it; of reach `Session`, the fork failed.
    fn refuse(&mut self, node: NodeId, e: CheckError) -> Result<Judged, SearchError> {
        match e.reach() {
            CheckReach::Blueprint => {
                self.refused.insert(node, e.clone());
                Ok(Judged::Refused(e))
            }
            CheckReach::Session => Err(SearchError::Fork(e)),
        }
    }

    /// The check of a child of `parent` whose own edits are `own`: the fork is brought to the
    /// parent's path, the start state is diagnosed if this is its first child, and the child is
    /// written as one more level, diagnosed at the input's scope and popped. The verdict is
    /// `Verdict::of_level` on top of the parent's findings, kept to `Stacked::files_of`.
    fn check_child(
        &mut self,
        s: &mut V::Session,
        cx: &Ctx<'_>,
        parent: NodeId,
        own: &[Edit],
        writes: &mut u32,
    ) -> Result<Judged, SearchError> {
        if let Some(e) = self.refused.get(&parent) {
            return Ok(Judged::Refused(e.clone()));
        }
        match self.to(s, cx, parent, writes) {
            Ok(true) => {}
            Ok(false) => return Ok(Judged::Skipped),
            Err(e) => return self.refuse(parent, e),
        }
        let root = cx.tree.root();
        if parent == root && !self.findings.contains_key(&root) {
            let found = match self.v.diagnose(s, &self.fork, cx.scope) {
                Ok(found) => found,
                Err(e) => return self.refuse(root, e),
            };
            let files = match self.v.files_of(s, &self.fork, cx.scope) {
                Ok(files) => files,
                Err(e) => return self.refuse(root, e),
            };
            let (_, kept) = Verdict::of_level(
                cx.baseline,
                &ScopedFindings::default(),
                &found,
                &Edit::deleted_paths(cx.start),
                &files,
            );
            self.findings.insert(root, kept);
        }
        let Some(below) = self.findings.get(&parent) else {
            return Ok(Judged::Skipped);
        };
        *writes = writes.saturating_add(1);
        if let Err(e) = self.v.write(s, &mut self.fork, own) {
            return Ok(Judged::Failed(e));
        }
        let diagnosed = self
            .v
            .diagnose(s, &self.fork, cx.scope)
            .and_then(|found| Ok((found, self.v.files_of(s, &self.fork, cx.scope)?)));
        // A failure of reach `Session` ends the search; the release reverts the level.
        if let Err(e) = &diagnosed
            && e.reach() == CheckReach::Session
        {
            return Ok(Judged::Failed(e.clone()));
        }
        if let Err(e) = self.v.pop(s, &mut self.fork) {
            return Ok(Judged::Failed(e));
        }
        Ok(match diagnosed {
            Ok((found, files)) => {
                let (verdict, kept) = Verdict::of_level(
                    cx.baseline,
                    below,
                    &found,
                    &Edit::deleted_paths(own),
                    &files,
                );
                Judged::Verdict(verdict, kept)
            }
            Err(e) => Judged::Failed(e),
        })
    }

    /// Ends the fork, reverting every level.
    fn release(self, s: &mut V::Session) -> Result<(), CheckError> {
        self.v.release(s, self.fork)
    }
}

/// Searches for the best verified changes that answer one decision.
///
/// `v` checks, `e` proposes, `a` measures, `j` judges and `clock` is the only source of time.
/// The expander and the verifier share the one session `s`, and the search's fork on it.
///
/// # Contract
/// - C1: depth 1 expands the start state, the roots of the input first; depth `d` above 1
///   expands the first `beam_parents` of `rank_final` over the passing nodes of depth `d - 1`,
///   each for at most `max(1, max_expand / parents)` children, and the search ends when there
///   is no such node. Each parent is expanded on the search's fork, with the levels of its path
///   on it (C12). A failed analysis of one parent, a refused write of its path included, is
///   recorded in the report and the parent yields nothing; a lost session is
///   `SearchError::Expand`, or `SearchError::Fork` when the fork lost it (C14); no root and no
///   child at depth 1 is `SearchError::NoCandidates`.
///   tested: app::search::tests::siblings_are_checked_consecutively
///   tested: app::search::tests::no_candidates_is_error
///   tested: app::search::tests::expand_errors_are_recorded_and_session_error_is_fatal
///   tested: app::search::tests::parents_are_expanded_on_the_fork
/// - C2: every child runs the stages of `apply_blueprints` against the text of its parent's
///   state, not the session base, so a read of the parent's text is fresh; a refusal is
///   `Fate::Blocked` with its `Blocking` and log entry and the child is never checked. A child
///   that passes has the edits of its parent overridden by its own, the file roles of its parent
///   plus its own, and the metrics the assessor gives for its change against the parent's text.
///   tested: app::search::tests::child_blueprint_reads_parent_state
///   tested: app::search::tests::best_planted_child_survives_with_fake_expander
///   tested: app::search::tests::child_inherits_build_script_role_of_parent
/// - C3: the judge is asked once per depth, about the children that passed the stages and
///   were not proposed by the Main AI, and not at all when there are none. A failed call
///   leaves every prior neutral and no flaw, and is recorded in the report; the provider of an
///   answer is recorded too.
///   tested: app::search::tests::judge_called_once_per_depth
///   tested: app::search::tests::judge_failure_is_recorded_and_search_goes_on
/// - C4: the beam holds `beam` minus the roots of the depth, and roots are never pruned.
///   The children left are checked grouped by parent in the parents' order, a parent's
///   children together in prune order, so siblings are checked one after another; a child
///   pruned instead gets `Fate::Pruned`, a `Stage::Prune` log entry and no check. Every
///   tenth flawed child, counted by the prune, is checked anyway and marked as audited.
///   tested: app::search::tests::roots_are_never_pruned
///   tested: app::search::tests::siblings_are_checked_consecutively
///   tested: app::search::tests::noul_pruned_are_audited_every_10th
/// - C5: before each check the clock is read: when the time spent plus `per_candidate_ms`
///   passes `hard_ms`, the whole search ends, `budget_exhausted` is set, and every child of
///   the beam not yet checked gets `Fate::TimedOut` without a check; so with 360 s and a
///   6 s limit at most 59 full checks begin after any expansion time. A child whose state is
///   already in the tree is `Fate::Pruned` for being a duplicate and is not checked. A check
///   cut off by `CheckError::TimedOut` is `Blocking::BudgetExhausted` and `Fate::TimedOut` and
///   the search goes on; a check that fails another way with a failure of reach `Blueprint`
///   is `Blocking::CheckFailed`, a `Stage::Check` log entry with `ok` false and
///   `Fate::Blocked(Stage::Check)`, and the search goes on; one of reach `Session` is
///   `SearchError::Check`; a check that adds errors is `Fate::Failed` and
///   `Blocking::NewErrors`, and a near miss when the errors are few. A `Decided` resolution
///   lists every blocking of the search.
///   tested: app::search::tests::budget_exhausted_returns_verified_so_far
///   tested: app::search::tests::hard_budget_with_nothing_verified_is_failed_exhausted
///   tested: app::search::tests::check_timeout_is_budget_exhausted_blocking
///   tested: app::search::tests::per_blueprint_check_error_blocks_only_that_child
///   tested: app::search::tests::session_check_error_stops_the_search
///   tested: app::search::tests::same_state_children_are_checked_once
/// - C6: after a depth, when the time spent has reached `soft_ms`, `soft_stopped` is set and
///   no further depth starts.
///   tested: app::search::tests::soft_budget_stops_after_the_depth
/// - C7: the revert is audited exactly once, after the fork is released and before the report
///   is returned, whether the resolution is `Decided` or `Failed`; an audit that fails is
///   `SearchError::RevertAudit`. A search that ends in an error is not audited.
///   tested: app::search::tests::revert_audit_runs_once_at_end
///   tested: app::search::tests::the_fork_is_released_before_the_audit_and_on_error
/// - C8: the ranking holds every node that passed, at any depth, by `rank_final`, each with
///   the Blueprints of its path and its prior. The resolution is `Decided` with every node of
///   the ranking as a candidate, in the order of the ranking; the marks of each candidate are
///   those of the Blueprints of its path and of the file roles its node edits. With no node
///   that passed the resolution is `Failed` with every blocking, and the budget flag, of the
///   search.
///   tested: app::search::tests::decided_lists_every_passing_node_by_reward
///   tested: app::search::tests::path_marks_are_the_union_of_the_path
///   tested: app::search::tests::hard_budget_with_nothing_verified_is_failed_exhausted
/// - C9: a prior decides which children are kept and in which order they are checked, never
///   the final order: `rank_final` is given no prior, so a judge that reverses every prior
///   leaves the ranking and the candidates unchanged, as long as the beam keeps the same
///   children and no two rewards are equal.
///   tested: app::search::tests::judge_prior_never_reorders_final
/// - C10: the trace has one entry per child of every depth, the roots included, in the order
///   they were proposed; its edits are present only on request. `depths` holds one entry per
///   depth that ran.
///   tested: app::search::tests::trace_has_one_entry_per_child_and_edits_only_on_request
/// - C11: a root with `builds_on` is `SearchError::ChainedRoot` for the first such root,
///   returned before anything runs: no check and no expansion.
///   tested: app::search::tests::root_with_builds_on_is_refused
/// - C12: the search keeps one stacked fork from before its first expansion until before its
///   audit. Before a parent is expanded, and before a child of it is checked, the fork holds the
///   levels of the parent's path: the start state when it has edits, then the own edits of each
///   node of the path from depth 1 on. It gets there from the levels it holds by popping to
///   their longest common prefix and writing the rest (`repath`). A child is one more level, its
///   own edits, popped after its diagnosis. No write takes a node's edits from the base.
///   tested: app::search::tests::children_are_written_on_top_of_their_parent
///   tested: app::search::tests::repath_pops_to_the_common_prefix
///   tested: app::search::tests::parents_are_expanded_on_the_fork
/// - C13: the verdict of a child is `Verdict::of_level` of the diagnosis of its level at the
///   input's scope, on top of its parent's findings, with the paths the child deletes, kept to
///   the `Stacked::files_of` of its level at that scope. A passing node's findings are those
///   its verdict was made from. The start state's are one diagnosis of its level kept the same
///   way, taken at the first check of a child of the root; an empty start has none.
///   tested: app::search::tests::the_verdict_keeps_the_findings_below
///   tested: app::search::tests::the_start_state_is_diagnosed_once
///   tested: ryo-v2-ra tests/stack.rs search_and_apply_give_the_same_verdict_on_a_chain
///   tested: ryo-v2-oracle tests/search_recall.rs stacked_verdicts_equal_the_flat_check
/// - C14: a failure of the fork outside the level of a child is reported by its reach. Of reach
///   `Session` (opening it, or a write, diagnosis or pop of the start state's level or of a
///   parent's path) it is `SearchError::Fork`. Of reach `Blueprint`, it is the check failure of
///   every child that would be written on that level, each recorded as C5 says, a child whose
///   own edits write that path again included: the start state's level for every child of the
///   root, a parent's path for that parent's children in the depth. For an expansion, it is the
///   failed analysis of that parent. A failure of reach `Session` of a child's own write,
///   diagnosis or pop is `SearchError::Check` with that child. A child whose diagnosis fails is
///   popped like the others.
///   tested: app::search::tests::a_failed_start_level_fails_every_root_child
///   tested: app::search::tests::a_refused_path_write_blocks_the_children
///   tested: app::search::tests::a_session_failure_of_a_pop_stops_the_search
///   tested: app::search::tests::a_session_failure_outside_a_childs_level_is_a_fork_error
/// - C15: the fork is released before the search returns, whatever it returns. A failed
///   release is `SearchError::Fork`, which wins over every end that is not a failure of reach
///   `Session`: a report, and `SearchError::NoCandidates`; a search that already ends in
///   `SearchError::Check`, `SearchError::Expand` or `SearchError::Fork` returns that error.
///   tested: app::search::tests::the_fork_is_released_before_the_audit_and_on_error
///   tested: app::search::tests::a_failed_release_is_a_search_error
pub fn search_decision<V, E, A, J>(
    v: &V,
    e: &E,
    a: &A,
    j: &J,
    clock: &dyn Clock,
    s: &mut V::Session,
    input: &SearchInput<'_>,
) -> Result<SearchReport, SearchError>
where
    V: Stacked + RevertAudit,
    E: Expander<Session = V::Session>,
    A: Assessor,
    J: Judge,
{
    if let Some((blueprint, parent)) = input
        .roots
        .iter()
        .find_map(|bp| bp.builds_on().map(|p| (bp.id(), p)))
    {
        return Err(SearchError::ChainedRoot { blueprint, parent });
    }
    let t0 = clock.now_ms();
    let elapsed = || clock.now_ms().saturating_sub(t0);
    let b = input.budget;
    let w = input.weights;
    let baseline = v.baseline(s);
    let provenance = v.provenance(s);
    let blind = v.blind_classes();
    let mut tree = SearchTree::new(input.start.to_vec());
    let mut run = Run::default();
    let mut passing: Vec<NodeId> = Vec::new();
    let mut last_passing: Vec<NodeId> = Vec::new();
    let mut staged_of: BTreeMap<NodeId, Staged> = BTreeMap::new();
    let mut walk = Walk::open(v, s, tree.root(), input.start).map_err(SearchError::Fork)?;

    let searched: Result<(), SearchError> = 'search: {
        for d in 1..=b.max_depth {
            walk.refused.clear();
            // 1. parents and children
            let parents: Vec<NodeId> = if d == 1 {
                vec![tree.root()]
            } else {
                let verified: Vec<(NodeId, u8, Reward)> = last_passing
                    .iter()
                    .filter_map(|id| {
                        let n = tree.node(*id)?;
                        Some((*id, n.depth(), *n.reward()?))
                    })
                    .collect();
                let mut ranked = rank_final(&verified, &w);
                ranked.truncate(usize::from(b.beam_parents));
                if ranked.is_empty() {
                    break;
                }
                ranked
            };
            let mut stats = DepthStats {
                depth: d,
                parents: sat16(parents.len()),
                ..DepthStats::default()
            };
            let t_expand = clock.now_ms();
            let per_parent = (usize::from(b.max_expand) / parents.len()).max(1);
            let mut children: Vec<Child> = Vec::new();
            if d == 1 {
                for bp in input.roots {
                    children.push(Child {
                        parent: tree.root(),
                        bp: bp.clone(),
                        root: true,
                    });
                }
            }
            for &p in &parents {
                let cx = Ctx {
                    tree: &tree,
                    staged_of: &staged_of,
                    start: input.start,
                    scope: input.scope,
                    baseline: &baseline,
                };
                match walk.to(s, &cx, p, &mut stats.writes) {
                    Ok(true) => {}
                    // Parents are passing nodes, each with its staged path.
                    Ok(false) => continue,
                    Err(err) if err.reach() == CheckReach::Blueprint => {
                        run.expand_errors.push(ExpandError::Analysis(format!(
                            "a level of the parent's path was refused: {err}"
                        )));
                        walk.refused.insert(p, err);
                        continue;
                    }
                    Err(err) => break 'search Err(SearchError::Fork(err)),
                }
                let state = tree.node(p).map(|n| n.edits().to_vec()).unwrap_or_default();
                match e.expand(s, &state, input.targets, per_parent) {
                    Ok(mut bps) => {
                        bps.truncate(per_parent);
                        children.extend(bps.into_iter().map(|bp| Child {
                            parent: p,
                            bp,
                            root: false,
                        }));
                    }
                    Err(err @ ExpandError::Analysis(_)) => run.expand_errors.push(err),
                    Err(err) => break 'search Err(SearchError::Expand(err)),
                }
            }
            if d == 1 && children.is_empty() {
                break 'search Err(SearchError::NoCandidates);
            }
            stats.generated = sat32(children.len());
            let t_stage = clock.now_ms();
            stats.expand_ms = t_stage.saturating_sub(t_expand);

            // 2. stages and assessment, against the parent's text
            let mut staged: Vec<Option<Pending>> = Vec::new();
            for child in children {
                let Some(parent) = tree.node(child.parent) else {
                    continue;
                };
                let on = if child.parent == tree.root() {
                    On::Start(input.start)
                } else {
                    match staged_of.get(&child.parent) {
                        Some(parent_staged) => On::Parent(parent_staged),
                        // Parents are passing nodes, each with its staged path.
                        None => continue,
                    }
                };
                let bp = child.bp;
                let proposal = ProposalId::of(&bp);
                let ix = run.trace.len();
                run.trace.push(TraceEntry {
                    depth: d,
                    parent: child.parent,
                    blueprint: bp.id(),
                    proposal,
                    command: bp.command().kind(),
                    state: None,
                    score: None,
                    rank: None,
                    audited: false,
                    root: child.root,
                    fate: Fate::TimedOut,
                    node: None,
                    edits: None,
                });
                let sess: &V::Session = &*s;
                let base = |p: &RelPath| v.text(sess, p);
                let facts = |p: &RelPath| v.facts(sess, p);
                match stage(
                    &bp,
                    on,
                    &base,
                    &facts,
                    &mut run.acc.log,
                    &mut run.acc.fingerprints_checked,
                ) {
                    Err(Refused { stage, blocking }) => {
                        run.trace[ix].fate = Fate::Blocked(stage);
                        run.blocking.push(blocking);
                    }
                    Ok(done) => {
                        let before: Vec<Edit> = done
                            .own()
                            .iter()
                            .map(|ed| match view_of(parent.edits(), &base, &ed.path) {
                                Some(text) => Edit::write(ed.path.clone(), text),
                                None => Edit::delete(ed.path.clone()),
                            })
                            .collect();
                        let metrics = a.assess(&before, done.own(), input.probes);
                        let state = StateFp::of(done.edits());
                        run.trace[ix].state = Some(state);
                        if input.keep_trace_edits {
                            run.trace[ix].edits = Some(done.edits().to_vec());
                        }
                        staged.push(Some(Pending {
                            trace: ix,
                            parent: child.parent,
                            staged: done,
                            metrics,
                            prior: Prior::NEUTRAL,
                            noul: None,
                            root: child.root,
                            state,
                            proposal,
                        }));
                    }
                }
            }
            stats.after_stages = sat32(staged.len());
            let t_judge = clock.now_ms();
            stats.assess_ms = t_judge.saturating_sub(t_stage);

            // 3. one judge call
            let proposals: Vec<Proposal> = staged
                .iter()
                .flatten()
                .filter(|c| !c.root)
                .map(|c| Proposal::new(c.staged.path().last(), d, c.metrics))
                .collect();
            if !proposals.is_empty() {
                match j.judge(&JudgeRequest::new(proposals)) {
                    Ok(judgement) => {
                        run.judge_providers.push(judgement.provider().clone());
                        for c in staged.iter_mut().flatten().filter(|c| !c.root) {
                            c.prior = judgement.prior_of(c.proposal);
                            c.noul = judgement.noul_of(c.proposal).cloned();
                        }
                    }
                    Err(err) => run.judge_errors.push(err),
                }
            }
            let t_prune = clock.now_ms();
            stats.judge_ms = t_prune.saturating_sub(t_judge);

            // 4. prune, then the order of the checks
            let roots_here = staged.iter().flatten().filter(|c| c.root).count();
            let keep = usize::from(b.beam).saturating_sub(roots_here);
            let cand_ix: Vec<usize> = staged
                .iter()
                .enumerate()
                .filter(|(_, c)| c.as_ref().is_some_and(|c| !c.root))
                .map(|(i, _)| i)
                .collect();
            let scored: Vec<Scored> = cand_ix
                .iter()
                .filter_map(|&i| staged[i].as_ref())
                .map(|c| Scored {
                    proposal: c.proposal,
                    state: c.state,
                    metrics: c.metrics,
                    prior: c.prior,
                    noul: c.noul.clone(),
                })
                .collect();
            let pruned = prune(&scored, &tree.states(), keep, b.audit_every, &w);
            for c in staged.iter().flatten() {
                run.trace[c.trace].score = Some(score(&c.metrics, c.prior, &w));
            }
            let trace_of = |k: usize| -> Option<usize> {
                staged
                    .get(*cand_ix.get(k)?)
                    .and_then(|c| c.as_ref())
                    .map(|c| c.trace)
            };
            for (pos, &k) in pruned.kept.iter().enumerate() {
                if let Some(t) = trace_of(k) {
                    run.trace[t].rank = u16::try_from(pos).ok();
                }
            }
            for &k in &pruned.audit {
                if let Some(t) = trace_of(k) {
                    run.trace[t].audited = true;
                }
            }
            for &(k, stage) in &pruned.dropped {
                if pruned.audit.contains(&k) {
                    continue;
                }
                let Some(c) = cand_ix.get(k).and_then(|&i| staged[i].as_ref()) else {
                    continue;
                };
                run.trace[c.trace].fate = Fate::Pruned(stage);
                run.acc.note(
                    c.staged.path().last().id(),
                    Stage::Prune,
                    false,
                    stage.to_string(),
                );
            }
            stats.kept = sat16(pruned.kept.len());
            stats.audited = sat16(pruned.audit.len());
            let mut order: Vec<usize> = staged
                .iter()
                .enumerate()
                .filter(|(_, c)| c.as_ref().is_some_and(|c| c.root))
                .map(|(i, _)| i)
                .collect();
            for p in &parents {
                for &k in pruned.kept.iter().chain(pruned.audit.iter()) {
                    let Some(&i) = cand_ix.get(k) else {
                        continue;
                    };
                    if staged[i].as_ref().is_some_and(|c| c.parent == *p) {
                        order.push(i);
                    }
                }
            }

            // 5. checks
            let mut depth_passing: Vec<NodeId> = Vec::new();
            for i in order {
                let Some(c) = staged[i].take() else {
                    continue;
                };
                let bp_id = c.staged.path().last().id();
                let t = c.trace;
                if run.budget_exhausted {
                    run.trace[t].fate = Fate::TimedOut;
                    stats.timed_out = stats.timed_out.saturating_add(1);
                    continue;
                }
                if tree.has_state(c.state) {
                    run.trace[t].fate = Fate::Pruned(PruneStage::Duplicate);
                    run.acc.note(
                        bp_id,
                        Stage::Prune,
                        false,
                        PruneStage::Duplicate.to_string(),
                    );
                    continue;
                }
                if elapsed().saturating_add(b.per_candidate_ms) > b.hard_ms {
                    run.budget_exhausted = true;
                    run.trace[t].fate = Fate::TimedOut;
                    stats.timed_out = stats.timed_out.saturating_add(1);
                    continue;
                }
                let id = match tree.insert(NewNode {
                    parent: c.parent,
                    blueprint: c.staged.path().last().clone(),
                    edits: c.staged.edits().to_vec(),
                    roles: c.staged.roles().clone(),
                    metrics: c.metrics,
                    prior: c.prior,
                }) {
                    Inserted::Added(id) => id,
                    Inserted::Merged(_) => {
                        run.trace[t].fate = Fate::Pruned(PruneStage::Duplicate);
                        continue;
                    }
                };
                run.trace[t].node = Some(id);
                let t_check = clock.now_ms();
                let cx = Ctx {
                    tree: &tree,
                    staged_of: &staged_of,
                    start: input.start,
                    scope: input.scope,
                    baseline: &baseline,
                };
                let judged =
                    match walk.check_child(s, &cx, c.parent, c.staged.own(), &mut stats.writes) {
                        Ok(judged) => judged,
                        Err(err) => break 'search Err(err),
                    };
                stats.check_ms = stats
                    .check_ms
                    .saturating_add(clock.now_ms().saturating_sub(t_check));
                let (result, below) = match judged {
                    Judged::Skipped => continue,
                    Judged::Verdict(verdict, kept) => (Ok((verdict, kept)), false),
                    Judged::Failed(err) => (Err(err), false),
                    Judged::Refused(err) => (Err(err), true),
                };
                if !below {
                    stats.checked = stats.checked.saturating_add(1);
                }
                match result {
                    Ok((verdict, kept)) => {
                        run.acc.verdicts.push((bp_id, verdict.clone()));
                        let errors = verdict.new_errors.len();
                        run.acc.note(
                            bp_id,
                            Stage::Check,
                            verdict.passed(),
                            format!("{errors} new error(s)"),
                        );
                        if verdict.passed() {
                            let along: Vec<StaticMetrics> = tree
                                .path(id)
                                .iter()
                                .skip(1)
                                .filter_map(|n| tree.node(*n).map(|x| *x.metrics()))
                                .collect();
                            tree.set_reward(id, Reward::along(&along, true));
                            staged_of.insert(id, c.staged);
                            walk.findings.insert(id, kept);
                            run.verdicts.insert(id, verdict);
                            passing.push(id);
                            depth_passing.push(id);
                            run.trace[t].fate = Fate::Passed;
                            stats.passed = stats.passed.saturating_add(1);
                        } else {
                            run.trace[t].fate = Fate::Failed { new_errors: errors };
                            run.near_miss.extend(NearMiss::of(bp_id, errors));
                            run.blocking.push(Blocking::NewErrors {
                                blueprint: bp_id,
                                verdict,
                            });
                        }
                    }
                    Err(CheckError::TimedOut { limit_ms }) => {
                        let what = if below { "a level below: " } else { "" };
                        run.acc.note(
                            bp_id,
                            Stage::Check,
                            false,
                            format!("{what}cut off after {limit_ms} ms"),
                        );
                        run.blocking.push(Blocking::BudgetExhausted {
                            blueprint: bp_id,
                            limit_ms,
                        });
                        run.trace[t].fate = Fate::TimedOut;
                        stats.timed_out = stats.timed_out.saturating_add(1);
                    }
                    Err(e) => {
                        let note = if below {
                            format!("a level below failed: {e}")
                        } else {
                            format!("check failed: {e}")
                        };
                        match e.into_blocking(bp_id) {
                            Ok(blocking) => {
                                run.acc.note(bp_id, Stage::Check, false, note);
                                run.blocking.push(blocking);
                                run.trace[t].fate = Fate::Blocked(Stage::Check);
                            }
                            Err(source) => {
                                break 'search Err(SearchError::Check {
                                    blueprint: bp_id,
                                    source,
                                });
                            }
                        }
                    }
                }
            }
            run.depths.push(stats);
            last_passing = depth_passing;
            // Only the passing nodes of this depth can be parents of the next.
            walk.findings.retain(|id, _| last_passing.contains(id));

            // 6. limits
            if run.budget_exhausted {
                break;
            }
            if elapsed() >= b.soft_ms {
                run.soft_stopped = true;
                break;
            }
        }
        Ok(())
    };

    // 7. the fork released, whatever the search ended in (C15)
    match (searched, walk.release(s)) {
        // A failure of reach `Session` already ends the search; the release's is the same lost
        // session.
        (
            Err(e @ (SearchError::Check { .. } | SearchError::Expand(_) | SearchError::Fork(_))),
            _,
        ) => {
            return Err(e);
        }
        (_, Err(released)) => return Err(SearchError::Fork(released)),
        (Err(e), Ok(())) => return Err(e),
        (Ok(()), Ok(())) => {}
    }

    // 8. the revert, audited once
    v.audit_revert(s).map_err(SearchError::RevertAudit)?;

    // 9. ranking and resolution
    let verified: Vec<(NodeId, u8, Reward)> = passing
        .iter()
        .filter_map(|id| {
            let n = tree.node(*id)?;
            Some((*id, n.depth(), *n.reward()?))
        })
        .collect();
    let ranking: Vec<RankedNode> = rank_final(&verified, &w)
        .into_iter()
        .filter_map(|id| {
            let n = tree.node(id)?;
            let reward = *n.reward()?;
            Some(RankedNode {
                node: id,
                path: tree
                    .path(id)
                    .iter()
                    .filter_map(|p| tree.node(*p).and_then(|x| x.blueprint()).map(Blueprint::id))
                    .collect(),
                depth: n.depth(),
                reward,
                reward_score: reward_score(&reward, &w),
                prior: n.prior(),
            })
        })
        .collect();
    let mut candidates = Vec::new();
    for r in &ranking {
        let (Some(verdict), Some(done)) = (run.verdicts.get(&r.node), staged_of.remove(&r.node))
        else {
            continue;
        };
        let leaf = done.path().last().id();
        if let Ok(c) = Candidate::new(done, verdict.clone(), &blind) {
            run.acc.note(leaf, Stage::Candidate, true, c.marks_line());
            candidates.push(c);
        }
    }
    let resolution = match NonEmpty::new(candidates) {
        Some(blueprints) => Resolution::Decided {
            blueprints,
            blocking: run.blocking,
            decision_log: run.acc.log,
            evidence: Evidence::new(
                input.scope,
                run.acc.fingerprints_checked,
                baseline.len(),
                run.acc.verdicts,
                b.decision_budget(),
                provenance,
                blind,
            ),
        },
        None => Resolution::Failed(
            Infeasible::new(run.blocking, run.near_miss, run.budget_exhausted)
                .unwrap_or_else(Infeasible::exhausted),
        ),
    };
    Ok(SearchReport {
        resolution,
        ranking,
        trace: run.trace,
        depths: run.depths,
        judge_providers: run.judge_providers,
        judge_errors: run.judge_errors,
        expand_errors: run.expand_errors,
        budget_exhausted: run.budget_exhausted,
        soft_stopped: run.soft_stopped,
        elapsed_ms: elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ports::{FileFacts, Verifier};
    use crate::domain::{
        BaseFp, BlueprintParts, CheckFailure, CleanDelta, Command, Criterion, DEFAULT_BUDGET,
        DEFAULT_WEIGHTS, DecisionRecord, ErrorCode, Finding, FindingCode, Ident, Judgement,
        Provenance, ReadRange, RiskMark, ScopedFindings, SpecRef, Splice, TextRange, UnseenClass,
    };
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    const BASE: &str = "let a = 1;";
    const LIB: &str = "src/lib.rs";

    fn rel(s: &str) -> RelPath {
        RelPath::parse(s).unwrap()
    }
    fn tr(a: u32, b: u32) -> TextRange {
        TextRange::new(a, b).unwrap()
    }
    fn bid(n: u16) -> BlueprintId {
        BlueprintId::parse(&format!("01890a5d-ac96-774b-bcce-b302099a{n:04x}")).unwrap()
    }
    fn w(text: &str) -> Edit {
        Edit::write(rel(LIB), text)
    }
    fn state(edits: Vec<Edit>) -> StateFp {
        StateFp::of(&edits)
    }
    fn root_state() -> StateFp {
        StateFp::of(&[])
    }
    fn clean(n: i32) -> StaticMetrics {
        StaticMetrics {
            clean: CleanDelta(n),
            ..StaticMetrics::default()
        }
    }
    fn make(n: u16, command: Command, splices: Vec<Splice>, text: &str) -> Blueprint {
        let end = u32::try_from(text.len()).unwrap();
        Blueprint::new(BlueprintParts {
            id: bid(n),
            command,
            splices,
            reads: vec![ReadRange::capture(rel(LIB), text, tr(0, end)).unwrap()],
            bases: vec![BaseFp::capture(rel(LIB), text)],
            spec_refs: vec![SpecRef::parse("spec#t").unwrap()],
            record: DecisionRecord::new("agent", "why").unwrap(),
            builds_on: None,
        })
        .unwrap()
    }
    /// Renames the first `from` of `text` to `to`; the read is the whole `text`.
    fn rename(n: u16, from: &str, to: &str, text: &str) -> Blueprint {
        let at = u32::try_from(text.find(from).unwrap()).unwrap();
        let end = at + u32::try_from(from.len()).unwrap();
        make(
            n,
            Command::RenameLocal {
                from: Ident::parse(from).unwrap(),
                to: Ident::parse(to).unwrap(),
            },
            vec![Splice::replace(rel(LIB), tr(at, end), to).unwrap()],
            text,
        )
    }
    /// Binds the `1 + 2` of `f(1 + 2);` to `x`.
    fn extract_x(n: u16, text: &str) -> Blueprint {
        make(
            n,
            Command::ExtractVariable {
                name: Ident::parse("x").unwrap(),
            },
            vec![
                Splice::insert(rel(LIB), 0, "let x = 1 + 2;\n").unwrap(),
                Splice::replace(rel(LIB), tr(2, 7), "x").unwrap(),
            ],
            text,
        )
    }
    fn errors(n: usize) -> ScopedFindings {
        ScopedFindings {
            files: vec![rel(LIB)],
            findings: (0..n)
                .map(|i| Finding {
                    code: FindingCode::Rustc(ErrorCode::parse("E0599").unwrap()),
                    message: format!("m{i}"),
                    path: rel(LIB),
                    line: 1,
                })
                .collect(),
            ..Default::default()
        }
    }
    fn first_text(edits: &[Edit]) -> String {
        edits
            .first()
            .and_then(|e| e.new_text.clone())
            .unwrap_or_default()
    }

    /// Virtual time that only moves when told to.
    #[derive(Clone, Default)]
    struct Ticker(Arc<AtomicU64>);
    impl Ticker {
        fn advance(&self, ms: u64) {
            self.0.fetch_add(ms, Ordering::SeqCst);
        }
    }
    impl Clock for Ticker {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    type CheckRule = Box<dyn Fn(&[Edit]) -> Result<ScopedFindings, CheckError>>;
    /// A failure for the `n`-th call (from 0) with these edits, or none.
    type FaultRule = Box<dyn Fn(&[Edit], usize) -> Option<CheckError>>;
    type FilesRule = Box<dyn Fn(&[Edit]) -> Vec<RelPath>>;

    /// One operation on the fake's fork, or an expansion, in the order they ran.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Op {
        Fork,
        Write(Vec<Edit>),
        /// The number of levels it diagnosed at.
        Diagnose(usize),
        FilesOf,
        /// The edits of the level it popped.
        Pop(Vec<Edit>),
        Release,
        Audit,
        /// The levels the fork held when the expander was called.
        Expand(Vec<Vec<Edit>>),
    }

    /// The fake fork's levels and the log of the operations, shared by `FakeV` and `FakeE`.
    #[derive(Clone, Default)]
    struct Shared {
        levels: std::rc::Rc<RefCell<Vec<Vec<Edit>>>>,
        ops: std::rc::Rc<RefCell<Vec<Op>>>,
    }

    struct FakeV {
        texts: BTreeMap<RelPath, String>,
        build_scripts: BTreeSet<RelPath>,
        rule: CheckRule,
        checks: RefCell<Vec<Vec<Edit>>>,
        events: RefCell<Vec<&'static str>>,
        audit_error: Option<CheckError>,
        tick: Option<(Ticker, u64)>,
        baseline: Vec<Finding>,
        /// The files `files_of` gives for composed edits; by default those of `rule`.
        files: Option<FilesRule>,
        shared: Shared,
        fork_error: Option<CheckError>,
        write_fault: FaultRule,
        pop_fault: FaultRule,
        release_error: Option<CheckError>,
    }
    impl Verifier for FakeV {
        type Session = ();
        fn text(&self, _: &(), path: &RelPath) -> Option<String> {
            self.texts.get(path).cloned()
        }
        fn facts(&self, _: &(), path: &RelPath) -> FileFacts {
            FileFacts {
                is_build_script: self.build_scripts.contains(path),
                in_proc_macro_crate: false,
            }
        }
        fn baseline(&self, _: &()) -> Vec<Finding> {
            self.baseline.clone()
        }
        fn provenance(&self, _: &()) -> Provenance {
            Provenance::new("fake", "0", BTreeMap::new(), BTreeMap::new()).unwrap()
        }
        fn blind_classes(&self) -> BTreeSet<UnseenClass> {
            BTreeSet::from(crate::app::ALWAYS_BLIND)
        }
        fn check(
            &self,
            _: &mut (),
            edits: &[Edit],
            _: Scope,
        ) -> Result<ScopedFindings, CheckError> {
            self.checks.borrow_mut().push(edits.to_vec());
            self.events.borrow_mut().push("check");
            if let Some((ticker, cost)) = &self.tick {
                ticker.advance(*cost);
            }
            (self.rule)(edits)
        }
    }
    impl RevertAudit for FakeV {
        fn audit_revert(&self, _: &mut ()) -> Result<(), CheckError> {
            self.events.borrow_mut().push("audit");
            self.op(Op::Audit);
            match &self.audit_error {
                Some(e) => Err(e.clone()),
                None => Ok(()),
            }
        }
    }
    /// A stack of levels whose every diagnosis answers from the flat rule of the composed edits,
    /// so a file keeps its findings unless the composed state changes (`Stacked::diagnose` C3),
    /// and whose `files_of` gives the files a check of those edits gives, none for none
    /// (`Stacked::files_of` C1, C2). Its faults are refusals the port allows.
    impl Stacked for FakeV {
        type Fork = ();
        fn fork(&self, _: &mut ()) -> Result<(), CheckError> {
            self.op(Op::Fork);
            match &self.fork_error {
                Some(e) => Err(e.clone()),
                None => Ok(()),
            }
        }
        fn write(&self, _: &mut (), _: &mut (), edits: &[Edit]) -> Result<(), CheckError> {
            let n = self.count(|op| *op == Op::Write(edits.to_vec()));
            self.op(Op::Write(edits.to_vec()));
            if let Some(e) = (self.write_fault)(edits, n) {
                return Err(e);
            }
            self.shared.levels.borrow_mut().push(edits.to_vec());
            Ok(())
        }
        fn levels(&self, _: &(), _: &()) -> Result<usize, CheckError> {
            Ok(self.shared.levels.borrow().len())
        }
        fn fork_text(&self, _: &(), _: &(), path: &RelPath) -> Result<Option<String>, CheckError> {
            let composed = self.composed();
            Ok(match composed.iter().find(|e| e.path == *path) {
                Some(e) => e.new_text.clone(),
                None => self.texts.get(path).cloned(),
            })
        }
        fn diagnose(&self, _: &(), _: &(), _: Scope) -> Result<ScopedFindings, CheckError> {
            self.op(Op::Diagnose(self.shared.levels.borrow().len()));
            let composed = self.composed();
            self.checks.borrow_mut().push(composed.clone());
            self.events.borrow_mut().push("check");
            if let Some((ticker, cost)) = &self.tick {
                ticker.advance(*cost);
            }
            (self.rule)(&composed)
        }
        fn files_of(&self, _: &(), _: &(), _: Scope) -> Result<Vec<RelPath>, CheckError> {
            self.op(Op::FilesOf);
            let composed = self.composed();
            if composed.is_empty() {
                return Ok(Vec::new());
            }
            match &self.files {
                Some(files) => Ok(files(&composed)),
                None => (self.rule)(&composed).map(|found| found.files),
            }
        }
        fn pop(&self, _: &mut (), _: &mut ()) -> Result<bool, CheckError> {
            let Some(top) = self.shared.levels.borrow_mut().pop() else {
                return Ok(false);
            };
            let n = self.count(|op| *op == Op::Pop(top.clone()));
            self.op(Op::Pop(top.clone()));
            match (self.pop_fault)(&top, n) {
                Some(e) => Err(e),
                None => Ok(true),
            }
        }
        fn release(&self, _: &mut (), _: ()) -> Result<(), CheckError> {
            self.op(Op::Release);
            self.shared.levels.borrow_mut().clear();
            match &self.release_error {
                Some(e) => Err(e.clone()),
                None => Ok(()),
            }
        }
    }
    impl FakeV {
        fn new(text: &str) -> Self {
            Self {
                texts: BTreeMap::from([(rel(LIB), text.to_owned())]),
                build_scripts: BTreeSet::new(),
                rule: Box::new(|_| Ok(ScopedFindings::default())),
                checks: RefCell::default(),
                events: RefCell::default(),
                audit_error: None,
                tick: None,
                baseline: Vec::new(),
                files: None,
                shared: Shared::default(),
                fork_error: None,
                write_fault: Box::new(|_, _| None),
                pop_fault: Box::new(|_, _| None),
                release_error: None,
            }
        }
        fn op(&self, op: Op) {
            self.shared.ops.borrow_mut().push(op);
        }
        fn count(&self, which: impl Fn(&Op) -> bool) -> usize {
            self.shared
                .ops
                .borrow()
                .iter()
                .filter(|op| which(op))
                .count()
        }
        /// The levels composed over the base texts, as `compose` puts a child on its parent.
        fn composed(&self) -> Vec<Edit> {
            let base = |p: &RelPath| self.texts.get(p).cloned();
            self.shared
                .levels
                .borrow()
                .iter()
                .fold(Vec::new(), |acc, level| {
                    crate::domain::search::compose(&acc, level, &base)
                })
        }
        fn ops(&self) -> Vec<Op> {
            self.shared.ops.borrow().clone()
        }
        fn writes(&self) -> Vec<Vec<Edit>> {
            self.ops()
                .into_iter()
                .filter_map(|op| match op {
                    Op::Write(edits) => Some(edits),
                    _ => None,
                })
                .collect()
        }
        fn checked_texts(&self) -> Vec<String> {
            self.checks.borrow().iter().map(|e| first_text(e)).collect()
        }
        fn audits(&self) -> usize {
            self.events
                .borrow()
                .iter()
                .filter(|e| **e == "audit")
                .count()
        }
    }

    #[derive(Default)]
    struct FakeE {
        by_state: BTreeMap<StateFp, Result<Vec<Blueprint>, ExpandError>>,
        calls: RefCell<Vec<(StateFp, usize)>>,
        /// The fake fork, whose levels each call records as an `Op::Expand`.
        shared: Shared,
    }
    impl Expander for FakeE {
        type Session = ();
        fn expand(
            &self,
            _: &mut (),
            state: &[Edit],
            _: &[Target],
            max: usize,
        ) -> Result<Vec<Blueprint>, ExpandError> {
            let fp = StateFp::of(state);
            self.calls.borrow_mut().push((fp, max));
            let levels = self.shared.levels.borrow().clone();
            self.shared.ops.borrow_mut().push(Op::Expand(levels));
            self.by_state.get(&fp).cloned().unwrap_or(Ok(vec![]))
        }
    }
    impl FakeE {
        fn set(&mut self, at: StateFp, bps: Vec<Blueprint>) {
            self.by_state.insert(at, Ok(bps));
        }
    }

    #[derive(Default)]
    struct FakeA {
        table: BTreeMap<String, StaticMetrics>,
        calls: RefCell<Vec<(Vec<Edit>, Vec<Edit>)>>,
    }
    impl Assessor for FakeA {
        fn assess(&self, before: &[Edit], after: &[Edit], _: &[ChangeProbe]) -> StaticMetrics {
            self.calls
                .borrow_mut()
                .push((before.to_vec(), after.to_vec()));
            self.table
                .get(&first_text(after))
                .copied()
                .unwrap_or_default()
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mode {
        Neutral,
        Ascending,
        Descending,
        Noul,
        Fail,
    }
    struct FakeJ {
        mode: Mode,
        calls: Cell<usize>,
        sizes: RefCell<Vec<usize>>,
    }
    impl Judge for FakeJ {
        fn judge(&self, req: &JudgeRequest) -> Result<Judgement, JudgeError> {
            self.calls.set(self.calls.get() + 1);
            let n = req.proposals().len();
            self.sizes.borrow_mut().push(n);
            if self.mode == Mode::Fail {
                return Err(JudgeError::Provider("down".to_owned()));
            }
            let mut priors = Vec::new();
            let mut nouls = Vec::new();
            for (i, p) in req.proposals().iter().enumerate() {
                let step = match self.mode {
                    Mode::Ascending => Some(i),
                    Mode::Descending => Some(n - 1 - i),
                    _ => None,
                };
                if let Some(k) = step {
                    let permille = u16::try_from(k * 50).unwrap();
                    priors.push((p.id(), Prior::new(permille).unwrap()));
                }
                if self.mode == Mode::Noul {
                    nouls.push((
                        p.id(),
                        Noul::new(Criterion::NamingMismatch, rel(LIB), tr(4, 5)),
                    ));
                }
            }
            Ok(Judgement::new(
                req,
                priors,
                nouls,
                ProviderTag::new("fake").unwrap(),
            ))
        }
    }

    struct Fx {
        v: FakeV,
        e: FakeE,
        a: FakeA,
        j: FakeJ,
        clock: Ticker,
        roots: Vec<Blueprint>,
        budget: SearchBudget,
        scope: Scope,
        keep_edits: bool,
        start: Vec<Edit>,
    }
    impl Fx {
        fn new(text: &str) -> Self {
            let v = FakeV::new(text);
            let e = FakeE {
                shared: v.shared.clone(),
                ..FakeE::default()
            };
            Self {
                v,
                e,
                a: FakeA::default(),
                j: FakeJ {
                    mode: Mode::Neutral,
                    calls: Cell::new(0),
                    sizes: RefCell::default(),
                },
                clock: Ticker::default(),
                roots: vec![],
                budget: SearchBudget {
                    max_depth: 1,
                    ..DEFAULT_SEARCH_BUDGET
                },
                scope: Scope::Workspace,
                keep_edits: false,
                start: vec![],
            }
        }
        fn run(&self) -> Result<SearchReport, SearchError> {
            search_decision(
                &self.v,
                &self.e,
                &self.a,
                &self.j,
                &self.clock,
                &mut (),
                &SearchInput {
                    start: &self.start,
                    roots: &self.roots,
                    targets: &[],
                    probes: &[],
                    scope: self.scope,
                    budget: self.budget,
                    weights: DEFAULT_WEIGHTS,
                    keep_trace_edits: self.keep_edits,
                },
            )
        }
        /// Registers the children `names` renamed from `a` at the root state.
        fn rename_children(&mut self, first_id: u16, names: &[&str]) -> Vec<Blueprint> {
            let bps: Vec<Blueprint> = names
                .iter()
                .enumerate()
                .map(|(i, n)| rename(first_id + u16::try_from(i).unwrap(), "a", n, BASE))
                .collect();
            self.e.set(root_state(), bps.clone());
            bps
        }
    }

    fn fates(r: &SearchReport) -> Vec<(BlueprintId, Fate)> {
        r.trace
            .iter()
            .map(|t| (t.blueprint, t.fate.clone()))
            .collect()
    }
    fn fate_of(r: &SearchReport, n: u16) -> Fate {
        r.trace
            .iter()
            .find(|t| t.blueprint == bid(n))
            .map(|t| t.fate.clone())
            .unwrap()
    }
    fn decided(r: &SearchReport) -> &NonEmpty<Candidate> {
        match &r.resolution {
            Resolution::Decided { blueprints, .. } => blueprints,
            other => panic!("expected Decided, got {other:?}"),
        }
    }
    fn failed(r: &SearchReport) -> &Infeasible {
        match &r.resolution {
            Resolution::Failed(i) => i,
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn best_planted_child_survives_with_fake_expander() {
        let mut fx = Fx::new(BASE);
        fx.rename_children(1, &["b", "c", "d"]);
        fx.a.table.insert("let b = 1;".to_owned(), clean(5));
        fx.a.table.insert("let c = 1;".to_owned(), clean(1));
        fx.v.rule = Box::new(|edits| {
            if first_text(edits) == "let d = 1;" {
                Ok(errors(3))
            } else {
                Ok(ScopedFindings::default())
            }
        });
        let r = fx.run().unwrap();

        let cs = decided(&r);
        assert_eq!(cs.len(), 2);
        assert_eq!(cs.first().blueprint(), bid(1));
        assert_eq!(cs.first().edits(), &[w("let b = 1;")]);
        assert_eq!(r.ranking.len(), 2);
        assert_eq!(r.ranking[0].path, vec![bid(1)]);
        assert_eq!(r.ranking[1].path, vec![bid(2)]);
        assert!(r.ranking[0].reward.passed);
        assert!(r.ranking[0].reward_score > r.ranking[1].reward_score);
        assert_eq!(
            fates(&r),
            vec![
                (bid(1), Fate::Passed),
                (bid(2), Fate::Passed),
                (bid(3), Fate::Failed { new_errors: 3 }),
            ]
        );
        assert!(r.trace.iter().all(|t| t.node.is_some()));
        // The assessor saw the parent's text and the child's whole text.
        assert_eq!(
            fx.a.calls.borrow()[0],
            (vec![w(BASE)], vec![w("let b = 1;")])
        );
        let d = &r.depths[0];
        assert_eq!(
            (d.generated, d.after_stages, d.kept, d.checked, d.passed),
            (3, 3, 3, 3, 2)
        );
        assert_eq!(fx.j.calls.get(), 1);
        assert_eq!(r.judge_providers, vec![ProviderTag::new("fake").unwrap()]);
        assert!(!r.budget_exhausted && !r.soft_stopped);
    }

    #[test]
    fn judge_prior_never_reorders_final() {
        let run_with = |mode: Mode| {
            let mut fx = Fx::new(BASE);
            fx.j.mode = mode;
            fx.rename_children(1, &["b", "c", "d"]);
            fx.a.table.insert("let b = 1;".to_owned(), clean(3));
            fx.a.table.insert("let c = 1;".to_owned(), clean(7));
            fx.a.table.insert("let d = 1;".to_owned(), clean(5));
            let r = fx.run().unwrap();
            let paths: Vec<Vec<BlueprintId>> = r.ranking.iter().map(|x| x.path.clone()).collect();
            let scores: Vec<i64> = r.ranking.iter().map(|x| x.reward_score).collect();
            let decided_ids: Vec<BlueprintId> =
                decided(&r).iter().map(Candidate::blueprint).collect();
            (paths, scores, decided_ids)
        };
        let up = run_with(Mode::Ascending);
        let down = run_with(Mode::Descending);
        assert_eq!(up, down);
        assert_eq!(up.2, vec![bid(2), bid(3), bid(1)]);
    }

    #[test]
    fn budget_exhausted_returns_verified_so_far() {
        let mut fx = Fx::new(BASE);
        fx.budget = SearchBudget {
            max_depth: 3,
            hard_ms: 20_000,
            ..DEFAULT_SEARCH_BUDGET
        };
        fx.v.tick = Some((fx.clock.clone(), 5_000));
        fx.rename_children(1, &["b", "c", "d", "e", "f"]);
        fx.e.set(
            state(vec![w("let b = 1;")]),
            vec![rename(9, "b", "z", "let b = 1;")],
        );
        let r = fx.run().unwrap();

        assert!(r.budget_exhausted);
        assert_eq!(fx.v.checks.borrow().len(), 3);
        assert_eq!(decided(&r).len(), 3);
        assert_eq!(
            fates(&r).into_iter().map(|(_, f)| f).collect::<Vec<_>>(),
            vec![
                Fate::Passed,
                Fate::Passed,
                Fate::Passed,
                Fate::TimedOut,
                Fate::TimedOut
            ]
        );
        assert_eq!(r.depths.len(), 1);
        assert_eq!((r.depths[0].checked, r.depths[0].timed_out), (3, 2));
        // The whole search stopped: nothing was expanded below the first depth.
        assert_eq!(fx.e.calls.borrow().len(), 1);
        assert_eq!(fx.v.audits(), 1);
    }

    #[test]
    fn hard_budget_with_nothing_verified_is_failed_exhausted() {
        let mut fx = Fx::new(BASE);
        fx.budget = SearchBudget {
            hard_ms: 1_000,
            ..DEFAULT_SEARCH_BUDGET
        };
        fx.rename_children(1, &["b", "c"]);
        let r = fx.run().unwrap();

        let i = failed(&r);
        assert!(i.budget_exhausted() && i.blocking().is_empty());
        assert!(r.budget_exhausted);
        assert!(fx.v.checks.borrow().is_empty());
        assert!(r.ranking.is_empty());
        assert_eq!(fate_of(&r, 1), Fate::TimedOut);
        assert_eq!(fx.v.audits(), 1);
    }

    #[test]
    fn check_timeout_is_budget_exhausted_blocking() {
        let timeout = |edits: &[Edit]| {
            if first_text(edits) == "let c = 1;" {
                Err(CheckError::TimedOut { limit_ms: 6_000 })
            } else {
                Ok(ScopedFindings::default())
            }
        };
        // One check is cut off, the search goes on with the others.
        let mut fx = Fx::new(BASE);
        fx.v.rule = Box::new(timeout);
        fx.rename_children(1, &["b", "c"]);
        let r = fx.run().unwrap();
        assert_eq!(decided(&r).len(), 1);
        assert_eq!(fate_of(&r, 2), Fate::TimedOut);
        assert!(!r.budget_exhausted);
        assert_eq!((r.depths[0].checked, r.depths[0].timed_out), (2, 1));

        // When nothing else passes, the cut-off check is the reason.
        let mut fx = Fx::new(BASE);
        fx.v.rule = Box::new(timeout);
        fx.e.set(root_state(), vec![rename(2, "a", "c", BASE)]);
        let r = fx.run().unwrap();
        let i = failed(&r);
        assert_eq!(
            i.blocking(),
            &[Blocking::BudgetExhausted {
                blueprint: bid(2),
                limit_ms: 6_000
            }]
        );
        assert!(!i.budget_exhausted());
    }

    #[test]
    fn per_blueprint_check_error_blocks_only_that_child() {
        let mut fx = Fx::new(BASE);
        fx.v.rule = Box::new(|edits| {
            if first_text(edits) == "let c = 1;" {
                Err(CheckError::Panicked("boom".to_owned()))
            } else {
                Ok(ScopedFindings::default())
            }
        });
        fx.rename_children(1, &["b", "c"]);
        let r = fx.run().unwrap();
        assert_eq!(decided(&r).len(), 1);
        assert_eq!(fate_of(&r, 1), Fate::Passed);
        assert_eq!(fate_of(&r, 2), Fate::Blocked(Stage::Check));
        let Resolution::Decided {
            blocking,
            decision_log,
            ..
        } = &r.resolution
        else {
            panic!("expected Decided");
        };
        assert_eq!(
            blocking,
            &vec![Blocking::CheckFailed {
                blueprint: bid(2),
                failure: CheckFailure::Panicked("boom".to_owned())
            }]
        );
        let entry = decision_log
            .entries()
            .iter()
            .find(|e| e.blueprint() == bid(2) && e.stage() == Stage::Check)
            .unwrap();
        assert!(!entry.ok());
        assert!(!r.budget_exhausted);
    }

    #[test]
    fn session_check_error_stops_the_search() {
        let mut fx = Fx::new(BASE);
        fx.v.rule = Box::new(|_| Err(CheckError::Poisoned("x".to_owned())));
        fx.rename_children(1, &["b", "c"]);
        assert!(matches!(
            fx.run(),
            Err(SearchError::Check {
                source: CheckError::Poisoned(_),
                ..
            })
        ));
        assert_eq!(fx.v.checks.borrow().len(), 1);
    }

    #[test]
    fn noul_pruned_are_audited_every_10th() {
        let mut fx = Fx::new(BASE);
        fx.j.mode = Mode::Noul;
        let names: Vec<String> = (0..21).map(|i| format!("n{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        fx.rename_children(100, &refs);
        let r = fx.run().unwrap();

        assert_eq!(fx.v.checks.borrow().len(), 3);
        let audited: Vec<BlueprintId> = r
            .trace
            .iter()
            .filter(|t| t.audited)
            .map(|t| t.blueprint)
            .collect();
        assert_eq!(audited, vec![bid(100), bid(110), bid(120)]);
        for t in &r.trace {
            if t.audited {
                assert_eq!(t.fate, Fate::Passed);
            } else {
                assert_eq!(t.fate, Fate::Pruned(PruneStage::Noul));
                assert!(t.node.is_none());
            }
        }
        assert_eq!((r.depths[0].kept, r.depths[0].audited), (0, 3));
        assert_eq!(decided(&r).len(), 3);
    }

    #[test]
    fn search_candidate_basis_is_against_the_loaded_text() {
        let mut fx = Fx::new(BASE);
        fx.budget = SearchBudget {
            max_depth: 2,
            ..DEFAULT_SEARCH_BUDGET
        };
        let other = rel("src/o.rs");
        fx.v.texts.insert(other.clone(), "abc".to_owned());
        let at_b = "let b = 1;";
        fx.rename_children(1, &["b"]);
        let end = u32::try_from(at_b.len()).unwrap();
        let child = Blueprint::new(BlueprintParts {
            id: bid(10),
            command: Command::RenameLocal {
                from: Ident::parse("b").unwrap(),
                to: Ident::parse("c").unwrap(),
            },
            splices: vec![Splice::replace(rel(LIB), tr(4, 5), "c").unwrap()],
            reads: vec![
                ReadRange::capture(rel(LIB), at_b, tr(0, end)).unwrap(),
                ReadRange::capture(other.clone(), "abc", tr(0, 3)).unwrap(),
            ],
            // against the parent's state, not the loaded text
            bases: vec![BaseFp::capture(rel(LIB), at_b)],
            spec_refs: vec![SpecRef::parse("spec#t").unwrap()],
            record: DecisionRecord::new("agent", "why").unwrap(),
            builds_on: None,
        })
        .unwrap();
        fx.e.set(state(vec![w(at_b)]), vec![child]);
        let r = fx.run().unwrap();
        let deep = decided(&r)
            .iter()
            .find(|c| c.path().len() == 2)
            .expect("a depth-2 candidate");
        assert_eq!(deep.basis().files(), &[BaseFp::capture(rel(LIB), BASE)][..]);
        assert_eq!(
            deep.basis().reads(),
            &[ReadRange::capture(other, "abc", tr(0, 3)).unwrap()][..]
        );
    }

    #[test]
    fn start_state_file_is_in_the_basis() {
        let mut fx = Fx::new(BASE);
        let s_rs = rel("src/s.rs");
        fx.v.texts.insert(s_rs.clone(), "start".to_owned());
        fx.start = vec![Edit::write(s_rs.clone(), "started")];
        fx.roots = vec![rename(1, "a", "b", BASE)];
        let r = fx.run().unwrap();
        let c = &decided(&r).as_slice()[0];
        assert_eq!(
            c.basis().files(),
            &[
                BaseFp::capture(rel(LIB), BASE),
                BaseFp::capture(s_rs, "start")
            ][..]
        );
    }

    #[test]
    fn siblings_are_checked_consecutively() {
        let mut fx = Fx::new(BASE);
        fx.budget = SearchBudget {
            max_depth: 2,
            ..DEFAULT_SEARCH_BUDGET
        };
        fx.rename_children(1, &["b", "c"]);
        let at_b = "let b = 1;";
        let at_c = "let c = 1;";
        fx.e.set(
            state(vec![w(at_b)]),
            vec![rename(10, "b", "b1", at_b), rename(11, "b", "b2", at_b)],
        );
        fx.e.set(
            state(vec![w(at_c)]),
            vec![rename(12, "c", "c1", at_c), rename(13, "c", "c2", at_c)],
        );
        for (text, n) in [
            (at_b, 7),
            (at_c, 6),
            ("let b1 = 1;", 9),
            ("let b2 = 1;", 1),
            ("let c1 = 1;", 5),
            ("let c2 = 1;", 4),
        ] {
            fx.a.table.insert(text.to_owned(), clean(n));
        }
        let r = fx.run().unwrap();

        // By score alone the order would interleave the parents: b1, c1, c2, b2.
        assert_eq!(
            fx.v.checked_texts(),
            vec![
                "let b = 1;",
                "let c = 1;",
                "let b1 = 1;",
                "let b2 = 1;",
                "let c1 = 1;",
                "let c2 = 1;"
            ]
        );
        // Two parents share the expansion budget.
        let max_seen: Vec<usize> = fx.e.calls.borrow().iter().map(|(_, m)| *m).collect();
        assert_eq!(max_seen, vec![200, 100, 100]);
        assert_eq!(r.depths.len(), 2);
        assert_eq!(r.depths[1].parents, 2);
        assert_eq!(r.ranking.len(), 6);
    }

    #[test]
    fn same_state_children_are_checked_once() {
        // Two children of one parent lead to the same state.
        let mut fx = Fx::new(BASE);
        fx.e.set(
            root_state(),
            vec![rename(1, "a", "b", BASE), rename(2, "a", "b", BASE)],
        );
        let r = fx.run().unwrap();
        assert_eq!(fx.v.checks.borrow().len(), 1);
        assert_eq!(
            fates(&r),
            vec![
                (bid(1), Fate::Passed),
                (bid(2), Fate::Pruned(PruneStage::Duplicate))
            ]
        );

        // A child leading to the state of a root that was checked first.
        let mut fx = Fx::new(BASE);
        fx.roots = vec![rename(3, "a", "b", BASE)];
        fx.e.set(root_state(), vec![rename(4, "a", "b", BASE)]);
        let r = fx.run().unwrap();
        assert_eq!(fx.v.checks.borrow().len(), 1);
        assert_eq!(fate_of(&r, 3), Fate::Passed);
        assert_eq!(fate_of(&r, 4), Fate::Pruned(PruneStage::Duplicate));
    }

    #[test]
    fn judge_called_once_per_depth() {
        let mut fx = Fx::new(BASE);
        fx.budget = SearchBudget {
            max_depth: 2,
            ..DEFAULT_SEARCH_BUDGET
        };
        fx.rename_children(1, &["b", "c"]);
        fx.e.set(
            state(vec![w("let b = 1;")]),
            vec![rename(10, "b", "b1", "let b = 1;")],
        );
        fx.e.set(
            state(vec![w("let c = 1;")]),
            vec![rename(11, "c", "c1", "let c = 1;")],
        );
        let r = fx.run().unwrap();
        assert_eq!(fx.j.calls.get(), 2);
        assert_eq!(*fx.j.sizes.borrow(), vec![2, 2]);
        assert_eq!(r.judge_providers.len(), 2);

        // Only roots: the judge is never asked.
        let mut fx = Fx::new(BASE);
        fx.roots = vec![rename(1, "a", "b", BASE)];
        let r = fx.run().unwrap();
        assert_eq!(fx.j.calls.get(), 0);
        assert!(r.judge_providers.is_empty());
        assert_eq!(decided(&r).len(), 1);
    }

    #[test]
    fn judge_failure_is_recorded_and_search_goes_on() {
        let mut fx = Fx::new(BASE);
        fx.j.mode = Mode::Fail;
        fx.rename_children(1, &["b", "c"]);
        let r = fx.run().unwrap();
        assert_eq!(
            r.judge_errors,
            vec![JudgeError::Provider("down".to_owned())]
        );
        assert!(r.judge_providers.is_empty());
        assert_eq!(decided(&r).len(), 2);
        assert!(r.ranking.iter().all(|x| x.prior == Prior::NEUTRAL));
    }

    #[test]
    fn child_blueprint_reads_parent_state() {
        let mut fx = Fx::new(BASE);
        fx.budget = SearchBudget {
            max_depth: 2,
            ..DEFAULT_SEARCH_BUDGET
        };
        let at_b = "let b = 1;";
        // Reads the text of the state after the rename: fresh at depth 2, stale against the base.
        fx.roots = vec![rename(3, "b", "c", at_b)];
        fx.e.set(root_state(), vec![rename(1, "a", "b", BASE)]);
        fx.e.set(state(vec![w(at_b)]), vec![rename(2, "b", "c", at_b)]);
        let r = fx.run().unwrap();

        assert_eq!(fate_of(&r, 3), Fate::Blocked(Stage::Freshness));
        assert_eq!(fate_of(&r, 1), Fate::Passed);
        assert_eq!(fate_of(&r, 2), Fate::Passed);
        assert_eq!(fx.v.checked_texts(), vec!["let b = 1;", "let c = 1;"]);
        assert!(r.ranking.iter().any(|x| x.path == vec![bid(1), bid(2)]));
        let Resolution::Decided { decision_log, .. } = &r.resolution else {
            panic!("expected Decided");
        };
        let last = decision_log
            .entries()
            .iter()
            .rfind(|e| e.blueprint() == bid(3))
            .unwrap();
        assert_eq!((last.stage(), last.ok()), (Stage::Freshness, false));
    }

    #[test]
    fn child_inherits_build_script_role_of_parent() {
        let mut fx = Fx::new(BASE);
        fx.budget = SearchBudget {
            max_depth: 2,
            ..DEFAULT_SEARCH_BUDGET
        };
        fx.v.texts.insert(rel("build.rs"), BASE.to_owned());
        fx.v.build_scripts.insert(rel("build.rs"));
        // The parent renames in the build script, the child in an ordinary source file.
        let parent = Blueprint::new(BlueprintParts {
            id: bid(1),
            command: Command::RenameLocal {
                from: Ident::parse("a").unwrap(),
                to: Ident::parse("b").unwrap(),
            },
            splices: vec![Splice::replace(rel("build.rs"), tr(4, 5), "b").unwrap()],
            reads: vec![ReadRange::capture(rel("build.rs"), BASE, tr(0, 10)).unwrap()],
            bases: vec![BaseFp::capture(rel("build.rs"), BASE)],
            spec_refs: vec![SpecRef::parse("spec#t").unwrap()],
            record: DecisionRecord::new("agent", "why").unwrap(),
            builds_on: None,
        })
        .unwrap();
        fx.e.set(root_state(), vec![parent]);
        fx.e.set(
            state(vec![Edit::write(rel("build.rs"), "let b = 1;")]),
            vec![rename(2, "a", "b", BASE)],
        );
        let r = fx.run().unwrap();

        let child = decided(&r)
            .iter()
            .find(|c| c.blueprint() == bid(2))
            .unwrap();
        assert_eq!(child.path().len(), 2);
        assert!(child.marks().contains(&RiskMark::BuildScript));
        assert!(child.unseen_classes().is_some());
        let parent = decided(&r)
            .iter()
            .find(|c| c.blueprint() == bid(1))
            .unwrap();
        assert!(parent.marks().contains(&RiskMark::BuildScript));
    }

    #[test]
    fn revert_audit_runs_once_at_end() {
        let mut fx = Fx::new(BASE);
        fx.budget = SearchBudget {
            max_depth: 2,
            ..DEFAULT_SEARCH_BUDGET
        };
        fx.rename_children(1, &["b", "c"]);
        fx.e.set(
            state(vec![w("let b = 1;")]),
            vec![rename(10, "b", "b1", "let b = 1;")],
        );
        fx.run().unwrap();
        assert_eq!(fx.v.audits(), 1);
        assert_eq!(fx.v.events.borrow().last(), Some(&"audit"));
        assert!(fx.v.checks.borrow().len() >= 3);

        // A search that fails is audited once too.
        let mut fx = Fx::new(BASE);
        fx.v.rule = Box::new(|_| Ok(errors(5)));
        fx.rename_children(1, &["b"]);
        let r = fx.run().unwrap();
        assert!(matches!(r.resolution, Resolution::Failed(_)));
        assert_eq!(fx.v.audits(), 1);

        // A failed audit is an error.
        let mut fx = Fx::new(BASE);
        fx.v.audit_error = Some(CheckError::RevertMismatch {
            paths: vec![rel(LIB)],
        });
        fx.rename_children(1, &["b"]);
        assert!(matches!(
            fx.run(),
            Err(SearchError::RevertAudit(CheckError::RevertMismatch { .. }))
        ));
    }

    #[test]
    fn roots_are_never_pruned() {
        let mut fx = Fx::new(BASE);
        fx.budget = SearchBudget {
            beam: 1,
            ..fx.budget
        };
        fx.roots = vec![rename(1, "a", "b", BASE), rename(2, "a", "c", BASE)];
        fx.e.set(
            root_state(),
            vec![
                rename(3, "a", "d", BASE),
                rename(4, "a", "e", BASE),
                rename(5, "a", "f", BASE),
            ],
        );
        fx.a.table.insert("let d = 1;".to_owned(), clean(9));
        let r = fx.run().unwrap();

        assert_eq!(fx.v.checked_texts(), vec!["let b = 1;", "let c = 1;"]);
        assert_eq!(fate_of(&r, 1), Fate::Passed);
        assert_eq!(fate_of(&r, 2), Fate::Passed);
        for n in [3, 4, 5] {
            assert!(matches!(fate_of(&r, n), Fate::Pruned(_)), "{n}");
        }
        assert!(r.trace.iter().filter(|t| t.root).count() == 2);
        // Roots are not shown to the judge.
        assert_eq!(*fx.j.sizes.borrow(), vec![3]);
    }

    #[test]
    fn root_with_builds_on_is_refused() {
        let plain = rename(1, "a", "b", BASE);
        let other = rename(2, "a", "c", BASE);
        let chained = Blueprint::new(BlueprintParts {
            id: other.id(),
            command: other.command().clone(),
            splices: other.splices().to_vec(),
            reads: other.reads().to_vec(),
            bases: other.bases().to_vec(),
            spec_refs: other.spec_refs().to_vec(),
            record: other.record().clone(),
            builds_on: Some(plain.id()),
        })
        .unwrap();
        let later = rename(4, "a", "e", BASE);
        let later = Blueprint::new(BlueprintParts {
            id: later.id(),
            command: later.command().clone(),
            splices: later.splices().to_vec(),
            reads: later.reads().to_vec(),
            bases: later.bases().to_vec(),
            spec_refs: later.spec_refs().to_vec(),
            record: later.record().clone(),
            builds_on: Some(plain.id()),
        })
        .unwrap();
        let mut fx = Fx::new(BASE);
        // The first chained root is named, not the last.
        fx.roots = vec![plain.clone(), chained, later];
        fx.e.set(root_state(), vec![rename(3, "a", "d", BASE)]);
        let err = fx.run().unwrap_err();
        assert!(
            matches!(&err, SearchError::ChainedRoot { blueprint, parent }
                if *blueprint == bid(2) && *parent == plain.id()),
            "{err}"
        );
        assert!(fx.v.events.borrow().is_empty(), "no check, no audit");
        assert!(fx.v.checked_texts().is_empty());
        assert!(fx.e.calls.borrow().is_empty(), "no expansion");
    }

    #[test]
    fn no_candidates_is_error() {
        let fx = Fx::new(BASE);
        assert!(matches!(fx.run(), Err(SearchError::NoCandidates)));
        assert!(fx.v.events.borrow().is_empty());
    }

    #[test]
    fn expand_errors_are_recorded_and_session_error_is_fatal() {
        let mut fx = Fx::new(BASE);
        fx.roots = vec![rename(1, "a", "b", BASE)];
        fx.e.by_state.insert(
            root_state(),
            Err(ExpandError::Analysis("no targets".to_owned())),
        );
        let r = fx.run().unwrap();
        assert_eq!(
            r.expand_errors,
            vec![ExpandError::Analysis("no targets".to_owned())]
        );
        assert_eq!(decided(&r).len(), 1);

        let mut fx = Fx::new(BASE);
        fx.roots = vec![rename(1, "a", "b", BASE)];
        fx.e.by_state.insert(
            root_state(),
            Err(ExpandError::Session(CheckError::Poisoned("x".to_owned()))),
        );
        assert!(matches!(
            fx.run(),
            Err(SearchError::Expand(ExpandError::Session(_)))
        ));
        assert!(fx.v.checks.borrow().is_empty());
    }

    #[test]
    fn decided_lists_every_passing_node_by_reward() {
        let mut fx = Fx::new(BASE);
        fx.rename_children(1, &["b", "c", "d", "e", "f"]);
        for (text, n) in [
            ("let b = 1;", 2),
            ("let c = 1;", 5),
            ("let d = 1;", 1),
            ("let e = 1;", 4),
            ("let f = 1;", 3),
        ] {
            fx.a.table.insert(text.to_owned(), clean(n));
        }
        let r = fx.run().unwrap();

        let ids: Vec<BlueprintId> = decided(&r).iter().map(Candidate::blueprint).collect();
        assert_eq!(ids, vec![bid(2), bid(4), bid(5), bid(1), bid(3)]);
        let ranked: Vec<BlueprintId> = r.ranking.iter().map(|x| x.path[0]).collect();
        assert_eq!(ranked, ids);
        let Resolution::Decided { evidence, .. } = &r.resolution else {
            panic!("expected Decided");
        };
        assert_eq!(evidence.verdicts().len(), 5);
        assert_eq!(evidence.budget(), fx.budget.decision_budget());
        assert_eq!(evidence.provenance().analyzer(), "fake");
        assert_eq!(
            evidence.blind_classes(),
            &BTreeSet::from(crate::app::ALWAYS_BLIND)
        );
    }

    #[test]
    fn path_marks_are_the_union_of_the_path() {
        // The root extracts a variable (Borrow, Behaviour); the leaf renames it (no marks).
        let base = "f(1 + 2);";
        let after = "let x = 1 + 2;\nf(x);";
        let mut fx = Fx::new(base);
        fx.budget = SearchBudget {
            max_depth: 2,
            ..DEFAULT_SEARCH_BUDGET
        };
        fx.e.set(root_state(), vec![extract_x(1, base)]);
        fx.e.set(state(vec![w(after)]), vec![rename(2, "x", "y", after)]);
        let r = fx.run().unwrap();
        let leaf = decided(&r)
            .iter()
            .find(|c| c.blueprint() == bid(2))
            .unwrap();
        assert_eq!(leaf.path().len(), 2);
        assert_eq!(
            leaf.marks(),
            &BTreeSet::from([RiskMark::Borrow, RiskMark::Behaviour])
        );
        assert_eq!(
            leaf.unseen_classes().unwrap().classes(),
            &BTreeSet::from([UnseenClass::Borrow])
        );

        // The other order: the root renames (no marks), the leaf extracts, so the leaf's own
        // marks count as well as the root's.
        let renamed = "g(1 + 2);";
        let mut fx = Fx::new(base);
        fx.budget = SearchBudget {
            max_depth: 2,
            ..DEFAULT_SEARCH_BUDGET
        };
        fx.e.set(root_state(), vec![rename(3, "f", "g", base)]);
        fx.e.set(state(vec![w(renamed)]), vec![extract_x(4, renamed)]);
        let r = fx.run().unwrap();
        let leaf = decided(&r)
            .iter()
            .find(|c| c.blueprint() == bid(4))
            .unwrap();
        assert_eq!(leaf.path().len(), 2);
        assert_eq!(
            leaf.marks(),
            &BTreeSet::from([RiskMark::Borrow, RiskMark::Behaviour])
        );
    }

    #[test]
    fn soft_budget_stops_after_the_depth() {
        let mut fx = Fx::new(BASE);
        fx.budget = SearchBudget {
            max_depth: 3,
            soft_ms: 10_000,
            ..DEFAULT_SEARCH_BUDGET
        };
        fx.v.tick = Some((fx.clock.clone(), 6_000));
        fx.rename_children(1, &["b", "c"]);
        fx.e.set(
            state(vec![w("let b = 1;")]),
            vec![rename(10, "b", "b1", "let b = 1;")],
        );
        let r = fx.run().unwrap();
        assert!(r.soft_stopped && !r.budget_exhausted);
        assert_eq!(r.depths.len(), 1);
        assert_eq!(r.elapsed_ms, 12_000);
        assert_eq!(decided(&r).len(), 2);
        assert_eq!(fx.e.calls.borrow().len(), 1);
    }

    #[test]
    fn trace_has_one_entry_per_child_and_edits_only_on_request() {
        let build = |keep: bool| {
            let mut fx = Fx::new(BASE);
            fx.keep_edits = keep;
            fx.e.set(
                root_state(),
                vec![
                    rename(1, "a", "b", BASE),
                    // Reads text the base does not have: stale.
                    rename(2, "a", "z", "let a = 9;"),
                ],
            );
            let r = fx.run().unwrap();
            (r, fx)
        };
        let (r, _) = build(false);
        assert_eq!(r.trace.len(), 2);
        assert!(r.trace.iter().all(|t| t.edits.is_none() && t.depth == 1));
        assert_eq!(r.trace[0].command, CommandKind::RenameLocal);
        assert_eq!(r.trace[0].state, Some(state(vec![w("let b = 1;")])));
        assert_eq!(r.trace[1].fate, Fate::Blocked(Stage::Freshness));
        assert_eq!(r.trace[1].state, None);
        assert_eq!(r.trace[1].node, None);
        assert_eq!((r.depths[0].generated, r.depths[0].after_stages), (2, 1));

        let (r, _) = build(true);
        assert_eq!(r.trace[0].edits, Some(vec![w("let b = 1;")]));
        assert_eq!(r.trace[1].edits, None);
        assert_eq!(
            r.trace[0].proposal,
            ProposalId::of(&rename(1, "a", "b", BASE))
        );
        assert!(!r.trace[0].root && r.trace[0].rank == Some(0));
    }

    /// Renames the first `from` of `text`, the whole text of `path`, to `to`.
    fn rename_in(n: u16, path: &str, from: &str, to: &str, text: &str) -> Blueprint {
        let at = u32::try_from(text.find(from).unwrap()).unwrap();
        let end = at + u32::try_from(from.len()).unwrap();
        let len = u32::try_from(text.len()).unwrap();
        Blueprint::new(BlueprintParts {
            id: bid(n),
            command: Command::RenameLocal {
                from: Ident::parse(from).unwrap(),
                to: Ident::parse(to).unwrap(),
            },
            splices: vec![Splice::replace(rel(path), tr(at, end), to).unwrap()],
            reads: vec![ReadRange::capture(rel(path), text, tr(0, len)).unwrap()],
            bases: vec![BaseFp::capture(rel(path), text)],
            spec_refs: vec![SpecRef::parse("spec#t").unwrap()],
            record: DecisionRecord::new("agent", "why").unwrap(),
            builds_on: None,
        })
        .unwrap()
    }

    /// Two parents at depth 1, `b` then `c` by reward, each with two children at depth 2:
    /// `b1`, `b2` and `c1`, `c2`.
    fn two_parents_two_children() -> Fx {
        let mut fx = Fx::new(BASE);
        fx.budget = SearchBudget {
            max_depth: 2,
            ..DEFAULT_SEARCH_BUDGET
        };
        fx.rename_children(1, &["b", "c"]);
        let at_b = "let b = 1;";
        let at_c = "let c = 1;";
        fx.e.set(
            state(vec![w(at_b)]),
            vec![rename(10, "b", "b1", at_b), rename(11, "b", "b2", at_b)],
        );
        fx.e.set(
            state(vec![w(at_c)]),
            vec![rename(12, "c", "c1", at_c), rename(13, "c", "c2", at_c)],
        );
        for (text, n) in [
            (at_b, 7),
            (at_c, 6),
            ("let b1 = 1;", 9),
            ("let b2 = 1;", 1),
            ("let c1 = 1;", 5),
            ("let c2 = 1;", 4),
        ] {
            fx.a.table.insert(text.to_owned(), clean(n));
        }
        fx
    }

    const S_RS: &str = "src/s.rs";
    fn started() -> Vec<Edit> {
        vec![Edit::write(rel(S_RS), "started")]
    }
    /// A search from the start state `started()` over `src/s.rs` at "start".
    fn from_start() -> Fx {
        let mut fx = Fx::new(BASE);
        fx.v.texts.insert(rel(S_RS), "start".to_owned());
        fx.start = started();
        fx
    }
    /// The state of `edits` on top of the start state.
    fn on_start(mut edits: Vec<Edit>) -> StateFp {
        edits.extend(started());
        edits.sort_by(|a, b| a.path.cmp(&b.path));
        state(edits)
    }

    #[test]
    fn children_are_written_on_top_of_their_parent() {
        let fx = two_parents_two_children();
        let r = fx.run().unwrap();
        assert_eq!(r.ranking.len(), 6);
        let lv = |text: &str| vec![w(text)];
        let child = |text: &str| {
            vec![
                Op::Write(lv(text)),
                Op::Diagnose(2),
                Op::FilesOf,
                Op::Pop(lv(text)),
            ]
        };
        let (b, c) = ("let b = 1;", "let c = 1;");
        let mut want = vec![Op::Fork, Op::Expand(vec![])];
        for text in [b, c] {
            want.extend([
                Op::Write(lv(text)),
                Op::Diagnose(1),
                Op::FilesOf,
                Op::Pop(lv(text)),
            ]);
        }
        // Depth 2: each parent's path for its expansion, then for its children's checks.
        want.extend([
            Op::Write(lv(b)),
            Op::Expand(vec![lv(b)]),
            Op::Pop(lv(b)),
            Op::Write(lv(c)),
            Op::Expand(vec![lv(c)]),
            Op::Pop(lv(c)),
            Op::Write(lv(b)),
        ]);
        want.extend(child("let b1 = 1;"));
        want.extend(child("let b2 = 1;"));
        want.extend([Op::Pop(lv(b)), Op::Write(lv(c))]);
        want.extend(child("let c1 = 1;"));
        want.extend(child("let c2 = 1;"));
        want.extend([Op::Release, Op::Audit]);
        assert_eq!(fx.v.ops(), want);
        // Each diagnosis is at the level of the depth of the child it judges.
        let writes: Vec<u32> = r.depths.iter().map(|d| d.writes).collect();
        assert_eq!(writes, vec![2, 8]);
        assert_eq!(
            writes.iter().sum::<u32>(),
            u32::try_from(fx.v.writes().len()).unwrap()
        );
    }

    #[test]
    fn repath_pops_to_the_common_prefix() {
        let mut fx = from_start();
        fx.budget = SearchBudget {
            max_depth: 3,
            ..DEFAULT_SEARCH_BUDGET
        };
        let (at_b, at_c) = ("let b = 1;", "let c = 1;");
        fx.e.set(
            on_start(vec![]),
            vec![rename(1, "a", "b", BASE), rename(2, "a", "c", BASE)],
        );
        fx.e.set(
            on_start(vec![w(at_b)]),
            vec![rename(10, "b", "b1", at_b), rename(11, "b", "b2", at_b)],
        );
        fx.e.set(on_start(vec![w(at_c)]), vec![rename(12, "c", "c1", at_c)]);
        // The parents of depth 3 in the order b1, b2, c1.
        for (text, n) in [
            (at_b, 7),
            (at_c, 6),
            ("let b1 = 1;", 9),
            ("let b2 = 1;", 8),
            ("let c1 = 1;", 1),
        ] {
            fx.a.table.insert(text.to_owned(), clean(n));
        }
        let r = fx.run().unwrap();
        assert_eq!(r.depths.len(), 3);
        assert_eq!(r.depths[2].parents, 3);

        let ops = fx.v.ops();
        let expands: Vec<usize> = ops
            .iter()
            .enumerate()
            .filter(|(_, op)| matches!(op, Op::Expand(_)))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(expands.len(), 6, "{ops:?}");
        let levels_at = |i: usize| match &ops[i] {
            Op::Expand(levels) => levels.clone(),
            other => panic!("{other:?}"),
        };
        // Every parent carries the start state at the bottom, written once and never popped.
        for &i in &expands {
            assert_eq!(levels_at(i).first(), Some(&started()), "{ops:?}");
        }
        assert_eq!(fx.v.writes().iter().filter(|e| **e == started()).count(), 1);
        assert!(!ops.contains(&Op::Pop(started())), "{ops:?}");
        // Between the parents of depth 3, the pops and writes are those `repath` gives.
        let mut shared = Vec::new();
        for pair in expands[3..].windows(2) {
            let (held, next) = (levels_at(pair[0]), levels_at(pair[1]));
            let common = held.iter().zip(&next).take_while(|(h, n)| h == n).count();
            shared.push(common);
            let between = &ops[pair[0] + 1..pair[1]];
            let pops = between.iter().filter(|op| matches!(op, Op::Pop(_))).count();
            let writes: Vec<Vec<Edit>> = between
                .iter()
                .filter_map(|op| match op {
                    Op::Write(edits) => Some(edits.clone()),
                    _ => None,
                })
                .collect();
            assert_eq!(pops, held.len() - common, "{between:?}");
            assert_eq!(writes, next[common..].to_vec(), "{between:?}");
            assert_eq!(between.len(), pops + writes.len(), "{between:?}");
        }
        // Two siblings share their grandparent; cousins share only the start state.
        assert_eq!(shared, vec![2, 1]);
    }

    #[test]
    fn parents_are_expanded_on_the_fork() {
        let fx = two_parents_two_children();
        fx.run().unwrap();
        let ops = fx.v.ops();
        let expanded: Vec<Vec<Vec<Edit>>> = ops
            .iter()
            .filter_map(|op| match op {
                Op::Expand(levels) => Some(levels.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            expanded,
            vec![
                vec![],
                vec![vec![w("let b = 1;")]],
                vec![vec![w("let c = 1;")]]
            ]
        );
        // One fork for the whole search, released after the last expansion and check.
        assert_eq!(ops.iter().filter(|op| **op == Op::Fork).count(), 1);
        assert_eq!(ops.iter().filter(|op| **op == Op::Release).count(), 1);
        assert_eq!(ops[ops.len() - 2..], [Op::Release, Op::Audit]);
    }

    #[test]
    fn the_verdict_keeps_the_findings_below() {
        let (a_rs, b_rs) = ("src/a.rs", "src/b.rs");
        let mut fx = Fx::new(BASE);
        fx.budget = SearchBudget {
            max_depth: 2,
            ..DEFAULT_SEARCH_BUDGET
        };
        fx.v.texts.insert(rel(a_rs), "let a = 1;".to_owned());
        fx.v.texts.insert(rel(b_rs), "let b = 1;".to_owned());
        let on_b = |message: &str| Finding {
            code: FindingCode::Rustc(ErrorCode::parse("E0599").unwrap()),
            message: message.to_owned(),
            path: rel(b_rs),
            line: 1,
        };
        fx.v.baseline = vec![on_b("old")];
        let parent = vec![Edit::write(rel(b_rs), "let p = 1;")];
        let at_parent = parent.clone();
        fx.v.rule = Box::new(move |edits| {
            let a_text = edits
                .iter()
                .find(|e| e.path == rel(a_rs))
                .and_then(|e| e.new_text.clone());
            Ok(match a_text.as_deref() {
                // The parent's level covers b, with its baseline error and an unseen class.
                None if edits == at_parent.as_slice() => ScopedFindings {
                    files: vec![rel(a_rs), rel(b_rs)],
                    findings: vec![on_b("old")],
                    unseen_at: BTreeMap::from([(
                        UnseenClass::UnresolvedPath,
                        BTreeSet::from([rel(b_rs)]),
                    )]),
                },
                // This child's level covers a only and adds nothing.
                Some("let c = 1;") => ScopedFindings {
                    files: vec![rel(a_rs)],
                    ..Default::default()
                },
                // This one diagnoses b again, with one more error.
                Some("let d = 1;") => ScopedFindings {
                    files: vec![rel(a_rs), rel(b_rs)],
                    findings: vec![on_b("more"), on_b("old")],
                    ..Default::default()
                },
                _ => ScopedFindings::default(),
            })
        });
        // A check of the composed edits covers the files they edit.
        fx.v.files = Some(Box::new(|edits| {
            edits.iter().map(|e| e.path.clone()).collect()
        }));
        fx.e.set(
            root_state(),
            vec![rename_in(1, b_rs, "b", "p", "let b = 1;")],
        );
        fx.e.set(
            state(parent),
            vec![
                rename_in(2, a_rs, "a", "c", "let a = 1;"),
                rename_in(3, a_rs, "a", "d", "let a = 1;"),
            ],
        );
        let r = fx.run().unwrap();
        assert_eq!(fate_of(&r, 1), Fate::Passed);
        assert_eq!(fate_of(&r, 2), Fate::Passed);
        assert_eq!(fate_of(&r, 3), Fate::Failed { new_errors: 1 });
        let Resolution::Decided { evidence, .. } = &r.resolution else {
            panic!("expected Decided");
        };
        let verdict = |n: u16| {
            evidence
                .verdicts()
                .iter()
                .find(|(id, _)| *id == bid(n))
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        // The class the parent observed on b is kept for the child that left b alone, and not
        // for the one that diagnosed b again without it.
        let kept = BTreeSet::from([UnseenClass::UnresolvedPath]);
        assert_eq!(verdict(1).unseen, kept);
        assert_eq!(verdict(2).unseen, kept);
        assert!(verdict(3).unseen.is_empty());
        assert_eq!(verdict(3).new_errors.len(), 1);
    }

    #[test]
    fn the_start_state_is_diagnosed_once() {
        let mut fx = from_start();
        fx.budget = SearchBudget {
            max_depth: 2,
            ..DEFAULT_SEARCH_BUDGET
        };
        let at_b = "let b = 1;";
        fx.e.set(
            on_start(vec![]),
            vec![rename(1, "a", "b", BASE), rename(2, "a", "c", BASE)],
        );
        fx.e.set(on_start(vec![w(at_b)]), vec![rename(10, "b", "b1", at_b)]);
        let r = fx.run().unwrap();
        assert_eq!(r.depths.len(), 2);
        assert_eq!(decided(&r).len(), 3);
        let ops = fx.v.ops();
        assert_eq!(
            ops.iter().filter(|op| **op == Op::Write(started())).count(),
            1
        );
        assert_eq!(
            ops.iter().filter(|op| **op == Op::Diagnose(1)).count(),
            1,
            "{ops:?}"
        );
        // Diagnosed at the first check of a root child, before that child is written.
        let at = ops.iter().position(|op| *op == Op::Diagnose(1)).unwrap();
        assert_eq!(
            ops[at - 2..at + 3],
            [
                Op::Write(started()),
                Op::Expand(vec![started()]),
                Op::Diagnose(1),
                Op::FilesOf,
                Op::Write(vec![w(at_b)]),
            ]
        );
        assert_eq!(fx.v.checks.borrow()[0], started());
    }

    #[test]
    fn a_failed_start_level_fails_every_root_child() {
        let panicked = CheckError::DetectorPanicked {
            detector: "d".to_owned(),
            file: rel(S_RS),
        };
        let refused = CheckError::Edit {
            path: rel(S_RS),
            reason: "no".to_owned(),
        };
        for (what, failure) in [("diagnosis", panicked), ("write", refused)] {
            let mut fx = from_start();
            // The second root writes the start's path back to its base text.
            fx.roots = vec![
                rename(1, "a", "b", BASE),
                rename_in(2, S_RS, "started", "start", "started"),
            ];
            let fail = failure.clone();
            if what == "diagnosis" {
                fx.v.rule = Box::new(move |edits| {
                    if edits == started().as_slice() {
                        Err(fail.clone())
                    } else {
                        Ok(ScopedFindings::default())
                    }
                });
            } else {
                fx.v.write_fault =
                    Box::new(move |edits, _| (edits == started().as_slice()).then(|| fail.clone()));
            }
            let r = fx.run().unwrap();
            let failure = failure.clone().into_blocking(bid(1)).unwrap();
            let Blocking::CheckFailed { failure, .. } = failure else {
                panic!("{what}: {failure:?}");
            };
            assert_eq!(
                failed(&r).blocking(),
                &[
                    Blocking::CheckFailed {
                        blueprint: bid(1),
                        failure: failure.clone()
                    },
                    Blocking::CheckFailed {
                        blueprint: bid(2),
                        failure
                    },
                ],
                "{what}"
            );
            assert_eq!(fate_of(&r, 1), Fate::Blocked(Stage::Check), "{what}");
            assert_eq!(fate_of(&r, 2), Fate::Blocked(Stage::Check), "{what}");
            // No child level is written: the start's is the only write.
            assert_eq!(fx.v.writes(), vec![started()], "{what}");
            assert_eq!(r.depths[0].checked, 0, "{what}");
            if what == "write" {
                assert_eq!(r.expand_errors.len(), 1);
                assert!(fx.e.calls.borrow().is_empty(), "the root yields nothing");
            }
        }
    }

    #[test]
    fn a_refused_path_write_blocks_the_children() {
        // The writes of b's level: its own check (0), its expansion (1), its children's (2).
        let refused = CheckError::Edit {
            path: rel(LIB),
            reason: "no".to_owned(),
        };
        let at_b = vec![w("let b = 1;")];
        let mut fx = two_parents_two_children();
        let (fail, b) = (refused.clone(), at_b.clone());
        fx.v.write_fault = Box::new(move |edits, n| (edits == b && n == 2).then(|| fail.clone()));
        let r = fx.run().unwrap();
        for n in [10, 11] {
            assert_eq!(fate_of(&r, n), Fate::Blocked(Stage::Check), "{n}");
        }
        for n in [12, 13] {
            assert_eq!(fate_of(&r, n), Fate::Passed, "{n}");
        }
        let Resolution::Decided { blocking, .. } = &r.resolution else {
            panic!("expected Decided");
        };
        assert_eq!(blocking.len(), 2);
        assert!(blocking.iter().all(|x| matches!(
            x,
            Blocking::CheckFailed {
                failure: CheckFailure::Edit { .. },
                ..
            }
        )));
        assert!(!fx.v.writes().contains(&vec![w("let b1 = 1;")]));
        assert!(r.expand_errors.is_empty());

        // Refused for b's expansion: an expand error, and b yields nothing.
        let mut fx = two_parents_two_children();
        let (fail, b) = (refused, at_b);
        fx.v.write_fault = Box::new(move |edits, n| (edits == b && n == 1).then(|| fail.clone()));
        let r = fx.run().unwrap();
        assert_eq!(r.expand_errors.len(), 1);
        assert!(matches!(r.expand_errors[0], ExpandError::Analysis(_)));
        assert!(
            r.trace
                .iter()
                .all(|t| t.blueprint != bid(10) && t.blueprint != bid(11))
        );
        assert_eq!(fate_of(&r, 12), Fate::Passed);
        assert_eq!(fate_of(&r, 13), Fate::Passed);
        assert_eq!(fx.e.calls.borrow().len(), 2, "the root and c only");
    }

    #[test]
    fn a_session_failure_of_a_pop_stops_the_search() {
        let mut fx = Fx::new(BASE);
        fx.rename_children(1, &["b", "c"]);
        fx.v.pop_fault = Box::new(|top, _| {
            (top == [w("let b = 1;")]).then(|| CheckError::RevertMismatch {
                paths: vec![rel(LIB)],
            })
        });
        let err = fx.run().unwrap_err();
        assert!(
            matches!(&err, SearchError::Check { blueprint, source: CheckError::RevertMismatch { .. } }
                if *blueprint == bid(1)),
            "{err}"
        );
        let ops = fx.v.ops();
        assert_eq!(ops.last(), Some(&Op::Release));
        assert!(!ops.contains(&Op::Audit));
        assert_eq!(fx.v.audits(), 0);
    }

    #[test]
    fn a_session_failure_outside_a_childs_level_is_a_fork_error() {
        let poisoned = || CheckError::Poisoned("x".to_owned());
        let at_b = vec![w("let b = 1;")];
        let cases: Vec<(&str, Fx)> = vec![
            ("the fork's open", {
                let mut fx = Fx::new(BASE);
                fx.rename_children(1, &["b"]);
                fx.v.fork_error = Some(poisoned());
                fx
            }),
            ("the start level's write", {
                let mut fx = from_start();
                fx.roots = vec![rename(1, "a", "b", BASE)];
                fx.v.write_fault = Box::new(move |edits, _| (edits == started()).then(poisoned));
                fx
            }),
            ("the start level's diagnosis", {
                let mut fx = from_start();
                fx.roots = vec![rename(1, "a", "b", BASE)];
                fx.v.rule = Box::new(move |edits| {
                    if edits == started().as_slice() {
                        Err(poisoned())
                    } else {
                        Ok(ScopedFindings::default())
                    }
                });
                fx
            }),
            ("a re-path pop for an expansion", {
                let mut fx = two_parents_two_children();
                let b = at_b.clone();
                fx.v.pop_fault = Box::new(move |top, n| (top == b && n == 1).then(poisoned));
                fx
            }),
            ("a re-path write for checks", {
                let mut fx = two_parents_two_children();
                let b = at_b.clone();
                fx.v.write_fault = Box::new(move |edits, n| (edits == b && n == 2).then(poisoned));
                fx
            }),
        ];
        for (what, fx) in cases {
            let err = fx.run().unwrap_err();
            assert!(
                matches!(err, SearchError::Fork(CheckError::Poisoned(_))),
                "{what}: {err}"
            );
            let ops = fx.v.ops();
            assert!(!ops.contains(&Op::Audit), "{what}");
            if what == "the fork's open" {
                assert_eq!(ops, vec![Op::Fork], "nothing to release");
            } else {
                assert_eq!(ops.last(), Some(&Op::Release), "{what}");
            }
        }
    }

    #[test]
    fn the_fork_is_released_before_the_audit_and_on_error() {
        let mut fx = Fx::new(BASE);
        fx.rename_children(1, &["b"]);
        fx.run().unwrap();
        let ops = fx.v.ops();
        assert_eq!(ops[ops.len() - 2..], [Op::Release, Op::Audit]);

        // An expander that loses its session: released, not audited.
        let mut fx = Fx::new(BASE);
        fx.roots = vec![rename(1, "a", "b", BASE)];
        fx.e.by_state.insert(
            root_state(),
            Err(ExpandError::Session(CheckError::Poisoned("x".to_owned()))),
        );
        assert!(matches!(fx.run(), Err(SearchError::Expand(_))));
        assert_eq!(fx.v.ops().last(), Some(&Op::Release));
        assert!(!fx.v.ops().contains(&Op::Audit));

        // Nothing to try: released all the same.
        let fx = Fx::new(BASE);
        assert!(matches!(fx.run(), Err(SearchError::NoCandidates)));
        assert_eq!(fx.v.ops(), vec![Op::Fork, Op::Expand(vec![]), Op::Release]);
    }

    #[test]
    fn a_failed_release_is_a_search_error() {
        let released = || CheckError::Poisoned("release".to_owned());
        // Instead of a report.
        let mut fx = Fx::new(BASE);
        fx.rename_children(1, &["b"]);
        fx.v.release_error = Some(released());
        let err = fx.run().unwrap_err();
        assert!(
            matches!(err, SearchError::Fork(CheckError::Poisoned(ref m)) if m == "release"),
            "{err}"
        );
        assert_eq!(fx.v.audits(), 0);

        // Instead of `NoCandidates`.
        let mut fx = Fx::new(BASE);
        fx.v.release_error = Some(released());
        let err = fx.run().unwrap_err();
        assert!(matches!(err, SearchError::Fork(_)), "{err}");

        // A child's session failure first: that is the error.
        let mut fx = Fx::new(BASE);
        fx.rename_children(1, &["b"]);
        fx.v.release_error = Some(released());
        fx.v.pop_fault = Box::new(|_, _| {
            Some(CheckError::RevertMismatch {
                paths: vec![rel(LIB)],
            })
        });
        let err = fx.run().unwrap_err();
        assert!(
            matches!(
                err,
                SearchError::Check {
                    source: CheckError::RevertMismatch { .. },
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn max_sessions_follows_decision_d3() {
        assert_eq!(max_sessions(30), 3);
        assert_eq!(max_sessions(0), 1);
        assert_eq!(max_sessions(8), 1);
        assert_eq!(max_sessions(13), 1);
        assert_eq!(max_sessions(14), 1);
        assert_eq!(max_sessions(20), 2);
        assert_eq!(max_sessions(64), 9);
    }

    #[test]
    fn default_search_budget_is_the_decided_numbers() {
        let b = DEFAULT_SEARCH_BUDGET;
        assert_eq!(
            (b.beam, b.max_expand, b.beam_parents, b.max_depth),
            (20, 200, 4, 3)
        );
        assert_eq!(
            (b.per_candidate_ms, b.soft_ms, b.hard_ms),
            (6_000, 300_000, 360_000)
        );
        assert_eq!(b.audit_every.get(), 10);
        assert_eq!(b.decision_budget(), DEFAULT_BUDGET);
        assert_eq!(ASSERT_REVERT_EVERY, 50);
        assert_eq!((EXPAND_DEPTH_MS, JUDGE_DEPTH_MS), (10_000, 30_000));
        assert_eq!((SESSION_RAM_GB, RESERVED_RAM_GB), (6, 8));
        assert_eq!(
            (
                crate::app::memory::LAYER_MIB,
                crate::app::memory::JOB_TRANSIENT_MIB
            ),
            (52, 10)
        );
        assert_eq!(
            (TRIGGER_DECISION_MEDIAN_MS, TRIGGER_CHECK_MEDIAN_MS),
            (300_000, 5_000)
        );
        assert_eq!(TRIGGER_RSS_KB, 8_388_608);
        assert_eq!(
            (TRIGGER_CHECK_GROWTH_PERMILLE, TRIGGER_AFTER_CHECKS),
            (1_500, 1_000)
        );
        assert_eq!(TRIGGER_HARNESS_MS, 600_000);
    }
}
