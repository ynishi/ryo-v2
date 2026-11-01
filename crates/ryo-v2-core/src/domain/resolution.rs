//! What came of a set of Blueprints: candidates that passed, or why none did.
//!
//! A candidate that passed carries its marks and the classes of error the in-process check cannot
//! see ([`unseen_classes`]) as names; which candidate to take is the caller's choice. The build
//! after the commit settles the unseen classes.
//!
//! The marks are derived here, from the Blueprints of a path and the roles of the files it edits
//! ([`risk_marks`]); a caller never hands them over.

use super::stage::Staged;
use super::{
    Basis, Blueprint, BlueprintId, CommandKind, DomainError, Drift, Edit, FileRole, Fingerprint,
    NonEmpty, Nonconformance, RelPath, RiskMark, Scope, SpliceError, UnseenClass, Verdict,
};
use std::collections::{BTreeMap, BTreeSet};

/// The classes of error the in-process check does not see that a candidate carries, named in its
/// result for the build after the commit to settle. Never empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnseenClasses(BTreeSet<UnseenClass>);

impl UnseenClasses {
    /// The classes.
    pub fn classes(&self) -> &BTreeSet<UnseenClass> {
        &self.0
    }

    /// The classes of `set`, or `None` if it is empty.
    ///
    /// # Contract
    /// - C1: `None` if and only if `set` is empty; otherwise the classes are `set`.
    ///   tested: domain::resolution::tests::unseen_classes_from_an_empty_set_is_none
    pub fn from_set(set: BTreeSet<UnseenClass>) -> Option<Self> {
        (!set.is_empty()).then_some(Self(set))
    }
}

impl std::fmt::Display for UnseenClasses {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<String> = self.0.iter().map(ToString::to_string).collect();
        write!(f, "unseen ({})", names.join(", "))
    }
}

/// The classes of unseen error that `marks` name, or `None` if there are none.
///
/// # Contract
/// - C1: the classes are those that [`RiskMark::unseen_class`] gives for `marks`; `None` when it
///   gives none (`Behaviour` is not a kind of error, so it adds no class).
///   tested: domain::resolution::tests::unseen_classes_follow_the_marks
///   tested: domain::resolution::tests::visibility_is_named_as_unseen
pub fn unseen_classes(marks: &BTreeSet<RiskMark>) -> Option<UnseenClasses> {
    UnseenClasses::from_set(marks.iter().filter_map(|m| m.unseen_class()).collect())
}

/// The classes a result names: those of `marks` that are in `blind`, together with `observed`.
///
/// # Contract
/// - C1: the classes are [`unseen_classes`] of `marks` that are in `blind`, together with all of
///   `observed`; `None` when there are none. A blind class no mark names is not named, and an
///   observed class is named whatever `blind` holds.
///   tested: domain::resolution::tests::candidate_names_what_it_touches_and_what_was_observed
///   tested: app::write::tests::recheck_names_the_unseen_it_observed
pub(crate) fn named_unseen(
    marks: &BTreeSet<RiskMark>,
    blind: &BTreeSet<UnseenClass>,
    observed: &BTreeSet<UnseenClass>,
) -> Option<UnseenClasses> {
    let mut named: BTreeSet<UnseenClass> = unseen_classes(marks)
        .map(|u| u.classes().intersection(blind).copied().collect())
        .unwrap_or_default();
    named.extend(observed.iter().copied());
    UnseenClasses::from_set(named)
}

