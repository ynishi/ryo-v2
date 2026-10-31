//! What came of a set of Blueprints: candidates that passed, or why none did.
//!
//! The central rule is [`disposition`]: whether a passing candidate may be applied without a
//! human, needs a build first, or needs a review. It is a table, read top to bottom, and an
//! outer check that is required is never skipped by an auto-apply.

use super::{
    BlueprintId, Edit, FileRole, Freshness, NonEmpty, Nonconformance, ReadRange, RelPath, RiskMark,
    Rule, RuleId, Scope, SpliceError, Verdict,
};
use std::collections::BTreeSet;

/// Why a build or test must run before a candidate is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OuterCheckReason {
    /// Borrow checking is not run by the analyzer.
    Borrow,
    /// A build script changed.
    BuildScript,
    /// A proc-macro crate changed.
    ProcMacroCrate,
    /// An item moved: privacy and cfg-gated paths across crates are not judged by the analyzer.
    Visibility,
}

impl std::fmt::Display for OuterCheckReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            OuterCheckReason::Borrow => "borrow",
            OuterCheckReason::BuildScript => "build-script",
            OuterCheckReason::ProcMacroCrate => "proc-macro-crate",
            OuterCheckReason::Visibility => "visibility",
        })
    }
}

/// The checks outside the analyzer that a candidate still needs. Never empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OuterCheck(BTreeSet<OuterCheckReason>);

impl OuterCheck {
    /// Why the check is needed.
    pub fn reasons(&self) -> &BTreeSet<OuterCheckReason> {
        &self.0
    }
}

impl std::fmt::Display for OuterCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<String> = self.0.iter().map(ToString::to_string).collect();
        write!(f, "outer check ({})", names.join(", "))
    }
}

/// Why a candidate needs a human.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewReason {
    /// No Rule covers the intent.
    NoRule,
    /// The edit is in a proc-macro crate.
    ProcMacroCrate,
    /// The intent can change behaviour.
    Behaviour,
    /// The check ran at a narrower scope than the Rule's review holds for.
    ScopeTooNarrow,
}

impl std::fmt::Display for ReviewReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ReviewReason::NoRule => "no rule covers the intent",
            ReviewReason::ProcMacroCrate => "the edit is in a proc-macro crate",
            ReviewReason::Behaviour => "the intent can change behaviour",
            ReviewReason::ScopeTooNarrow => "the check scope is narrower than the rule's",
        })
    }
}

/// What may happen to a passing candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// A Rule covers it and nothing else is needed.
    AutoApplicable {
        /// The Rule.
        rule: RuleId,
    },
    /// A Rule covers it, once the outer check has passed.
    AfterOuterCheck {
        /// The Rule.
        rule: RuleId,
    },
    /// A human decides.
    NeedsReview {
        /// Why.
        reason: ReviewReason,
    },
}

impl std::fmt::Display for Disposition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Disposition::AutoApplicable { rule } => write!(f, "auto-applicable (rule {rule})"),
            Disposition::AfterOuterCheck { rule } => {
                write!(f, "applicable after the outer check (rule {rule})")
            }
            Disposition::NeedsReview { reason } => write!(f, "needs review: {reason}"),
        }
    }
}

/// The checks outside the analyzer that `marks` call for, or `None` if there are none.
///
/// # Contract
/// - C1: the reasons are those of `Borrow`, `BuildScript`, `ProcMacroCrate` and `Visibility`
///   among `marks`; `None` when there is none of them (`Behaviour` alone asks for a review, not
///   a build).
///   tested: domain::resolution::tests::outer_check_is_never_auto_applicable
///   tested: domain::resolution::tests::visibility_owes_outer_check_not_review
pub fn outer_check(marks: &BTreeSet<RiskMark>) -> Option<OuterCheck> {
    let reasons: BTreeSet<OuterCheckReason> = marks
        .iter()
        .filter_map(|m| match m {
            RiskMark::Borrow => Some(OuterCheckReason::Borrow),
            RiskMark::BuildScript => Some(OuterCheckReason::BuildScript),
            RiskMark::ProcMacroCrate => Some(OuterCheckReason::ProcMacroCrate),
            RiskMark::Visibility => Some(OuterCheckReason::Visibility),
            RiskMark::Behaviour => None,
        })
        .collect();
    if reasons.is_empty() {
        None
    } else {
        Some(OuterCheck(reasons))
    }
}

