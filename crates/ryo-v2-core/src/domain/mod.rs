//! The rules and types of the problem, in its own words.
//!
//! Pure: nothing here touches the file system, a process, the environment, the
//! network, the clock or a random source. A value that comes from outside
//! (a file's contents, the current time, a fresh id) is passed in by the caller.
//! A value with rules is a type, not a `String`: an enum for a closed set,
//! a newtype that checks its input for anything else.
//!
//! # Glossary
//! Format and checking: crates/ryo-v2-core/tests/doc_contract.rs.
//!
//! - Blueprint: one proposed change: a command, splices against base text, the
//!   read ranges it rests on, the base fingerprint of each file it edits, spec refs, a
//!   decision record and, optionally, the blueprint it builds on.
//! - BlueprintParts: the parts of a blueprint before they are checked against each other.
//! - BaseFp: the fingerprint of the whole base text of one file a blueprint edits; the file is
//!   fresh only while its whole text is unchanged.
//! - BlueprintSet: the blueprints submitted together in one apply; those not on one chain are
//!   alternatives, a blueprint with `builds_on` is against its parent's result.
//! - Splice: one change of one text range of one file's base text.
//! - SpliceKind: what a splice does to its range.
//!   includes: Replace, Insert, Delete
//! - TextRange: a half-open UTF-8 byte range `[start, end)` of one file's base text.
//! - ReadRange: a path, a text range and its fingerprint: the text the proposer read, checked
//!   for the files a blueprint only reads.
//! - Fingerprint: an algorithm-tagged hash of the bytes of a range or of a whole text, a pure
//!   function of the text.
//! - Freshness: the result of checking a read range or a base fingerprint against the current
//!   text.
//!   includes: Fresh, Stale, OutOfRange
//! - Edit: the whole new text of one file, built by splicing; what the verifier consumes and a
//!   writer writes once `prepare_write` has returned it as a `CheckedWrite`.
//! - Basis: what a candidate's verdict rests on in the loaded text: the base fingerprint of every
//!   file its path edits and the read ranges of the files it only reads. The files a check read
//!   beyond it are covered by the re-check before a write.
//! - CheckedWrite: the edits of a candidate that passed the check before a write, with the verdict
//!   of the re-check and the unseen classes it names; only `prepare_write` makes one.
//! - WriteRefusal: why the check before a write refused.
//!   includes: NotACandidate, AmbiguousPick, Drift, Appeared, UnrecordedSession, StaleSession,
//!   NewErrors, Check
//! - Drift: one way the files no longer match a basis.
//!   includes: File, Read, Missing
//! - CommandKind: a typed action of the one-layer action space.
//!   includes: RenameLocal, ExtractVariable, AddDerive, ExtractFunction, MoveItem
//! - Command: what a blueprint carries out: its CommandKind and validated parameters; what CONCEPT.md
//!   calls a mutation of the code, and a mutation list is a chain of them.
//! - CommandSpec: an entry of the catalog: a CommandKind and the risks it always carries.
//! - RiskMark: a reason, named in the result, that the engine's own verification is not enough.
//!   Borrow, BuildScript, ProcMacroCrate and Visibility each name an unseen class (UnseenClass).
//!   includes: Borrow, BuildScript, ProcMacroCrate, Behaviour, Visibility
//! - UnseenClass: a kind of error the in-process check does not see; a candidate names it in its
//!   result and the build after the commit settles it. A mark says what an edit can do and a blind
//!   class what the checker does not see; a candidate names a class only when both hold, or when
//!   the check observed it. A blind class no mark names is in the evidence only.
//!   includes: Borrow, BuildScript, ProcMacroCrate, Visibility, ConstEval, StructLiteralPath,
//!   UnusedTypeParam, DeriveWithoutServer, UnresolvedPath, DuplicateName, FieldBounds, TestCode,
//!   UnexpandedMacro
//! - UnseenClasses: the non-empty set of unseen classes a candidate names.
//! - BlindClasses: the classes of error a checker does not see, declared on its port; the evidence
//!   of a decision carries them.
//! - FileRole: what a touched file is to the crate graph.
//!   includes: Source, BuildScript, ProcMacroSource, Manifest
//! - Staged: a path of blueprints that passed every stage before the check, with the edits it
//!   would make, the roles of the files it edits and its basis; the only input of a candidate.
//! - Resolution: the outcome of resolving a set of blueprints.
//!   includes: Decided, Stopped, Failed
//! - Blocking: why a blueprint was not made a candidate.
//!   includes: Unverifiable, Stale, MissingFile, Nonconforming, Splice, NewErrors, BudgetExhausted,
//!   CheckFailed, ParentFailed
//! - CheckError: failure of a verifier check.
//!   includes: Edit, Cancelled, Panicked, DetectorPanicked, RevertMismatch, Poisoned, TimedOut,
//!   ForkInUse, AlreadyApplied, Load
//! - CheckReach: how far a check failure reaches: the one blueprint, or the session.
//!   includes: Blueprint, Session
//! - CheckFailure: why the check of a blueprint did not finish when the session survived it.
//!   includes: Edit, Cancelled, Panicked, DetectorPanicked
//! - Verdict: the new errors relative to the baseline and the unseen classes the check observed;
//!   passed if and only if there are no new errors.
//! - Evidence: what a decision rested on: the scope, the verdicts, the budget, the provenance and
//!   the blind classes of the checker.
//! - DecisionRecord: the proposer's identity and rationale, carried in the blueprint.
//! - SpecRef: a reference `<doc>#<item>` to one item of a specification document.
//! - Provenance: what a decision can be reproduced from: the analyzer, its version and
//!   configuration, and the fingerprint of the workspace it saw.
//! - SearchTree: the states reached by the search, each once, with the path of edits that
//!   reached it first.
//! - SearchNode: one state of the search tree, with its edits, file roles, metrics, prior and reward.
//! - NodeId: the position of a node in its search tree; the root is 0.
//! - StateFp: a fingerprint of the files an edit path leads to; the same files, the same state.
//! - Target: a file and, if known, a range of it where a change is wanted.
//! - ChangeProbe: a change a later edit might make, tried to see how easily a state extends.
//!   includes: AddVariant, AddField, AddMethod
//! - StaticMetrics: what is known of a candidate's edits before any check: files, crates,
//!   bytes, clean-code change and extensibility change.
//! - CleanDelta: the change in clean-code errors; higher is better.
//! - ExtDelta: the change in extensibility; higher is better.
//! - Reward: what a checked path earned, summed over its steps: passed, clean, extensibility
//!   and bytes written.
//! - Weights: how much each static metric and the prior count in a score.
//! - SearchBudget: how much search is allowed: beam, expansions, depth and milliseconds.
//! - Scored: a candidate with its state, metrics, prior and Noul, as pruning sees it.
//! - PruneStage: why pruning dropped a candidate.
//!   includes: Noul, Duplicate, Dominated, Rank
//! - Prior: the judge's estimate in thousandths that a proposal is worth checking.
//! - Criterion: a kind of flaw a judge looks for in a proposal.
//!   includes: LayerViolation, LeakyAbstraction, NamingMismatch, BehaviourChange, SpecConflict
//! - Noul: a flaw a judge found in a proposal: the criterion, the file and the range.
//! - Proposal: what a judge is shown of one blueprint: command, splices and static metrics.
//! - ProposalId: the identity of a proposal by what it does, not by its blueprint's id.
//! - JudgeRequest: the proposals put to a judge and the criteria to judge them by.
//! - Judgement: a judge's answer to a request: priors and flaws, and who answered.
//! - ProviderTag: a short label of which judge answered; never empty.
//! - Expander: the one that proposes the next blueprints from a node.
//! - Assessor: the one that measures a candidate's static metrics.
//! - Judge: the one that gives priors and finds flaws in proposals before they are checked.
//! - Verifier: the checker the decisions rely on: the texts, facts, baseline, provenance and blind
//!   classes of a loaded workspace, and a check of edits that leaves no trace.
//! - Analyzer: the lower port of an analyzer adapter: load a workspace, one fork with one write,
//!   its diagnostics and its discard; what the oracle's verify and the bench measure.
//! - Stacked: a verifier whose forks take writes on top of earlier ones, each a level; read and
//!   diagnosed at the top level, reverted level by level.
//! - Level: one write of a stacked fork, on top of the level below; the base state is below the
//!   first.
//! - RevertAudit: a check that undoing a candidate's edits returns the files to the base.
//! - ExpandError: why an expander could not propose.
//! - JudgeError: why a judge could not answer.
//! - SearchError: why a search could not run to its end.
//! - SearchReport: what a search did: the ranked result and the trace of every candidate.
//! - TraceEntry: one candidate's fate in a search, with its node and reason.
//! - Fate: what became of a candidate in a search.
//!   includes: Blocked, Pruned, TimedOut, Failed, Passed
//! - MetricJudge: a judge that answers from static metrics alone, with no model.
//! - ReplayJudge: a judge that replays recorded answers.