/// The marks of a change made by `kinds` to files of `roles`.
///
/// # Contract
/// - C1: the marks are the union of the catalog risks of `kinds` and, for the roles, `BuildScript`
///   for `FileRole::BuildScript` and `ProcMacroCrate` for `FileRole::ProcMacroSource`; `Source`
///   and `Manifest` add nothing.
///   tested: domain::resolution::tests::risk_marks_are_command_risks_plus_roles
pub fn risk_marks(
    kinds: impl IntoIterator<Item = CommandKind>,
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

/// The names of `marks`, comma separated, in their order (empty for no marks).
pub(crate) fn mark_names(marks: &BTreeSet<RiskMark>) -> String {
    let names: Vec<String> = marks.iter().map(ToString::to_string).collect();
    names.join(", ")
}

/// A Blueprint, or a path of them, that passed the analyzer, with the edits it would make and
/// its marks.
///
/// ```compile_fail
/// use ryo_v2_core::domain::Candidate;
/// // Only the engine builds a candidate: its marks are derived, never handed over.
/// let _ = Candidate::new;
/// ```
///
/// # Contract
/// - C1 [invariant]: its `edits` are the texts of the state its basis was taken on, and its basis
///   is what `prepare_write` refuses a write on.
///   tested: app::apply::tests::candidate_basis_is_stale_after_outside_change
///   tested: app::write::tests::drifted_candidate_is_refused_and_never_checked
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    path: NonEmpty<Blueprint>,
    edits: Vec<Edit>,
    verdict: Verdict,
    basis: Basis,
    marks: BTreeSet<RiskMark>,
    unseen_classes: Option<UnseenClasses>,
}

impl Candidate {
    /// Builds the candidate of a passing verdict.
    ///
    /// `staged` holds the Blueprints that lead to it, the root first and the leaf last (one for
    /// an apply), the edits they make, the roles of every file they edit and their basis. `blind`
    /// is the classes its checker does not see.
    ///
    /// # Contract
    /// - C1: returns `Err(verdict)` unless `verdict.passed()`.
    ///   tested: domain::resolution::tests::candidate_refuses_failed_verdict
    /// - C2: the marks are [`risk_marks`] of the command kinds of the path of `staged` and its
    ///   roles; `unseen_classes` is the classes of those marks ([`unseen_classes`]) that are in
    ///   `blind`, together with the classes `verdict.unseen` observed, and `None` when there are
    ///   none.
    ///   tested: domain::resolution::tests::candidate_names_its_marks
    ///   tested: domain::resolution::tests::crate_graph_edits_never_pass_silently
    ///   tested: domain::resolution::tests::candidate_names_what_it_touches_and_what_was_observed
    ///   tested: domain::resolution::tests::empty_blind_names_nothing_from_marks
    pub(crate) fn new(
        staged: Staged,
        verdict: Verdict,
        blind: &BTreeSet<UnseenClass>,
    ) -> Result<Self, Verdict> {
        if !verdict.passed() {
            return Err(verdict);
        }
        let (path, edits, roles, basis) = staged.into_parts();
        let marks = risk_marks(
            path.iter().map(|b| b.command().kind()),
            roles.iter().copied(),
        );
        Ok(Self {
            path,
            edits,
            basis,
            unseen_classes: named_unseen(&marks, blind, &verdict.unseen),
            verdict,
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

    /// What its verdict rests on; `prepare_write` checks it against the files before `edits` are
    /// written, and refuses the write when it has drifted.
    pub fn basis(&self) -> &Basis {
        &self.basis
    }

    /// The risks it carries.
    pub fn marks(&self) -> &BTreeSet<RiskMark> {
        &self.marks
    }

    /// The classes of error the checker does not see that it names: the blind classes its marks
    /// name, and those observed in its check. A blind class that no mark names is in the
    /// evidence only.
    pub fn unseen_classes(&self) -> Option<&UnseenClasses> {
        self.unseen_classes.as_ref()
    }

    /// Its marks as one line, `marks: borrow, behaviour`, or `marks: none` when it has none.
    pub fn marks_line(&self) -> String {
        if self.marks.is_empty() {
            return "marks: none".to_owned();
        }
        format!("marks: {}", mark_names(&self.marks))
    }
}

/// Why the check of a Blueprint did not finish, when the session survived it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckFailure {
    /// An edit could not be applied.
    Edit {
        /// The offending file.
        path: RelPath,
        /// Why.
        reason: String,
    },
    /// The check was cancelled.
    Cancelled,
    /// The analyzer panicked.
    Panicked(String),
    /// A detector panicked on a file the check read, so the check cannot judge that file.
    DetectorPanicked {
        /// The detector, by the adapter's name for it.
        detector: String,
        /// The file.
        file: RelPath,
    },
}

impl std::fmt::Display for CheckFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CheckFailure::Edit { path, reason } => write!(f, "edit rejected: {path}: {reason}"),
            CheckFailure::Cancelled => write!(f, "check cancelled"),
            CheckFailure::Panicked(m) => write!(f, "analyzer panicked: {m}"),
            CheckFailure::DetectorPanicked { detector, file } => {
                write!(f, "detector {detector} panicked on {file}")
            }
        }
    }
}