/// What may happen to a candidate that passed the analyzer.
///
/// # Contract
/// - C1: the first matching row wins: `Behaviour` gives `NeedsReview{Behaviour}`, then
///   `ProcMacroCrate` gives `NeedsReview{ProcMacroCrate}`, then no Rule gives `NoRule`, then
///   a `scope` that does not cover the Rule's `min_scope` gives `ScopeTooNarrow`, then an
///   outer check gives `AfterOuterCheck`, else `AutoApplicable`.
///   tested: domain::resolution::tests::disposition_table_first_row_wins
/// - C2: a candidate whose outer check is `Some` is never `AutoApplicable`.
///   tested: domain::resolution::tests::outer_check_is_never_auto_applicable
pub fn disposition(marks: &BTreeSet<RiskMark>, rule: Option<&Rule>, scope: Scope) -> Disposition {
    let review = |reason| Disposition::NeedsReview { reason };
    if marks.contains(&RiskMark::Behaviour) {
        return review(ReviewReason::Behaviour);
    }
    if marks.contains(&RiskMark::ProcMacroCrate) {
        return review(ReviewReason::ProcMacroCrate);
    }
    let Some(rule) = rule else {
        return review(ReviewReason::NoRule);
    };
    if !scope.covers(rule.min_scope()) {
        return review(ReviewReason::ScopeTooNarrow);
    }
    if outer_check(marks).is_some() {
        Disposition::AfterOuterCheck { rule: rule.id() }
    } else {
        Disposition::AutoApplicable { rule: rule.id() }
    }
}

/// A Blueprint that passed the analyzer, with the edits it would make and what to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    blueprint: BlueprintId,
    edits: Vec<Edit>,
    verdict: Verdict,
    marks: BTreeSet<RiskMark>,
    outer_check: Option<OuterCheck>,
    disposition: Disposition,
}

impl Candidate {
    /// Builds the candidate of a passing verdict.
    ///
    /// # Contract
    /// - C1: returns `Err(verdict)` unless `verdict.passed()`.
    ///   tested: domain::resolution::tests::candidate_refuses_failed_verdict
    /// - C2: `outer_check` and `disposition` are those of [`outer_check`] and [`disposition`]
    ///   for `marks`, `rule` and `scope`.
    ///   tested: domain::resolution::tests::crate_graph_edits_never_pass_silently
    pub fn new(
        blueprint: BlueprintId,
        edits: Vec<Edit>,
        verdict: Verdict,
        marks: BTreeSet<RiskMark>,
        rule: Option<&Rule>,
        scope: Scope,
    ) -> Result<Self, Verdict> {
        if !verdict.passed() {
            return Err(verdict);
        }
        Ok(Self {
            blueprint,
            edits,
            verdict,
            outer_check: outer_check(&marks),
            disposition: disposition(&marks, rule, scope),
            marks,
        })
    }

    /// The Blueprint it came from.
    pub fn blueprint(&self) -> BlueprintId {
        self.blueprint
    }

    /// Whole-file edits it would make. Nothing is written to disk.
    pub fn edits(&self) -> &[Edit] {
        &self.edits
    }

    /// The (passing) verdict.
    pub fn verdict(&self) -> &Verdict {
        &self.verdict
    }

    /// The risks it carries.
    pub fn marks(&self) -> &BTreeSet<RiskMark> {
        &self.marks
    }

    /// The checks still needed outside the analyzer.
    pub fn outer_check(&self) -> Option<&OuterCheck> {
        self.outer_check.as_ref()
    }

    /// What may happen to it.
    pub fn disposition(&self) -> Disposition {
        self.disposition
    }
}

