//! The use case: the best-first beam search of one decision.
//!
//! Children of a state are proposed by an expansion, run through the stages of
//! [`crate::app::apply`] against the text of their parent, assessed from the texts alone, judged
//! once per depth and pruned to a beam; only the beam reaches the analyzer. The search ends with
//! a [`Resolution`], the ranking of every verified node and a trace of every child.
//!
//! The search runs on N live forks ([`Forks`], D11 stage 4): each verified node owns its Layer,
//! a child is a job on its parent's Layer and a parent is expanded by a read job on its own
//! (C12). At most the forks' width of jobs are in flight, and their outcomes are taken in the
//! order they were submitted (C16).
//!
//! The numbers of the budget are those of the decisions in force in `SEARCH_LEDGER.md`: D1 (time),
//! D2 (revert audit), D3 (memory per session), D4 (the triggers that call for perf work) and D11
//! (the forks).

use crate::app::apply::Acc;
use crate::app::memory::{MemoryBudget, RssStop};
use crate::app::ports::{
    Assessor, CheckError, CheckReach, Clock, ExpandError, Forks, Gauge, JobId, Judge, JudgeError,
    Limit, Next, RevertAudit,
};
use crate::domain::search::{prune, rank_final, reward_score, score};
use crate::domain::stage::{On, Refused, Staged, stage, view_of};
use crate::domain::{
    Blocking, Blueprint, BlueprintId, Candidate, ChangeProbe, CommandKind, Edit, Evidence,
    Infeasible, Inserted, JudgeRequest, NearMiss, NewNode, NodeId, NonEmpty, Noul, Prior, Proposal,
    ProposalId, ProviderTag, PruneStage, RelPath, Resolution, Reward, Scope, ScopedFindings,
    Scored, SearchBudget, SearchTree, Stage, StateFp, StaticMetrics, Target, Verdict, Weights,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::num::NonZeroU16;
use std::time::Duration;

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
/// Decision D11 (SEARCH_LEDGER.md), stage 4: the jobs a search's forks run at once, the width a
/// caller asks of its `Forks` (with a caller that only waits, c = 0), from Table F1: the small
/// set's median check is 82 ms at 4 against 77 ms at 1, and doubles at 8.
pub const SEARCH_FORKS: usize = 4;
/// Decision D11 (SEARCH_LEDGER.md), stage 4: how long one wait of the search in `Forks::next`
/// lasts before it reads its stops again (C5, C17), in ms.
pub const SEARCH_TICK_MS: u64 = 100;

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
    /// How far the RSS of the process may grow during the search (the RSS stop, C17).
    pub memory: MemoryBudget,
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
    /// The time ran out: its check was cut off, or the decision's budget ended before its turn;
    /// or it was withdrawn at the memory stop (C17).
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
    /// The node it became; `None` when its check was never submitted.
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
    /// Children with the fate of a time out, cut off, not reached or withdrawn.
    pub timed_out: u16,
    /// Time in the expansions: from the first expansion job's submit to the last one's outcome.
    pub expand_ms: u64,
    /// Time in the stages and the assessor.
    pub assess_ms: u64,
    /// Time in the judge.
    pub judge_ms: u64,
    /// Time in the checks: from the first check job's submit to the last one's outcome, and at
    /// depth 1 the start state's job.
    pub check_ms: u64,
    /// The write jobs of this depth (`Forks::submit`): the children's checks, and at depth 1 the
    /// start state's job. An expansion writes nothing and is not counted.
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
    /// Whether the RSS stop ended the search (C17).
    pub memory_stopped: bool,
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
    /// The search's forks failed outside the check of a child: they could not be opened or
    /// closed, a call of the port (`submit`, `expand`, `next`, `cancel`, `layer_files`) failed,
    /// or the start state's job failed with reach `Session`; the session is held or poisoned.
    #[error("the search's forks failed: {0}")]
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
    memory_stopped: bool,
    /// The failure of the start state's job, when it ended the search (C13).
    start_failed: Option<CheckError>,
}

impl Run {
    /// Whether a stop keeps every further job from being submitted (C5, C17).
    fn stopped(&self) -> bool {
        self.budget_exhausted || self.memory_stopped
    }
}

fn sat16(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

fn sat32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// What a job of the search is for.
enum Slot {
    /// The start state's job (C13).
    Start,
    /// The expansion of a parent.
    Expand(NodeId),
    /// The check of a child, inserted as `node`.
    Child(NodeId, Box<Pending>),
}

/// One turn of the search, in submission order (C16).
enum Turn {
    /// A job and what it is for.
    Job(JobId, Slot),
    /// A child that was a duplicate at its submit: its log entry waits for its turn, so the
    /// decision log is in the order of the checks whatever the width.
    Duplicate(BlueprintId),
}

/// The turn at the head, with its job's outcome when it is a job (what [`Window::pop`] gives).
type Popped<L> = (Turn, Option<Next<L>>);

/// The two stops read before each submit and on each `Pending` (C5, C17).
struct Stops<'a> {
    clock: &'a dyn Clock,
    t0: u64,
    budget: SearchBudget,
    gauge: &'a dyn Gauge,
    rss: RssStop,
}

impl Stops<'_> {
    fn elapsed(&self) -> u64 {
        self.clock.now_ms().saturating_sub(self.t0)
    }

    /// C5: a check that starts now may not finish within the hard limit.
    fn hard(&self) -> bool {
        self.elapsed().saturating_add(self.budget.per_candidate_ms) > self.budget.hard_ms
    }

    /// C17: the RSS has grown past the search's memory budget.
    fn memory(&self) -> bool {
        self.rss.reached(self.gauge)
    }
}

/// The jobs of the search between their submit and their turn (C16): at most `width` submitted
/// and not yet received from `Forks::next`, the outcomes received before their turn in `early`.
struct Window<'v, V: Forks> {
    v: &'v V,
    width: usize,
    turns: VecDeque<Turn>,
    early: BTreeMap<JobId, Next<V::Layer>>,
    /// Submitted and not yet received: what a stop may cancel.
    undelivered: BTreeSet<JobId>,
}

/// The id of an outcome of a job.
fn job_of<L>(next: &Next<L>) -> Option<JobId> {
    match next {
        Next::Done(id, _) | Next::Expanded(id, _) | Next::Withdrawn(id) => Some(*id),
        Next::Pending | Next::Idle => None,
    }
}

impl<'v, V: Forks> Window<'v, V> {
    fn room(&self) -> bool {
        self.undelivered.len() < self.width
    }

    fn push(&mut self, id: JobId, slot: Slot) {
        self.undelivered.insert(id);
        self.turns.push_back(Turn::Job(id, slot));
    }

    /// Cancels every job not yet received (C17). A cancel that returns false is followed by
    /// the job's own outcome, consumed like any other.
    fn cancel_all(&mut self, s: &mut V::Session) -> Result<(), CheckError> {
        for id in self.undelivered.clone() {
            self.v.cancel(s, id)?;
        }
        Ok(())
    }

    /// Reads the stops on a `Pending` (C5, C17): the hard limit stops further submits, and
    /// cancels the jobs in flight only when `cancel_on_hard` (the start state's job, which has no
    /// cut-off of its own); the RSS stop cancels them.
    fn on_pending(
        &mut self,
        s: &mut V::Session,
        run: &mut Run,
        stops: &Stops<'_>,
        cancel_on_hard: bool,
    ) -> Result<(), CheckError> {
        if !run.budget_exhausted && stops.hard() {
            run.budget_exhausted = true;
            if cancel_on_hard {
                self.cancel_all(s)?;
            }
        }
        if !run.memory_stopped && stops.memory() {
            run.memory_stopped = true;
            self.cancel_all(s)?;
        }
        Ok(())
    }

    /// The turn at the head and its outcome, waited for in `Forks::next` when it has not been
    /// received; `None` when no turn is left and, with `want_room`, when an outcome of another job
    /// freed a place, so the caller can submit before the head is in.
    fn pop(
        &mut self,
        s: &mut V::Session,
        run: &mut Run,
        stops: &Stops<'_>,
        cancel_on_hard: bool,
        want_room: bool,
    ) -> Result<Option<Popped<V::Layer>>, CheckError> {
        let head = match self.turns.front() {
            None => return Ok(None),
            Some(Turn::Duplicate(_)) => {
                return Ok(self.turns.pop_front().map(|t| (t, None)));
            }
            Some(Turn::Job(id, _)) => *id,
        };
        let next = loop {
            if let Some(next) = self.early.remove(&head) {
                break next;
            }
            let next = self.v.next(s, Duration::from_millis(SEARCH_TICK_MS))?;
            let Some(id) = job_of(&next) else {
                if matches!(next, Next::Idle) {
                    return Err(CheckError::Panicked(
                        "the forks are idle while a job of the search is in flight".to_owned(),
                    ));
                }
                self.on_pending(s, run, stops, cancel_on_hard)?;
                continue;
            };
            self.undelivered.remove(&id);
            if id == head {
                break next;
            }
            self.early.insert(id, next);
            if want_room && self.room() {
                return Ok(None);
            }
        };
        Ok(self.turns.pop_front().map(|t| (t, Some(next))))
    }
}