/// Why a Blueprint was not made a candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocking {
    /// A splice is in a file the analyzer cannot judge.
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
    /// The splices are not what the command says.
    Nonconforming {
        /// The Blueprint.
        blueprint: BlueprintId,
        /// What differs.
        why: Nonconformance,
    },
    /// The splices could not be applied to the text.
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
    /// The check of the Blueprint did not finish; the session survived it.
    CheckFailed {
        /// The Blueprint.
        blueprint: BlueprintId,
        /// What went wrong.
        failure: CheckFailure,
    },
    /// The Blueprint it builds on did not pass, so it was not checked.
    ParentFailed {
        /// The Blueprint.
        blueprint: BlueprintId,
        /// The Blueprint it builds on.
        parent: BlueprintId,
    },
}

impl Blocking {
    /// The Blueprint that was blocked.
    ///
    /// # Contract
    /// - C1: every variant, `BudgetExhausted`, `CheckFailed` and `ParentFailed` included, names
    ///   the Blueprint it was made for.
    ///   tested: domain::resolution::tests::budget_exhausted_names_its_blueprint
    pub fn blueprint(&self) -> BlueprintId {
        match self {
            Blocking::Unverifiable { blueprint, .. }
            | Blocking::Stale { blueprint, .. }
            | Blocking::MissingFile { blueprint, .. }
            | Blocking::Nonconforming { blueprint, .. }
            | Blocking::Splice { blueprint, .. }
            | Blocking::NewErrors { blueprint, .. }
            | Blocking::BudgetExhausted { blueprint, .. }
            | Blocking::CheckFailed { blueprint, .. }
            | Blocking::ParentFailed { blueprint, .. } => *blueprint,
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
            Blocking::CheckFailed { blueprint, failure } => {
                write!(f, "{blueprint}: check did not finish: {failure}")
            }
            Blocking::ParentFailed { blueprint, parent } => {
                write!(f, "{blueprint}: builds on {parent}, which did not pass")
            }
        }
    }
}

