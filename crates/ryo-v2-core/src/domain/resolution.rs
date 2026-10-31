//! What came of a set of Blueprints: candidates that passed, or why none did.
//!
//! The central rule is [`disposition`]: whether a passing candidate may be applied without a
//! human, needs a build first, or needs a review. It is a table, read top to bottom, and an
//! outer check that is required is never skipped by an auto-apply.
//!
//! The marks and the Rule that feed the table are derived here, from the Blueprints of a path
//! and the roles of the files it edits ([`risk_marks`], [`covering_rule`]); a caller never
//! hands them over.

use super::stage::Staged;
use super::{
    Basis, Blueprint, BlueprintId, DomainError, Drift, Edit, FileRole, Fingerprint, IntentKind,
    NonEmpty, Nonconformance, RelPath, RiskMark, Rule, RuleCatalog, RuleId, Scope, SpliceError,
    Verdict,
};
use std::collections::{BTreeMap, BTreeSet};

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

/// The marks of a change made by `kinds` to files of `roles`.
///
/// # Contract
/// - C1: the marks are the union of the catalog risks of `kinds` and, for the roles, `BuildScript`
///   for `FileRole::BuildScript` and `ProcMacroCrate` for `FileRole::ProcMacroSource`; `Source`
///   and `Manifest` add nothing.
///   tested: domain::resolution::tests::risk_marks_are_intent_risks_plus_roles
pub fn risk_marks(
    kinds: impl IntoIterator<Item = IntentKind>,
    roles: impl IntoIterator<Item = FileRole>,
) -> BTreeSet<RiskMark> {
    let mut marks = BTreeSet::new();
    for kind in kinds {
        marks.extend(kind.spec().risks);
    }
    for role in roles {
        match role {
            FileRole::BuildScript => {
                marks.insert(RiskMark::BuildScript);
            }
            FileRole::ProcMacroSource => {
                marks.insert(RiskMark::ProcMacroCrate);
            }
            FileRole::Source | FileRole::Manifest => {}
        }
    }
    marks
}

/// The Rule that covers every intent of `kinds` (root first, leaf last), or `None`.
///
/// # Contract
/// - C1: `None` for no kinds and when any kind has no Rule in `rules` (the catalog holds each
///   Rule under its own intent, `RuleCatalog` C2); otherwise the Rule with the widest
///   `min_scope`, the later one (the leaf's) on a tie.
///   tested: domain::resolution::tests::covering_rule_needs_every_kind
pub fn covering_rule<'r>(kinds: &[IntentKind], rules: &'r RuleCatalog) -> Option<&'r Rule> {
    let mut best: Option<&Rule> = None;
    for kind in kinds {
        let rule = rules.for_intent(*kind)?;
        best = match best {
            Some(b) if !rule.min_scope().covers(b.min_scope()) => Some(b),
            _ => Some(rule),
        };
    }
    best
}

/// A Blueprint, or a path of them, that passed the analyzer, with the edits it would make and
/// what to do next.
///
/// ```compile_fail
/// use ryo_v2_core::domain::Candidate;
/// // Only the engine builds a candidate: its marks and Rule are derived, never handed over.
/// let _ = Candidate::new;
/// ```
///
/// # Contract
/// - C1 [invariant]: its `edits` are the texts of the state its basis was taken on; they may be
///   written only when the basis has no drift against the files, and nothing splices them onto
///   a changed text: a drifted candidate is decided again.
///   tested: app::apply::tests::candidate_basis_is_stale_after_outside_change
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    path: NonEmpty<Blueprint>,
    edits: Vec<Edit>,
    verdict: Verdict,
    basis: Basis,
    marks: BTreeSet<RiskMark>,
    outer_check: Option<OuterCheck>,
    disposition: Disposition,
}