/// Why a Blueprint was not made a candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocking {
    /// A spec is in a file the analyzer cannot judge.
    Unverifiable {
        /// The Blueprint.
        blueprint: BlueprintId,
        /// The file.
        path: RelPath,
        /// Its role.
        role: FileRole,
    },
    /// Something the Blueprint read has changed. Lists every read that is not fresh.
    Stale {
        /// The Blueprint.
        blueprint: BlueprintId,
        /// The reads and their state.
        reads: Vec<(ReadRange, Freshness)>,
    },
    /// A file the Blueprint reads or edits is not in the workspace.
    MissingFile {
        /// The Blueprint.
        blueprint: BlueprintId,
        /// The file.
        path: RelPath,
    },
    /// The mutations are not what the intent says.
    Nonconforming {
        /// The Blueprint.
        blueprint: BlueprintId,
        /// What differs.
        why: Nonconformance,
    },
    /// The mutations could not be applied to the text.
    Splice {
        /// The Blueprint.
        blueprint: BlueprintId,
        /// What failed.
        err: SpliceError,
    },
    /// The edits introduced errors.
    NewErrors {
        /// The Blueprint.
        blueprint: BlueprintId,
        /// The errors.
        verdict: Verdict,
    },
    /// The check was cut off at the per-candidate limit.
    BudgetExhausted {
        /// The Blueprint.
        blueprint: BlueprintId,
        /// The limit, in milliseconds.
        limit_ms: u64,
    },
}

impl Blocking {
    /// The Blueprint that was blocked.
    ///
    /// # Contract
    /// - C1: every variant, `BudgetExhausted` included, names the Blueprint it was made for.
    ///   tested: domain::resolution::tests::budget_exhausted_names_its_blueprint
    pub fn blueprint(&self) -> BlueprintId {
        match self {
            Blocking::Unverifiable { blueprint, .. }
            | Blocking::Stale { blueprint, .. }
            | Blocking::MissingFile { blueprint, .. }
            | Blocking::Nonconforming { blueprint, .. }
            | Blocking::Splice { blueprint, .. }
            | Blocking::NewErrors { blueprint, .. }
            | Blocking::BudgetExhausted { blueprint, .. } => *blueprint,
        }
    }
}

impl std::fmt::Display for Blocking {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Blocking::Unverifiable {
                blueprint,
                path,
                role,
            } => write!(f, "{blueprint}: {path} ({role}) cannot be verified"),
            Blocking::Stale { blueprint, reads } => {
                write!(f, "{blueprint}: {} read(s) are stale", reads.len())
            }
            Blocking::MissingFile { blueprint, path } => {
                write!(f, "{blueprint}: {path} is not in the workspace")
            }
            Blocking::Nonconforming { blueprint, why } => {
                write!(f, "{blueprint}: does not match its intent: {why}")
            }
            Blocking::Splice { blueprint, err } => {
                write!(f, "{blueprint}: cannot apply the mutations: {err}")
            }
            Blocking::NewErrors { blueprint, verdict } => write!(
                f,
                "{blueprint}: {} new error(s), {} unmapped",
                verdict.new_errors.errors.len(),
                verdict.new_errors.unmapped.len()
            ),
            Blocking::BudgetExhausted {
                blueprint,
                limit_ms,
            } => write!(f, "{blueprint}: check cut off after {limit_ms} ms"),
        }
    }
}

/// At most this many new errors makes a failed Blueprint a near miss.
pub const NEAR_MISS_MAX: usize = 2;
/// At most this many outer checks are suggested at once.
pub const OUTER_CHECK_MAX: usize = 3;

/// A Blueprint that failed by only a few new errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NearMiss {
    /// The Blueprint.
    pub blueprint: BlueprintId,
    /// New errors plus unmapped ones.
    pub new_errors: usize,
}

/// The task and the code disagree about what was asked. No value exists yet: this is the
/// reserved outcome of a spec check that is not part of this step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecMismatch {}

impl std::fmt::Display for SpecMismatch {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {}
    }
}

/// No Blueprint could be made a candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Infeasible {
    blocking: Vec<Blocking>,
    near_miss: Vec<NearMiss>,
    budget_exhausted: bool,
}

