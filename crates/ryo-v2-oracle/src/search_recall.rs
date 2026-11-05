//! The H6 harness: how often the search keeps the best change of a decision.
//!
//! A decision is a planted state ([`crate::planted`]): clean code with the inverse of one or two
//! refactors injected, so the best change (the one that restores the clean text) is known by
//! construction. `run_case` runs `search_decision` from the planted state and reads the trace:
//!
//! - `generated`: some child at the depth of the case restores the clean text. This measures the
//!   expander, not the pruning, and is reported on its own.
//! - `hit`: such a child survived the pruning (it was checked, or audited). **recall@20** is
//!   hits over generated decisions, never over all decisions.
//!
//! Everything but [`run_case`] is a pure function of a [`SearchReport`] and the planted state, so
//! the tables are tested without a language server. [`run_case`] is generic over the ports of
//! `ryo-v2-core`; the rust-analyzer instances live in the `search-recall` binary.
//!
//! A run also reports the panics its sessions contained and went on past ([`ContainedPanics`]):
//! a detector or expander panic costs one file or one call, and a measurement that hid it would
//! read a lower recall as a weaker search.
//!
//! Adopt at recall >= [`ADOPT_AT`], discard below [`DISCARD_BELOW`], hold between, and say
//! nothing below [`MIN_DECISIONS`] compared decisions. With 0 misses in 30 the true miss rate
//! is at most 0.1 at 95 % (the rule of three).

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use ryo_v2_core::app::memory::MemoryBudget;
use ryo_v2_core::app::ports::{Assessor, CheckError, Clock, Forks, Gauge, Judge, RevertAudit};
use ryo_v2_core::app::search::{
    DEFAULT_SEARCH_BUDGET, Fate, SearchError, SearchInput, SearchReport, TRIGGER_AFTER_CHECKS,
    TRIGGER_CHECK_GROWTH_PERMILLE, TRIGGER_CHECK_MEDIAN_MS, TRIGGER_DECISION_MEDIAN_MS,
    TRIGGER_HARNESS_MS, TRIGGER_RSS_KB, TraceEntry, search_decision,
};
use ryo_v2_core::app::verdict_of;
use ryo_v2_core::domain::{
    ChangeProbe, DEFAULT_WEIGHTS, DomainError, Edit, Ident, PruneStage, RelPath, Resolution, Scope,
    SearchBudget, Stage, Target, TextRange,
};
use ryo_v2_core::infra::overlay::{OverlayError, overlay_edits};
use ryo_v2_ra::{Backend, DetectorPanic, ExpanderPanic};
use serde::{Serialize, Serializer};

use crate::planted::{PlantKind, texts_match};

/// Recall at or above this adopts H6.
pub const ADOPT_AT: f64 = 0.8;
/// Recall below this discards H6.
pub const DISCARD_BELOW: f64 = 0.5;
/// Fewer compared decisions than this decide nothing.
pub const MIN_DECISIONS: usize = 30;
/// The beam of the default budget: the "20" of recall@20.
pub const RECALL_K: usize = 20;

/// Why a case could not be loaded or run.
#[derive(Debug, thiserror::Error)]
pub enum RecallError {
    /// A file or directory could not be read.
    #[error("read {path}: {source}")]
    Io {
        /// The path.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A case file is not valid TOML of the expected shape.
    #[error("parse {path}: {source}")]
    Toml {
        /// The path.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: toml::de::Error,
    },
    /// A case is well-formed but wrong.
    #[error("{path}: {reason}")]
    Invalid {
        /// The case file.
        path: PathBuf,
        /// What is wrong.
        reason: String,
    },
    /// The overlay could not be turned into edits.
    #[error(transparent)]
    Overlay(#[from] OverlayError),
    /// The check of the planted state failed as a process.
    #[error("check of the planted state failed: {0}")]
    Check(#[source] CheckError),
    /// The search could not run to its end.
    #[error(transparent)]
    Search(#[from] SearchError),
}

/// A target of a case file: a file and optionally a byte range of its planted text.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetFile {
    /// Path relative to the workspace.
    pub path: String,
    /// Start byte, with `end`.
    #[serde(default)]
    pub start: Option<u32>,
    /// End byte, with `start`.
    #[serde(default)]
    pub end: Option<u32>,
}

/// A probe of a case file (`kind` is `add-variant`, `add-field` or `add-method`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeFile {
    /// The probe kind.
    pub kind: String,
    /// The type the change is probed on.
    #[serde(rename = "type")]
    pub ty: String,
}

/// The shape of a `cases/<id>.toml` file of the planted corpus.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlantedCaseFile {
    /// The base workspace: absolute, or relative to the corpus directory.
    pub workspace: String,
    /// The overlay directory: absolute, or relative to the parent of `cases/`.
    pub overlay: String,
    /// The [`PlantKind`] ids planted, in order.
    pub kinds: Vec<String>,
    /// The depth of the best: one per kind.
    pub depth: u8,
    /// Where the case came from.
    pub origin: String,
    /// The touched functions and items.
    pub targets: Vec<TargetFile>,
    /// The probes the assessor measures extensibility with; filled by hand.
    #[serde(default)]
    pub probes: Vec<ProbeFile>,
    /// The helper `inline-function` folded.
    #[serde(default)]
    pub original_fn: Option<String>,
    /// When set, the case is not run; the string says why.
    #[serde(default)]
    pub skip: Option<String>,
}

/// A loaded case.
#[derive(Debug, Clone)]
pub struct PlantedCase {
    /// File stem of the case file.
    pub id: String,
    /// The name of the base workspace: the last component of its path.
    pub ws: String,
    /// The base workspace.
    pub workspace: PathBuf,
    /// The overlay directory.
    pub overlay: PathBuf,
    /// The kinds planted.
    pub kinds: Vec<PlantKind>,
    /// The depth of the best.
    pub depth: u8,
    /// Where the case came from.
    pub origin: String,
    /// The targets of the search.
    pub targets: Vec<Target>,
    /// The probes of the assessor.
    pub probes: Vec<ChangeProbe>,
    /// The helper `inline-function` folded.
    pub original_fn: Option<String>,
    /// Why the case is skipped, if it is.
    pub skip: Option<String>,
}

/// Parses the text of a case file.
pub fn parse_case_file(text: &str) -> Result<PlantedCaseFile, toml::de::Error> {
    toml::from_str(text)
}

fn read_dir_sorted(dir: &Path) -> Result<Vec<PathBuf>, RecallError> {
    let io = |source| RecallError::Io {
        path: dir.to_path_buf(),
        source,
    };
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(io)? {
        out.push(entry.map_err(io)?.path());
    }
    out.sort();
    Ok(out)
}

fn find_cases_dirs(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), RecallError> {
    for path in read_dir_sorted(dir)? {
        if !path.is_dir() {
            continue;
        }
        match path.file_name().and_then(|n| n.to_str()) {
            Some("target" | ".git") => {}
            Some("cases") => out.push(path),
            _ => find_cases_dirs(&path, out)?,
        }
    }
    Ok(())
}

impl PlantedCase {
    /// Walks `root` (not `target` or `.git`) and loads every `cases/*.toml`, sorted by
    /// (workspace, id). A relative `workspace` is relative to `corpus`, a relative `overlay` to the
    /// parent of `cases/`. It is `Invalid` when a kind is unknown, `depth` is not the number of
    /// kinds, or the overlay directory does not exist.
    pub fn load_all(root: &Path, corpus: &Path) -> Result<Vec<PlantedCase>, RecallError> {
        let mut dirs = Vec::new();
        find_cases_dirs(root, &mut dirs)?;
        let mut cases = Vec::new();
        for dir in dirs {
            let ws_dir = dir.parent().unwrap_or(root);
            for file in read_dir_sorted(&dir)? {
                if file.extension().is_some_and(|e| e == "toml") {
                    cases.push(load_one(&file, ws_dir, corpus)?);
                }
            }
        }
        cases.sort_by(|a, b| (&a.workspace, &a.id).cmp(&(&b.workspace, &b.id)));
        Ok(cases)
    }

    /// The edits of the planted state over the base workspace.
    pub fn edits(&self) -> Result<Vec<Edit>, RecallError> {
        Ok(overlay_edits(&self.workspace, &self.overlay)?)
    }

    /// The clean text of every planted path: its text in the base workspace.
    pub fn clean_texts(&self, edits: &[Edit]) -> Result<BTreeMap<RelPath, String>, RecallError> {
        let mut out = BTreeMap::new();
        for e in edits {
            let file = self.workspace.join(e.path.as_str());
            let text = std::fs::read_to_string(&file)
                .map_err(|source| RecallError::Io { path: file, source })?;
            out.insert(e.path.clone(), text);
        }
        Ok(out)
    }
}

fn load_one(file: &Path, ws_dir: &Path, corpus: &Path) -> Result<PlantedCase, RecallError> {
    let invalid = |reason: String| RecallError::Invalid {
        path: file.to_path_buf(),
        reason,
    };
    let text = std::fs::read_to_string(file).map_err(|source| RecallError::Io {
        path: file.to_path_buf(),
        source,
    })?;
    let cf = parse_case_file(&text).map_err(|source| RecallError::Toml {
        path: file.to_path_buf(),
        source,
    })?;
    let workspace = if Path::new(&cf.workspace).is_absolute() {
        PathBuf::from(&cf.workspace)
    } else {
        corpus.join(&cf.workspace)
    };
    let overlay = if Path::new(&cf.overlay).is_absolute() {
        PathBuf::from(&cf.overlay)
    } else {
        ws_dir.join(&cf.overlay)
    };
    if !overlay.is_dir() {
        return Err(invalid(format!(
            "overlay directory not found: {}",
            overlay.display()
        )));
    }
    let kinds = cf
        .kinds
        .iter()
        .map(|k| PlantKind::parse(k).ok_or_else(|| invalid(format!("unknown kind {k:?}"))))
        .collect::<Result<Vec<_>, _>>()?;
    if kinds.is_empty() || usize::from(cf.depth) != kinds.len() {
        return Err(invalid(format!(
            "depth {} is not the number of kinds {}",
            cf.depth,
            kinds.len()
        )));
    }
    let dom = |what: &str, e: DomainError| invalid(format!("{what}: {e}"));
    let mut targets = Vec::new();
    for t in &cf.targets {
        let path = RelPath::parse(&t.path).map_err(|e| dom("target path", e))?;
        let range = match (t.start, t.end) {
            (Some(s), Some(e)) => Some(TextRange::new(s, e).map_err(|e| dom("target range", e))?),
            (None, None) => None,
            _ => {
                return Err(invalid(
                    "a target needs both start and end, or neither".into(),
                ));
            }
        };
        targets.push(Target::new(path, range));
    }
    let mut probes = Vec::new();
    for p in &cf.probes {
        let ty = Ident::parse(&p.ty).map_err(|e| dom("probe type", e))?;
        probes.push(match p.kind.as_str() {
            "add-variant" => ChangeProbe::AddVariant { ty },
            "add-field" => ChangeProbe::AddField { ty },
            "add-method" => ChangeProbe::AddMethod { ty },
            other => return Err(invalid(format!("unknown probe kind {other:?}"))),
        });
    }
    let id = file
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| invalid("file name is not UTF-8".into()))?
        .to_string();
    let ws = workspace
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    Ok(PlantedCase {
        id,
        ws,
        workspace,
        overlay,
        kinds,
        depth: cf.depth,
        origin: cf.origin,
        targets,
        probes,
        original_fn: cf.original_fn,
        skip: cf.skip,
    })
}