impl Candidate {
    /// Builds the candidate of a passing verdict.
    ///
    /// `staged` holds the Blueprints that lead to it, the root first and the leaf last (one for
    /// an apply), the edits they make, the roles of every file they edit and their basis.
    ///
    /// # Contract
    /// - C1: returns `Err(verdict)` unless `verdict.passed()`.
    ///   tested: domain::resolution::tests::candidate_refuses_failed_verdict
    /// - C2: the marks are [`risk_marks`] of the intent kinds of the path of `staged` and its
    ///   roles, the Rule is [`covering_rule`] of those kinds, and `outer_check` and
    ///   `disposition` follow from them with [`outer_check`] and [`disposition`]; a path with an
    ///   intent whose catalog risks hold `Behaviour` is `NeedsReview{Behaviour}` whatever the
    ///   catalog holds.
    ///   tested: domain::resolution::tests::candidate_derives_marks_and_rule
    ///   tested: domain::resolution::tests::crate_graph_edits_never_pass_silently
    pub(crate) fn new(
        staged: Staged,
        verdict: Verdict,
        rules: &RuleCatalog,
        scope: Scope,
    ) -> Result<Self, Verdict> {
        if !verdict.passed() {
            return Err(verdict);
        }
        let (path, edits, roles, basis) = staged.into_parts();
        let kinds: Vec<IntentKind> = path.iter().map(|b| b.intent().kind()).collect();
        let marks = risk_marks(kinds.iter().copied(), roles.iter().copied());
        let rule = covering_rule(&kinds, rules);
        Ok(Self {
            path,
            edits,
            verdict,
            basis,
            outer_check: outer_check(&marks),
            disposition: disposition(&marks, rule, scope),
            marks,
        })
    }

    /// The Blueprint it came from: the last of its path.
    pub fn blueprint(&self) -> BlueprintId {
        self.path.last().id()
    }

    /// The Blueprints that lead to it, the root first and the leaf last.
    pub fn path(&self) -> &[Blueprint] {
        self.path.as_slice()
    }

    /// Whole-file edits it would make. Nothing is written to disk.
    pub fn edits(&self) -> &[Edit] {
        &self.edits
    }

    /// The (passing) verdict.
    pub fn verdict(&self) -> &Verdict {
        &self.verdict
    }

