//! The use case: the best-first beam search of one decision.
//!
//! Children of a state are proposed by an [`Expander`], run through the stages of
//! [`crate::app::apply`] against the text of their parent, assessed from the texts alone, judged
//! once per depth and pruned to a beam; only the beam reaches the analyzer. The search ends with
//! a [`Resolution`], the ranking of every verified node and a trace of every child.
//!
//! The numbers of the budget are those of the decisions in force in `SEARCH_LEDGER.md`: D1 (time),
//! D2 (revert audit), D3 (memory per session) and D4 (the triggers that call for perf work).

use crate::app::apply::{Acc, Prepared, Ready, prepare};
use crate::app::ports::{
    Assessor, CheckError, Clock, ExpandError, Expander, Judge, JudgeError, RevertAudit,
};
use crate::domain::search::{compose, prune, rank_final, reward_score, score};
use crate::domain::{
    Blocking, Blueprint, BlueprintId, Candidate, ChangeProbe, Edit, Evidence, Infeasible, Inserted,
    IntentKind, JudgeRequest, NEAR_MISS_MAX, NearMiss, NewNode, NodeId, NonEmpty, Noul,
    OUTER_CHECK_MAX, Prior, Proposal, ProposalId, ProviderTag, PruneStage, RelPath, Resolution,
    Reward, RiskMark, Rule, RuleCatalog, Scope, Scored, SearchBudget, SearchTree, Stage, StateFp,
    StaticMetrics, Target, Verdict, Weights,
};
use std::collections::{BTreeMap, BTreeSet};
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
    /// The registered rules.
    pub rules: &'a RuleCatalog,
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
    /// A stage before the check refused it.
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
    /// The intent it calls.
    pub intent: IntentKind,
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
    /// Time in the expander.
    pub expand_ms: u64,
    /// Time in the stages and the assessor.
    pub assess_ms: u64,
    /// Time in the judge.
    pub judge_ms: u64,
    /// Time in the checks.
    pub check_ms: u64,
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
    /// A check failed as a process, not as a verdict, and the search cannot go on.
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
    /// The session did not return to its baseline after the checks.
    #[error("revert audit failed: {0}")]
    RevertAudit(#[source] CheckError),
    /// The first depth had nothing to try.
    #[error("no candidate: no root and the expander produced nothing")]
    NoCandidates,
}

/// A child before the stages.
struct Child {
    parent: NodeId,
    bp: Blueprint,
    root: bool,
}

/// A child that passed the stages, waiting for the prune and the check.
struct Staged {
    trace: usize,
    parent: NodeId,
    bp: Blueprint,
    edits: Vec<Edit>,
    marks: BTreeSet<RiskMark>,
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

/// The rule that covers a whole path, or `None`.
fn path_rule<'r>(tree: &SearchTree, leaf: NodeId, rules: &'r RuleCatalog) -> Option<&'r Rule> {
    let mut best: Option<&Rule> = None;
    for id in tree.path(leaf) {
        let Some(bp) = tree.node(id).and_then(|n| n.blueprint()) else {
            continue;
        };
        let rule = rules.for_intent(bp.intent().kind())?;
        best = match best {
            Some(b) if !rule.min_scope().covers(b.min_scope()) => Some(b),
            _ => Some(rule),
        };
    }
    best
}