// ---------------------------------------------------------------------------------------------
// Matching and reading the trace
// ---------------------------------------------------------------------------------------------

/// What is known by construction about one decision.
#[derive(Debug, Clone, Copy)]
pub struct Truth<'a> {
    /// The planted state over the base.
    pub planted: &'a [Edit],
    /// The clean text of every planted path.
    pub clean: &'a BTreeMap<RelPath, String>,
    /// The helper `inline-function` folded.
    pub original_fn: Option<&'a str>,
    /// The depth of the best.
    pub depth: u8,
}

/// Whether the state with `edits` is the clean one: every edit is a write to a planted path whose
/// text equals the clean text up to whitespace runs and the name of the helper the expander
/// gave a function it extracted (read as `original_fn`). Empty edits are the base, which is
/// clean, so they always match.
pub fn entry_matches(edits: &[Edit], truth: &Truth<'_>) -> bool {
    edits.iter().all(|e| {
        let Some(clean) = truth.clean.get(&e.path) else {
            return false;
        };
        let Some(text) = &e.new_text else {
            return false;
        };
        let Some(planted) = truth
            .planted
            .iter()
            .find(|p| p.path == e.path)
            .and_then(|p| p.new_text.as_deref())
        else {
            return false;
        };
        texts_match(text, clean, planted, truth.original_fn)
    })
}

/// The stage name that dropped a child, or what became of it.
pub fn fate_name(fate: &Fate) -> &'static str {
    match fate {
        Fate::Blocked(stage) => match stage {
            Stage::FileRole => "file-role",
            Stage::Freshness => "freshness",
            Stage::Conformance => "conformance",
            Stage::Splice => "splice",
            Stage::Marks => "marks",
            Stage::Prune => "prune",
            Stage::Check => "check",
            Stage::Candidate => "candidate",
        },
        Fate::Pruned(stage) => match stage {
            PruneStage::Noul => "noul",
            PruneStage::Duplicate => "duplicate",
            PruneStage::Dominated => "dominated",
            PruneStage::Rank => "rank",
        },
        Fate::TimedOut => "timed-out",
        Fate::Failed { .. } => "failed",
        Fate::Passed => "passed",
    }
}

/// Whether the child got past the pruning: it was checked, or audited.
fn survived(t: &TraceEntry) -> bool {
    t.audited || matches!(t.fate, Fate::Passed | Fate::Failed { .. } | Fate::TimedOut)
}

/// The kinds as their kebab-case ids, the shape a row has in the report.
fn kind_ids<S: Serializer>(kinds: &[PlantKind], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_seq(kinds.iter().map(|k| k.id()))
}

/// One decision, measured or skipped.
///
/// # Contract
/// - C1: serialized, a row is an object with one key per field, of the field's name and value;
///   the kinds are their kebab-case ids. The keys and values of the hand-built row it replaced
///   are kept, so reports before and after compare key by key. A row has no backend key: the
///   backend is the run's, named once by the report's backend key (`Report` C1), so the rows of
///   a run on the undo log and of one on the Engine compare key by key.
///   tested: search_recall::tests::row_json_keeps_its_keys_and_values
///   tested: search_recall::tests::row_json_carries_the_depth_times
///   tested: search_recall::tests::row_json_carries_the_level_writes
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DecisionRow {
    /// The case id.
    pub id: String,
    /// The base workspace.
    pub ws: String,
    /// The kinds planted.
    #[serde(serialize_with = "kind_ids")]
    pub kinds: Vec<PlantKind>,
    /// The depth of the best.
    pub depth: u8,
    /// Why the decision was not measured.
    pub skipped: Option<String>,
    /// Some child at `depth` restored the clean text.
    pub generated: bool,
    /// Such a child survived the pruning.
    pub hit: bool,
    /// Its place in the beam (from 0).
    pub best_rank: Option<u16>,
    /// What became of the best child (a stage name, `passed`, ...); `None` if not generated.
    pub fate: Option<String>,
    /// A node that restores the clean text is among the first three of the ranking.
    pub top3: bool,
    /// The place of the first such node in the ranking (from 0).
    pub final_rank: Option<u16>,
    /// The search ended in `Decided`.
    pub decided: bool,
    /// Children proposed per depth.
    pub candidates: Vec<u32>,
    /// Children kept per depth.
    pub kept: Vec<u16>,
    /// Flawed children checked anyway.
    pub noul_audited: u32,
    /// Of those, the ones that passed the check (a judge flaw that was a false alarm).
    pub noul_audited_passed: u32,
    /// Wall time of the search in ms.
    pub wall_ms: u64,
    /// The time of each check in ms (filled by the caller that observes them).
    pub check_ms: Vec<u64>,
    /// Per depth: the time in the checks of the depth in ms (`DepthStats::check_ms`).
    pub depth_check_ms: Vec<u64>,
    /// Per depth: the time in the expansions in ms, from the first expansion job's submit to the
    /// last one's outcome (`DepthStats::expand_ms`).
    pub depth_expand_ms: Vec<u64>,
    /// Per depth: the checks that ran (`DepthStats::checked`).
    pub depth_checked: Vec<u16>,
    /// Per depth: the parents expanded (`DepthStats::parents`).
    pub depth_parents: Vec<u16>,
    /// Per depth: the write jobs of the search's forks (`DepthStats::writes`).
    pub depth_writes: Vec<u32>,
    /// The Blueprints of the trace whose fate is `TimedOut`, in trace order: the children a
    /// cut-off, the budget or the memory stop kept from a verdict.
    pub timed_out: Vec<String>,
    /// The stops that ended the search, of `budget_exhausted`, `soft_stopped` and
    /// `memory_stopped`, in that order. With `timed_out`, a row with neither has no wall-clock
    /// effect.
    pub stops: Vec<String>,
}

impl DecisionRow {
    /// A row for a decision that was not measured.
    pub fn skipped(case: &PlantedCase, why: impl Into<String>) -> DecisionRow {
        DecisionRow {
            id: case.id.clone(),
            ws: case.ws.clone(),
            kinds: case.kinds.clone(),
            depth: case.depth,
            skipped: Some(why.into()),
            generated: false,
            hit: false,
            best_rank: None,
            fate: None,
            top3: false,
            final_rank: None,
            decided: false,
            candidates: Vec::new(),
            kept: Vec::new(),
            noul_audited: 0,
            noul_audited_passed: 0,
            wall_ms: 0,
            check_ms: Vec::new(),
            depth_check_ms: Vec::new(),
            depth_expand_ms: Vec::new(),
            depth_checked: Vec::new(),
            depth_parents: Vec::new(),
            depth_writes: Vec::new(),
            timed_out: Vec::new(),
            stops: Vec::new(),
        }
    }
}

/// Reads a finished search against the truth of the case.
pub fn analyze(case: &PlantedCase, truth: &Truth<'_>, report: &SearchReport) -> DecisionRow {
    let matching: Vec<&TraceEntry> = report
        .trace
        .iter()
        .filter(|t| t.depth == truth.depth)
        .filter(|t| {
            t.edits
                .as_deref()
                .is_some_and(|edits| entry_matches(edits, truth))
        })
        .collect();
    let best = matching
        .iter()
        .find(|t| survived(t))
        .or_else(|| matching.first())
        .copied();
    // Any depth: the nodes that are the clean state, for the final ranking.
    let clean_nodes: BTreeSet<_> = report
        .trace
        .iter()
        .filter(|t| {
            t.edits
                .as_deref()
                .is_some_and(|edits| entry_matches(edits, truth))
        })
        .filter_map(|t| t.node)
        .collect();
    let final_rank = report
        .ranking
        .iter()
        .position(|r| clean_nodes.contains(&r.node))
        .map(|i| u16::try_from(i).unwrap_or(u16::MAX));
    DecisionRow {
        id: case.id.clone(),
        ws: case.ws.clone(),
        kinds: case.kinds.clone(),
        depth: case.depth,
        skipped: None,
        generated: best.is_some(),
        hit: matching.iter().any(|t| survived(t)),
        best_rank: best.and_then(|t| t.rank),
        fate: best.map(|t| fate_name(&t.fate).to_string()),
        top3: final_rank.is_some_and(|r| r < 3),
        final_rank,
        decided: matches!(report.resolution, Resolution::Decided { .. }),
        candidates: report.depths.iter().map(|d| d.generated).collect(),
        kept: report.depths.iter().map(|d| d.kept).collect(),
        noul_audited: count32(report.trace.iter().filter(|t| t.audited).count()),
        noul_audited_passed: count32(
            report
                .trace
                .iter()
                .filter(|t| t.audited && matches!(t.fate, Fate::Passed))
                .count(),
        ),
        wall_ms: report.elapsed_ms,
        check_ms: Vec::new(),
        depth_check_ms: report.depths.iter().map(|d| d.check_ms).collect(),
        depth_expand_ms: report.depths.iter().map(|d| d.expand_ms).collect(),
        depth_checked: report.depths.iter().map(|d| d.checked).collect(),
        depth_parents: report.depths.iter().map(|d| d.parents).collect(),
        depth_writes: report.depths.iter().map(|d| d.writes).collect(),
        timed_out: report
            .trace
            .iter()
            .filter(|t| t.fate == Fate::TimedOut)
            .map(|t| t.blueprint.to_string())
            .collect(),
        stops: [
            (report.budget_exhausted, "budget_exhausted"),
            (report.soft_stopped, "soft_stopped"),
            (report.memory_stopped, "memory_stopped"),
        ]
        .into_iter()
        .filter(|(set, _)| *set)
        .map(|(_, name)| name.to_owned())
        .collect(),
    }
}