/// At most this many new errors makes a failed Blueprint a near miss.
pub const NEAR_MISS_MAX: usize = 2;

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
    /// The splices against the command.
    Conformance,
    /// The splices spliced into the text.
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
    blind_classes: BTreeSet<UnseenClass>,
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
        blind_classes: BTreeSet<UnseenClass>,
    ) -> Self {
        Self {
            scope,
            fingerprints_checked,
            baseline_size,
            verdicts,
            budget,
            provenance,
            blind_classes,
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

    /// The classes of error the checker does not see, as it declared them for this decision.
    /// Empty when it sees every class.
    ///
    /// # Contract
    /// - C1: they are the classes the checker declares it does not see, whole, whatever the
    ///   candidates name.
    ///   tested: app::apply::tests::evidence_carries_the_checkers_blind_classes
    ///   tested: app::search::tests::decided_lists_every_passing_node_by_reward
    pub fn blind_classes(&self) -> &BTreeSet<UnseenClass> {
        &self.blind_classes
    }
}

/// The outcome of resolving a set of Blueprints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// At least one Blueprint passed, in input order.
    ///
    /// # Contract
    /// - C1: the candidates are alternatives on the loaded state, not parts of one change; before
    ///   writing one, `prepare_write` checks its basis and re-checks its edits, and writing one
    ///   makes every other candidate on a shared file drift.
    ///   tested: app::apply::tests::second_alternative_is_stale_after_first_is_written
    Decided {
        /// The candidates.
        blueprints: NonEmpty<Candidate>,
        /// Why each Blueprint that is not a candidate was blocked, in the order they were decided.
        blocking: Vec<Blocking>,
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
        BaseFp, BlueprintParts, Command, DecisionRecord, Diagnostic, Diagnostics, ErrorCode, Ident,
        ReadRange, Splice, TextRange,
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
    fn call(kind: CommandKind) -> Command {
        match kind {
            CommandKind::RenameLocal => Command::RenameLocal {
                from: Ident::parse("a").unwrap(),
                to: Ident::parse("b").unwrap(),
            },
            CommandKind::ExtractVariable => Command::ExtractVariable {
                name: Ident::parse("t").unwrap(),
            },
            CommandKind::AddDerive => Command::AddDerive {
                derives: NonEmpty::one(DeriveName::parse("Clone").unwrap()),
            },
            CommandKind::ExtractFunction => Command::ExtractFunction {
                name: Ident::parse("h").unwrap(),
            },
            CommandKind::MoveItem => Command::MoveItem {
                item: Ident::parse("i").unwrap(),
                to: RelPath::parse("b.rs").unwrap(),
            },
        }
    }
    /// A Blueprint of `kind`; the command is not checked against its splices when it is built.
    fn blueprint(n: u8, kind: CommandKind) -> Blueprint {
        let path = RelPath::parse("a.rs").unwrap();
        let range = TextRange::new(0, 1).unwrap();
        Blueprint::new(BlueprintParts {
            id: BlueprintId::parse(&format!("01890a5d-ac96-774b-bcce-b302099a80{n:02x}")).unwrap(),
            command: call(kind),
            splices: vec![Splice::replace(path.clone(), range, "y").unwrap()],
            reads: vec![ReadRange::capture(path.clone(), "x", range).unwrap()],
            bases: vec![BaseFp::capture(path, "x")],
            spec_refs: vec![],
            record: DecisionRecord::new("agent", "why").unwrap(),
            builds_on: None,
        })
        .unwrap()
    }
    fn path_of(kinds: &[CommandKind]) -> NonEmpty<Blueprint> {
        NonEmpty::new(
            kinds
                .iter()
                .zip(1u8..)
                .map(|(k, n)| blueprint(n, *k))
                .collect(),
        )
        .unwrap()
    }
    fn staged(kinds: &[CommandKind], roles: BTreeSet<FileRole>, edits: Vec<Edit>) -> Staged {
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
    /// The classes the four marks name, all blind.
    fn mark_blind() -> BTreeSet<UnseenClass> {
        BTreeSet::from([
            UnseenClass::Borrow,
            UnseenClass::BuildScript,
            UnseenClass::ProcMacroCrate,
            UnseenClass::Visibility,
        ])
    }

    #[test]
    fn unseen_classes_from_an_empty_set_is_none() {
        assert_eq!(UnseenClasses::from_set(BTreeSet::new()), None);
        let one = BTreeSet::from([UnseenClass::ConstEval]);
        assert_eq!(
            UnseenClasses::from_set(one.clone()).unwrap().classes(),
            &one
        );
    }

    #[test]
    fn empty_blind_names_nothing_from_marks() {
        // Every mark that names a class, and a checker that is blind to none: nothing is named.
        let c = Candidate::new(
            staged(
                &[CommandKind::ExtractFunction, CommandKind::MoveItem],
                BTreeSet::from([FileRole::BuildScript, FileRole::ProcMacroSource]),
                vec![],
            ),
            passing(),
            &BTreeSet::new(),
        )
        .unwrap();
        assert!(c.marks().contains(&RiskMark::Borrow));
        assert!(c.marks().contains(&RiskMark::Visibility));
        assert!(c.marks().contains(&RiskMark::BuildScript));
        assert!(c.marks().contains(&RiskMark::ProcMacroCrate));
        assert_eq!(c.unseen_classes(), None);
    }

    #[test]
    fn candidate_names_what_it_touches_and_what_was_observed() {
        let borrow = || staged(&[CommandKind::ExtractVariable], BTreeSet::new(), vec![]);
        let observed = Verdict {
            unseen: BTreeSet::from([UnseenClass::UnexpandedMacro]),
            ..passing()
        };
        // The mark names `Borrow` and the checker is blind to it: named.
        let blind = BTreeSet::from([UnseenClass::Borrow, UnseenClass::ConstEval]);
        let c = Candidate::new(borrow(), passing(), &blind).unwrap();
        assert_eq!(
            c.unseen_classes().unwrap().classes(),
            &BTreeSet::from([UnseenClass::Borrow])
        );
        // The checker sees borrows: the mark names a class that is not blind, so nothing; a blind
        // class no mark names (`ConstEval`) is not named either.
        let other = BTreeSet::from([UnseenClass::ConstEval]);
        let c = Candidate::new(borrow(), passing(), &other).unwrap();
        assert_eq!(c.unseen_classes(), None);
        // An observed class is named whatever the marks and the blind classes are.
        let c = Candidate::new(borrow(), observed.clone(), &other).unwrap();
        assert_eq!(
            c.unseen_classes().unwrap().classes(),
            &BTreeSet::from([UnseenClass::UnexpandedMacro])
        );
        let c = Candidate::new(borrow(), observed, &blind).unwrap();
        assert_eq!(
            c.unseen_classes().unwrap().classes(),
            &BTreeSet::from([UnseenClass::Borrow, UnseenClass::UnexpandedMacro])
        );
    }

    #[test]
    fn unseen_classes_follow_the_marks() {
        for mask in 0..32u8 {
            let m = marks(mask);
            let expected: BTreeSet<UnseenClass> =
                m.iter().filter_map(|k| k.unseen_class()).collect();
            match unseen_classes(&m) {
                Some(u) => {
                    assert!(!expected.is_empty(), "{m:?}");
                    assert_eq!(u.classes(), &expected, "{m:?}");
                }
                None => assert!(expected.is_empty(), "{m:?}"),
            }
        }
        assert!(unseen_classes(&marks(0)).is_none());
        assert!(unseen_classes(&BTreeSet::from([RiskMark::Behaviour])).is_none());
        let both =
            unseen_classes(&BTreeSet::from([RiskMark::Borrow, RiskMark::BuildScript])).unwrap();
        assert_eq!(
            both.classes(),
            &BTreeSet::from([UnseenClass::Borrow, UnseenClass::BuildScript])
        );
        let named =
            unseen_classes(&BTreeSet::from([RiskMark::Borrow, RiskMark::Visibility])).unwrap();
        assert_eq!(named.to_string(), "unseen (borrow, visibility)");
    }

    #[test]
    fn visibility_is_named_as_unseen() {
        let vis = BTreeSet::from([RiskMark::Visibility]);
        assert_eq!(
            unseen_classes(&vis).unwrap().classes(),
            &BTreeSet::from([UnseenClass::Visibility])
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
        let cf = Blocking::CheckFailed {
            blueprint: bid(),
            failure: CheckFailure::Panicked("boom".to_owned()),
        };
        assert_eq!(cf.blueprint(), bid());
        assert_eq!(
            cf.to_string(),
            format!("{}: check did not finish: analyzer panicked: boom", bid())
        );
        // A child of a failed parent names the child, not the parent.
        let parent = BlueprintId::parse("01890a5d-ac96-774b-bcce-b302099a80ff").unwrap();
        let pf = Blocking::ParentFailed {
            blueprint: bid(),
            parent,
        };
        assert_eq!(pf.blueprint(), bid());
        assert_eq!(
            pf.to_string(),
            format!("{}: builds on {parent}, which did not pass", bid())
        );
    }

    #[test]
    fn crate_graph_edits_never_pass_silently() {
        // The pure half; `app::apply::tests` drives the same cases through `apply_blueprints`.
        for (role, mark, named) in [
            (
                FileRole::BuildScript,
                RiskMark::BuildScript,
                UnseenClass::BuildScript,
            ),
            (
                FileRole::ProcMacroSource,
                RiskMark::ProcMacroCrate,
                UnseenClass::ProcMacroCrate,
            ),
        ] {
            let c = Candidate::new(
                staged(&[CommandKind::MoveItem], BTreeSet::from([role]), vec![]),
                passing(),
                &mark_blind(),
            )
            .unwrap();
            assert!(c.marks().contains(&mark), "{role}");
            assert!(
                c.unseen_classes().unwrap().classes().contains(&named),
                "{role}"
            );
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
        let err = Candidate::new(
            staged(&[CommandKind::MoveItem], BTreeSet::new(), vec![]),
            v.clone(),
            &mark_blind(),
        )
        .unwrap_err();
        assert_eq!(err, v);
        let ok = Candidate::new(
            staged(
                &[CommandKind::RenameLocal, CommandKind::MoveItem],
                BTreeSet::new(),
                vec![Edit::delete(RelPath::parse("a.rs").unwrap())],
            ),
            passing(),
            &mark_blind(),
        )
        .unwrap();
        assert_eq!(ok.blueprint(), ok.path()[1].id());
        assert_eq!(ok.path().len(), 2);
        assert_eq!(ok.edits().len(), 1);
        assert!(ok.verdict().passed());
        assert_eq!(ok.marks(), &BTreeSet::from([RiskMark::Visibility]));
    }

    #[test]
    fn candidate_names_its_marks() {
        let mut paths: Vec<Vec<CommandKind>> = CommandKind::ALL.iter().map(|k| vec![*k]).collect();
        for a in CommandKind::ALL {
            for b in CommandKind::ALL {
                paths.push(vec![a, b]);
            }
        }
        for kinds in &paths {
            for roles in role_subsets() {
                let c = Candidate::new(
                    staged(kinds, roles.clone(), vec![]),
                    passing(),
                    &mark_blind(),
                )
                .unwrap();
                let marks = risk_marks(kinds.iter().copied(), roles.iter().copied());
                assert_eq!(c.marks(), &marks, "{kinds:?} {roles:?}");
                assert_eq!(c.unseen_classes(), unseen_classes(&marks).as_ref());
                let kinds_of: Vec<_> = c.path().iter().map(|b| b.command().kind()).collect();
                assert_eq!(&kinds_of, kinds);
            }
        }
    }

    #[test]
    fn risk_marks_are_command_risks_plus_roles() {
        assert!(risk_marks([], []).is_empty());
        for kind in CommandKind::ALL {
            assert_eq!(risk_marks([kind], []), kind.spec().risks, "{kind}");
        }
        for role in ALL_ROLES {
            let expected = match role {
                FileRole::BuildScript => BTreeSet::from([RiskMark::BuildScript]),
                FileRole::ProcMacroSource => BTreeSet::from([RiskMark::ProcMacroCrate]),
                FileRole::Source | FileRole::Manifest => BTreeSet::new(),
            };
            assert_eq!(risk_marks([], [role]), expected, "{role}");
            for kind in CommandKind::ALL {
                let mut all = kind.spec().risks;
                all.extend(expected.iter().copied());
                assert_eq!(risk_marks([kind], [role]), all, "{kind} {role}");
            }
        }
        assert_eq!(
            risk_marks(
                [CommandKind::MoveItem, CommandKind::ExtractVariable],
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