/// Searches for the best verified changes that answer one decision.
///
/// `v` checks, `e` proposes, `a` measures, `j` judges and `clock` is the only source of time.
/// The expander and the verifier share the one session `s`.
///
/// # Contract
/// - C1: depth 1 expands the start state, the roots of the input first; depth `d` above 1
///   expands the first `beam_parents` of `rank_final` over the passing nodes of depth `d - 1`,
///   each for at most `max(1, max_expand / parents)` children, and the search ends when there
///   is no such node. A failed analysis of one parent is recorded in the report and the parent
///   yields nothing; a lost session is `SearchError::Expand`; no root and no child at depth 1
///   is `SearchError::NoCandidates`.
///   tested: app::search::tests::siblings_are_checked_consecutively
///   tested: app::search::tests::no_candidates_is_error
///   tested: app::search::tests::expand_errors_are_recorded_and_session_error_is_fatal
/// - C2: every child runs the stages of `apply_blueprints` against the text of its parent's
///   state, not the session base, so a read of the parent's text is fresh; a refusal is
///   `Fate::Blocked` with its `Blocking` and log entry and the child is never checked. A child
///   that passes has the edits of its parent overridden by its own, the marks of its parent
///   plus its own, and the metrics the assessor gives for its change against the parent's text.
///   tested: app::search::tests::child_blueprint_reads_parent_state
///   tested: app::search::tests::best_planted_child_survives_with_fake_expander
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
///   the search goes on; a check that fails any other way is `SearchError::Check`; a check
///   that adds errors is `Fate::Failed` and `Blocking::NewErrors`, and a near miss when
///   the errors are few.
///   tested: app::search::tests::budget_exhausted_returns_verified_so_far
///   tested: app::search::tests::hard_budget_with_nothing_verified_is_failed_exhausted
///   tested: app::search::tests::check_timeout_is_budget_exhausted_blocking
///   tested: app::search::tests::same_state_children_are_checked_once
/// - C6: after a depth, when the time spent has reached `soft_ms`, `soft_stopped` is set and
///   no further depth starts.
///   tested: app::search::tests::soft_budget_stops_after_the_depth
/// - C7: the revert is audited exactly once, after the last check and before the report is
///   returned, whether the resolution is `Decided` or `Failed`; an audit that fails is
///   `SearchError::RevertAudit`. A search that ends in an error is not audited.
///   tested: app::search::tests::revert_audit_runs_once_at_end
/// - C8: the ranking holds every node that passed, at any depth, by `rank_final`, each with
///   the Blueprints of its path and its prior. The resolution is `Decided` with the first
///   `OUTER_CHECK_MAX` of the ranking as candidates in that order, each with the marks of its
///   path and the rule of its path; the rule is `None` unless every intent on the path has a
///   rule, and then the one with the widest `min_scope`, the leaf's on a tie. With no node that
///   passed the resolution is `Failed` with every blocking, and the budget flag, of the search.
///   tested: app::search::tests::decided_lists_top_outer_check_max_by_reward
///   tested: app::search::tests::path_rule_needs_a_rule_for_every_kind
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
    V: RevertAudit,
    E: Expander<Session = V::Session>,
    A: Assessor,
    J: Judge,
{
    let t0 = clock.now_ms();
    let elapsed = || clock.now_ms().saturating_sub(t0);
    let b = input.budget;
    let w = input.weights;
    let baseline = v.baseline(s);
    let mut tree = SearchTree::new(input.start.to_vec());
    let mut run = Run::default();
    let mut passing: Vec<NodeId> = Vec::new();
    let mut last_passing: Vec<NodeId> = Vec::new();

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
                Err(err) => return Err(SearchError::Expand(err)),
            }
        }
        if d == 1 && children.is_empty() {
            return Err(SearchError::NoCandidates);
        }
        stats.generated = sat32(children.len());
        let t_stage = clock.now_ms();
        stats.expand_ms = t_stage.saturating_sub(t_expand);

        // 2. stages and assessment, against the parent's text
        let mut staged: Vec<Option<Staged>> = Vec::new();
        for child in children {
            let Some(parent) = tree.node(child.parent) else {
                continue;
            };
            let bp = child.bp;
            let proposal = ProposalId::of(&bp);
            let ix = run.trace.len();
            run.trace.push(TraceEntry {
                depth: d,
                parent: child.parent,
                blueprint: bp.id(),
                proposal,
                intent: bp.intent().kind(),
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
            let view = |p: &RelPath| -> Option<String> {
                match parent.edits().iter().rev().find(|ed| ed.path == *p) {
                    Some(ed) => ed.new_text.clone(),
                    None => base(p),
                }
            };
            match prepare(v, sess, &bp, &view, &mut run.acc) {
                Prepared::Blocked { stage, blocking } => {
                    run.trace[ix].fate = Fate::Blocked(stage);
                    run.blocking.push(blocking);
                }
                Prepared::Ready(Ready {
                    edits: own,
                    marks: own_marks,
                }) => {
                    let edits = compose(parent.edits(), &own, &base);
                    let mut marks = parent.marks().clone();
                    marks.extend(own_marks);
                    let before: Vec<Edit> = own
                        .iter()
                        .map(|ed| match view(&ed.path) {
                            Some(text) => Edit::write(ed.path.clone(), text),
                            None => Edit::delete(ed.path.clone()),
                        })
                        .collect();
                    let metrics = a.assess(&before, &own, input.probes);
                    let state = StateFp::of(&edits);
                    run.trace[ix].state = Some(state);
                    if input.keep_trace_edits {
                        run.trace[ix].edits = Some(edits.clone());
                    }
                    staged.push(Some(Staged {
                        trace: ix,
                        parent: child.parent,
                        bp,
                        edits,
                        marks,
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
            .map(|c| Proposal::new(&c.bp, d, c.metrics))
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
            run.acc
                .note(c.bp.id(), Stage::Prune, false, stage.to_string());
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
            let bp_id = c.bp.id();
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
                blueprint: c.bp,
                edits: c.edits,
                marks: c.marks,
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
            let result = match tree.node(id) {
                Some(n) => v.check(s, n.edits(), input.scope),
                None => continue,
            };
            stats.check_ms = stats
                .check_ms
                .saturating_add(clock.now_ms().saturating_sub(t_check));
            stats.checked = stats.checked.saturating_add(1);
            match result {
                Ok(found) => {
                    let verdict = Verdict::from_baseline(&baseline, &found);
                    run.acc.verdicts.push((bp_id, verdict.clone()));
                    let errors =
                        verdict.new_errors.errors.len() + verdict.new_errors.unmapped.len();
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
                        run.verdicts.insert(id, verdict);
                        passing.push(id);
                        depth_passing.push(id);
                        run.trace[t].fate = Fate::Passed;
                        stats.passed = stats.passed.saturating_add(1);
                    } else {
                        run.trace[t].fate = Fate::Failed { new_errors: errors };
                        if (1..=NEAR_MISS_MAX).contains(&errors) {
                            run.near_miss.push(NearMiss {
                                blueprint: bp_id,
                                new_errors: errors,
                            });
                        }
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
                Err(source) => {
                    return Err(SearchError::Check {
                        blueprint: bp_id,
                        source,
                    });
                }
            }
        }
        run.depths.push(stats);
        last_passing = depth_passing;

        // 6. limits
        if run.budget_exhausted {
            break;
        }
        if elapsed() >= b.soft_ms {
            run.soft_stopped = true;
            break;
        }
    }

    // 7. the revert, audited once
    v.audit_revert(s).map_err(SearchError::RevertAudit)?;

    // 8. ranking and resolution
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
    for r in ranking.iter().take(OUTER_CHECK_MAX) {
        let (Some(n), Some(verdict)) = (tree.node(r.node), run.verdicts.get(&r.node)) else {
            continue;
        };
        let Some(leaf) = n.blueprint() else {
            continue;
        };
        let rule = path_rule(&tree, r.node, input.rules);
        if let Ok(c) = Candidate::new(
            leaf.id(),
            n.edits().to_vec(),
            verdict.clone(),
            n.marks().clone(),
            rule,
            input.scope,
        ) {
            run.acc.note(
                leaf.id(),
                Stage::Candidate,
                true,
                c.disposition().to_string(),
            );
            candidates.push(c);
        }
    }
    let resolution = match NonEmpty::new(candidates) {
        Some(blueprints) => Resolution::Decided {
            blueprints,
            decision_log: run.acc.log,
            evidence: Evidence {
                scope: input.scope,
                fingerprints_checked: run.acc.fingerprints_checked,
                baseline_size: baseline.len(),
                verdicts: run.acc.verdicts,
                budget: b.decision_budget(),
            },
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
        CleanDelta, Criterion, DEFAULT_BUDGET, DEFAULT_WEIGHTS, DecisionRecord, DeriveName,
        Disposition, ErrorCode, Finding, FindingCode, Ident, IntentCall, Judgement, MutationSpec,
        ReadRange, ReviewReason, ReviewRecord, RuleId, ScopedFindings, SpecRef, TextRange,
    };
    use std::cell::{Cell, RefCell};
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
    fn rid(n: u8) -> RuleId {
        RuleId::parse(&format!("01890a5d-ac96-774b-bcce-b302099a80{n:02x}")).unwrap()
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
    fn make(n: u16, intent: IntentCall, specs: Vec<MutationSpec>, text: &str) -> Blueprint {
        let end = u32::try_from(text.len()).unwrap();
        Blueprint::new(
            bid(n),
            intent,
            specs,
            vec![ReadRange::capture(rel(LIB), text, tr(0, end)).unwrap()],
            vec![SpecRef::new("t").unwrap()],
            DecisionRecord::new("agent", "why").unwrap(),
        )
        .unwrap()
    }
    /// Renames the first `from` of `text` to `to`; the read is the whole `text`.
    fn rename(n: u16, from: &str, to: &str, text: &str) -> Blueprint {
        let at = u32::try_from(text.find(from).unwrap()).unwrap();
        let end = at + u32::try_from(from.len()).unwrap();
        make(
            n,
            IntentCall::RenameLocal {
                from: Ident::parse(from).unwrap(),
                to: Ident::parse(to).unwrap(),
            },
            vec![MutationSpec::replace(rel(LIB), tr(at, end), to).unwrap()],
            text,
        )
    }
    fn derive_clone(n: u16, text: &str) -> Blueprint {
        make(
            n,
            IntentCall::AddDerive {
                derives: NonEmpty::new(vec![DeriveName::parse("Clone").unwrap()]).unwrap(),
            },
            vec![MutationSpec::insert(rel(LIB), 0, "#[derive(Clone)]\n").unwrap()],
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

    struct FakeV {
        texts: BTreeMap<RelPath, String>,
        rule: CheckRule,
        checks: RefCell<Vec<Vec<Edit>>>,
        events: RefCell<Vec<&'static str>>,
        audit_error: Option<CheckError>,
        tick: Option<(Ticker, u64)>,
    }
    impl Verifier for FakeV {
        type Session = ();
        fn text(&self, _: &(), path: &RelPath) -> Option<String> {
            self.texts.get(path).cloned()
        }
        fn facts(&self, _: &(), _: &RelPath) -> FileFacts {
            FileFacts::default()
        }
        fn baseline(&self, _: &()) -> Vec<Finding> {
            vec![]
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
            match &self.audit_error {
                Some(e) => Err(e.clone()),
                None => Ok(()),
            }
        }
    }
    impl FakeV {
        fn new(text: &str) -> Self {
            Self {
                texts: BTreeMap::from([(rel(LIB), text.to_owned())]),
                rule: Box::new(|_| Ok(ScopedFindings::default())),
                checks: RefCell::default(),
                events: RefCell::default(),
                audit_error: None,
                tick: None,
            }
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
        rules: RuleCatalog,
        budget: SearchBudget,
        scope: Scope,
        keep_edits: bool,
    }
    impl Fx {
        fn new(text: &str) -> Self {
            Self {
                v: FakeV::new(text),
                e: FakeE::default(),
                a: FakeA::default(),
                j: FakeJ {
                    mode: Mode::Neutral,
                    calls: Cell::new(0),
                    sizes: RefCell::default(),
                },
                clock: Ticker::default(),
                roots: vec![],
                rules: RuleCatalog::default(),
                budget: SearchBudget {
                    max_depth: 1,
                    ..DEFAULT_SEARCH_BUDGET
                },
                scope: Scope::Workspace,
                keep_edits: false,
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
                    start: &[],
                    roots: &self.roots,
                    targets: &[],
                    probes: &[],
                    rules: &self.rules,
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
            .rfind(|e| e.blueprint == bid(3))
            .unwrap();
        assert_eq!((last.stage, last.ok), (Stage::Freshness, false));
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
    fn decided_lists_top_outer_check_max_by_reward() {
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

        assert_eq!(OUTER_CHECK_MAX, 3);
        let ids: Vec<BlueprintId> = decided(&r).iter().map(Candidate::blueprint).collect();
        assert_eq!(ids, vec![bid(2), bid(4), bid(5)]);
        let ranked: Vec<BlueprintId> = r.ranking.iter().map(|x| x.path[0]).collect();
        assert_eq!(ranked, vec![bid(2), bid(4), bid(5), bid(1), bid(3)]);
        let Resolution::Decided { evidence, .. } = &r.resolution else {
            panic!("expected Decided");
        };
        assert_eq!(evidence.verdicts.len(), 5);
        assert_eq!(evidence.budget, fx.budget.decision_budget());
    }

    /// The disposition of the candidate whose Blueprint is `leaf`.
    fn disposition_of(r: &SearchReport, leaf: u16) -> Disposition {
        decided(r)
            .iter()
            .find(|c| c.blueprint() == bid(leaf))
            .unwrap()
            .disposition()
    }
    fn rule(n: u8, kind: IntentKind, scope: Scope) -> Rule {
        Rule::register(
            rid(n),
            kind,
            ReviewRecord {
                reviewer: "me".to_owned(),
                reviewed_at_ms: 1,
                note: String::new(),
            },
            scope,
        )
        .unwrap()
    }
    /// Search over two depths: `first` then a rename of `S`/`b`; returns the report.
    fn two_kind_path(rules: Vec<Rule>, scope: Scope) -> SearchReport {
        let base = "struct S;";
        let after = "#[derive(Clone)]\nstruct S;";
        let mut fx = Fx::new(base);
        fx.budget = SearchBudget {
            max_depth: 2,
            ..DEFAULT_SEARCH_BUDGET
        };
        fx.rules = RuleCatalog::new(rules).unwrap();
        fx.scope = scope;
        fx.e.set(root_state(), vec![derive_clone(1, base)]);
        fx.e.set(state(vec![w(after)]), vec![rename(2, "S", "T", after)]);
        fx.run().unwrap()
    }

    #[test]
    fn path_rule_needs_a_rule_for_every_kind() {
        // Same kind twice, one rule: the rule covers the whole path.
        let mut fx = Fx::new(BASE);
        fx.budget = SearchBudget {
            max_depth: 2,
            ..DEFAULT_SEARCH_BUDGET
        };
        fx.rules =
            RuleCatalog::new(vec![rule(1, IntentKind::RenameLocal, Scope::Affected)]).unwrap();
        fx.e.set(root_state(), vec![rename(1, "a", "b", BASE)]);
        fx.e.set(
            state(vec![w("let b = 1;")]),
            vec![rename(2, "b", "c", "let b = 1;")],
        );
        let r = fx.run().unwrap();
        assert_eq!(
            disposition_of(&r, 2),
            Disposition::AutoApplicable { rule: rid(1) }
        );

        // Two kinds, a rule for the leaf's kind only: no rule for the path.
        let r = two_kind_path(
            vec![rule(1, IntentKind::RenameLocal, Scope::Affected)],
            Scope::Workspace,
        );
        assert_eq!(
            disposition_of(&r, 2),
            Disposition::NeedsReview {
                reason: ReviewReason::NoRule
            }
        );

        // A rule for each kind: the widest min_scope wins, here the first kind's, which a
        // check at the narrow scope does not cover; the leaf's own rule alone would pass.
        let r = two_kind_path(
            vec![
                rule(1, IntentKind::RenameLocal, Scope::Affected),
                rule(2, IntentKind::AddDerive, Scope::Workspace),
            ],
            Scope::Affected,
        );
        assert_eq!(
            disposition_of(&r, 2),
            Disposition::NeedsReview {
                reason: ReviewReason::ScopeTooNarrow
            }
        );

        // Equal min_scope: the leaf's rule.
        let r = two_kind_path(
            vec![
                rule(1, IntentKind::RenameLocal, Scope::Affected),
                rule(2, IntentKind::AddDerive, Scope::Affected),
            ],
            Scope::Affected,
        );
        assert_eq!(
            disposition_of(&r, 2),
            Disposition::AutoApplicable { rule: rid(1) }
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
        assert_eq!(r.trace[0].intent, IntentKind::RenameLocal);
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