fn count32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The result of one case: the row, and the edits of the candidates the search decided on.
#[derive(Debug, Clone)]
pub struct CaseRun {
    /// The decision.
    pub row: DecisionRow,
    /// The edits of each `Decided` candidate in the order of the ranking, whole planted state
    /// included; the outer `cargo check` of the harness reads them, all of them or the top of
    /// them by [`outer_sample`].
    pub decided_edits: Vec<Vec<Edit>>,
}

/// Which candidates of each decision the outer `cargo check` of a run measures.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OuterSample {
    /// No outer check ran (no `--outer-check`).
    #[default]
    Off,
    /// Every candidate (`--outer-check`).
    All,
    /// The top `n` of each decision's ranking (`--outer-check --outer-sample N`).
    Top(NonZeroUsize),
}

impl std::fmt::Display for OuterSample {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OuterSample::Off => f.write_str("none (no outer check)"),
            OuterSample::All => f.write_str("all"),
            OuterSample::Top(n) => write!(f, "top {n} of each decision"),
        }
    }
}

/// The candidates the outer `cargo check` measures under `sample`.
///
/// # Contract
/// - C1: `Off` gives none, `All` every candidate, and `Top(n)` the first
///   `min(n, candidates.len())`, in the order given.
///   tested: search_recall::tests::outer_sample_takes_the_top_of_the_ranking
pub fn outer_sample<T>(candidates: &[T], sample: OuterSample) -> &[T] {
    match sample {
        OuterSample::Off => &[],
        OuterSample::All => candidates,
        OuterSample::Top(n) => &candidates[..n.get().min(candidates.len())],
    }
}

/// The value of `--outer-sample`: a number of at least 1.
///
/// # Contract
/// - C1: a decimal number of at least 1 is accepted; 0 and anything that is not a number are
///   refused.
///   tested: search_recall::tests::outer_sample_zero_is_refused
pub fn parse_outer_sample(s: &str) -> Result<NonZeroUsize, String> {
    match s.parse::<usize>() {
        Ok(n) => NonZeroUsize::new(n).ok_or_else(|| "--outer-sample is at least 1".to_owned()),
        Err(_) => Err("--outer-sample is a number".to_owned()),
    }
}

/// Runs one planted case on a loaded session.
///
/// The planted state is checked first; one that does not verify is skipped (the harness cannot
/// ask a search to improve code that does not compile). Then `search_decision` runs from the
/// planted edits with no roots, at `case.depth`, and the trace is read with [`analyze`].
/// `NoCandidates` is a measured decision that generated nothing; any other search failure is an
/// error, after which the session must be reloaded. The search runs on `v`'s forks, with the RSS
/// stop of `memory` read through `gauge`.
#[allow(clippy::too_many_arguments)]
pub fn run_case<V, A, J>(
    v: &V,
    a: &A,
    j: &J,
    clock: &dyn Clock,
    (gauge, memory): (&dyn Gauge, MemoryBudget),
    s: &mut V::Session,
    case: &PlantedCase,
    budget: SearchBudget,
) -> Result<CaseRun, RecallError>
where
    V: Forks + RevertAudit,
    A: Assessor,
    J: Judge,
{
    let skipped = |why: String| CaseRun {
        row: DecisionRow::skipped(case, why),
        decided_edits: Vec::new(),
    };
    if let Some(why) = &case.skip {
        return Ok(skipped(why.clone()));
    }
    let edits = case.edits()?;
    if edits.is_empty() {
        return Ok(skipped("the overlay makes no edit".into()));
    }
    let clean = case.clean_texts(&edits)?;
    let baseline = v.baseline(s);
    let verdict = match verdict_of(v, s, &edits, Scope::Affected, &baseline) {
        Ok(verdict) => verdict,
        // The planted state cannot be judged, and the session is intact: skip the case without
        // the reload a search error makes.
        Err(e @ CheckError::DetectorPanicked { .. }) => {
            return Ok(skipped(format!("planted state cannot be checked: {e}")));
        }
        Err(e) => return Err(RecallError::Check(e)),
    };
    if !verdict.passed() {
        return Ok(skipped(format!(
            "planted state does not verify: {} new errors, {} unmapped",
            verdict.new_errors.errors.len(),
            verdict.new_errors.unmapped.len()
        )));
    }
    let mut budget = budget;
    budget.max_depth = case.depth.max(1);
    let input = SearchInput {
        start: &edits,
        roots: &[],
        targets: &case.targets,
        probes: &case.probes,
        scope: Scope::Affected,
        budget,
        memory,
        weights: DEFAULT_WEIGHTS,
        keep_trace_edits: true,
    };
    let t0 = clock.now_ms();
    let report = match search_decision(v, a, j, clock, gauge, s, &input) {
        Ok(r) => r,
        Err(SearchError::NoCandidates) => {
            let mut row = DecisionRow::skipped(case, "");
            row.skipped = None;
            row.wall_ms = clock.now_ms().saturating_sub(t0);
            row.fate = Some("no-candidates".into());
            return Ok(CaseRun {
                row,
                decided_edits: Vec::new(),
            });
        }
        Err(err) => return Err(err.into()),
    };
    let truth = Truth {
        planted: &edits,
        clean: &clean,
        original_fn: case.original_fn.as_deref(),
        depth: case.depth,
    };
    let row = analyze(case, &truth, &report);
    let decided_edits = match &report.resolution {
        Resolution::Decided { blueprints, .. } => {
            blueprints.iter().map(|c| c.edits().to_vec()).collect()
        }
        _ => Vec::new(),
    };
    Ok(CaseRun { row, decided_edits })
}

// ---------------------------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------------------------

/// What the numbers say about H6.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Fewer than [`MIN_DECISIONS`] compared decisions.
    Insufficient,
    /// Recall at or above [`ADOPT_AT`].
    Adopt,
    /// Recall between the two thresholds.
    Hold,
    /// Recall below [`DISCARD_BELOW`].
    Discard,
}

impl Outcome {
    /// The word printed.
    pub fn id(self) -> &'static str {
        match self {
            Outcome::Insufficient => "insufficient",
            Outcome::Adopt => "adopt",
            Outcome::Hold => "hold",
            Outcome::Discard => "discard",
        }
    }
}

/// The H6 verdict for `compared` decisions with recall `recall`.
pub fn verdict(compared: usize, recall: Option<f64>) -> Outcome {
    match recall {
        Some(r) if compared >= MIN_DECISIONS => {
            if r >= ADOPT_AT {
                Outcome::Adopt
            } else if r < DISCARD_BELOW {
                Outcome::Discard
            } else {
                Outcome::Hold
            }
        }
        _ => Outcome::Insufficient,
    }
}

/// Counts over a set of rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Rows measured (not skipped).
    pub measured: usize,
    /// Rows skipped.
    pub skipped: usize,
    /// Measured rows whose best child was generated.
    pub generated: usize,
    /// Of those, the ones where the best survived the pruning.
    pub hit: usize,
    /// Measured rows with a clean state in the first three of the ranking.
    pub top3: usize,
}

impl Counts {
    /// Counts `rows`.
    pub fn of<'a>(rows: impl IntoIterator<Item = &'a DecisionRow>) -> Counts {
        let mut c = Counts::default();
        for r in rows {
            if r.skipped.is_some() {
                c.skipped += 1;
                continue;
            }
            c.measured += 1;
            c.generated += usize::from(r.generated);
            c.hit += usize::from(r.hit);
            c.top3 += usize::from(r.top3);
        }
        c
    }

    /// recall@20: hits over decisions whose best was generated; `None` when none was.
    pub fn recall(&self) -> Option<f64> {
        (self.generated > 0).then(|| self.hit as f64 / self.generated as f64)
    }

    /// Hits over all measured decisions (what recall would be if a best not generated were a miss).
    pub fn recall_of_all(&self) -> Option<f64> {
        (self.measured > 0).then(|| self.hit as f64 / self.measured as f64)
    }

    /// Generated over measured.
    pub fn generation_rate(&self) -> Option<f64> {
        (self.measured > 0).then(|| self.generated as f64 / self.measured as f64)
    }

    /// Decisions with a clean state in the first three, over measured.
    pub fn top3_rate(&self) -> Option<f64> {
        (self.measured > 0).then(|| self.top3 as f64 / self.measured as f64)
    }
}

fn ratio(r: Option<f64>) -> String {
    r.map_or_else(|| "-".to_string(), |v| format!("{v:.2}"))
}

fn median(mut v: Vec<u64>) -> Option<u64> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    Some(v[v.len() / 2])
}

/// Trigger 3, growth: how many checks at each end of a run are compared (the median of the
/// first this many against the median of the last this many).
pub const GROWTH_WINDOW: usize = 100;

/// The median of the last [`GROWTH_WINDOW`] checks in thousandths of the median of the first
/// [`GROWTH_WINDOW`]; `None` when fewer than `TRIGGER_AFTER_CHECKS` checks ran.
///
/// The checks of several workspaces (one session each) are flattened into one list, so on small
/// bases the first and last windows can come from different sessions; the trigger is meant for
/// the one-workspace large run.
///
/// # Contract
/// - C1: `None` below `TRIGGER_AFTER_CHECKS` checks; else `last * 1000 / max(first, 1)` with
///   `first` and `last` the medians of the first and the last `GROWTH_WINDOW` entries of
///   `checks`, in the order given.
///   tested: search_recall::tests::check_growth_compares_the_ends_of_the_run
pub fn check_growth_permille(checks: &[u64]) -> Option<u64> {
    if checks.len() < TRIGGER_AFTER_CHECKS as usize {
        return None;
    }
    let w = GROWTH_WINDOW.min(checks.len());
    let first = median(checks[..w].to_vec())?;
    let last = median(checks[checks.len() - w..].to_vec())?;
    Some(last * 1000 / first.max(1))
}

/// What a run reports besides its decisions: the beam it ran with and the trigger numbers of decision D4 in SEARCH_LEDGER.md.
///
/// The wall time is split in two because only one part is the search's: `outer_check_ms` is the
/// oracle's own `cargo check` (the baseline of each workspace and every decided candidate under
/// `--outer-check`), which runs after each search and outside it, and no perf step of D4
/// shortens it. Trigger 4 reads [`RunInfo::search_ms`], the rest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunInfo {
    /// The beam of the search: the "k" of recall@k. 0 reads as [`RECALL_K`].
    pub beam: u16,
    /// Wall time of the whole run in ms, outer check included.
    pub harness_ms: u64,
    /// The part of `harness_ms` spent in the outer `cargo check`, in ms (0 without
    /// `--outer-check`).
    pub outer_check_ms: u64,
    /// Which candidates of each decision the outer check measured: `Off` without
    /// `--outer-check`. It is the denominator of the rejection rate.
    pub outer_sample: OuterSample,
    /// The search budget the run used; `None` when unknown, which leaves trigger 4 not measured.
    pub budget: Option<SearchBudget>,
    /// Resident set size at the end of the run in kB, when known.
    pub rss_kb: Option<u64>,
    /// The backend the run's forks ran on: the Engine, which the search's forks need (D11,
    /// stage 4).
    pub backend: Backend,
    /// The jobs the search's forks ran at once (`Forks::width`, `--width`); 0 when unknown.
    pub width: usize,
}