pub mod baseline;
pub mod basis;
pub mod blueprint;
pub mod command;
pub mod diagnostic;
pub mod edit;
pub mod error;
pub mod error_code;
pub mod file_role;
pub mod finding;
pub mod ids;
pub mod judge;
pub mod non_empty;
pub mod rel_path;
pub mod resolution;
pub mod scope;
pub mod search;
pub mod splice;
pub(crate) mod stage;
pub mod text;
pub mod verdict;
pub use basis::{Basis, Drift};
pub use blueprint::{
    Blueprint, BlueprintError, BlueprintParts, BlueprintSet, DecisionRecord, SpecRef,
};
pub use command::{
    Command, CommandKind, CommandSpec, DeriveName, Ident, Nonconformance, RiskMark, UnseenClass,
};
pub use diagnostic::{Diagnostic, Diagnostics, UnmappedDiagnostic};
pub use edit::Edit;
pub use error::DomainError;
pub use error_code::ErrorCode;
pub use file_role::{FileFacts, FileRole};
pub use finding::{Finding, FindingCode, ScopedFindings};
pub use ids::BlueprintId;
pub use judge::{
    Criterion, JudgeRequest, Judgement, Noul, Prior, Proposal, ProposalId, ProviderTag,
};
pub use non_empty::NonEmpty;
pub use rel_path::RelPath;
pub use resolution::{
    Blocking, Candidate, CheckFailure, DEFAULT_BUDGET, DecisionBudget, DecisionLog, Evidence,
    Infeasible, LogEntry, NEAR_MISS_MAX, NearMiss, Provenance, Resolution, SpecMismatch, Stage,
    UnseenClasses,
};
pub use scope::Scope;
pub use search::{
    ChangeProbe, CleanDelta, DEFAULT_WEIGHTS, ExtDelta, Inserted, NewNode, NodeId, PruneStage,
    Pruned, Reward, Scored, SearchBudget, SearchNode, SearchTree, StateFp, StaticMetrics, Target,
    Weights,
};
pub use splice::{Splice, SpliceKind};
pub use text::{BaseFp, Fingerprint, FpAlgo, Freshness, ReadRange, SpliceError, TextRange};
pub use verdict::Verdict;