impl Infeasible {
    /// Builds the failure.
    ///
    /// # Contract
    /// - C1: returns `None` when `blocking` is empty and the budget is not exhausted: a
    ///   failure always has a reason.
    ///   tested: domain::resolution::tests::infeasible_needs_a_reason
    pub fn new(
        blocking: Vec<Blocking>,
        near_miss: Vec<NearMiss>,
        budget_exhausted: bool,
    ) -> Option<Self> {
        if blocking.is_empty() && !budget_exhausted {
            return None;
        }
        Some(Self {
            blocking,
            near_miss,
            budget_exhausted,
        })
    }

    /// A failure with no Blueprint to blame: nothing was explored.
    pub(crate) fn exhausted() -> Self {
        Self {
            blocking: vec![],
            near_miss: vec![],
            budget_exhausted: true,
        }
    }

    /// Why each Blueprint was blocked, in Blueprint order.
    pub fn blocking(&self) -> &[Blocking] {
        &self.blocking
    }

    /// Blueprints that failed by a few errors.
    pub fn near_miss(&self) -> &[NearMiss] {
        &self.near_miss
    }

    /// Whether the search stopped on its budget.
    pub fn budget_exhausted(&self) -> bool {
        self.budget_exhausted
    }
}

impl std::fmt::Display for Infeasible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "infeasible: {} blocked, {} near miss{}",
            self.blocking.len(),
            self.near_miss.len(),
            if self.budget_exhausted {
                ", budget exhausted"
            } else {
                ""
            }
        )
    }
}

/// A stage of the decision on one Blueprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Stage {
    /// File roles of the edited paths.
    FileRole,
    /// Freshness of the reads.
    Freshness,
    /// The mutations against the intent.
    Conformance,
    /// The mutations spliced into the text.
    Splice,
    /// Risk marks.
    Marks,
    /// The search drops candidates before they are checked.
    Prune,
    /// The analyzer check.
    Check,
    /// The candidate.
    Candidate,
}

impl std::fmt::Display for Stage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Stage::FileRole => "file-role",
            Stage::Freshness => "freshness",
            Stage::Conformance => "conformance",
            Stage::Splice => "splice",
            Stage::Marks => "marks",
            Stage::Prune => "prune",
            Stage::Check => "check",
            Stage::Candidate => "candidate",
        })
    }
}

/// One step of the decision on one Blueprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// The Blueprint.
    pub blueprint: BlueprintId,
    /// The stage.
    pub stage: Stage,
    /// Whether it passed. A `false` is the last entry of its Blueprint.
    pub ok: bool,
    /// What happened, in a few words.
    pub note: String,
}

/// The steps taken, in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecisionLog(Vec<LogEntry>);

impl DecisionLog {
    /// Appends an entry.
    pub fn push(&mut self, entry: LogEntry) {
        self.0.push(entry);
    }

    /// The entries in order.
    pub fn entries(&self) -> &[LogEntry] {
        &self.0
    }
}

/// How much search a decision may spend. Recorded, not enforced, in this step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecisionBudget {
    /// Candidates tried per depth.
    pub candidates_per_depth: u32,
    /// Depth.
    pub max_depth: u32,
    /// Milliseconds per candidate check.
    pub per_candidate_ms: u64,
}

/// The budget used when none is given.
pub const DEFAULT_BUDGET: DecisionBudget = DecisionBudget {
    candidates_per_depth: 20,
    max_depth: 3,
    per_candidate_ms: 6_000,
};

/// What the decision rested on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    /// The scope the checks ran at.
    pub scope: Scope,
    /// How many reads were compared with the current text.
    pub fingerprints_checked: usize,
    /// How many baseline findings there were.
    pub baseline_size: usize,
    /// The verdict of every check that ran.
    pub verdicts: Vec<(BlueprintId, Verdict)>,
    /// The budget.
    pub budget: DecisionBudget,
}