/// Whether a run with `budget` is judged by trigger 4 (decision D4 in SEARCH_LEDGER.md): its
/// budget is nowhere larger than `DEFAULT_SEARCH_BUDGET`.
///
/// A larger budget (`--exhaustive`, a wider `--beam`) lifts the limits the promise is made
/// under, so its time breaks no promise; a narrower one can only be faster, so a fire there is a
/// fire at the default too. `max_depth` is set per case and `audit_every` costs no time, so
/// neither is compared.
///
/// # Contract
/// - C1: true iff `beam`, `max_expand`, `beam_parents`, `per_candidate_ms`, `soft_ms` and
///   `hard_ms` are each at most their value in `DEFAULT_SEARCH_BUDGET`.
///   tested: search_recall::tests::trigger_4_judges_runs_within_the_default_budget
pub fn trigger_4_judges(budget: &SearchBudget) -> bool {
    let d = DEFAULT_SEARCH_BUDGET;
    budget.beam <= d.beam
        && budget.max_expand <= d.max_expand
        && budget.beam_parents <= d.beam_parents
        && budget.per_candidate_ms <= d.per_candidate_ms
        && budget.soft_ms <= d.soft_ms
        && budget.hard_ms <= d.hard_ms
}

impl RunInfo {
    /// The numbers of one finished `search-recall run`: the budget it searched with, its whole
    /// wall time, the part of it spent in the outer `cargo check`, the sample the outer check
    /// took, the RSS, the backend its forks ran on and their width.
    ///
    /// # Contract
    /// - C1: `beam` is `budget.beam`, `budget` is `Some(budget)`, `outer_sample`, `backend` and
    ///   `width` are the ones given, and both times are whole ms, saturating at `u64::MAX`.
    ///   tested: search_recall::tests::run_info_of_a_run_carries_budget_and_outer_time
    pub fn of_run(
        budget: SearchBudget,
        whole: std::time::Duration,
        outer_check: std::time::Duration,
        outer_sample: OuterSample,
        rss_kb: Option<u64>,
        (backend, width): (Backend, usize),
    ) -> Self {
        let ms = |d: std::time::Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        RunInfo {
            beam: budget.beam,
            harness_ms: ms(whole),
            outer_check_ms: ms(outer_check),
            outer_sample,
            budget: Some(budget),
            rss_kb,
            backend,
            width,
        }
    }

    /// The wall time of the run without the outer check: what trigger 4 compares; 0 when the
    /// outer check is reported longer than the run.
    pub fn search_ms(&self) -> u64 {
        self.harness_ms.saturating_sub(self.outer_check_ms)
    }

    /// The k of recall@k.
    pub fn k(&self) -> usize {
        if self.beam == 0 {
            RECALL_K
        } else {
            usize::from(self.beam)
        }
    }
}

/// The trigger status (decision D4 in SEARCH_LEDGER.md) lines for `rows` and a run.
///
/// # Contract
/// - C1: trigger 3 fires when the RSS is above `TRIGGER_RSS_KB` or the check growth is above
///   `TRIGGER_CHECK_GROWTH_PERMILLE`, each read only after `TRIGGER_AFTER_CHECKS` checks; the
///   line reads FIRED if either half fired, else "not measured" if either half was not
///   measured, else "not fired".
///   tested: search_recall::tests::trigger_3_reads_the_growth_limit
///   tested: search_recall::tests::triggers_follow_the_decided_numbers
/// - C2: trigger 4 compares [`RunInfo::search_ms`] (the run without its outer check) with
///   `TRIGGER_HARNESS_MS`: FIRED above it, "not fired" at or below it; "not measured" when the
///   budget is unknown or [`trigger_4_judges`] refuses it, whatever the time. The line also
///   prints the outer-check and whole-run times.
///   tested: search_recall::tests::trigger_4_reads_the_search_time_of_a_default_run
pub fn trigger_lines(rows: &[DecisionRow], t: RunInfo) -> Vec<String> {
    let measured: Vec<&DecisionRow> = rows.iter().filter(|r| r.skipped.is_none()).collect();
    let decision = median(measured.iter().map(|r| r.wall_ms).collect());
    let checks: Vec<u64> = measured
        .iter()
        .flat_map(|r| r.check_ms.iter().copied())
        .collect();
    let n_checks = checks.len();
    let growth = check_growth_permille(&checks);
    let check = median(checks);
    let status = |fired: Option<bool>| match fired {
        Some(true) => "FIRED",
        Some(false) => "not fired",
        None => "not measured",
    };
    let ms = |v: Option<u64>| v.map_or_else(|| "-".to_string(), |v| format!("{v} ms"));
    let rss_fired = if n_checks >= TRIGGER_AFTER_CHECKS as usize {
        t.rss_kb.map(|r| r > TRIGGER_RSS_KB)
    } else {
        None
    };
    let growth_fired = growth.map(|g| g > TRIGGER_CHECK_GROWTH_PERMILLE);
    let trigger_3 = match (rss_fired, growth_fired) {
        (Some(true), _) | (_, Some(true)) => "FIRED",
        (None, _) | (_, None) => "not measured",
        _ => "not fired",
    };
    vec![
        format!(
            "trigger 1, median decision > {TRIGGER_DECISION_MEDIAN_MS} ms: {} ({})",
            ms(decision),
            status(decision.map(|d| d > TRIGGER_DECISION_MEDIAN_MS))
        ),
        format!(
            "trigger 2, median warm check > {TRIGGER_CHECK_MEDIAN_MS} ms: {} over {n_checks} checks ({})",
            ms(check),
            status(check.map(|c| c > TRIGGER_CHECK_MEDIAN_MS))
        ),
        format!(
            "trigger 3, RSS > {TRIGGER_RSS_KB} kB or check time > {TRIGGER_CHECK_GROWTH_PERMILLE} permille of the first {GROWTH_WINDOW} after {TRIGGER_AFTER_CHECKS} checks: RSS {} kB ({}), check time {} permille ({}) after {n_checks} checks ({trigger_3})",
            t.rss_kb.map_or_else(|| "-".to_string(), |v| v.to_string()),
            status(rss_fired),
            growth.map_or_else(|| "-".to_string(), |v| v.to_string()),
            status(growth_fired)
        ),
        format!(
            "trigger 4, one run without the outer check > {TRIGGER_HARNESS_MS} ms: {} ms (outer check {} ms, whole run {} ms) ({})",
            t.search_ms(),
            t.outer_check_ms,
            t.harness_ms,
            match t.budget {
                Some(b) if trigger_4_judges(&b) => status(Some(t.search_ms() > TRIGGER_HARNESS_MS)),
                Some(_) => "not measured: budget beyond the default",
                None => "not measured: budget unknown",
            }
        ),
    ]
}

fn group_by(
    rows: &[DecisionRow],
    key: impl Fn(&DecisionRow) -> Vec<String>,
) -> BTreeMap<String, Vec<&DecisionRow>> {
    let mut out: BTreeMap<String, Vec<&DecisionRow>> = BTreeMap::new();
    for r in rows {
        for k in key(r) {
            out.entry(k).or_default().push(r);
        }
    }
    out
}

fn counts_row(label: &str, c: &Counts) -> String {
    format!(
        "| {label} | {} | {} | {} | {} | {} | {} | {} | {} |",
        c.measured,
        c.skipped,
        c.generated,
        c.hit,
        ratio(c.recall()),
        ratio(c.recall_of_all()),
        ratio(c.generation_rate()),
        ratio(c.top3_rate()),
    )
}

fn counts_head(k: usize) -> String {
    format!(
        "| set | decisions | skipped | generated | hit | recall@{k} | hit/all | generation | top-3 |\n|---|---|---|---|---|---|---|---|---|"
    )
}

/// The tables of a run as Markdown (the shape of `SEARCH_LEDGER.md`).
pub fn markdown(rows: &[DecisionRow], t: RunInfo) -> String {
    let all = Counts::of(rows);
    let k = t.k();
    let mut out = String::new();
    out.push_str(&format!("{}\n", counts_head(k)));
    out.push_str(&format!("{}\n", counts_row("all", &all)));
    for (ws, rs) in group_by(rows, |r| vec![r.ws.clone()]) {
        out.push_str(&format!(
            "{}\n",
            counts_row(&format!("ws {ws}"), &Counts::of(rs.iter().copied()))
        ));
    }
    for (kind, rs) in group_by(rows, |r| {
        r.kinds.iter().map(|k| k.id().to_string()).collect()
    }) {
        out.push_str(&format!(
            "{}\n",
            counts_row(&format!("kind {kind}"), &Counts::of(rs.iter().copied()))
        ));
    }
    for (depth, rs) in group_by(rows, |r| vec![r.depth.to_string()]) {
        out.push_str(&format!(
            "{}\n",
            counts_row(&format!("depth {depth}"), &Counts::of(rs.iter().copied()))
        ));
    }
    out.push_str(&format!(
        "\nVerdict: {} (recall@{k} {} over {} generated decisions; thresholds adopt >= {ADOPT_AT}, discard < {DISCARD_BELOW}, at least {MIN_DECISIONS} compared)\n",
        verdict(all.generated, all.recall()).id(),
        ratio(all.recall()),
        all.generated,
    ));
    out.push_str("\n| decision | kinds | depth | generated | hit | beam rank | fate | final rank | proposed | kept | wall ms |\n|---|---|---|---|---|---|---|---|---|---|---|\n");
    for r in rows {
        if let Some(why) = &r.skipped {
            out.push_str(&format!(
                "| {}/{} | skipped: {why} | | | | | | | | | |\n",
                r.ws, r.id
            ));
            continue;
        }
        let join = |v: Vec<String>| v.join("/");
        out.push_str(&format!(
            "| {}/{} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            r.ws,
            r.id,
            r.kinds.iter().map(|k| k.id()).collect::<Vec<_>>().join("+"),
            r.depth,
            if r.generated { "yes" } else { "no" },
            if r.hit { "yes" } else { "no" },
            r.best_rank
                .map_or_else(|| "-".to_string(), |v| v.to_string()),
            r.fate.as_deref().unwrap_or("-"),
            r.final_rank
                .map_or_else(|| "-".to_string(), |v| v.to_string()),
            join(r.candidates.iter().map(u32::to_string).collect()),
            join(r.kept.iter().map(u16::to_string).collect()),
            r.wall_ms,
        ));
    }
    let mut dropped: BTreeMap<&str, usize> = BTreeMap::new();
    for r in rows
        .iter()
        .filter(|r| r.skipped.is_none() && r.generated && !r.hit)
    {
        *dropped.entry(r.fate.as_deref().unwrap_or("-")).or_default() += 1;
    }
    out.push_str("\n| dropped at | decisions |\n|---|---|\n");
    if dropped.is_empty() {
        out.push_str("| (none: every generated best survived) | 0 |\n");
    }
    for (stage, n) in dropped {
        out.push_str(&format!("| {stage} | {n} |\n"));
    }
    let audited: u32 = rows.iter().map(|r| r.noul_audited).sum();
    let passed: u32 = rows.iter().map(|r| r.noul_audited_passed).sum();
    out.push_str(&format!(
        "\nNoul audit: {audited} flawed children checked anyway, {passed} of them passed the check.\n\n"
    ));
    for line in trigger_lines(rows, t) {
        out.push_str(&format!("- {line}\n"));
    }
    if t.outer_sample != OuterSample::Off {
        out.push_str(&format!("- outer sample: {}\n", t.outer_sample));
    }
    out
}