/// An outcome that is not the one its slot expects: the forks broke their contract (C1).
fn mismatched(what: &str) -> CheckError {
    CheckError::Panicked(format!(
        "the forks gave an outcome of the wrong kind for {what}"
    ))
}

/// Searches for the best verified changes that answer one decision.
///
/// `v` checks and expands on its forks, `a` measures, `j` judges, `clock` is the only source of
/// time and `gauge` the only reading of memory.
///
/// # Contract
/// - C1: depth 1 expands the start state, the roots of the input first; depth `d` above 1
///   expands the first `beam_parents` of `rank_final` over the passing nodes of depth `d - 1`,
///   each for at most `max(1, max_expand / parents)` children, and the search ends when there
///   is no such node. Each parent is expanded by a read job on its own Layer (C12), the
///   expansions taken in the parents' order (C16). A failed analysis of one parent
///   (`ExpandError::Analysis`) is recorded in the report and the parent yields nothing; a lost
///   session is `SearchError::Expand`; no root and no child at depth 1 is
///   `SearchError::NoCandidates`, unless the memory stop withdrew the expansions (C17).
///   tested: app::search::tests::siblings_are_checked_consecutively
///   tested: app::search::tests::no_candidates_is_error
///   tested: app::search::tests::expand_errors_are_recorded_and_session_error_is_fatal
///   tested: app::search::tests::parents_are_expanded_on_their_layer
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
/// - C5: before each check job is submitted, in the order of C4, the clock is read: when the
///   time spent plus `per_candidate_ms` passes `hard_ms`, `budget_exhausted` is set, no further
///   check is submitted and every child not yet submitted gets `Fate::TimedOut` with no node and
///   no check. The same reading is taken on each `Next::Pending` while the search waits. The
///   jobs already submitted are not cancelled: each started within the limit, its own cut-off
///   ends it, and its outcome is consumed; so with 360 s and a 6 s limit no check starts that
///   cannot finish. A child whose state is already in the tree is `Fate::Pruned` for being a
///   duplicate and is not checked. A check cut off by `CheckError::TimedOut` is
///   `Blocking::BudgetExhausted` and `Fate::TimedOut` and the search goes on; a check that fails
///   another way with a failure of reach `Blueprint` is `Blocking::CheckFailed`, a
///   `Stage::Check` log entry with `ok` false and `Fate::Blocked(Stage::Check)`, and the search
///   goes on; one of reach `Session` is `SearchError::Check`; a check that adds errors is
///   `Fate::Failed` and `Blocking::NewErrors`, and a near miss when the errors are few. A
///   `Decided` resolution lists every blocking of the search.
///   tested: app::search::tests::budget_exhausted_returns_verified_so_far
///   tested: app::search::tests::hard_budget_with_nothing_verified_is_failed_exhausted
///   tested: app::search::tests::the_hard_budget_is_checked_before_each_submit
///   tested: app::search::tests::check_timeout_is_budget_exhausted_blocking
///   tested: app::search::tests::per_blueprint_check_error_blocks_only_that_child
///   tested: app::search::tests::session_check_error_stops_the_search
///   tested: app::search::tests::same_state_children_are_checked_once
/// - C6: after a depth, when the time spent has reached `soft_ms`, `soft_stopped` is set and
///   no further depth starts.
///   tested: app::search::tests::soft_budget_stops_after_the_depth
/// - C7: the revert is audited exactly once, after the forks are closed and before the report
///   is returned, whether the resolution is `Decided` or `Failed`; an audit that fails is
///   `SearchError::RevertAudit`. A search that ends in an error is not audited.
///   tested: app::search::tests::revert_audit_runs_once_at_end
///   tested: app::search::tests::layers_are_dropped_before_close_on_every_path
/// - C8: the ranking holds every node that passed, at any depth, by `rank_final`, each with
///   the Blueprints of its path and its prior. The resolution is `Decided` with every node of
///   the ranking as a candidate, in the order of the ranking; the marks of each candidate are
///   those of the Blueprints of its path and of the file roles its node edits. With no node
///   that passed the resolution is `Failed` with every blocking of the search, and the budget
///   flag set when the hard limit or the memory stop ended it.
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
/// - C12: the search opens the forks once, before its first job, and closes them before its
///   audit. Each verified node owns a Layer: the root the Layer of the start state's job, or the
///   base's root for an empty start; a passing child the Layer of its own check job. A child is
///   the job `Forks::submit` of its own edits on its parent's Layer, at the input's scope, with
///   `Limit::Check`; a parent is expanded by `Forks::expand` of its Layer with the fingerprint of
///   its state's edits. No job writes a node's edits from the base, and none writes a parent's
///   path again.
///   tested: app::search::tests::children_are_jobs_on_their_parents_layer
///   tested: app::search::tests::parents_are_expanded_on_their_layer
/// - C13: the verdict of a child is `Verdict::of_level` of its job's findings on top of its
///   parent's findings, with the paths the child deletes, kept to the `Forks::layer_files` of its
///   job's Layer at the input's scope. A passing node's findings are those its verdict was made
///   from. The start state's are those of one job of the start's edits on the base's root,
///   submitted with `Limit::None` (the start is no candidate, so D1's cut-off is not its) and
///   consumed before depth 1, kept the same way over no findings; an empty start has none and
///   no job. A start job that fails with reach `Blueprint` ends the search with a report: no
///   depth runs, each root is in the trace with the blocking of that failure and
///   `Fate::Blocked(Stage::Check)`, and the resolution is `Failed` with `Infeasible::start`
///   naming the failure (a `TimedOut` is no failure: the roots are `Fate::TimedOut`, the
///   blockings `BudgetExhausted`).
///   tested: app::search::tests::the_verdict_keeps_the_findings_below
///   tested: app::search::tests::the_start_state_is_one_job
///   tested: app::search::tests::the_start_job_is_not_cut_off
///   tested: app::search::tests::a_failed_start_state_is_a_failed_report_naming_it
///   tested: ryo-v2-ra tests/stack.rs search_and_apply_give_the_same_verdict_on_a_chain
///   tested: ryo-v2-oracle tests/search_recall.rs search_verdicts_equal_the_reference_check
/// - C14: a failed call of the forks (`open`, `submit`, `expand`, `next`, `cancel`, and
///   `layer_files` with reach `Session`) is `SearchError::Fork`, and so is a start job's
///   `Next::Done` with an error of reach `Session`; a child's with one is `SearchError::Check`
///   with that child. A `layer_files` of reach `Blueprint` is the check failure of its child.
///   tested: app::search::tests::a_session_failure_of_a_call_is_a_fork_error
///   tested: app::search::tests::session_check_error_stops_the_search
/// - C15: every Layer the search holds, and every outcome it has received, is dropped and the
///   forks are closed before the search returns, whatever it returns; `close` withdraws the jobs
///   still in flight. A failed close is `SearchError::Fork`, which wins over every end that is
///   not a failure of reach `Session`: a report, and `SearchError::NoCandidates`; a search that
///   already ends in `SearchError::Check`, `SearchError::Expand` or `SearchError::Fork` returns
///   that error.
///   tested: app::search::tests::layers_are_dropped_before_close_on_every_path
///   tested: app::search::tests::a_failed_close_is_a_search_error
/// - C16: at most `Forks::width` jobs, checks and expansions, are submitted and not yet received
///   from `Forks::next`; outcomes are consumed in submission order, and one received before its
///   turn waits for it and frees its place. Tree insertion and the duplicate check happen at
///   submit, in that order. Every outcome received is consumed. So for a search with no
///   wall-clock effect (no flag set, no `TimedOut`), the report's fields other than times are
///   those of width 1, whatever the width and the order the jobs finish in.
///   tested: app::search::tests::parallel_search_equals_serial
///   tested: app::search::tests::the_window_never_holds_more_than_w
/// - C17: the RSS stop is `RssStop::start(gauge, input.memory)`, read at the search's start;
///   it is read before each job is submitted and on each `Next::Pending`. When it is reached,
///   `memory_stopped` is set, nothing more is submitted (no expansion and no child of this
///   depth, none of a later depth) and the jobs not yet received are cancelled; the outcomes
///   already received are consumed. A withdrawn child is `Fate::TimedOut`, counted in
///   `timed_out`, with no blocking, no log entry and no `CheckError`; a withdrawn expansion
///   yields nothing and records no expand error; a cancel that returns false is consumed as its
///   job's outcome. The start state's job, which has no cut-off, is cancelled also when the hard
///   limit of C5 is reached while the search waits for it; a start that is withdrawn, or never
///   submitted because a stop was reached, ends the search with no depth: each root is in the
///   trace as `Fate::TimedOut` and the resolution is `Failed` with the budget flag.
///   tested: app::search::tests::the_rss_stop_is_relative_to_the_search_start
///   tested: app::search::tests::a_withdrawn_child_is_timed_out_never_blocked
///   tested: app::search::tests::an_outcome_received_before_a_stop_is_consumed
///   tested: app::search::tests::a_cancel_that_returns_false_is_consumed_as_its_done
///   tested: app::search::tests::a_stop_during_expansion_submits_no_child
///   tested: app::search::tests::the_start_job_is_cut_by_the_hard_budget_and_the_rss_stop
/// - C18: a failed or withdrawn child's Layer is dropped when it is consumed; a passing child's
///   is kept until the next depth picks its parents, and dropped then if it is not one; a
///   parent's Layer, the root's included, is dropped once every child of its depth is consumed;
///   after the last depth every Layer is dropped before `close`.
///   tested: app::search::tests::layers_follow_the_retention_rule
pub fn search_decision<V, A, J>(
    v: &V,
    a: &A,
    j: &J,
    clock: &dyn Clock,
    gauge: &dyn Gauge,
    s: &mut V::Session,
    input: &SearchInput<'_>,
) -> Result<SearchReport, SearchError>
where
    V: Forks + RevertAudit,
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
    let stops = Stops {
        clock,
        t0,
        budget: b,
        gauge,
        rss: RssStop::start(gauge, input.memory),
    };
    let baseline = v.baseline(s);
    let provenance = v.provenance(s);
    let blind = v.blind_classes();
    let mut tree = SearchTree::new(input.start.to_vec());
    let mut run = Run::default();
    let mut passing: Vec<NodeId> = Vec::new();
    let mut last_passing: Vec<NodeId> = Vec::new();
    let mut staged_of: BTreeMap<NodeId, Staged> = BTreeMap::new();
    // The findings of the root and of every node that can still be a parent (C13).
    let mut findings: BTreeMap<NodeId, ScopedFindings> = BTreeMap::new();
    // The Layer of each node that can still be a parent, with its state's fingerprint (C18).
    let mut layers: BTreeMap<NodeId, (V::Layer, StateFp)> = BTreeMap::new();
    let root_layer = v.open(s).map_err(SearchError::Fork)?;
    layers.insert(tree.root(), (root_layer, StateFp::of(input.start)));
    let mut window = Window {
        v,
        width: v.width(s).max(1),
        turns: VecDeque::new(),
        early: BTreeMap::new(),
        undelivered: BTreeSet::new(),
    };

    let searched: Result<(), SearchError> = 'search: {
        // 0. the start state, one job with no cut-off (C13, C17)
        let (mut start_ms, mut start_writes) = (0, 0);
        if input.start.is_empty() {
            findings.insert(tree.root(), ScopedFindings::default());
        } else {
            let t_start = clock.now_ms();
            if stops.hard() {
                run.budget_exhausted = true;
            } else if stops.memory() {
                run.memory_stopped = true;
            } else {
                let Some((root_layer, _)) = layers.get(&tree.root()) else {
                    break 'search Err(SearchError::Fork(mismatched("the root")));
                };
                let id = match v.submit(s, root_layer, input.start, input.scope, Limit::None) {
                    Ok(id) => id,
                    Err(e) => break 'search Err(SearchError::Fork(e)),
                };
                start_writes = 1;
                window.push(id, Slot::Start);
                let next = match window.pop(s, &mut run, &stops, true, false) {
                    Ok(Some((Turn::Job(_, Slot::Start), Some(next)))) => next,
                    Ok(_) => break 'search Err(SearchError::Fork(mismatched("the start"))),
                    Err(e) => break 'search Err(SearchError::Fork(e)),
                };
                match next {
                    Next::Done(_, Ok(checked)) => {
                        match v.layer_files(s, &checked.layer, input.scope) {
                            Ok(files) => {
                                let (_, kept) = Verdict::of_level(
                                    &baseline,
                                    &ScopedFindings::default(),
                                    &checked.findings,
                                    &Edit::deleted_paths(input.start),
                                    &files,
                                );
                                findings.insert(tree.root(), kept);
                                layers
                                    .insert(tree.root(), (checked.layer, StateFp::of(input.start)));
                            }
                            Err(e) if e.reach() == CheckReach::Session => {
                                break 'search Err(SearchError::Fork(e));
                            }
                            Err(e) => run.start_failed = Some(e),
                        }
                    }
                    Next::Done(_, Err(e)) if e.reach() == CheckReach::Session => {
                        break 'search Err(SearchError::Fork(e));
                    }
                    Next::Done(_, Err(e)) => run.start_failed = Some(e),
                    // Withdrawn at a stop (C17).
                    Next::Withdrawn(_) => {}
                    Next::Expanded(..) | Next::Pending | Next::Idle => {
                        break 'search Err(SearchError::Fork(mismatched("the start")));
                    }
                }
            }
            start_ms = clock.now_ms().saturating_sub(t_start);
            if run.start_failed.is_some() || !findings.contains_key(&tree.root()) {
                // No depth runs: the roots are in the trace with what became of the start.
                for bp in input.roots {
                    let bp_id = bp.id();
                    let fate = match run.start_failed.clone() {
                        Some(e) => {
                            run.acc.note(
                                bp_id,
                                Stage::Check,
                                false,
                                format!("the start state's check failed: {e}"),
                            );
                            let timed_out = matches!(e, CheckError::TimedOut { .. });
                            if let Ok(blocking) = e.into_blocking(bp_id) {
                                run.blocking.push(blocking);
                            }
                            if timed_out {
                                Fate::TimedOut
                            } else {
                                Fate::Blocked(Stage::Check)
                            }
                        }
                        None => Fate::TimedOut,
                    };
                    run.trace.push(TraceEntry {
                        depth: 1,
                        parent: tree.root(),
                        blueprint: bp_id,
                        proposal: ProposalId::of(bp),
                        command: bp.command().kind(),
                        state: None,
                        score: None,
                        rank: None,
                        audited: false,
                        root: true,
                        fate,
                        node: None,
                        edits: None,
                    });
                }
                break 'search Ok(());
            }
        }

        for d in 1..=b.max_depth {
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
                ranked
            };
            // A passing node that is not picked is a parent no more (C18).
            layers.retain(|id, _| parents.contains(id));
            findings.retain(|id, _| parents.contains(id));
            if parents.is_empty() {
                break;
            }
            let mut stats = DepthStats {
                depth: d,
                parents: sat16(parents.len()),
                ..DepthStats::default()
            };
            if d == 1 {
                stats.check_ms = start_ms;
                stats.writes = start_writes;
            }
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
            let mut to_expand = parents.iter().copied().peekable();
            loop {
                while let Some(&p) = to_expand.peek() {
                    if !run.memory_stopped && !window.room() {
                        break;
                    }
                    to_expand.next();
                    if run.memory_stopped {
                        continue;
                    }
                    if stops.memory() {
                        run.memory_stopped = true;
                        if let Err(e) = window.cancel_all(s) {
                            break 'search Err(SearchError::Fork(e));
                        }
                        continue;
                    }
                    let Some((layer, state)) = layers.get(&p) else {
                        continue;
                    };
                    match v.expand(s, layer, *state, input.targets, per_parent) {
                        Ok(id) => window.push(id, Slot::Expand(p)),
                        Err(e) => break 'search Err(SearchError::Fork(e)),
                    }
                }
                let more = to_expand.peek().is_some();
                let (turn, next) = match window.pop(s, &mut run, &stops, false, more) {
                    Ok(None) if !more => break,
                    Ok(None) => continue,
                    Ok(Some(popped)) => popped,
                    Err(e) => break 'search Err(SearchError::Fork(e)),
                };
                let parent = match turn {
                    Turn::Job(_, Slot::Expand(p)) => p,
                    _ => break 'search Err(SearchError::Fork(mismatched("an expansion"))),
                };
                match next {
                    Some(Next::Expanded(_, Ok(mut bps))) => {
                        bps.truncate(per_parent);
                        children.extend(bps.into_iter().map(|bp| Child {
                            parent,
                            bp,
                            root: false,
                        }));
                    }
                    Some(Next::Expanded(_, Err(err @ ExpandError::Analysis(_)))) => {
                        run.expand_errors.push(err);
                    }
                    Some(Next::Expanded(_, Err(err))) => {
                        break 'search Err(SearchError::Expand(err));
                    }
                    // Withdrawn at the memory stop: the parent yields nothing (C17).
                    Some(Next::Withdrawn(_)) => {}
                    _ => break 'search Err(SearchError::Fork(mismatched("an expansion"))),
                }
            }
            if d == 1 && children.is_empty() && !run.memory_stopped {
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

            // 5. checks, windowed (C5, C16, C17)
            let t_check = clock.now_ms();
            let mut depth_passing: Vec<NodeId> = Vec::new();
            let mut order = order.into_iter().peekable();
            loop {
                while let Some(&i) = order.peek() {
                    if !run.stopped() && !window.room() {
                        break;
                    }
                    order.next();
                    let Some(c) = staged[i].take() else {
                        continue;
                    };
                    let bp_id = c.staged.path().last().id();
                    let t = c.trace;
                    if run.stopped() {
                        run.trace[t].fate = Fate::TimedOut;
                        stats.timed_out = stats.timed_out.saturating_add(1);
                        continue;
                    }
                    if tree.has_state(c.state) {
                        run.trace[t].fate = Fate::Pruned(PruneStage::Duplicate);
                        window.turns.push_back(Turn::Duplicate(bp_id));
                        continue;
                    }
                    if stops.hard() {
                        run.budget_exhausted = true;
                    } else if stops.memory() {
                        run.memory_stopped = true;
                        if let Err(e) = window.cancel_all(s) {
                            break 'search Err(SearchError::Fork(e));
                        }
                    }
                    if run.stopped() {
                        run.trace[t].fate = Fate::TimedOut;
                        stats.timed_out = stats.timed_out.saturating_add(1);
                        continue;
                    }
                    let Some((layer, _)) = layers.get(&c.parent) else {
                        // Parents are passing nodes, each with its Layer.
                        continue;
                    };
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
                    match v.submit(s, layer, c.staged.own(), input.scope, Limit::Check) {
                        Ok(job) => window.push(job, Slot::Child(id, Box::new(c))),
                        Err(e) => break 'search Err(SearchError::Fork(e)),
                    }
                    stats.writes = stats.writes.saturating_add(1);
                }
                let more = order.peek().is_some();
                let (turn, next) = match window.pop(s, &mut run, &stops, false, more) {
                    Ok(None) if !more => break,
                    Ok(None) => continue,
                    Ok(Some(popped)) => popped,
                    Err(e) => break 'search Err(SearchError::Fork(e)),
                };
                let (id, c) = match turn {
                    Turn::Duplicate(bp_id) => {
                        run.acc.note(
                            bp_id,
                            Stage::Prune,
                            false,
                            PruneStage::Duplicate.to_string(),
                        );
                        continue;
                    }
                    Turn::Job(_, Slot::Child(id, c)) => (id, *c),
                    Turn::Job(..) => break 'search Err(SearchError::Fork(mismatched("a check"))),
                };
                let bp_id = c.staged.path().last().id();
                let t = c.trace;
                let result = match next {
                    Some(Next::Done(_, Ok(checked))) => {
                        match v.layer_files(s, &checked.layer, input.scope) {
                            Ok(files) => {
                                let below = findings.get(&c.parent).cloned().unwrap_or_default();
                                let (verdict, kept) = Verdict::of_level(
                                    &baseline,
                                    &below,
                                    &checked.findings,
                                    &Edit::deleted_paths(c.staged.own()),
                                    &files,
                                );
                                Ok((verdict, kept, checked.layer))
                            }
                            Err(e) if e.reach() == CheckReach::Session => {
                                break 'search Err(SearchError::Fork(e));
                            }
                            Err(e) => Err(e),
                        }
                    }
                    Some(Next::Done(_, Err(e))) => Err(e),
                    // Withdrawn at the memory stop: no fact about the Blueprint (C17).
                    Some(Next::Withdrawn(_)) => {
                        run.trace[t].fate = Fate::TimedOut;
                        stats.timed_out = stats.timed_out.saturating_add(1);
                        continue;
                    }
                    _ => break 'search Err(SearchError::Fork(mismatched("a check"))),
                };
                stats.checked = stats.checked.saturating_add(1);
                match result {
                    Ok((verdict, kept, layer)) => {
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
                            layers.insert(id, (layer, c.state));
                            staged_of.insert(id, c.staged);
                            findings.insert(id, kept);
                            run.verdicts.insert(id, verdict);
                            passing.push(id);
                            depth_passing.push(id);
                            run.trace[t].fate = Fate::Passed;
                            stats.passed = stats.passed.saturating_add(1);
                        } else {
                            // A failed child's Layer is dropped here (C18).
                            drop(layer);
                            run.trace[t].fate = Fate::Failed { new_errors: errors };
                            run.near_miss.extend(NearMiss::of(bp_id, errors));
                            run.blocking.push(Blocking::NewErrors {
                                blueprint: bp_id,
                                verdict,
                            });
                        }
                    }
                    Err(CheckError::TimedOut { limit_ms }) => {
                        run.acc.note(
                            bp_id,
                            Stage::Check,
                            false,
                            format!("cut off after {limit_ms} ms"),
                        );
                        run.blocking.push(Blocking::BudgetExhausted {
                            blueprint: bp_id,
                            limit_ms,
                        });
                        run.trace[t].fate = Fate::TimedOut;
                        stats.timed_out = stats.timed_out.saturating_add(1);
                    }
                    Err(e) => {
                        let note = format!("check failed: {e}");
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
            stats.check_ms = stats
                .check_ms
                .saturating_add(clock.now_ms().saturating_sub(t_check));
            run.depths.push(stats);
            // The parents' Layers go once their depth is consumed (C18).
            for p in &parents {
                layers.remove(p);
            }
            last_passing = depth_passing;
            // Only the passing nodes of this depth can be parents of the next.
            findings.retain(|id, _| last_passing.contains(id));

            // 6. limits
            if run.stopped() {
                break;
            }
            if elapsed() >= b.soft_ms {
                run.soft_stopped = true;
                break;
            }
        }
        Ok(())
    };

    // 7. every Layer and outcome dropped, then the forks closed, whatever the search ended in
    // (C15, C18)
    drop(window);
    drop(layers);
    match (searched, v.close(s)) {
        // A failure of reach `Session` already ends the search; the close's is the same lost
        // session.
        (
            Err(e @ (SearchError::Check { .. } | SearchError::Expand(_) | SearchError::Fork(_))),
            _,
        ) => {
            return Err(e);
        }
        (_, Err(closed)) => return Err(SearchError::Fork(closed)),
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
    let start_failure = run.start_failed.clone().and_then(|e| e.into_failure().ok());
    let stopped = run.stopped();
    let resolution = match (NonEmpty::new(candidates), start_failure) {
        (Some(blueprints), _) => Resolution::Decided {
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
        (None, Some(failure)) => Resolution::Failed(Infeasible::of_start(failure, run.blocking)),
        (None, None) => Resolution::Failed(
            Infeasible::new(run.blocking, run.near_miss, stopped)
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
        memory_stopped: run.memory_stopped,
        elapsed_ms: elapsed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::ports::{Checked, FileFacts, Verifier};
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
    /// A failure for the `n`-th submit (from 0) of these own edits, or none.
    type FaultRule = Box<dyn Fn(&[Edit], usize) -> Option<CheckError>>;
    type FilesRule = Box<dyn Fn(&[Edit]) -> Vec<RelPath>>;
    /// Called with the number of a `next` call (from 0) before it answers: `true` makes it
    /// `Pending`, with nothing finished.
    type NextHook = Box<dyn Fn(usize) -> bool>;
    /// The children of a state, from its composed edits.
    type GenerateRule = Box<dyn Fn(&[Edit]) -> Vec<Blueprint>>;

    /// One call on the fake's forks, in the order they ran.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Op {
        Open,
        /// A job: the composed edits of its parent's Layer, its own edits and its limit.
        Submit {
            on: Vec<Edit>,
            own: Vec<Edit>,
            limit: Limit,
        },
        /// An expansion: the composed edits of its Layer, and the Layers alive at its submit.
        Expand {
            on: Vec<Edit>,
            live: usize,
        },
        /// A cancel, and what it returned.
        Cancel(bool),
        /// The Layers alive at the close.
        Close {
            live: usize,
        },
        Audit,
    }

    /// A Layer of the fake: the edits that take the base to its state.
    #[derive(Clone, Debug)]
    struct FakeLayer {
        edits: Vec<Edit>,
        /// A clone of the fake's `held`, which counts the Layers alive; never read.
        _held: Arc<()>,
    }

    #[derive(Default)]
    struct Jobs {
        next_id: u64,
        /// Jobs not delivered, whether they finished, and their outcome.
        in_flight: BTreeMap<JobId, (bool, Next<FakeLayer>)>,
        /// Outcomes known at once (refused edits, withdrawals), delivered first.
        ready: VecDeque<Next<FakeLayer>>,
        nexts: usize,
        /// The most jobs submitted and not delivered at once.
        most_out: usize,
    }

    impl Jobs {
        fn out(&self) -> usize {
            self.in_flight.len() + self.ready.len()
        }
    }

    /// The expansions of the fake, by the fingerprint of the state.
    #[derive(Default)]
    struct FakeE {
        by_state: BTreeMap<StateFp, Result<Vec<Blueprint>, ExpandError>>,
        calls: RefCell<Vec<(StateFp, usize)>>,
        /// The children of a state that `by_state` does not name, from its composed edits.
        generate: Option<GenerateRule>,
    }
    impl FakeE {
        fn set(&mut self, at: StateFp, bps: Vec<Blueprint>) {
            self.by_state.insert(at, Ok(bps));
        }
    }

    /// A pseudo-random number of `seed` and `n`.
    fn mix(seed: u64, n: u64) -> u64 {
        let mut x = seed ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        x ^ (x >> 31)
    }

    /// A fake `Forks` whose every job runs at its submit: a check job's diagnosis answers from
    /// `rule` of its Layer's composed edits, so a file keeps its findings unless the composed
    /// state changes, and `layer_files` gives the files a check of those edits gives, none for
    /// none (`Forks` C2, `Stacked::files_of` C1, C2). A job finishes at the first `next` after its
    /// submit that is not `Pending`, and outcomes come out of submission order: the newest
    /// first, or in a seeded order. Its faults are refusals the port allows.
    struct FakeV {
        texts: BTreeMap<RelPath, String>,
        build_scripts: BTreeSet<RelPath>,
        rule: CheckRule,
        /// The composed edits of every check job, in submission order.
        checks: RefCell<Vec<Vec<Edit>>>,
        events: RefCell<Vec<&'static str>>,
        audit_error: Option<CheckError>,
        /// Each check job advances the clock by its cost, at its submit.
        tick: Option<(Ticker, u64)>,
        baseline: Vec<Finding>,
        /// The files `layer_files` gives for composed edits; by default those of `rule`.
        files: Option<FilesRule>,
        e: FakeE,
        width: usize,
        /// `None`: the newest job first; `Some(seed)`: a pseudo-random one of those in flight.
        order: Option<u64>,
        next_hook: Option<NextHook>,
        /// A call of these names fails with this error, of reach Session.
        fails: BTreeMap<&'static str, CheckError>,
        /// A job's own edits refused at its submit: an outcome of reach Blueprint, or an error of
        /// `submit` of reach Session.
        submit_fault: FaultRule,
        jobs: RefCell<Jobs>,
        ops: RefCell<Vec<Op>>,
        held: Arc<()>,
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
    impl Forks for FakeV {
        type Layer = FakeLayer;
        fn open(&self, _: &mut ()) -> Result<FakeLayer, CheckError> {
            self.op(Op::Open);
            self.fail("open")?;
            Ok(self.layer(Vec::new()))
        }
        fn width(&self, _: &()) -> usize {
            self.width
        }
        fn submit(
            &self,
            _: &mut (),
            parent: &FakeLayer,
            edits: &[Edit],
            _: Scope,
            limit: Limit,
        ) -> Result<JobId, CheckError> {
            self.fail("submit")?;
            let n = self.count(|op| matches!(op, Op::Submit { own, .. } if own == edits));
            self.op(Op::Submit {
                on: parent.edits.clone(),
                own: edits.to_vec(),
                limit,
            });
            let id = self.new_id();
            if let Some(e) = (self.submit_fault)(edits, n) {
                if e.reach() == CheckReach::Session {
                    return Err(e);
                }
                self.deliver_now(Next::Done(id, Err(e)));
                return Ok(id);
            }
            let base = |p: &RelPath| self.texts.get(p).cloned();
            let composed = crate::domain::search::compose(&parent.edits, edits, &base);
            self.checks.borrow_mut().push(composed.clone());
            self.events.borrow_mut().push("check");
            if let Some((ticker, cost)) = &self.tick {
                ticker.advance(*cost);
            }
            let outcome = (self.rule)(&composed).map(|findings| Checked {
                layer: self.layer(composed),
                findings,
            });
            self.queue(id, Next::Done(id, outcome));
            Ok(id)
        }
        fn expand(
            &self,
            _: &mut (),
            layer: &FakeLayer,
            state: StateFp,
            _: &[Target],
            max: usize,
        ) -> Result<JobId, CheckError> {
            self.fail("expand")?;
            self.op(Op::Expand {
                on: layer.edits.clone(),
                live: self.live(),
            });
            self.e.calls.borrow_mut().push((state, max));
            let children = match (self.e.by_state.get(&state), &self.e.generate) {
                (Some(children), _) => children.clone(),
                (None, Some(generate)) => Ok(generate(&layer.edits)),
                (None, None) => Ok(Vec::new()),
            };
            let id = self.new_id();
            self.queue(id, Next::Expanded(id, children));
            Ok(id)
        }
        fn next(&self, _: &mut (), _: Duration) -> Result<Next<FakeLayer>, CheckError> {
            self.fail("next")?;
            let n = {
                let mut jobs = self.jobs.borrow_mut();
                jobs.nexts += 1;
                jobs.nexts - 1
            };
            if let Some(hook) = &self.next_hook
                && hook(n)
            {
                return Ok(Next::Pending);
            }
            let mut jobs = self.jobs.borrow_mut();
            if let Some(next) = jobs.ready.pop_front() {
                return Ok(next);
            }
            for (finished, _) in jobs.in_flight.values_mut() {
                *finished = true;
            }
            let pick = match self.order {
                None => jobs.in_flight.keys().next_back().copied(),
                Some(seed) => {
                    let len = jobs.in_flight.len().max(1) as u64;
                    let at = usize::try_from(mix(seed, n as u64) % len).unwrap();
                    jobs.in_flight.keys().nth(at).copied()
                }
            };
            Ok(match pick.and_then(|id| jobs.in_flight.remove(&id)) {
                Some((_, next)) => next,
                None => Next::Idle,
            })
        }
        fn cancel(&self, _: &mut (), job: JobId) -> Result<bool, CheckError> {
            self.fail("cancel")?;
            let mut jobs = self.jobs.borrow_mut();
            let withdrawn = match jobs.in_flight.get(&job) {
                Some((true, _)) => false,
                Some((false, _)) => {
                    jobs.in_flight.remove(&job);
                    jobs.ready.push_back(Next::Withdrawn(job));
                    true
                }
                None if jobs.ready.iter().any(|next| job_of(next) == Some(job)) => false,
                None => return Err(CheckError::ForkInUse),
            };
            drop(jobs);
            self.op(Op::Cancel(withdrawn));
            Ok(withdrawn)
        }
        fn layer_files(
            &self,
            _: &(),
            layer: &FakeLayer,
            _: Scope,
        ) -> Result<Vec<RelPath>, CheckError> {
            self.fail("layer_files")?;
            if layer.edits.is_empty() {
                return Ok(Vec::new());
            }
            match &self.files {
                Some(files) => Ok(files(&layer.edits)),
                None => (self.rule)(&layer.edits).map(|found| found.files),
            }
        }
        fn layer_text(
            &self,
            _: &(),
            layer: &FakeLayer,
            path: &RelPath,
        ) -> Result<Option<String>, CheckError> {
            Ok(match layer.edits.iter().find(|e| e.path == *path) {
                Some(e) => e.new_text.clone(),
                None => self.texts.get(path).cloned(),
            })
        }
        fn close(&self, _: &mut ()) -> Result<(), CheckError> {
            {
                let mut jobs = self.jobs.borrow_mut();
                jobs.in_flight.clear();
                jobs.ready.clear();
            }
            self.op(Op::Close { live: self.live() });
            self.fail("close")
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
                e: FakeE::default(),
                width: SEARCH_FORKS,
                order: None,
                next_hook: None,
                fails: BTreeMap::new(),
                submit_fault: Box::new(|_, _| None),
                jobs: RefCell::default(),
                ops: RefCell::default(),
                held: Arc::new(()),
            }
        }
        fn fail(&self, call: &str) -> Result<(), CheckError> {
            match self.fails.get(call) {
                Some(e) => Err(e.clone()),
                None => Ok(()),
            }
        }
        fn layer(&self, edits: Vec<Edit>) -> FakeLayer {
            FakeLayer {
                edits,
                _held: Arc::clone(&self.held),
            }
        }
        /// The Layers alive.
        fn live(&self) -> usize {
            Arc::strong_count(&self.held) - 1
        }
        fn new_id(&self) -> JobId {
            let mut jobs = self.jobs.borrow_mut();
            jobs.next_id += 1;
            JobId::new(jobs.next_id)
        }
        fn note_out(&self) {
            let mut jobs = self.jobs.borrow_mut();
            jobs.most_out = jobs.most_out.max(jobs.out());
        }
        fn queue(&self, id: JobId, next: Next<FakeLayer>) {
            self.jobs.borrow_mut().in_flight.insert(id, (false, next));
            self.note_out();
        }
        fn deliver_now(&self, next: Next<FakeLayer>) {
            self.jobs.borrow_mut().ready.push_back(next);
            self.note_out();
        }
        fn op(&self, op: Op) {
            self.ops.borrow_mut().push(op);
        }
        fn count(&self, which: impl Fn(&Op) -> bool) -> usize {
            self.ops.borrow().iter().filter(|op| which(op)).count()
        }
        fn ops(&self) -> Vec<Op> {
            self.ops.borrow().clone()
        }
        /// The own edits of every job, in submission order.
        fn writes(&self) -> Vec<Vec<Edit>> {
            self.ops()
                .into_iter()
                .filter_map(|op| match op {
                    Op::Submit { own, .. } => Some(own),
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
        fn most_out(&self) -> usize {
            self.jobs.borrow().most_out
        }
    }

    /// A gauge that reads what the test sets.
    struct FakeGauge(std::sync::Mutex<Option<u64>>);
    impl FakeGauge {
        fn set(&self, rss_kb: u64) {
            *self.0.lock().unwrap() = Some(rss_kb);
        }
    }
    impl Gauge for FakeGauge {
        fn rss_kb(&self) -> Option<u64> {
            *self.0.lock().unwrap()
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
        a: FakeA,
        j: FakeJ,
        clock: Ticker,
        gauge: Arc<FakeGauge>,
        memory: MemoryBudget,
        roots: Vec<Blueprint>,
        budget: SearchBudget,
        scope: Scope,
        keep_edits: bool,
        start: Vec<Edit>,
    }
    impl Fx {
        fn new(text: &str) -> Self {
            Self {
                v: FakeV::new(text),
                a: FakeA::default(),
                j: FakeJ {
                    mode: Mode::Neutral,
                    calls: Cell::new(0),
                    sizes: RefCell::default(),
                },
                clock: Ticker::default(),
                gauge: Arc::new(FakeGauge(std::sync::Mutex::new(Some(1_000_000)))),
                memory: MemoryBudget::unbounded(),
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
                &self.a,
                &self.j,
                &self.clock,
                &*self.gauge,
                &mut (),
                &SearchInput {
                    start: &self.start,
                    roots: &self.roots,
                    targets: &[],
                    probes: &[],
                    scope: self.scope,
                    budget: self.budget,
                    memory: self.memory,
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
            self.v.e.set(root_state(), bps.clone());
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
        fx.v.e.set(
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
        assert_eq!(fx.v.e.calls.borrow().len(), 1);
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
        fx.v.e.set(root_state(), vec![rename(2, "a", "c", BASE)]);
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
        // One job at a time: at a wider window c is submitted before b's outcome is consumed.
        fx.v.width = 1;
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
        fx.v.e.set(state(vec![w(at_b)]), vec![child]);
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
        fx.v.e.set(
            state(vec![w(at_b)]),
            vec![rename(10, "b", "b1", at_b), rename(11, "b", "b2", at_b)],
        );
        fx.v.e.set(
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
        let max_seen: Vec<usize> = fx.v.e.calls.borrow().iter().map(|(_, m)| *m).collect();
        assert_eq!(max_seen, vec![200, 100, 100]);
        assert_eq!(r.depths.len(), 2);
        assert_eq!(r.depths[1].parents, 2);
        assert_eq!(r.ranking.len(), 6);
    }

    #[test]
    fn same_state_children_are_checked_once() {
        // Two children of one parent lead to the same state.
        let mut fx = Fx::new(BASE);
        fx.v.e.set(
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
        fx.v.e.set(root_state(), vec![rename(4, "a", "b", BASE)]);
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
        fx.v.e.set(
            state(vec![w("let b = 1;")]),
            vec![rename(10, "b", "b1", "let b = 1;")],
        );
        fx.v.e.set(
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
        fx.v.e.set(root_state(), vec![rename(1, "a", "b", BASE)]);
        fx.v.e
            .set(state(vec![w(at_b)]), vec![rename(2, "b", "c", at_b)]);
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
        fx.v.e.set(root_state(), vec![parent]);
        fx.v.e.set(
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
        fx.v.e.set(
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
        fx.v.e.set(
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
        fx.v.e.set(root_state(), vec![rename(3, "a", "d", BASE)]);
        let err = fx.run().unwrap_err();
        assert!(
            matches!(&err, SearchError::ChainedRoot { blueprint, parent }
                if *blueprint == bid(2) && *parent == plain.id()),
            "{err}"
        );
        assert!(fx.v.events.borrow().is_empty(), "no check, no audit");
        assert!(fx.v.checked_texts().is_empty());
        assert!(fx.v.e.calls.borrow().is_empty(), "no expansion");
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
        fx.v.e.by_state.insert(
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
        fx.v.e.by_state.insert(
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
        fx.v.e.set(root_state(), vec![extract_x(1, base)]);
        fx.v.e
            .set(state(vec![w(after)]), vec![rename(2, "x", "y", after)]);
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
        fx.v.e.set(root_state(), vec![rename(3, "f", "g", base)]);
        fx.v.e
            .set(state(vec![w(renamed)]), vec![extract_x(4, renamed)]);
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
        fx.v.e.set(
            state(vec![w("let b = 1;")]),
            vec![rename(10, "b", "b1", "let b = 1;")],
        );
        let r = fx.run().unwrap();
        assert!(r.soft_stopped && !r.budget_exhausted);
        assert_eq!(r.depths.len(), 1);
        assert_eq!(r.elapsed_ms, 12_000);
        assert_eq!(decided(&r).len(), 2);
        assert_eq!(fx.v.e.calls.borrow().len(), 1);
    }

    #[test]
    fn trace_has_one_entry_per_child_and_edits_only_on_request() {
        let build = |keep: bool| {
            let mut fx = Fx::new(BASE);
            fx.keep_edits = keep;
            fx.v.e.set(
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
        fx.v.e.set(
            state(vec![w(at_b)]),
            vec![rename(10, "b", "b1", at_b), rename(11, "b", "b2", at_b)],
        );
        fx.v.e.set(
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
        fx.v.e.set(
            root_state(),
            vec![rename_in(1, b_rs, "b", "p", "let b = 1;")],
        );
        fx.v.e.set(
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

    /// The jobs the fake ran, as (the parent's composed edits, its own edits, its limit).
    fn submits(fx: &Fx) -> Vec<(Vec<Edit>, Vec<Edit>, Limit)> {
        fx.v.ops()
            .into_iter()
            .filter_map(|op| match op {
                Op::Submit { on, own, limit } => Some((on, own, limit)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn children_are_jobs_on_their_parents_layer() {
        let fx = two_parents_two_children();
        let r = fx.run().unwrap();
        assert_eq!(r.ranking.len(), 6);
        let lv = |text: &str| vec![w(text)];
        let job = |on: Vec<Edit>, own: &str| (on, lv(own), Limit::Check);
        let (b, c) = ("let b = 1;", "let c = 1;");
        // Each child is one job of its own edits on its parent's Layer; no path is written again.
        assert_eq!(
            submits(&fx),
            vec![
                job(vec![], b),
                job(vec![], c),
                job(lv(b), "let b1 = 1;"),
                job(lv(b), "let b2 = 1;"),
                job(lv(c), "let c1 = 1;"),
                job(lv(c), "let c2 = 1;"),
            ]
        );
        let writes: Vec<u32> = r.depths.iter().map(|d| d.writes).collect();
        assert_eq!(writes, vec![2, 4]);
        assert_eq!(
            writes.iter().sum::<u32>(),
            u32::try_from(fx.v.writes().len()).unwrap()
        );
    }

    #[test]
    fn parents_are_expanded_on_their_layer() {
        let fx = two_parents_two_children();
        fx.run().unwrap();
        let ops = fx.v.ops();
        let expanded: Vec<Vec<Edit>> = ops
            .iter()
            .filter_map(|op| match op {
                Op::Expand { on, .. } => Some(on.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            expanded,
            vec![vec![], vec![w("let b = 1;")], vec![w("let c = 1;")]]
        );
        // With the fingerprint of the state of each, kept with its Layer.
        let states: Vec<StateFp> = fx.v.e.calls.borrow().iter().map(|(s, _)| *s).collect();
        assert_eq!(
            states,
            vec![
                root_state(),
                state(vec![w("let b = 1;")]),
                state(vec![w("let c = 1;")])
            ]
        );
        // The forks are opened once and closed once, after the last job, before the audit.
        assert_eq!(ops.iter().filter(|op| **op == Op::Open).count(), 1);
        assert_eq!(
            ops.iter()
                .filter(|op| matches!(op, Op::Close { .. }))
                .count(),
            1
        );
        assert_eq!(ops[ops.len() - 2..], [Op::Close { live: 0 }, Op::Audit]);
    }

    #[test]
    fn layers_follow_the_retention_rule() {
        let run = |beam_parents: u8| {
            let mut fx = Fx::new(BASE);
            fx.budget = SearchBudget {
                max_depth: 2,
                beam_parents,
                ..DEFAULT_SEARCH_BUDGET
            };
            fx.rename_children(1, &["b", "c", "d"]);
            let (at_b, at_c) = ("let b = 1;", "let c = 1;");
            fx.v.e
                .set(state(vec![w(at_b)]), vec![rename(10, "b", "b1", at_b)]);
            fx.v.e
                .set(state(vec![w(at_c)]), vec![rename(11, "c", "c1", at_c)]);
            fx.a.table.insert(at_b.to_owned(), clean(7));
            fx.a.table.insert(at_c.to_owned(), clean(6));
            fx.v.rule = Box::new(|edits| {
                Ok(if first_text(edits) == "let d = 1;" {
                    errors(1)
                } else {
                    ScopedFindings::default()
                })
            });
            let r = fx.run().unwrap();
            assert_eq!(fate_of(&r, 3), Fate::Failed { new_errors: 1 });
            fx.v.ops()
                .into_iter()
                .filter_map(|op| match op {
                    Op::Expand { live, .. } => Some(("expand", live)),
                    Op::Close { live } => Some(("close", live)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        // The root's alone; then b's and c's, the root's and the failed d's gone; none at the
        // close.
        assert_eq!(
            run(4),
            vec![("expand", 1), ("expand", 2), ("expand", 2), ("close", 0)]
        );
        // c is not picked as a parent: its Layer goes before the expansions of depth 2.
        assert_eq!(run(1), vec![("expand", 1), ("expand", 1), ("close", 0)]);
    }

    #[test]
    fn the_start_state_is_one_job() {
        let mut fx = from_start();
        fx.budget = SearchBudget {
            max_depth: 2,
            ..DEFAULT_SEARCH_BUDGET
        };
        let at_b = "let b = 1;";
        fx.v.e.set(
            on_start(vec![]),
            vec![rename(1, "a", "b", BASE), rename(2, "a", "c", BASE)],
        );
        fx.v.e
            .set(on_start(vec![w(at_b)]), vec![rename(10, "b", "b1", at_b)]);
        let r = fx.run().unwrap();
        assert_eq!(r.depths.len(), 2);
        assert_eq!(decided(&r).len(), 3);
        let ops = fx.v.ops();
        // One job of the start's edits on the base's root, before the first expansion, which
        // reads the start's Layer; the root's children are jobs on that Layer.
        assert_eq!(
            ops[..3],
            [
                Op::Open,
                Op::Submit {
                    on: vec![],
                    own: started(),
                    limit: Limit::None
                },
                Op::Expand {
                    on: started(),
                    live: 1
                },
            ]
        );
        assert_eq!(fx.v.writes().iter().filter(|e| **e == started()).count(), 1);
        let jobs = submits(&fx);
        for (on, own, _) in &jobs[1..3] {
            assert_eq!(on, &started(), "{own:?}");
        }
        assert_eq!(fx.v.checks.borrow()[0], started());
        assert_eq!(r.depths[0].writes, 3, "the start's job and two children");
    }

    #[test]
    fn the_start_job_is_not_cut_off() {
        let mut fx = from_start();
        fx.roots = vec![rename(1, "a", "b", BASE)];
        fx.run().unwrap();
        let limits: Vec<Limit> = submits(&fx).into_iter().map(|(_, _, l)| l).collect();
        assert_eq!(limits, vec![Limit::None, Limit::Check]);
    }

    #[test]
    fn a_failed_start_state_is_a_failed_report_naming_it() {
        let panicked = CheckError::DetectorPanicked {
            detector: "d".to_owned(),
            file: rel(S_RS),
        };
        let refused = CheckError::Edit {
            path: rel(S_RS),
            reason: "no".to_owned(),
        };
        for (what, failure) in [("diagnosis", panicked), ("write", refused)] {
            for with_roots in [true, false] {
                let mut fx = from_start();
                if with_roots {
                    fx.roots = vec![
                        rename(1, "a", "b", BASE),
                        rename_in(2, S_RS, "started", "start", "started"),
                    ];
                }
                fx.v.e
                    .set(on_start(vec![]), vec![rename(3, "a", "c", BASE)]);
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
                    fx.v.submit_fault = Box::new(move |edits, _| {
                        (edits == started().as_slice()).then(|| fail.clone())
                    });
                }
                let at = format!("{what}, roots {with_roots}");
                // A report, never `NoCandidates`.
                let r = fx.run().unwrap();
                let i = failed(&r);
                let want = failure.clone().into_failure().unwrap();
                assert_eq!(i.start(), Some(&want), "{at}");
                assert!(!i.budget_exhausted() && !r.budget_exhausted, "{at}");
                assert!(r.depths.is_empty(), "{at}: no depth runs");
                assert!(fx.v.e.calls.borrow().is_empty(), "{at}: nothing expanded");
                assert_eq!(fx.v.writes(), vec![started()], "{at}: the start's job only");
                assert!(r.trace.iter().all(|t| t.node.is_none()), "{at}");
                if with_roots {
                    assert_eq!(
                        fates(&r),
                        vec![
                            (bid(1), Fate::Blocked(Stage::Check)),
                            (bid(2), Fate::Blocked(Stage::Check))
                        ],
                        "{at}"
                    );
                    assert_eq!(
                        i.blocking(),
                        &[
                            Blocking::CheckFailed {
                                blueprint: bid(1),
                                failure: want.clone()
                            },
                            Blocking::CheckFailed {
                                blueprint: bid(2),
                                failure: want
                            },
                        ],
                        "{at}"
                    );
                } else {
                    assert!(r.trace.is_empty() && i.blocking().is_empty(), "{at}");
                }
                assert_eq!(fx.v.audits(), 1, "{at}");
            }
        }
    }

    /// 1,000 kB of growth past the RSS at the search's start.
    fn small_budget() -> MemoryBudget {
        MemoryBudget::of_available(RESERVED_RAM_GB * 1024 * 1024 + 1_000)
    }

    #[test]
    fn the_start_job_is_cut_by_the_hard_budget_and_the_rss_stop() {
        for hard in [true, false] {
            let mut fx = from_start();
            fx.roots = vec![rename(1, "a", "b", BASE)];
            fx.memory = small_budget();
            let (clock, gauge) = (fx.clock.clone(), fx.gauge.clone());
            // The first wait for the start's job passes the hard limit, or the memory budget.
            fx.v.next_hook = Some(Box::new(move |n| {
                if n != 0 {
                    return false;
                }
                if hard {
                    clock.advance(DEFAULT_SEARCH_BUDGET.hard_ms);
                } else {
                    gauge.set(1_001_001);
                }
                true
            }));
            let r = fx.run().unwrap();
            assert_eq!(
                (r.budget_exhausted, r.memory_stopped),
                (hard, !hard),
                "{hard}"
            );
            assert!(failed(&r).budget_exhausted() && failed(&r).start().is_none());
            assert!(fx.v.ops().contains(&Op::Cancel(true)), "{hard}");
            assert_eq!(fates(&r), vec![(bid(1), Fate::TimedOut)], "{hard}");
            assert_eq!(r.trace[0].node, None);
            assert!(r.depths.is_empty() && fx.v.e.calls.borrow().is_empty());
            assert_eq!(fx.v.writes(), vec![started()]);
        }
    }

    #[test]
    fn the_rss_stop_is_relative_to_the_search_start() {
        // A session that grew once starts its search at 9 GB; its budget counts from there.
        for (rss, stopped) in [(9_000_000, false), (9_001_000, false), (9_001_001, true)] {
            let mut fx = Fx::new(BASE);
            fx.rename_children(1, &["b", "c"]);
            fx.gauge.set(9_000_000);
            fx.memory = small_budget();
            let gauge = fx.gauge.clone();
            // The read of the first wait for the checks.
            fx.v.next_hook = Some(Box::new(move |n| {
                if n == 1 {
                    gauge.set(rss);
                }
                n == 1
            }));
            let r = fx.run().unwrap();
            assert_eq!(r.memory_stopped, stopped, "{rss}");
            let passed = if stopped { 0 } else { 2 };
            assert_eq!(r.depths[0].passed, passed, "{rss}");
        }
    }

    /// Three children b, c, d of the root (checked in that order) on forks of width 2, with the
    /// RSS stop reached on the third `next`: by then b has finished and is not received, c is
    /// received before its turn, and d has not finished.
    fn stopped_while_checking() -> Fx {
        let mut fx = Fx::new(BASE);
        fx.v.width = 2;
        fx.rename_children(1, &["b", "c", "d"]);
        for (text, n) in [("let b = 1;", 3), ("let c = 1;", 2), ("let d = 1;", 1)] {
            fx.a.table.insert(text.to_owned(), clean(n));
        }
        fx.memory = small_budget();
        let gauge = fx.gauge.clone();
        // `next` 0 gives the expansion; 1 finishes b and c and gives c, which frees the place
        // d is submitted to; 2 reads the stop.
        fx.v.next_hook = Some(Box::new(move |n| {
            if n == 2 {
                gauge.set(1_001_001);
            }
            n == 2
        }));
        fx
    }

    #[test]
    fn an_outcome_received_before_a_stop_is_consumed() {
        let fx = stopped_while_checking();
        let r = fx.run().unwrap();
        assert!(r.memory_stopped);
        assert_eq!(fate_of(&r, 2), Fate::Passed);
        assert!(decided(&r).iter().any(|c| c.blueprint() == bid(2)));
    }

    #[test]
    fn a_cancel_that_returns_false_is_consumed_as_its_done() {
        let fx = stopped_while_checking();
        let r = fx.run().unwrap();
        let cancels: Vec<Op> =
            fx.v.ops()
                .into_iter()
                .filter(|op| matches!(op, Op::Cancel(_)))
                .collect();
        // b had finished: not withdrawn, and its verdict is the search's; d had not.
        assert_eq!(cancels, vec![Op::Cancel(false), Op::Cancel(true)]);
        assert_eq!(fate_of(&r, 1), Fate::Passed);
        assert_eq!(
            decided(&r)
                .iter()
                .map(Candidate::blueprint)
                .collect::<Vec<_>>(),
            vec![bid(1), bid(2)]
        );
    }

    #[test]
    fn a_withdrawn_child_is_timed_out_never_blocked() {
        let fx = stopped_while_checking();
        let r = fx.run().unwrap();
        assert_eq!(fate_of(&r, 3), Fate::TimedOut);
        assert!(r.trace[2].node.is_some(), "it was submitted");
        let Resolution::Decided {
            blocking,
            decision_log,
            evidence,
            ..
        } = &r.resolution
        else {
            panic!("expected Decided");
        };
        assert!(blocking.is_empty(), "{blocking:?}");
        assert!(
            !decision_log
                .entries()
                .iter()
                .any(|e| e.blueprint() == bid(3) && e.stage() == Stage::Check)
        );
        assert!(evidence.verdicts().iter().all(|(id, _)| *id != bid(3)));
        assert_eq!((r.depths[0].checked, r.depths[0].timed_out), (2, 1));
        assert!(!r.budget_exhausted);
    }

    #[test]
    fn a_stop_during_expansion_submits_no_child() {
        let mut fx = two_parents_two_children();
        fx.memory = small_budget();
        let gauge = fx.gauge.clone();
        // `next` 0 gives the root's expansion, 1 and 2 the checks of b and c; 3 is the first
        // wait for the expansions of depth 2.
        fx.v.next_hook = Some(Box::new(move |n| {
            if n == 3 {
                gauge.set(1_001_001);
            }
            n == 3
        }));
        let r = fx.run().unwrap();
        assert!(r.memory_stopped && !r.budget_exhausted);
        // Both expansions withdrawn: no child, and no expand error.
        assert_eq!(
            fx.v.ops()
                .iter()
                .filter(|op| **op == Op::Cancel(true))
                .count(),
            2
        );
        assert!(r.expand_errors.is_empty());
        assert!(r.trace.iter().all(|t| t.depth == 1));
        assert_eq!(fx.v.writes().len(), 2);
        assert_eq!(r.depths.len(), 2);
        assert_eq!(r.depths[1].generated, 0);
        assert_eq!(decided(&r).len(), 2);
    }

    #[test]
    fn the_hard_budget_is_checked_before_each_submit() {
        let at = |width: usize| {
            let mut fx = Fx::new(BASE);
            fx.v.width = width;
            fx.budget = SearchBudget {
                hard_ms: 20_000,
                ..fx.budget
            };
            fx.v.tick = Some((fx.clock.clone(), 5_000));
            fx.rename_children(1, &["b", "c", "d", "e", "f"]);
            let r = fx.run().unwrap();
            let nodes: Vec<bool> = r.trace.iter().map(|t| t.node.is_some()).collect();
            (
                fates(&r),
                nodes,
                fx.v.checks.borrow().len(),
                r.budget_exhausted,
            )
        };
        // The order of the serial search: three checks begin within the limit.
        let serial = at(1);
        assert_eq!(serial.2, 3);
        assert!(serial.3);
        assert_eq!(serial.1, vec![true, true, true, false, false]);
        for width in 2..=8 {
            assert_eq!(at(width), serial, "{width}");
        }
    }

    #[test]
    fn the_window_never_holds_more_than_w() {
        for width in 1..=8 {
            let mut fx = Fx::new(BASE);
            fx.v.width = width;
            let names: Vec<String> = (0..20).map(|i| format!("n{i:02}")).collect();
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            fx.rename_children(100, &refs);
            let r = fx.run().unwrap();
            assert_eq!(r.depths[0].checked, 20);
            assert_eq!(fx.v.most_out(), width, "{width}");
        }
    }

    /// An id of the Blueprint that renames to `name`.
    fn id_of_name(name: &str) -> u16 {
        name.bytes()
            .fold(7u16, |h, b| h.wrapping_mul(31).wrapping_add(u16::from(b)))
            | 0x8000
    }

    /// A search of depth 3 whose every state `let X = 1;` has the children that rename `X` to
    /// `Xa`, `Xb`, `Xc` and `zz`: a name ending in `c` adds errors, one with `bb` is cut off,
    /// `rab` panics, and `zz` is a duplicate across parents. With `order`, the fake gives its
    /// outcomes in that seeded order and answers some waits with `Pending`.
    fn generated(width: usize, order: Option<u64>) -> SearchReport {
        let mut fx = Fx::new(BASE);
        fx.budget = SearchBudget {
            max_depth: 3,
            beam: 6,
            ..DEFAULT_SEARCH_BUDGET
        };
        fx.v.width = width;
        fx.v.order = order;
        if let Some(seed) = order {
            fx.v.next_hook = Some(Box::new(move |n| {
                mix(seed.wrapping_add(7), n as u64).is_multiple_of(3)
            }));
        }
        fx.roots = vec![rename(1, "a", "r", BASE)];
        fx.v.e.generate = Some(Box::new(|edits: &[Edit]| {
            let text = if edits.is_empty() {
                BASE.to_owned()
            } else {
                first_text(edits)
            };
            let name = text
                .trim_start_matches("let ")
                .split(' ')
                .next()
                .unwrap_or_default()
                .to_owned();
            ["a", "b", "c"]
                .iter()
                .map(|s| format!("{name}{s}"))
                .chain(["zz".to_owned()])
                .filter(|to| *to != name)
                .map(|to| rename(id_of_name(&to), &name, &to, &text))
                .collect()
        }));
        fx.v.rule = Box::new(|edits| {
            let text = first_text(edits);
            let name = text
                .trim_start_matches("let ")
                .split(' ')
                .next()
                .unwrap_or("");
            if name.contains("bb") {
                Err(CheckError::TimedOut { limit_ms: 6_000 })
            } else if name == "rab" {
                Err(CheckError::Panicked("rab".to_owned()))
            } else if name.ends_with('c') {
                Ok(errors(2))
            } else {
                Ok(ScopedFindings::default())
            }
        });
        let r = fx.run().unwrap();
        assert!(fx.v.most_out() <= width);
        r
    }

    #[test]
    fn parallel_search_equals_serial() {
        let serial = generated(1, None);
        // The fixture reaches every fate the windowed consumption must order.
        assert_eq!(serial.depths.len(), 3);
        let fates: Vec<&Fate> = serial.trace.iter().map(|t| &t.fate).collect();
        for want in [
            Fate::Passed,
            Fate::Failed { new_errors: 2 },
            Fate::TimedOut,
            Fate::Blocked(Stage::Check),
            Fate::Pruned(PruneStage::Duplicate),
        ] {
            assert!(fates.contains(&&want), "{want:?} in {fates:?}");
        }
        for width in 1..=8 {
            for seed in 0..20 {
                assert_eq!(
                    generated(width, Some(seed)),
                    serial,
                    "width {width}, seed {seed}"
                );
            }
        }
    }

    #[test]
    fn layers_are_dropped_before_close_on_every_path() {
        let poisoned = || CheckError::Poisoned("x".to_owned());
        let cases: Vec<(&str, Fx)> = vec![
            ("a report", two_parents_two_children()),
            ("an expansion's lost session", {
                let mut fx = Fx::new(BASE);
                fx.roots = vec![rename(1, "a", "b", BASE)];
                fx.v.e
                    .by_state
                    .insert(root_state(), Err(ExpandError::Session(poisoned())));
                fx
            }),
            ("no candidate", Fx::new(BASE)),
            ("a check's lost session, a Layer received early", {
                let mut fx = Fx::new(BASE);
                fx.rename_children(1, &["b", "c"]);
                fx.v.rule = Box::new(move |edits| {
                    if first_text(edits) == "let b = 1;" {
                        Err(poisoned())
                    } else {
                        Ok(ScopedFindings::default())
                    }
                });
                fx
            }),
            ("a failed next", {
                let mut fx = Fx::new(BASE);
                fx.rename_children(1, &["b"]);
                fx.v.fails.insert("next", poisoned());
                fx
            }),
        ];
        for (what, fx) in cases {
            let ended = fx.run();
            let ops = fx.v.ops();
            let closes: Vec<&Op> = ops
                .iter()
                .filter(|op| matches!(op, Op::Close { .. }))
                .collect();
            assert_eq!(closes, vec![&Op::Close { live: 0 }], "{what}");
            match ended {
                Ok(_) => assert_eq!(ops.last(), Some(&Op::Audit), "{what}"),
                Err(e) => {
                    assert_eq!(ops.last(), Some(&Op::Close { live: 0 }), "{what}: {e}");
                    assert_eq!(fx.v.audits(), 0, "{what}");
                }
            }
        }
    }

    #[test]
    fn a_failed_close_is_a_search_error() {
        let closed = || CheckError::Poisoned("close".to_owned());
        // Instead of a report.
        let mut fx = Fx::new(BASE);
        fx.rename_children(1, &["b"]);
        fx.v.fails.insert("close", closed());
        let err = fx.run().unwrap_err();
        assert!(
            matches!(err, SearchError::Fork(CheckError::Poisoned(ref m)) if m == "close"),
            "{err}"
        );
        assert_eq!(fx.v.audits(), 0);

        // Instead of `NoCandidates`.
        let mut fx = Fx::new(BASE);
        fx.v.fails.insert("close", closed());
        let err = fx.run().unwrap_err();
        assert!(matches!(err, SearchError::Fork(_)), "{err}");

        // A child's session failure first: that is the error.
        let mut fx = Fx::new(BASE);
        fx.rename_children(1, &["b"]);
        fx.v.fails.insert("close", closed());
        fx.v.rule = Box::new(|_| {
            Err(CheckError::RevertMismatch {
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
    fn a_session_failure_of_a_call_is_a_fork_error() {
        let poisoned = || CheckError::Poisoned("x".to_owned());
        let mut cases: Vec<(&str, Fx)> = Vec::new();
        for call in ["open", "submit", "expand", "next", "layer_files"] {
            let mut fx = Fx::new(BASE);
            fx.rename_children(1, &["b"]);
            fx.v.fails.insert(call, poisoned());
            cases.push((call, fx));
        }
        cases.push(("the start's job", {
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
        }));
        cases.push(("a cancel", {
            let mut fx = stopped_while_checking();
            fx.v.fails.insert("cancel", poisoned());
            fx
        }));
        for (what, fx) in cases {
            let err = fx.run().unwrap_err();
            assert!(
                matches!(err, SearchError::Fork(CheckError::Poisoned(_))),
                "{what}: {err}"
            );
            let ops = fx.v.ops();
            assert!(!ops.contains(&Op::Audit), "{what}");
            if what == "open" {
                assert_eq!(ops, vec![Op::Open], "nothing to close");
            } else {
                assert_eq!(ops.last(), Some(&Op::Close { live: 0 }), "{what}");
            }
        }
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
        assert_eq!((SEARCH_FORKS, SEARCH_TICK_MS), (4, 100));
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