    /// What its verdict rests on; check it against the files before writing `edits`.
    ///
    /// The `edits` are the texts of the checked state. They may be written only when
    /// `basis().check` of the files is empty, and nothing splices them onto a changed text: a
    /// drifted candidate is decided again.
    pub fn basis(&self) -> &Basis {
        &self.basis
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
    /// Something the Blueprint rests on has changed. Lists every drift.
    Stale {
        /// The Blueprint.
        blueprint: BlueprintId,
        /// Every way the files no longer match its basis.
        drift: Vec<Drift>,
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
            Blocking::Stale { blueprint, drift } => {
                let files = drift
                    .iter()
                    .filter(|d| matches!(d, Drift::File { .. }))
                    .count();
                let reads = drift
                    .iter()
                    .filter(|d| matches!(d, Drift::Read { .. }))
                    .count();
                write!(
                    f,
                    "{blueprint}: {files} base(s) and {reads} read(s) are stale"
                )
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
    blueprint: BlueprintId,
    new_errors: usize,
}

impl NearMiss {
    /// The near miss of a Blueprint that failed with `new_errors` new errors (unmapped included).
    ///
    /// # Contract
    /// - C1: `Some` if and only if `new_errors` is from 1 to `NEAR_MISS_MAX`.
    ///   tested: domain::resolution::tests::near_miss_is_one_to_max_errors
    pub(crate) fn of(blueprint: BlueprintId, new_errors: usize) -> Option<Self> {
        (1..=NEAR_MISS_MAX).contains(&new_errors).then_some(Self {
            blueprint,
            new_errors,
        })
    }

    /// The Blueprint.
    pub fn blueprint(&self) -> BlueprintId {
        self.blueprint
    }

    /// New errors plus unmapped ones.
    pub fn new_errors(&self) -> usize {
        self.new_errors
    }
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
    blueprint: BlueprintId,
    stage: Stage,
    ok: bool,
    note: String,
}

impl LogEntry {
    /// An entry of the decision on `blueprint`.
    pub(crate) fn new(
        blueprint: BlueprintId,
        stage: Stage,
        ok: bool,
        note: impl Into<String>,
    ) -> Self {
        Self {
            blueprint,
            stage,
            ok,
            note: note.into(),
        }
    }

    /// The Blueprint.
    pub fn blueprint(&self) -> BlueprintId {
        self.blueprint
    }

    /// The stage.
    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// Whether it passed. A `false` is the last entry of its Blueprint.
    pub fn ok(&self) -> bool {
        self.ok
    }

    /// What happened, in a few words.
    pub fn note(&self) -> &str {
        &self.note
    }
}

/// The steps taken, in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecisionLog(Vec<LogEntry>);

impl DecisionLog {
    /// Appends an entry.
    pub(crate) fn push(&mut self, entry: LogEntry) {
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

/// What a decision can be reproduced from: the analyzer, its configuration and the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    analyzer: String,
    version: String,
    config: BTreeMap<String, String>,
    files: BTreeMap<RelPath, Fingerprint>,
}

impl Provenance {
    /// Builds the provenance of a decision. The domain never reads these values: the verifier
    /// supplies them.
    ///
    /// # Contract
    /// - C1: refuses an analyzer name or version that is empty after trimming, and a config key
    ///   that is empty after trimming, with [`DomainError::Empty`].
    ///   tested: domain::resolution::tests::provenance_refuses_empty
    /// - C2: [`Provenance::workspace`] is the [`workspace_fingerprint`] of [`Provenance::files`].
    ///   tested: domain::resolution::tests::provenance_workspace_is_of_its_files
    pub fn new(
        analyzer: &str,
        version: &str,
        config: BTreeMap<String, String>,
        files: BTreeMap<RelPath, Fingerprint>,
    ) -> Result<Self, DomainError> {
        if analyzer.trim().is_empty() {
            return Err(DomainError::Empty("analyzer".to_owned()));
        }
        if version.trim().is_empty() {
            return Err(DomainError::Empty("analyzer version".to_owned()));
        }
        if config.keys().any(|k| k.trim().is_empty()) {
            return Err(DomainError::Empty("config key".to_owned()));
        }
        Ok(Self {
            analyzer: analyzer.to_owned(),
            version: version.to_owned(),
            config,
            files,
        })
    }

    /// The provenance of an analyzer that recorded nothing: named `unknown`, with no
    /// configuration and no files. For an adapter whose own values fail [`Provenance::new`],
    /// which a constant name and version never do.
    pub fn unrecorded() -> Self {
        Self {
            analyzer: "unknown".to_owned(),
            version: "unknown".to_owned(),
            config: BTreeMap::new(),
            files: BTreeMap::new(),
        }
    }

    /// The analyzer's name.
    pub fn analyzer(&self) -> &str {
        &self.analyzer
    }

    /// The analyzer's version.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The analyzer's configuration.
    pub fn config(&self) -> &BTreeMap<String, String> {
        &self.config
    }

    /// The fingerprint of each file of the workspace the analyzer saw.
    pub fn files(&self) -> &BTreeMap<RelPath, Fingerprint> {
        &self.files
    }

    /// The fingerprint of the workspace the analyzer saw.
    pub fn workspace(&self) -> Fingerprint {
        workspace_fingerprint(self.files.iter().map(|(p, f)| (p, *f)))
    }
}

/// The fingerprint of a workspace: the fingerprints of its files, by path.
///
/// # Contract
/// - C1: independent of the order of `files`; it changes when a path or a file's fingerprint
///   changes and when a file is added or removed. It is the [`Fingerprint`] of, for each file in
///   path order, the length of the path (8 bytes, little endian), the path, and the text form of
///   the file's fingerprint.
///   tested: domain::resolution::tests::workspace_fingerprint_follows_files
pub fn workspace_fingerprint<'a>(
    files: impl IntoIterator<Item = (&'a RelPath, Fingerprint)>,
) -> Fingerprint {
    let sorted: BTreeMap<&RelPath, Fingerprint> = files.into_iter().collect();
    let mut bytes = Vec::new();
    for (path, fp) in sorted {
        bytes.extend_from_slice(&(path.as_str().len() as u64).to_le_bytes());
        bytes.extend_from_slice(path.as_str().as_bytes());
        bytes.extend_from_slice(fp.to_string().as_bytes());
    }
    Fingerprint::of(&bytes)
}

/// What the decision rested on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    scope: Scope,
    fingerprints_checked: usize,
    baseline_size: usize,
    verdicts: Vec<(BlueprintId, Verdict)>,
    budget: DecisionBudget,
    provenance: Provenance,
}

impl Evidence {
    /// The evidence of a decision.
    pub(crate) fn new(
        scope: Scope,
        fingerprints_checked: usize,
        baseline_size: usize,
        verdicts: Vec<(BlueprintId, Verdict)>,
        budget: DecisionBudget,
        provenance: Provenance,
    ) -> Self {
        Self {
            scope,
            fingerprints_checked,
            baseline_size,
            verdicts,
            budget,
            provenance,
        }
    }

    /// The scope the checks ran at.
    pub fn scope(&self) -> Scope {
        self.scope
    }

    /// How many base fingerprints and reads of files without a base were compared with the
    /// current text. It used to count reads only; the count changed meaning when the base
    /// fingerprint was introduced.
    pub fn fingerprints_checked(&self) -> usize {
        self.fingerprints_checked
    }

    /// How many baseline findings there were.
    pub fn baseline_size(&self) -> usize {
        self.baseline_size
    }

    /// The verdict of every check that ran.
    pub fn verdicts(&self) -> &[(BlueprintId, Verdict)] {
        &self.verdicts
    }

    /// The budget.
    pub fn budget(&self) -> DecisionBudget {
        self.budget
    }

    /// What the decision can be reproduced from.
    pub fn provenance(&self) -> &Provenance {
        &self.provenance
    }
}

/// The outcome of resolving a set of Blueprints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// At least one Blueprint passed, in input order.
    ///
    /// # Contract
    /// - C1: the candidates are alternatives on the loaded state, not parts of one change; each
    ///   one's basis is what to check before writing it, and writing one makes every other
    ///   candidate on a shared file drift.
    ///   tested: app::apply::tests::second_alternative_is_stale_after_first_is_written
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
    use crate::domain::DeriveName;
    use crate::domain::{
        BaseFp, BlueprintParts, DecisionRecord, Diagnostic, Diagnostics, ErrorCode, Ident,
        IntentCall, MutationSpec, ReadRange, ReviewRecord, TextRange,
    };

    fn empty_basis() -> Basis {
        Basis::new(vec![], vec![])
    }

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
    fn review() -> ReviewRecord {
        ReviewRecord {
            reviewer: "me".to_owned(),
            reviewed_at_ms: 1,
            note: String::new(),
        }
    }
    fn rid(n: u8) -> RuleId {
        RuleId::parse(&format!("01890a5d-ac96-774b-bcce-b302099a80{n:02x}")).unwrap()
    }
    fn rule() -> Rule {
        Rule::register(rid(0x58), IntentKind::MoveItem, review(), Scope::Workspace).unwrap()
    }
    fn call(kind: IntentKind) -> IntentCall {
        match kind {
            IntentKind::RenameLocal => IntentCall::RenameLocal {
                from: Ident::parse("a").unwrap(),
                to: Ident::parse("b").unwrap(),
            },
            IntentKind::ExtractVariable => IntentCall::ExtractVariable {
                name: Ident::parse("t").unwrap(),
            },
            IntentKind::AddDerive => IntentCall::AddDerive {
                derives: NonEmpty::one(DeriveName::parse("Clone").unwrap()),
            },
            IntentKind::ExtractFunction => IntentCall::ExtractFunction {
                name: Ident::parse("h").unwrap(),
            },
            IntentKind::MoveItem => IntentCall::MoveItem {
                item: Ident::parse("i").unwrap(),
                to: RelPath::parse("b.rs").unwrap(),
            },
        }
    }
    /// A Blueprint of `kind`; the intent is not checked against its specs when it is built.
    fn blueprint(n: u8, kind: IntentKind) -> Blueprint {
        let path = RelPath::parse("a.rs").unwrap();
        let range = TextRange::new(0, 1).unwrap();
        Blueprint::new(BlueprintParts {
            id: BlueprintId::parse(&format!("01890a5d-ac96-774b-bcce-b302099a80{n:02x}")).unwrap(),
            intent: call(kind),
            specs: vec![MutationSpec::replace(path.clone(), range, "y").unwrap()],
            reads: vec![ReadRange::capture(path.clone(), "x", range).unwrap()],
            bases: vec![BaseFp::capture(path, "x")],
            spec_refs: vec![],
            record: DecisionRecord::new("agent", "why").unwrap(),
        })
        .unwrap()
    }
    fn path_of(kinds: &[IntentKind]) -> NonEmpty<Blueprint> {
        NonEmpty::new(
            kinds
                .iter()
                .zip(1u8..)
                .map(|(k, n)| blueprint(n, *k))
                .collect(),
        )
        .unwrap()
    }
    fn staged(kinds: &[IntentKind], roles: BTreeSet<FileRole>, edits: Vec<Edit>) -> Staged {
        Staged::for_tests(path_of(kinds), roles, edits, empty_basis())
    }
    const ALL_ROLES: [FileRole; 4] = [
        FileRole::Source,
        FileRole::BuildScript,
        FileRole::ProcMacroSource,
        FileRole::Manifest,
    ];
    fn role_subsets() -> Vec<BTreeSet<FileRole>> {
        (0..16u8)
            .map(|mask| {
                ALL_ROLES
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| mask & (1 << i) != 0)
                    .map(|(_, r)| *r)
                    .collect()
            })
            .collect()
    }
    /// A catalog holding a Rule of `scope` for each of `kinds`, registration checks aside.
    fn unchecked(kinds: &[IntentKind], scope: Scope) -> RuleCatalog {
        RuleCatalog::new(
            kinds
                .iter()
                .zip(0x60u8..)
                .map(|(k, n)| Rule::unchecked_for_tests(rid(n), *k, review(), scope))
                .collect(),
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
        let rules = RuleCatalog::new(vec![rule()]).unwrap();
        for role in [FileRole::BuildScript, FileRole::ProcMacroSource] {
            let c = Candidate::new(
                staged(&[IntentKind::MoveItem], BTreeSet::from([role]), vec![]),
                passing(),
                &rules,
                Scope::Workspace,
            )
            .unwrap();
            assert!(c.outer_check().is_some(), "{role}");
            assert!(!matches!(
                c.disposition(),
                Disposition::AutoApplicable { .. }
            ));
        }
    }

    #[test]
    fn near_miss_is_one_to_max_errors() {
        assert_eq!(NearMiss::of(bid(), 0), None);
        let one = NearMiss::of(bid(), 1).unwrap();
        assert_eq!((one.blueprint(), one.new_errors()), (bid(), 1));
        assert!(NearMiss::of(bid(), NEAR_MISS_MAX).is_some());
        assert_eq!(NearMiss::of(bid(), NEAR_MISS_MAX + 1), None);
    }

    #[test]
    fn candidate_refuses_failed_verdict() {
        let v = failing();
        let none = RuleCatalog::default();
        let err = Candidate::new(
            staged(&[IntentKind::MoveItem], BTreeSet::new(), vec![]),
            v.clone(),
            &none,
            Scope::Affected,
        )
        .unwrap_err();
        assert_eq!(err, v);
        let ok = Candidate::new(
            staged(
                &[IntentKind::RenameLocal, IntentKind::MoveItem],
                BTreeSet::new(),
                vec![Edit::delete(RelPath::parse("a.rs").unwrap())],
            ),
            passing(),
            &none,
            Scope::Affected,
        )
        .unwrap();
        assert_eq!(ok.blueprint(), ok.path()[1].id());
        assert_eq!(ok.path().len(), 2);
        assert_eq!(ok.edits().len(), 1);
        assert!(ok.verdict().passed());
        assert_eq!(ok.marks(), &BTreeSet::from([RiskMark::Visibility]));
    }

    #[test]
    fn candidate_derives_marks_and_rule() {
        let registrable: Vec<IntentKind> = IntentKind::ALL
            .into_iter()
            .filter(|k| Rule::register(rid(1), *k, review(), Scope::Affected).is_ok())
            .collect();
        let catalogs = [
            RuleCatalog::default(),
            unchecked(&registrable, Scope::Workspace),
            unchecked(&IntentKind::ALL, Scope::Workspace),
            unchecked(&IntentKind::ALL, Scope::Affected),
        ];
        let mut paths: Vec<Vec<IntentKind>> = IntentKind::ALL.iter().map(|k| vec![*k]).collect();
        for a in IntentKind::ALL {
            for b in IntentKind::ALL {
                paths.push(vec![a, b]);
            }
        }
        let mut seen_auto = false;
        for kinds in &paths {
            for roles in role_subsets() {
                for rules in &catalogs {
                    for scope in [Scope::Affected, Scope::Workspace] {
                        let c = Candidate::new(
                            staged(kinds, roles.clone(), vec![]),
                            passing(),
                            rules,
                            scope,
                        )
                        .unwrap();
                        let marks = risk_marks(kinds.iter().copied(), roles.iter().copied());
                        let rule = covering_rule(kinds, rules);
                        assert_eq!(c.marks(), &marks);
                        assert_eq!(c.outer_check(), outer_check(&marks).as_ref());
                        assert_eq!(c.disposition(), disposition(&marks, rule, scope));
                        let kinds_of: Vec<_> = c.path().iter().map(|b| b.intent().kind()).collect();
                        assert_eq!(&kinds_of, kinds);
                        if kinds
                            .iter()
                            .any(|k| k.spec().risks.contains(&RiskMark::Behaviour))
                        {
                            assert_eq!(
                                c.disposition(),
                                Disposition::NeedsReview {
                                    reason: ReviewReason::Behaviour
                                },
                                "{kinds:?} {roles:?}"
                            );
                        }
                        seen_auto |= matches!(c.disposition(), Disposition::AutoApplicable { .. });
                    }
                }
            }
        }
        assert!(seen_auto, "the table has an auto-applicable row to guard");
    }

    #[test]
    fn risk_marks_are_intent_risks_plus_roles() {
        assert!(risk_marks([], []).is_empty());
        for kind in IntentKind::ALL {
            assert_eq!(risk_marks([kind], []), kind.spec().risks, "{kind}");
        }
        for role in ALL_ROLES {
            let expected = match role {
                FileRole::BuildScript => BTreeSet::from([RiskMark::BuildScript]),
                FileRole::ProcMacroSource => BTreeSet::from([RiskMark::ProcMacroCrate]),
                FileRole::Source | FileRole::Manifest => BTreeSet::new(),
            };
            assert_eq!(risk_marks([], [role]), expected, "{role}");
            for kind in IntentKind::ALL {
                let mut all = kind.spec().risks;
                all.extend(expected.iter().copied());
                assert_eq!(risk_marks([kind], [role]), all, "{kind} {role}");
            }
        }
        assert_eq!(
            risk_marks(
                [IntentKind::MoveItem, IntentKind::ExtractVariable],
                [FileRole::Source, FileRole::BuildScript]
            ),
            BTreeSet::from([
                RiskMark::Visibility,
                RiskMark::Borrow,
                RiskMark::Behaviour,
                RiskMark::BuildScript
            ])
        );
    }

    #[test]
    fn covering_rule_needs_every_kind() {
        use IntentKind::{AddDerive, MoveItem, RenameLocal};
        let narrow_and_wide = RuleCatalog::new(vec![
            Rule::unchecked_for_tests(rid(1), RenameLocal, review(), Scope::Affected),
            Rule::unchecked_for_tests(rid(2), MoveItem, review(), Scope::Workspace),
        ])
        .unwrap();
        let ids =
            |kinds: &[IntentKind], rules: &RuleCatalog| covering_rule(kinds, rules).map(Rule::id);
        // no kinds, or a kind without a rule
        assert_eq!(ids(&[], &narrow_and_wide), None);
        assert_eq!(ids(&[AddDerive], &narrow_and_wide), None);
        assert_eq!(ids(&[MoveItem, AddDerive], &narrow_and_wide), None);
        assert_eq!(ids(&[AddDerive, MoveItem], &narrow_and_wide), None);
        assert_eq!(ids(&[MoveItem], &RuleCatalog::default()), None);
        // one rule per kind: the wider min_scope wins, whatever the order
        assert_eq!(ids(&[MoveItem], &narrow_and_wide), Some(rid(2)));
        assert_eq!(
            ids(&[RenameLocal, MoveItem], &narrow_and_wide),
            Some(rid(2))
        );
        assert_eq!(
            ids(&[MoveItem, RenameLocal], &narrow_and_wide),
            Some(rid(2))
        );
        // a tie goes to the leaf, the later one
        let tie = unchecked(&[RenameLocal, MoveItem], Scope::Workspace);
        assert_eq!(
            ids(&[RenameLocal, MoveItem], &tie),
            tie.for_intent(MoveItem).map(Rule::id)
        );
        assert_eq!(
            ids(&[MoveItem, RenameLocal], &tie),
            tie.for_intent(RenameLocal).map(Rule::id)
        );
    }

    fn files_of(list: &[(&str, &str)]) -> BTreeMap<RelPath, Fingerprint> {
        list.iter()
            .map(|(p, t)| (RelPath::parse(p).unwrap(), Fingerprint::of(t.as_bytes())))
            .collect()
    }

    #[test]
    fn provenance_refuses_empty() {
        let files = files_of(&[("a.rs", "x")]);
        let cfg = BTreeMap::from([("k".to_owned(), "v".to_owned())]);
        let p = Provenance::new("ra", "1.0", cfg.clone(), files.clone()).unwrap();
        assert_eq!((p.analyzer(), p.version()), ("ra", "1.0"));
        assert_eq!(p.config(), &cfg);
        assert_eq!(p.files(), &files);
        for (a, v) in [("", "1"), (" \t", "1"), ("ra", ""), ("ra", "  ")] {
            assert!(
                matches!(
                    Provenance::new(a, v, cfg.clone(), files.clone()),
                    Err(DomainError::Empty(_))
                ),
                "{a:?} {v:?}"
            );
        }
        let blank_key = BTreeMap::from([(" ".to_owned(), "v".to_owned())]);
        assert!(matches!(
            Provenance::new("ra", "1", blank_key, files.clone()),
            Err(DomainError::Empty(_))
        ));
        let none = Provenance::unrecorded();
        assert_eq!((none.analyzer(), none.version()), ("unknown", "unknown"));
        assert!(none.config().is_empty() && none.files().is_empty());
        // an empty value is allowed, and so is an empty config
        assert!(Provenance::new("ra", "1", BTreeMap::new(), files.clone()).is_ok());
        let empty_value = BTreeMap::from([("k".to_owned(), String::new())]);
        assert!(Provenance::new("ra", "1", empty_value, files).is_ok());
    }

    #[test]
    fn provenance_workspace_is_of_its_files() {
        let files = files_of(&[("a.rs", "x"), ("b.rs", "y")]);
        let p = Provenance::new("ra", "1", BTreeMap::new(), files.clone()).unwrap();
        assert_eq!(
            p.workspace(),
            workspace_fingerprint(files.iter().map(|(p, f)| (p, *f)))
        );
        let other = Provenance::new("ra", "1", BTreeMap::new(), files_of(&[("a.rs", "x")]));
        assert_ne!(p.workspace(), other.unwrap().workspace());
    }

    #[test]
    fn workspace_fingerprint_follows_files() {
        let p = |s: &str| RelPath::parse(s).unwrap();
        let f = |s: &str| Fingerprint::of(s.as_bytes());
        let (a, b, c) = (p("a.rs"), p("b.rs"), p("c.rs"));
        let fp = |files: &[(&RelPath, Fingerprint)]| workspace_fingerprint(files.iter().copied());
        let base = fp(&[(&a, f("x")), (&b, f("y"))]);
        // order does not matter
        assert_eq!(base, fp(&[(&b, f("y")), (&a, f("x"))]));
        // a file's fingerprint, a path, an added file and a removed file each move it
        assert_ne!(base, fp(&[(&a, f("x")), (&b, f("z"))]));
        assert_ne!(base, fp(&[(&a, f("x")), (&c, f("y"))]));
        assert_ne!(base, fp(&[(&a, f("x")), (&b, f("y")), (&c, f(""))]));
        assert_ne!(base, fp(&[(&a, f("x"))]));
        assert_ne!(fp(&[]), fp(&[(&a, f(""))]));
        // the definition
        let mut bytes = Vec::new();
        for (path, text) in [("a.rs", "x"), ("b.rs", "y")] {
            bytes.extend_from_slice(&(path.len() as u64).to_le_bytes());
            bytes.extend_from_slice(path.as_bytes());
            bytes.extend_from_slice(f(text).to_string().as_bytes());
        }
        assert_eq!(base, Fingerprint::of(&bytes));
        assert_eq!(fp(&[]), Fingerprint::of(&[]));
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