/// The text printed to the terminal: the tables of [`markdown`] plus the skipped reasons.
pub fn terminal(rows: &[DecisionRow], t: RunInfo) -> String {
    markdown(rows, t)
}

/// The panics a run contained and went on past, by workspace.
///
/// Sessions record the panics of the detector and of the expander instead of failing; the run
/// collects them here so the count is printed, zero included.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainedPanics {
    detector: BTreeSet<(String, DetectorPanic)>,
    expander: BTreeSet<(String, ExpanderPanic)>,
}

impl ContainedPanics {
    /// Adds what a session recorded; adding the same session twice counts each panic once.
    pub fn absorb(&mut self, ws: &str, detector: &[DetectorPanic], expander: &[ExpanderPanic]) {
        for d in detector {
            self.detector.insert((ws.to_owned(), d.clone()));
        }
        for e in expander {
            self.expander.insert((ws.to_owned(), e.clone()));
        }
    }

    /// No panic recorded.
    pub fn is_empty(&self) -> bool {
        self.detector.is_empty() && self.expander.is_empty()
    }

    /// Number of distinct detector panics.
    pub fn detector(&self) -> usize {
        self.detector.len()
    }

    /// Number of distinct expander panics.
    pub fn expander(&self) -> usize {
        self.expander.len()
    }

    /// `contained panics: {d} detector, {e} expander` followed by one line per panic:
    /// `detector panic {ws} {detector} {path}: {message}` / `expander panic {ws} {kind|-} {path|-}: {message}`
    /// (message: first line only). Always at least the first line.
    pub fn lines(&self) -> Vec<String> {
        let first = |m: &str| m.lines().next().unwrap_or("").to_owned();
        let mut out = vec![format!(
            "contained panics: {} detector, {} expander",
            self.detector(),
            self.expander()
        )];
        for (ws, d) in &self.detector {
            out.push(format!(
                "detector panic {ws} {} {}: {}",
                d.detector,
                d.path,
                first(&d.message)
            ));
        }
        for (ws, e) in &self.expander {
            out.push(format!(
                "expander panic {ws} {} {}: {}",
                e.kind.map_or("-", |k| k.id()),
                e.path.as_ref().map_or("-", |p| p.as_str()),
                first(&e.message)
            ));
        }
        out
    }

    /// The panics as the `--report` file holds them. The entries are the report's own shapes, not
    /// those of the records a session keeps: a detector entry leaves the detector out, and an
    /// expander entry writes its kind as an id.
    pub fn report(&self) -> PanicsReport<'_> {
        PanicsReport {
            detector: self
                .detector
                .iter()
                .map(|(ws, d)| DetectorEntry {
                    ws,
                    path: d.path.as_str(),
                    message: &d.message,
                })
                .collect(),
            expander: self
                .expander
                .iter()
                .map(|(ws, e)| ExpanderEntry {
                    ws,
                    kind: e.kind.map(|k| k.id()),
                    path: e.path.as_ref().map(|p| p.as_str()),
                    message: &e.message,
                })
                .collect(),
        }
    }
}

/// The contained panics of a run, as the `--report` file holds them: `detector` entries of `ws`,
/// `path` and `message`, and `expander` entries of `ws`, `kind` (its id, or null), `path` (or null)
/// and `message`.
#[derive(Debug, Serialize)]
pub struct PanicsReport<'a> {
    detector: Vec<DetectorEntry<'a>>,
    expander: Vec<ExpanderEntry<'a>>,
}

/// A detector panic in the report.
#[derive(Debug, Serialize)]
struct DetectorEntry<'a> {
    ws: &'a str,
    path: &'a str,
    message: &'a str,
}

/// An expander panic in the report.
#[derive(Debug, Serialize)]
struct ExpanderEntry<'a> {
    ws: &'a str,
    kind: Option<&'static str>,
    path: Option<&'a str>,
    message: &'a str,
}

/// `outer_sample` in the report: null without an outer check, `"all"`, or the number of candidates
/// per decision.
fn outer_sample_value<S: Serializer>(
    sample: &OuterSample,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match sample {
        OuterSample::Off => serializer.serialize_none(),
        OuterSample::All => serializer.serialize_str("all"),
        OuterSample::Top(n) => n.get().serialize(serializer),
    }
}

/// The whole run, as the `--report` file holds it.
///
/// # Contract
/// - C1: serialized, the report is the object `--report` writes: the keys decisions, skipped,
///   generated, hit, beam, recall_at_beam, hit_over_all, generation_rate, top3_rate, verdict,
///   harness_ms, outer_check_ms, outer_sample, search_ms, rss_kb, triggers and rows with the
///   values the hand-built report gave them, and contained_panics, with its detector and expander
///   entries, when panics are given; rows are as `DecisionRow` C1 says. The key backend, first,
///   names the backend of the run (`RunInfo::backend`), `"undo-log"` or `"engine"`, so that two
///   runs on the same cases can be told apart; every other key is the same on both. The key
///   width, second, is `RunInfo::width`.
///   tested: search_recall::tests::report_json_keeps_its_keys_and_values
#[derive(Debug, Serialize)]
pub struct Report<'a> {
    backend: Backend,
    width: usize,
    decisions: usize,
    skipped: usize,
    generated: usize,
    hit: usize,
    beam: usize,
    recall_at_beam: Option<f64>,
    hit_over_all: Option<f64>,
    generation_rate: Option<f64>,
    top3_rate: Option<f64>,
    verdict: &'static str,
    harness_ms: u64,
    outer_check_ms: u64,
    #[serde(serialize_with = "outer_sample_value")]
    outer_sample: OuterSample,
    search_ms: u64,
    rss_kb: Option<u64>,
    triggers: Vec<String>,
    rows: &'a [DecisionRow],
    #[serde(skip_serializing_if = "Option::is_none")]
    contained_panics: Option<PanicsReport<'a>>,
}

impl<'a> Report<'a> {
    /// The report of `rows` under `t`, with the contained `panics` when the run collected them.
    pub fn of(
        rows: &'a [DecisionRow],
        t: RunInfo,
        panics: Option<&'a ContainedPanics>,
    ) -> Report<'a> {
        let all = Counts::of(rows);
        Report {
            backend: t.backend,
            width: t.width,
            decisions: all.measured,
            skipped: all.skipped,
            generated: all.generated,
            hit: all.hit,
            beam: t.k(),
            recall_at_beam: all.recall(),
            hit_over_all: all.recall_of_all(),
            generation_rate: all.generation_rate(),
            top3_rate: all.top3_rate(),
            verdict: verdict(all.generated, all.recall()).id(),
            harness_ms: t.harness_ms,
            outer_check_ms: t.outer_check_ms,
            outer_sample: t.outer_sample,
            search_ms: t.search_ms(),
            rss_kb: t.rss_kb,
            triggers: trigger_lines(rows, t),
            rows,
            contained_panics: panics.map(ContainedPanics::report),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryo_v2_core::app::search::{DepthStats, RankedNode};
    use ryo_v2_core::domain::{
        BlueprintId, CommandKind, Fingerprint, Infeasible, Prior, ProposalId, Reward, SearchTree,
        StateFp,
    };
    use ryo_v2_ra::GapDetector;
    use serde_json::{Value, json};

    #[test]
    fn contained_panics_lines_count_and_list() {
        let mut p = ContainedPanics::default();
        assert!(p.is_empty());
        assert_eq!(p.lines(), vec!["contained panics: 0 detector, 0 expander"]);
        let d = [DetectorPanic {
            detector: GapDetector::DuplicateName,
            path: RelPath::parse("src/a.rs").unwrap(),
            message: "boom\nmore".to_owned(),
        }];
        let e = [ExpanderPanic {
            kind: Some(CommandKind::ExtractVariable),
            path: None,
            message: "bang".to_owned(),
        }];
        p.absorb("ws1", &d, &e);
        p.absorb("ws1", &d, &e);
        assert!(!p.is_empty());
        assert_eq!((p.detector(), p.expander()), (1, 1));
        let lines = p.lines();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "contained panics: 1 detector, 1 expander");
        assert_eq!(lines[1], "detector panic ws1 duplicate-name src/a.rs: boom");
        assert_eq!(
            lines[2],
            format!(
                "expander panic ws1 {} -: bang",
                CommandKind::ExtractVariable.id()
            )
        );
        assert_eq!(
            serde_json::to_value(p.report()).unwrap()["expander"][0]["ws"],
            "ws1"
        );
    }

    fn rel(s: &str) -> RelPath {
        RelPath::parse(s).unwrap()
    }

    const CLEAN: &str = "pub fn run(a: u32) -> u32 {\n    let sum = a + 1;\n    sum\n}\n";
    const PLANTED: &str = "pub fn run(a: u32) -> u32 {\n    let sum = a + 1;\n    sum\n}\n\nfn helper(a: u32) -> u32 {\n    a + 2\n}\n";