/// The outcome of resolving a set of Blueprints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// At least one Blueprint passed, in input order.
    Decided {
        /// The candidates.
        blueprints: NonEmpty<Candidate>,
        /// How each Blueprint was decided.
        decision_log: DecisionLog,
        /// What it rested on.
        evidence: Evidence,
    },
    /// The task and the code disagree.
    Stopped(SpecMismatch),
    /// No Blueprint passed.
    Failed(Infeasible),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Diagnostic, Diagnostics, ErrorCode, IntentKind, ReviewRecord};

    const ALL_MARKS: [RiskMark; 5] = [
        RiskMark::Borrow,
        RiskMark::BuildScript,
        RiskMark::ProcMacroCrate,
        RiskMark::Behaviour,
        RiskMark::Visibility,
    ];

    fn marks(mask: u8) -> BTreeSet<RiskMark> {
        ALL_MARKS
            .iter()
            .enumerate()
            .filter(|(i, _)| mask & (1 << i) != 0)
            .map(|(_, m)| *m)
            .collect()
    }
    fn bid() -> BlueprintId {
        BlueprintId::parse("01890a5d-ac96-774b-bcce-b302099a8057").unwrap()
    }
    fn rule() -> Rule {
        Rule::register(
            RuleId::parse("01890a5d-ac96-774b-bcce-b302099a8058").unwrap(),
            IntentKind::MoveItem,
            ReviewRecord {
                reviewer: "me".to_owned(),
                reviewed_at_ms: 1,
                note: String::new(),
            },
            Scope::Workspace,
        )
        .unwrap()
    }
    fn failing() -> Verdict {
        Verdict::from_diagnostics(Diagnostics {
            errors: vec![Diagnostic {
                code: ErrorCode::parse("E0599").unwrap(),
                path: RelPath::parse("a.rs").unwrap(),
                line: 1,
            }],
            unmapped: vec![],
        })
    }
    fn passing() -> Verdict {
        Verdict::from_diagnostics(Diagnostics::default())
    }

    #[test]
    fn outer_check_is_never_auto_applicable() {
        let rule = rule();
        for mask in 0..32u8 {
            let m = marks(mask);
            for r in [None, Some(&rule)] {
                for scope in [Scope::Affected, Scope::Workspace] {
                    let d = disposition(&m, r, scope);
                    let oc = outer_check(&m);
                    if oc.is_some() {
                        assert!(
                            !matches!(d, Disposition::AutoApplicable { .. }),
                            "{m:?} {r:?} {scope}"
                        );
                    }
                    if matches!(d, Disposition::AutoApplicable { .. }) {
                        assert!(oc.is_none() && r.is_some() && !m.contains(&RiskMark::Behaviour));
                    }
                    let c = Candidate::new(bid(), vec![], passing(), m.clone(), r, scope).unwrap();
                    assert_eq!(c.disposition(), d);
                    assert_eq!(c.outer_check(), oc.as_ref());
                }
            }
        }
        assert!(outer_check(&marks(0)).is_none());
        assert!(outer_check(&BTreeSet::from([RiskMark::Behaviour])).is_none());
        let both = outer_check(&BTreeSet::from([RiskMark::Borrow, RiskMark::BuildScript])).unwrap();
        assert_eq!(
            both.reasons(),
            &BTreeSet::from([OuterCheckReason::Borrow, OuterCheckReason::BuildScript])
        );
    }

    #[test]
    fn visibility_owes_outer_check_not_review() {
        let rule = rule();
        let vis = BTreeSet::from([RiskMark::Visibility]);
        assert_eq!(
            outer_check(&vis).unwrap().reasons(),
            &BTreeSet::from([OuterCheckReason::Visibility])
        );
        assert_eq!(OuterCheckReason::Visibility.to_string(), "visibility");
        assert_eq!(
            disposition(&vis, Some(&rule), Scope::Workspace),
            Disposition::AfterOuterCheck { rule: rule.id() }
        );
        assert_eq!(
            disposition(&vis, None, Scope::Workspace),
            Disposition::NeedsReview {
                reason: ReviewReason::NoRule
            }
        );
        assert_eq!(Stage::Prune.to_string(), "prune");
        assert!(Stage::Marks < Stage::Prune && Stage::Prune < Stage::Check);
    }

    #[test]
    fn budget_exhausted_names_its_blueprint() {
        let b = Blocking::BudgetExhausted {
            blueprint: bid(),
            limit_ms: 6_000,
        };
        assert_eq!(b.blueprint(), bid());
        assert_eq!(
            b.to_string(),
            format!("{}: check cut off after 6000 ms", bid())
        );
        let i = Infeasible::new(vec![b.clone()], vec![], false).unwrap();
        assert_eq!(i.blocking(), std::slice::from_ref(&b));
    }

    #[test]
    fn disposition_table_first_row_wins() {
        let rule = rule();
        let id = rule.id();
        let m = |ms: &[RiskMark]| ms.iter().copied().collect::<BTreeSet<_>>();
        let review = |reason| Disposition::NeedsReview { reason };
        // Behaviour beats everything
        assert_eq!(
            disposition(
                &m(&[RiskMark::Behaviour, RiskMark::ProcMacroCrate]),
                Some(&rule),
                Scope::Workspace
            ),
            review(ReviewReason::Behaviour)
        );
        assert_eq!(
            disposition(&m(&[RiskMark::Behaviour]), None, Scope::Affected),
            review(ReviewReason::Behaviour)
        );
        // ProcMacroCrate beats NoRule and an outer check
        assert_eq!(
            disposition(
                &m(&[RiskMark::ProcMacroCrate, RiskMark::Borrow]),
                None,
                Scope::Workspace
            ),
            review(ReviewReason::ProcMacroCrate)
        );
        // NoRule beats the outer check
        assert_eq!(
            disposition(&m(&[RiskMark::Borrow]), None, Scope::Workspace),
            review(ReviewReason::NoRule)
        );
        // ScopeTooNarrow beats the outer check
        assert_eq!(
            disposition(&m(&[RiskMark::Borrow]), Some(&rule), Scope::Affected),
            review(ReviewReason::ScopeTooNarrow)
        );
        assert_eq!(
            disposition(&m(&[]), Some(&rule), Scope::Affected),
            review(ReviewReason::ScopeTooNarrow)
        );
        assert_eq!(
            disposition(&m(&[RiskMark::BuildScript]), Some(&rule), Scope::Workspace),
            Disposition::AfterOuterCheck { rule: id }
        );
        assert_eq!(
            disposition(&m(&[]), Some(&rule), Scope::Workspace),
            Disposition::AutoApplicable { rule: id }
        );
    }

    #[test]
    fn crate_graph_edits_never_pass_silently() {
        // The pure half; `app::apply::tests` drives the same cases through `apply_blueprints`.
        let rule = rule();
        for mark in [RiskMark::BuildScript, RiskMark::ProcMacroCrate] {
            let m = BTreeSet::from([mark]);
            let c =
                Candidate::new(bid(), vec![], passing(), m, Some(&rule), Scope::Workspace).unwrap();
            assert!(c.outer_check().is_some(), "{mark}");
            assert!(!matches!(
                c.disposition(),
                Disposition::AutoApplicable { .. }
            ));
        }
    }

    #[test]
    fn candidate_refuses_failed_verdict() {
        let v = failing();
        let err = Candidate::new(
            bid(),
            vec![],
            v.clone(),
            BTreeSet::new(),
            None,
            Scope::Affected,
        )
        .unwrap_err();
        assert_eq!(err, v);
        let ok = Candidate::new(
            bid(),
            vec![Edit::delete(RelPath::parse("a.rs").unwrap())],
            passing(),
            BTreeSet::new(),
            None,
            Scope::Affected,
        )
        .unwrap();
        assert_eq!(ok.blueprint(), bid());
        assert_eq!(ok.edits().len(), 1);
        assert!(ok.verdict().passed());
        assert!(ok.marks().is_empty());
    }

    #[test]
    fn infeasible_needs_a_reason() {
        assert!(Infeasible::new(vec![], vec![], false).is_none());
        let exhausted = Infeasible::new(vec![], vec![], true).unwrap();
        assert!(exhausted.budget_exhausted());
        let b = Blocking::MissingFile {
            blueprint: bid(),
            path: RelPath::parse("a.rs").unwrap(),
        };
        let i = Infeasible::new(vec![b.clone()], vec![], false).unwrap();
        assert_eq!(i.blocking(), std::slice::from_ref(&b));
        assert!(i.near_miss().is_empty());
        assert_eq!(b.blueprint(), bid());
        assert!(!b.to_string().is_empty());
    }
}
