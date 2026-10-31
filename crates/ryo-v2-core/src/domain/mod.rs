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
//! - Blueprint: one proposed change: an intent call, mutation specs against base text, the
//!   read ranges it rests on, the base fingerprint of each file it edits, spec refs and a
//!   decision record.
//! - BlueprintParts: the parts of a blueprint before they are checked against each other.
//! - BaseFp: the fingerprint of the whole base text of one file a blueprint edits; the file is
//!   fresh only while its whole text is unchanged.
//! - BlueprintSet: the blueprints submitted together in one apply, the candidates for one decision.
//! - MutationSpec: one splice on one file of a text range of the base text.
//!   includes: Replace, Insert, Delete
//! - TextRange: a half-open UTF-8 byte range `[start, end)` of one file's base text.
//! - ReadRange: a path, a text range and its fingerprint: the text the proposer read, checked
//!   for the files a blueprint only reads.
//! - Fingerprint: an algorithm-tagged hash of the bytes of a range or of a whole text, a pure
//!   function of the text.
//! - Freshness: the result of checking a read range or a base fingerprint against the current
//!   text.
//!   includes: Fresh, Stale, OutOfRange
//! - Edit: the whole new text of one file, built by splicing; what the verifier consumes.
//! - IntentKind: a typed action of the one-layer action space.
//!   includes: RenameLocal, ExtractVariable, AddDerive, ExtractFunction, MoveItem
//! - IntentCall: one use of an intent in a blueprint: its kind and validated parameters.
//! - RiskMark: a reason the engine's own verification is not enough.
//!   includes: Borrow, BuildScript, ProcMacroCrate, Behaviour, Visibility
//! - FileRole: what a touched file is to the crate graph.
//!   includes: Source, BuildScript, ProcMacroSource, Manifest
//! - Rule: the record of one review done at registration, allowing auto-apply of one intent kind.
//! - RuleCatalog: the registered rules, at most one per intent kind.
//! - Resolution: the outcome of resolving a set of blueprints.
//!   includes: Decided, Stopped, Failed
//! - Disposition: what may happen to a passing candidate.
//!   includes: AutoApplicable, AfterOuterCheck, NeedsReview
//! - Blocking: why a blueprint was not made a candidate.
//!   includes: Unverifiable, Stale, MissingFile, Nonconforming, Splice, NewErrors, BudgetExhausted
//! - CheckError: failure of a verifier check.
//!   includes: Edit, Cancelled, Panicked, RevertMismatch, Poisoned, TimedOut
//! - Verdict: the new errors relative to the baseline; passed if and only if there are none.
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
//! - Proposal: what a judge is shown of one blueprint: intent, specs and static metrics.
//! - ProposalId: the identity of a proposal by what it does, not by its blueprint's id.
//! - JudgeRequest: the proposals put to a judge and the criteria to judge them by.
//! - Judgement: a judge's answer to a request: priors and flaws, and who answered.
//! - ProviderTag: a short label of which judge answered; never empty.
//! - Expander: the one that proposes the next blueprints from a node.
//! - Assessor: the one that measures a candidate's static metrics.
//! - Judge: the one that gives priors and finds flaws in proposals before they are checked.
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
pub mod blueprint;
pub mod diagnostic;
pub mod edit;
pub mod error;
pub mod error_code;
pub mod file_role;
pub mod finding;
pub mod ids;
pub mod intent;
pub mod judge;
pub mod mutation;
pub mod non_empty;
pub mod rel_path;
pub mod resolution;
pub mod rule;
pub mod scope;
pub mod search;
pub mod text;
pub mod verdict;
pub use blueprint::{
    Blueprint, BlueprintError, BlueprintParts, BlueprintSet, DecisionRecord, SpecRef,
};
pub use diagnostic::{Diagnostic, Diagnostics, UnmappedDiagnostic};
pub use edit::Edit;
pub use error::DomainError;
pub use error_code::ErrorCode;
pub use file_role::{FileFacts, FileRole};
pub use finding::{Finding, FindingCode, ScopedFindings};
pub use ids::{BlueprintId, RuleId};
pub use intent::{DeriveName, Ident, IntentCall, IntentKind, IntentSpec, Nonconformance, RiskMark};
pub use judge::{
    Criterion, JudgeRequest, Judgement, Noul, Prior, Proposal, ProposalId, ProviderTag,
};
pub use mutation::{MutationSpec, SpecKind};
pub use non_empty::NonEmpty;
pub use rel_path::RelPath;
pub use resolution::{
    Blocking, Candidate, DEFAULT_BUDGET, DecisionBudget, DecisionLog, Disposition, Evidence,
    Infeasible, LogEntry, NEAR_MISS_MAX, NearMiss, OUTER_CHECK_MAX, OuterCheck, OuterCheckReason,
    Provenance, Resolution, ReviewReason, SpecMismatch, Stage,
};
pub use rule::{ReviewRecord, Rule, RuleCatalog, RuleError};
pub use scope::Scope;
pub use search::{
    ChangeProbe, CleanDelta, DEFAULT_WEIGHTS, ExtDelta, Inserted, NewNode, NodeId, PruneStage,
    Pruned, Reward, Scored, SearchBudget, SearchNode, SearchTree, StateFp, StaticMetrics, Target,
    Weights,
};
pub use text::{BaseFp, Fingerprint, FpAlgo, Freshness, ReadRange, SpliceError, TextRange};
pub use verdict::Verdict;