    fn case() -> PlantedCase {
        PlantedCase {
            id: "c".into(),
            ws: "lab".into(),
            workspace: PathBuf::from("/nowhere"),
            overlay: PathBuf::from("/nowhere"),
            kinds: vec![PlantKind::ObscureName],
            depth: 1,
            origin: "test".into(),
            targets: Vec::new(),
            probes: Vec::new(),
            original_fn: None,
            skip: None,
        }
    }

    fn planted() -> Vec<Edit> {
        vec![Edit::write(rel("a/src/lib.rs"), PLANTED)]
    }

    fn clean() -> BTreeMap<RelPath, String> {
        BTreeMap::from([(rel("a/src/lib.rs"), CLEAN.to_string())])
    }

    fn entry(
        depth: u8,
        fate: Fate,
        rank: Option<u16>,
        edits: Option<Vec<Edit>>,
        tag: u8,
    ) -> TraceEntry {
        let root = SearchTree::new(Vec::new()).root();
        TraceEntry {
            depth,
            parent: root,
            blueprint: BlueprintId::parse("01890a5d-ac96-774b-bcce-b302099a8057").unwrap(),
            proposal: ProposalId::parse(&Fingerprint::of(&[tag]).to_string()).unwrap(),
            command: CommandKind::RenameLocal,
            state: edits.as_deref().map(StateFp::of),
            score: None,
            rank,
            audited: false,
            root: false,
            fate,
            node: None,
            edits,
        }
    }

    fn report(trace: Vec<TraceEntry>) -> SearchReport {
        let generated = u32::try_from(trace.len()).unwrap();
        SearchReport {
            resolution: Resolution::Failed(Infeasible::new(vec![], vec![], true).unwrap()),
            ranking: Vec::new(),
            trace,
            depths: vec![DepthStats {
                depth: 1,
                generated,
                kept: 2,
                ..DepthStats::default()
            }],
            judge_providers: Vec::new(),
            judge_errors: Vec::new(),
            expand_errors: Vec::new(),
            budget_exhausted: false,
            soft_stopped: false,
            memory_stopped: false,
            elapsed_ms: 42,
        }
    }

    fn truth<'a>(p: &'a [Edit], c: &'a BTreeMap<RelPath, String>) -> Truth<'a> {
        Truth {
            planted: p,
            clean: c,
            original_fn: None,
            depth: 1,
        }
    }

    #[test]
    fn planted_best_rank_is_reported() {
        let (p, c) = (planted(), clean());
        let other = Some(vec![Edit::write(rel("a/src/lib.rs"), "pub fn x() {}\n")]);
        let restored = Some(vec![Edit::write(rel("a/src/lib.rs"), CLEAN)]);
        let kept = report(vec![
            entry(1, Fate::Passed, Some(0), other.clone(), 1),
            entry(1, Fate::Passed, Some(3), restored.clone(), 2),
        ]);
        let row = analyze(&case(), &truth(&p, &c), &kept);
        assert!(row.generated && row.hit, "{row:?}");
        assert_eq!(row.best_rank, Some(3));
        assert_eq!(row.fate.as_deref(), Some("passed"));
        assert_eq!(
            (row.candidates.clone(), row.kept.clone()),
            (vec![2], vec![2])
        );
        assert_eq!(row.wall_ms, 42);
        // The best is generated but dropped by the dominance rule: a miss, with its stage.
        let dropped = report(vec![
            entry(1, Fate::Passed, Some(0), other.clone(), 1),
            entry(
                1,
                Fate::Pruned(PruneStage::Dominated),
                None,
                restored.clone(),
                2,
            ),
        ]);
        let row = analyze(&case(), &truth(&p, &c), &dropped);
        assert!(row.generated && !row.hit, "{row:?}");
        assert_eq!(row.fate.as_deref(), Some("dominated"));
        // An exact base (no edits) is the clean state too; a different depth does not count.
        let base = report(vec![entry(1, Fate::Passed, Some(1), Some(Vec::new()), 3)]);
        assert!(analyze(&case(), &truth(&p, &c), &base).hit);
        let deeper = report(vec![entry(2, Fate::Passed, Some(1), restored, 4)]);
        let row = analyze(&case(), &truth(&p, &c), &deeper);
        assert!(!row.generated && row.fate.is_none(), "{row:?}");
        // A child dropped before any check never carries edits: not generated.
        let blocked = report(vec![entry(1, Fate::Blocked(Stage::Splice), None, None, 5)]);
        assert!(!analyze(&case(), &truth(&p, &c), &blocked).generated);
    }

    #[test]
    fn final_rank_reads_the_ranking() {
        let (p, c) = (planted(), clean());
        let root = SearchTree::new(Vec::new()).root();
        let mut e = entry(
            1,
            Fate::Passed,
            Some(0),
            Some(vec![Edit::write(rel("a/src/lib.rs"), CLEAN)]),
            1,
        );
        e.node = Some(root);
        let mut r = report(vec![e]);
        r.ranking = vec![RankedNode {
            node: root,
            path: Vec::new(),
            depth: 1,
            reward: Reward {
                passed: true,
                clean: 0,
                extensibility: 0,
                edit_bytes: 0,
            },
            reward_score: 0,
            prior: Prior::NEUTRAL,
        }];
        let row = analyze(&case(), &truth(&p, &c), &r);
        assert_eq!(row.final_rank, Some(0));
        assert!(row.top3);
    }

    #[test]
    fn best_match_ignores_whitespace_and_alpha_renames_fn() {
        let (p, c) = (planted(), clean());
        let t = truth(&p, &c);
        let spaced = "pub fn run(a: u32) -> u32 {\n\n        let   sum = a + 1;\n  sum }\n";
        assert!(entry_matches(
            &[Edit::write(rel("a/src/lib.rs"), spaced)],
            &t
        ));
        assert!(!entry_matches(
            &[Edit::write(rel("a/src/lib.rs"), PLANTED)],
            &t
        ));
        assert!(!entry_matches(&[Edit::write(rel("other.rs"), CLEAN)], &t));
        assert!(!entry_matches(&[Edit::delete(rel("a/src/lib.rs"))], &t));
        // inline-function: the helper the expander named `fun_name` is the original `add`.
        let clean_fn = "pub fn run(a: u32, b: u32) -> u32 {\n    let sum = add(a, b);\n    sum\n}\n\nfn add(a: u32, b: u32) -> u32 {\n    a + b\n}\n";
        let planted_fn = "pub fn run(a: u32, b: u32) -> u32 {\n    let sum = a + b;\n    sum\n}\n";
        let generated = "pub fn run(a: u32, b: u32) -> u32 {\n    let sum = fun_name(a, b);\n    sum\n}\n\nfn fun_name(a: u32, b: u32) -> u32 {\n    a + b\n}\n";
        let cm = BTreeMap::from([(rel("a/src/lib.rs"), clean_fn.to_string())]);
        let pe = vec![Edit::write(rel("a/src/lib.rs"), planted_fn)];
        let with_orig = Truth {
            planted: &pe,
            clean: &cm,
            original_fn: Some("add"),
            depth: 1,
        };
        let edits = [Edit::write(rel("a/src/lib.rs"), generated)];
        assert!(entry_matches(&edits, &with_orig));
        let without = Truth {
            original_fn: None,
            ..with_orig
        };
        assert!(!entry_matches(&edits, &without));
        // A reordered parameter list is a different text.
        let reordered = generated.replace(
            "(a: u32, b: u32) -> u32 {\n    a + b",
            "(b: u32, a: u32) -> u32 {\n    a + b",
        );
        assert!(!entry_matches(
            &[Edit::write(rel("a/src/lib.rs"), reordered)],
            &with_orig
        ));
    }

    fn row(id: &str, generated: bool, hit: bool) -> DecisionRow {
        let mut r = DecisionRow::skipped(&case(), "");
        r.id = id.into();
        r.skipped = None;
        r.generated = generated;
        r.hit = hit;
        r
    }

    #[test]
    fn recall_counts_only_generated_bests() {
        let rows = vec![
            row("a", true, true),
            row("b", true, true),
            row("c", true, false),
            row("d", false, false),
            DecisionRow::skipped(&case(), "planted state does not verify"),
        ];
        let c = Counts::of(&rows);
        assert_eq!((c.measured, c.skipped, c.generated, c.hit), (4, 1, 3, 2));
        assert!((c.recall().unwrap() - 2.0 / 3.0).abs() < 1e-9);
        assert!((c.recall_of_all().unwrap() - 0.5).abs() < 1e-9);
        assert!((c.generation_rate().unwrap() - 0.75).abs() < 1e-9);
        assert_eq!(Counts::of(&Vec::<DecisionRow>::new()).recall(), None);
        let md = markdown(&rows, RunInfo::default());
        assert!(
            md.contains("| all | 4 | 1 | 3 | 2 | 0.67 | 0.50 | 0.75 |"),
            "{md}"
        );
        assert!(md.contains("Verdict: insufficient"), "{md}");
        assert!(md.contains("recall@20"), "{md}");
        let narrow = markdown(
            &rows,
            RunInfo {
                beam: 5,
                ..RunInfo::default()
            },
        );
        assert!(
            narrow.contains("recall@5 ") && !narrow.contains("recall@20"),
            "{narrow}"
        );
        assert!(
            md.contains("| dominated |") || md.contains("| - | 1 |"),
            "{md}"
        );
    }

    #[test]
    fn verdict_thresholds_follow_h6() {
        assert_eq!(verdict(29, Some(1.0)), Outcome::Insufficient);
        assert_eq!(verdict(30, None), Outcome::Insufficient);
        assert_eq!(verdict(30, Some(0.8)), Outcome::Adopt);
        assert_eq!(verdict(30, Some(0.79)), Outcome::Hold);
        assert_eq!(verdict(30, Some(0.5)), Outcome::Hold);
        assert_eq!(verdict(30, Some(0.49)), Outcome::Discard);
        assert_eq!(verdict(100, Some(1.0)).id(), "adopt");
    }

    #[test]
    fn case_toml_rejects_unknown_fields() {
        let ok = "workspace = \"search/base/search-lab\"\noverlay = \"planted/x\"\nkinds = [\"obscure-name\"]\ndepth = 1\norigin = \"planted:obscure-name:0\"\ntargets = [{ path = \"geo/src/lib.rs\", start = 1, end = 9 }]\nprobes = []\n";
        let parsed = parse_case_file(ok).unwrap();
        assert_eq!(parsed.kinds, ["obscure-name"]);
        assert_eq!(parsed.targets[0].end, Some(9));
        assert!(parse_case_file(&format!("{ok}extra = 1\n")).is_err());
        assert!(
            parse_case_file(&ok.replace(
                "probes = []",
                "probes = [{ kind = \"add-field\", type = \"Rect\", more = 1 }]"
            ))
            .is_err()
        );
        let with_probe = ok.replace(
            "probes = []",
            "probes = [{ kind = \"add-field\", type = \"Rect\" }]",
        );
        assert_eq!(parse_case_file(&with_probe).unwrap().probes[0].ty, "Rect");
        assert!(parse_case_file(&ok.replace("depth = 1\n", "")).is_err());
    }

    #[test]
    fn triggers_follow_the_decided_numbers() {
        let mut r = row("a", true, true);
        r.wall_ms = TRIGGER_DECISION_MEDIAN_MS + 1;
        r.check_ms = vec![10, 20, 30];
        let lines = trigger_lines(
            &[r],
            RunInfo {
                beam: 5,
                harness_ms: 5,
                budget: Some(DEFAULT_SEARCH_BUDGET),
                rss_kb: Some(1),
                ..RunInfo::default()
            },
        );
        assert!(lines[0].contains("FIRED"), "{lines:?}");
        assert!(
            lines[1].contains("20 ms") && lines[1].contains("not fired"),
            "{lines:?}"
        );
        assert!(lines[2].contains("not measured"), "{lines:?}");
        assert!(lines[3].contains("not fired"), "{lines:?}");
    }

    #[test]
    fn trigger_4_reads_the_search_time_of_a_default_run() {
        let line = |harness_ms: u64, outer_check_ms: u64, budget: Option<SearchBudget>| {
            let lines = trigger_lines(
                &[row("a", true, true)],
                RunInfo {
                    harness_ms,
                    outer_check_ms,
                    budget,
                    ..RunInfo::default()
                },
            );
            assert!(lines[3].starts_with("trigger 4, "), "{lines:?}");
            lines[3].clone()
        };
        let default = Some(DEFAULT_SEARCH_BUDGET);
        let at = line(TRIGGER_HARNESS_MS, 0, default);
        assert!(
            at.ends_with("(not fired)"),
            "the limit itself does not fire: {at}"
        );
        let over = line(TRIGGER_HARNESS_MS + 1, 0, default);
        assert!(over.ends_with("(FIRED)"), "one ms above it fires: {over}");
        assert!(
            over.contains(&format!("{} ms (outer check 0 ms", TRIGGER_HARNESS_MS + 1)),
            "{over}"
        );
        // Run 3 of SEARCH_LEDGER "ryo-rs (large run)": over the limit only by its outer check.
        let outer = line(983_033, 634_965, default);
        assert!(
            outer.ends_with("(not fired)"),
            "outer check is excluded: {outer}"
        );
        assert!(
            outer.contains(": 348068 ms (outer check 634965 ms, whole run 983033 ms)"),
            "{outer}"
        );
        let outer_over = line(TRIGGER_HARNESS_MS + 1 + 5, 5, default);
        assert!(outer_over.ends_with("(FIRED)"), "{outer_over}");
        let mut narrow = DEFAULT_SEARCH_BUDGET;
        narrow.beam = 5;
        assert!(line(TRIGGER_HARNESS_MS + 1, 0, Some(narrow)).ends_with("(FIRED)"));
        let mut exhaustive = DEFAULT_SEARCH_BUDGET;
        exhaustive.beam = 200;
        exhaustive.soft_ms = u64::MAX;
        exhaustive.hard_ms = u64::MAX;
        let wide = line(TRIGGER_HARNESS_MS + 1, 0, Some(exhaustive));
        assert!(
            wide.ends_with("(not measured: budget beyond the default)"),
            "an exhaustive run is not judged: {wide}"
        );
        let skewed = line(5, 9, default);
        assert!(
            skewed.contains(": 0 ms (outer check 9 ms, whole run 5 ms) (not fired)"),
            "an outer time above the whole saturates to 0: {skewed}"
        );
        let unknown = line(TRIGGER_HARNESS_MS + 1, 0, None);
        assert!(
            unknown.ends_with("(not measured: budget unknown)"),
            "{unknown}"
        );
        let json = serde_json::to_value(Report::of(
            &[row("a", true, true)],
            RunInfo {
                harness_ms: 983_033,
                outer_check_ms: 634_965,
                budget: default,
                ..RunInfo::default()
            },
            None,
        ))
        .unwrap();
        assert_eq!(json["search_ms"], 348_068);
        assert_eq!(json["outer_check_ms"], 634_965);
        assert_eq!(json["harness_ms"], 983_033);
    }

    #[test]
    fn run_info_of_a_run_carries_budget_and_outer_time() {
        use std::time::Duration;
        let mut b = DEFAULT_SEARCH_BUDGET;
        b.beam = 200;
        let t = RunInfo::of_run(
            b,
            Duration::from_micros(983_033_999),
            Duration::from_millis(634_965),
            OuterSample::Top(NonZeroUsize::new(4).unwrap()),
            Some(7),
            (Backend::Engine, 4),
        );
        assert_eq!(
            t,
            RunInfo {
                beam: 200,
                harness_ms: 983_033,
                outer_check_ms: 634_965,
                outer_sample: OuterSample::Top(NonZeroUsize::new(4).unwrap()),
                budget: Some(b),
                rss_kb: Some(7),
                backend: Backend::Engine,
                width: 4,
            }
        );
        assert_eq!(t.search_ms(), 348_068);
        let flat = RunInfo::of_run(
            b,
            Duration::MAX,
            Duration::ZERO,
            OuterSample::Off,
            None,
            (Backend::UndoLog, 1),
        );
        assert_eq!((flat.backend, flat.width), (Backend::UndoLog, 1));
        assert_eq!(flat.harness_ms, u64::MAX);
    }

    #[test]
    fn report_names_the_outer_sample() {
        let rows = [row("a", true, true)];
        let off = RunInfo::default();
        assert_eq!(
            serde_json::to_value(Report::of(&rows, off, None)).unwrap()["outer_sample"],
            Value::Null
        );
        assert!(!markdown(&rows, off).contains("outer sample"));
        let all = RunInfo {
            outer_sample: OuterSample::All,
            ..RunInfo::default()
        };
        assert_eq!(
            serde_json::to_value(Report::of(&rows, all, None)).unwrap()["outer_sample"],
            "all"
        );
        assert!(markdown(&rows, all).contains("- outer sample: all\n"));
        let three = RunInfo {
            outer_sample: OuterSample::Top(NonZeroUsize::new(3).unwrap()),
            ..RunInfo::default()
        };
        assert_eq!(
            serde_json::to_value(Report::of(&rows, three, None)).unwrap()["outer_sample"],
            3
        );
        assert!(markdown(&rows, three).contains("- outer sample: top 3 of each decision\n"));
    }

    /// The seven keys a row has beyond the 18 of the hand-built row: five per depth, and the
    /// children timed out and the stops (stage 4), each empty for a row that was not measured.
    const DEPTH_KEYS: [&str; 7] = [
        "depth_check_ms",
        "depth_expand_ms",
        "depth_checked",
        "depth_parents",
        "depth_writes",
        "timed_out",
        "stops",
    ];

    #[test]
    fn row_json_carries_the_level_writes() {
        let (p, c) = (planted(), clean());
        let mut r = report(vec![entry(
            1,
            Fate::Passed,
            Some(0),
            Some(vec![Edit::write(rel("a/src/lib.rs"), CLEAN)]),
            1,
        )]);
        r.depths = vec![
            DepthStats {
                depth: 1,
                writes: 5,
                ..DepthStats::default()
            },
            DepthStats {
                depth: 2,
                writes: 17,
                ..DepthStats::default()
            },
        ];
        r.trace.push(entry(2, Fate::TimedOut, None, None, 1));
        r.memory_stopped = true;
        let row = analyze(&case(), &truth(&p, &c), &r);
        assert_eq!(row.depth_writes, vec![5, 17]);
        assert_eq!(row.timed_out, vec![r.trace[1].blueprint.to_string()]);
        assert_eq!(row.stops, vec!["memory_stopped".to_owned()]);
        let json = serde_json::to_value(&row).unwrap();
        assert_eq!(json["depth_writes"], json!([5, 17]));
    }

    #[test]
    fn row_json_carries_the_depth_times() {
        let (p, c) = (planted(), clean());
        let mut r = report(vec![entry(
            1,
            Fate::Passed,
            Some(0),
            Some(vec![Edit::write(rel("a/src/lib.rs"), CLEAN)]),
            1,
        )]);
        r.depths = vec![
            DepthStats {
                depth: 1,
                parents: 1,
                generated: 9,
                kept: 4,
                checked: 3,
                expand_ms: 11,
                check_ms: 120,
                ..DepthStats::default()
            },
            DepthStats {
                depth: 2,
                parents: 4,
                generated: 30,
                kept: 6,
                checked: 5,
                expand_ms: 31,
                check_ms: 340,
                ..DepthStats::default()
            },
        ];
        let row = analyze(&case(), &truth(&p, &c), &r);
        assert_eq!(row.depth_check_ms, vec![120, 340]);
        assert_eq!(row.depth_expand_ms, vec![11, 31]);
        assert_eq!(row.depth_checked, vec![3, 5]);
        assert_eq!(row.depth_parents, vec![1, 4]);
        let json = serde_json::to_value(&row).unwrap();
        assert_eq!(json["depth_check_ms"], json!([120, 340]));
        assert_eq!(json["depth_expand_ms"], json!([11, 31]));
        assert_eq!(json["depth_checked"], json!([3, 5]));
        assert_eq!(json["depth_parents"], json!([1, 4]));
        // A row that was not measured has no depth.
        let skipped = serde_json::to_value(DecisionRow::skipped(&case(), "why")).unwrap();
        for key in DEPTH_KEYS {
            assert_eq!(skipped[key], json!([]), "{key}");
        }
    }

    #[test]
    fn row_json_keeps_its_keys_and_values() {
        let row = DecisionRow {
            id: "c1".into(),
            ws: "lab".into(),
            kinds: vec![PlantKind::ObscureName, PlantKind::InlineFunction],
            depth: 2,
            skipped: None,
            generated: true,
            hit: true,
            best_rank: Some(3),
            fate: Some("passed".into()),
            top3: true,
            final_rank: None,
            decided: true,
            candidates: vec![12, 7],
            kept: vec![5, 4],
            noul_audited: 2,
            noul_audited_passed: 1,
            wall_ms: 1234,
            check_ms: vec![40, 50, 60],
            depth_check_ms: vec![90, 120],
            depth_expand_ms: vec![10, 20],
            depth_checked: vec![3, 4],
            depth_parents: vec![1, 2],
            depth_writes: vec![2, 8],
            timed_out: vec!["b".into()],
            stops: vec!["budget_exhausted".into()],
        };
        let mut got = serde_json::to_value(&row).unwrap();
        assert_eq!(got["timed_out"], json!(["b"]));
        assert_eq!(got["stops"], json!(["budget_exhausted"]));
        let object = got.as_object_mut().unwrap();
        // The backend is the report's key, not the row's.
        assert!(!object.contains_key("backend"));
        for key in DEPTH_KEYS {
            assert!(object.remove(key).is_some(), "the row has {key}");
        }
        assert_eq!(
            got,
            json!({
                "id": "c1",
                "ws": "lab",
                "kinds": ["obscure-name", "inline-function"],
                "depth": 2,
                "skipped": null,
                "generated": true,
                "hit": true,
                "best_rank": 3,
                "fate": "passed",
                "top3": true,
                "final_rank": null,
                "decided": true,
                "candidates": [12, 7],
                "kept": [5, 4],
                "noul_audited": 2,
                "noul_audited_passed": 1,
                "wall_ms": 1234,
                "check_ms": [40, 50, 60],
            })
        );
        // The other shape of every option: a row that was not measured.
        let mut got = serde_json::to_value(DecisionRow::skipped(&case(), "why")).unwrap();
        let object = got.as_object_mut().unwrap();
        for key in DEPTH_KEYS {
            assert!(object.remove(key).is_some(), "the row has {key}");
        }
        assert_eq!(
            got,
            json!({
                "id": "c",
                "ws": "lab",
                "kinds": ["obscure-name"],
                "depth": 1,
                "skipped": "why",
                "generated": false,
                "hit": false,
                "best_rank": null,
                "fate": null,
                "top3": false,
                "final_rank": null,
                "decided": false,
                "candidates": [],
                "kept": [],
                "noul_audited": 0,
                "noul_audited_passed": 0,
                "wall_ms": 0,
                "check_ms": [],
            })
        );
    }

    #[test]
    fn report_json_keeps_its_keys_and_values() {
        let rows = [row("a", true, true), DecisionRow::skipped(&case(), "why")];
        let mut panics = ContainedPanics::default();
        panics.absorb(
            "ws1",
            &[DetectorPanic {
                detector: GapDetector::DuplicateName,
                path: rel("src/a.rs"),
                message: "boom\nmore".to_owned(),
            }],
            &[
                ExpanderPanic {
                    kind: Some(CommandKind::ExtractVariable),
                    path: None,
                    message: "bang".to_owned(),
                },
                ExpanderPanic {
                    kind: None,
                    path: Some(rel("src/b.rs")),
                    message: "bing".to_owned(),
                },
            ],
        );
        let info = |outer_sample| RunInfo {
            harness_ms: 1000,
            outer_check_ms: 400,
            outer_sample,
            rss_kb: Some(777),
            // Named, not the default (the Engine, D11 stage 4): the run below on the Engine
            // differs from this one in the key backend alone.
            backend: Backend::UndoLog,
            ..RunInfo::default()
        };
        let all = info(OuterSample::All);
        let expected = json!({
            "backend": "undo-log",
            "width": 0,
            "decisions": 1,
            "skipped": 1,
            "generated": 1,
            "hit": 1,
            "beam": RECALL_K,
            "recall_at_beam": 1.0,
            "hit_over_all": 1.0,
            "generation_rate": 1.0,
            "top3_rate": 0.0,
            "verdict": "insufficient",
            "harness_ms": 1000,
            "outer_check_ms": 400,
            "outer_sample": "all",
            "search_ms": 600,
            "rss_kb": 777,
            "triggers": trigger_lines(&rows, all),
            "rows": [
                serde_json::to_value(&rows[0]).unwrap(),
                serde_json::to_value(&rows[1]).unwrap(),
            ],
            "contained_panics": {
                "detector": [{"ws": "ws1", "path": "src/a.rs", "message": "boom\nmore"}],
                "expander": [
                    {"ws": "ws1", "kind": null, "path": "src/b.rs", "message": "bing"},
                    {"ws": "ws1", "kind": "extract-variable", "path": null, "message": "bang"},
                ],
            },
        });
        let got = serde_json::to_value(Report::of(&rows, all, Some(&panics))).unwrap();
        assert_eq!(got, expected);
        // Without panics the key is absent; the other values are the same.
        let mut without = expected;
        without.as_object_mut().unwrap().remove("contained_panics");
        let got = serde_json::to_value(Report::of(&rows, all, None)).unwrap();
        assert_eq!(got, without);
        // The three shapes of `outer_sample`.
        let shape = |sample| {
            serde_json::to_value(Report::of(&rows, info(sample), None)).unwrap()["outer_sample"]
                .clone()
        };
        assert_eq!(shape(OuterSample::Off), Value::Null);
        assert_eq!(shape(OuterSample::All), json!("all"));
        assert_eq!(
            shape(OuterSample::Top(NonZeroUsize::new(3).unwrap())),
            json!(3)
        );
        // A run without a known memory size says null.
        let unknown = RunInfo {
            rss_kb: None,
            ..all
        };
        let got = serde_json::to_value(Report::of(&rows, unknown, None)).unwrap();
        assert_eq!(got["rss_kb"], Value::Null);
        // The backend is the first key, and a run on the Engine differs from the same run on
        // the undo log in that key alone.
        let text = serde_json::to_string(&Report::of(&rows, all, None)).unwrap();
        assert!(text.starts_with(r#"{"backend":"undo-log","#), "{text}");
        let engine = RunInfo {
            backend: Backend::Engine,
            ..all
        };
        let mut got = serde_json::to_value(Report::of(&rows, engine, None)).unwrap();
        assert_eq!(got["backend"], json!("engine"));
        got["backend"] = json!("undo-log");
        assert_eq!(got, without);
    }

    #[test]
    fn outer_sample_takes_the_top_of_the_ranking() {
        let top = |n| OuterSample::Top(NonZeroUsize::new(n).unwrap());
        let ranked = [10, 20, 30, 40, 50];
        assert!(outer_sample(&ranked, OuterSample::Off).is_empty());
        assert_eq!(outer_sample(&ranked, OuterSample::All), &ranked);
        assert_eq!(outer_sample(&ranked, top(2)), &[10, 20]);
        assert_eq!(outer_sample(&ranked, top(5)), &ranked);
        assert_eq!(outer_sample(&ranked, top(9)), &ranked);
        let none: [u8; 0] = [];
        assert!(outer_sample(&none, top(3)).is_empty());
    }

    #[test]
    fn outer_sample_zero_is_refused() {
        assert!(parse_outer_sample("0").is_err());
        assert!(parse_outer_sample("-1").is_err());
        assert!(parse_outer_sample("x").is_err());
        assert_eq!(parse_outer_sample("1"), Ok(NonZeroUsize::MIN));
        assert_eq!(parse_outer_sample("12"), Ok(NonZeroUsize::new(12).unwrap()));
    }

    #[test]
    fn trigger_4_judges_runs_within_the_default_budget() {
        let d = DEFAULT_SEARCH_BUDGET;
        assert!(trigger_4_judges(&d));
        let mut depth = d;
        depth.max_depth = u8::MAX;
        assert!(trigger_4_judges(&depth), "max_depth is set per case");
        let mut audit = d;
        audit.audit_every = std::num::NonZeroU16::MAX;
        assert!(trigger_4_judges(&audit), "audit_every costs no time");
        audit.audit_every = std::num::NonZeroU16::MIN;
        assert!(trigger_4_judges(&audit), "audit_every costs no time");
        let narrow = SearchBudget {
            beam: 1,
            max_expand: 1,
            beam_parents: 1,
            per_candidate_ms: 1,
            soft_ms: 1,
            hard_ms: 1,
            ..d
        };
        assert!(trigger_4_judges(&narrow));
        let widen: [fn(&mut SearchBudget); 6] = [
            |b| b.beam += 1,
            |b| b.max_expand += 1,
            |b| b.beam_parents += 1,
            |b| b.per_candidate_ms += 1,
            |b| b.soft_ms += 1,
            |b| b.hard_ms += 1,
        ];
        for (i, f) in widen.iter().enumerate() {
            let mut b = d;
            f(&mut b);
            assert!(!trigger_4_judges(&b), "knob {i} above the default");
        }
    }

    #[test]
    fn trigger_3_reads_the_growth_limit() {
        let n = TRIGGER_AFTER_CHECKS as usize;
        let at_limit = 1_000 * TRIGGER_CHECK_GROWTH_PERMILLE / 1_000;
        let line = |last: u64, len: usize| {
            let mut r = row("a", true, true);
            let mut checks = vec![1_000; len];
            for c in &mut checks[len - GROWTH_WINDOW..] {
                *c = last;
            }
            r.check_ms = checks;
            let lines = trigger_lines(
                &[r],
                RunInfo {
                    beam: 5,
                    harness_ms: 5,
                    rss_kb: Some(1),
                    ..RunInfo::default()
                },
            );
            let growth = lines[2]
                .rsplit_once("check time ")
                .map_or(String::new(), |(_, rest)| rest.to_string());
            (growth, lines)
        };
        let (growth, lines) = line(at_limit, n);
        assert!(growth.contains("not fired"), "{lines:?}");
        let (growth, lines) = line(at_limit + 1, n);
        assert!(growth.contains("FIRED"), "{lines:?}");
        let (growth, lines) = line(at_limit + 1, n - 1);
        assert!(growth.contains("not measured"), "{lines:?}");
    }

    #[test]
    fn check_growth_compares_the_ends_of_the_run() {
        assert_eq!(check_growth_permille(&vec![10; 999]), None);
        assert_eq!(check_growth_permille(&vec![10; 1_000]), Some(1_000));
        let mut zero_first = vec![0; 1_000];
        for c in &mut zero_first[900..] {
            *c = 5;
        }
        assert_eq!(check_growth_permille(&zero_first), Some(5_000));
        let mut slow_middle = vec![10; 1_000];
        for c in &mut slow_middle[100..900] {
            *c = 99_999;
        }
        assert_eq!(check_growth_permille(&slow_middle), Some(1_000));
        let mut one_off = vec![10; 1_000];
        one_off[99] = 7;
        one_off[900] = 3;
        assert_eq!(check_growth_permille(&one_off), Some(1_000));
        let mut grown = vec![10; 1_000];
        for c in &mut grown[900..] {
            *c = 20;
        }
        assert_eq!(check_growth_permille(&grown), Some(2_000));
    }
}
