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
//! Adopt at recall >= [`ADOPT_AT`], discard below [`DISCARD_BELOW`], hold between, and say
//! nothing below [`MIN_DECISIONS`] compared decisions. With 0 misses in 30 the true miss rate
//! is at most 0.1 at 95 % (the rule of three).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use ryo_v2_core::app::ports::{Assessor, CheckError, Clock, Expander, Judge, RevertAudit};
use ryo_v2_core::app::search::{
    Fate, SearchError, SearchInput, SearchReport, TRIGGER_AFTER_CHECKS,
    TRIGGER_CHECK_GROWTH_PERMILLE, TRIGGER_CHECK_MEDIAN_MS, TRIGGER_DECISION_MEDIAN_MS,
    TRIGGER_HARNESS_MS, TRIGGER_RSS_KB, TraceEntry, search_decision,
};
use ryo_v2_core::domain::{
    ChangeProbe, DEFAULT_WEIGHTS, DomainError, Edit, Ident, PruneStage, RelPath, Resolution,
    RuleCatalog, Scope, SearchBudget, Stage, Target, TextRange, Verdict,
};
use ryo_v2_core::infra::overlay::{OverlayError, overlay_edits};
use serde_json::{Value, json};

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

/// One decision, measured or skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionRow {
    /// The case id.
    pub id: String,
    /// The base workspace.
    pub ws: String,
    /// The kinds planted.
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
        }
    }

    /// The row as JSON.
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "ws": self.ws,
            "kinds": self.kinds.iter().map(|k| k.id()).collect::<Vec<_>>(),
            "depth": self.depth,
            "skipped": self.skipped,
            "generated": self.generated,
            "hit": self.hit,
            "best_rank": self.best_rank,
            "fate": self.fate,
            "top3": self.top3,
            "final_rank": self.final_rank,
            "decided": self.decided,
            "candidates": self.candidates,
            "kept": self.kept,
            "noul_audited": self.noul_audited,
            "noul_audited_passed": self.noul_audited_passed,
            "wall_ms": self.wall_ms,
            "check_ms": self.check_ms,
        })
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
    /// The edits of each `Decided` candidate (at most `OUTER_CHECK_MAX`), whole planted state
    /// included; the outer `cargo check` of the harness reads them.
    pub decided_edits: Vec<Vec<Edit>>,
}

/// Runs one planted case on a loaded session.
///
/// The planted state is checked first; one that does not verify is skipped (the harness cannot
/// ask a search to improve code that does not compile). Then `search_decision` runs from the
/// planted edits with no roots, at `case.depth`, and the trace is read with [`analyze`].
/// `NoCandidates` is a measured decision that generated nothing; any other search failure is an
/// error, after which the session must be reloaded.
#[allow(clippy::too_many_arguments)]
pub fn run_case<V, E, A, J>(
    v: &V,
    e: &E,
    a: &A,
    j: &J,
    clock: &dyn Clock,
    s: &mut V::Session,
    case: &PlantedCase,
    budget: SearchBudget,
) -> Result<CaseRun, RecallError>
where
    V: RevertAudit,
    E: Expander<Session = V::Session>,
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
    let checked = v
        .check(s, &edits, Scope::Affected)
        .map_err(RecallError::Check)?;
    let verdict = Verdict::from_baseline(&v.baseline(s), &checked);
    if !verdict.passed() {
        return Ok(skipped(format!(
            "planted state does not verify: {} new errors, {} unmapped",
            verdict.new_errors.errors.len(),
            verdict.new_errors.unmapped.len()
        )));
    }
    let mut budget = budget;
    budget.max_depth = case.depth.max(1);
    let rules = RuleCatalog::default();
    let input = SearchInput {
        start: &edits,
        roots: &[],
        targets: &case.targets,
        probes: &case.probes,
        rules: &rules,
        scope: Scope::Affected,
        budget,
        weights: DEFAULT_WEIGHTS,
        keep_trace_edits: true,
    };
    let t0 = clock.now_ms();
    let report = match search_decision(v, e, a, j, clock, s, &input) {
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunInfo {
    /// The beam of the search: the "k" of recall@k. 0 reads as [`RECALL_K`].
    pub beam: u16,
    /// Wall time of the whole run in ms.
    pub harness_ms: u64,
    /// Resident set size at the end of the run in kB, when known.
    pub rss_kb: Option<u64>,
}

impl RunInfo {
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
            "trigger 4, one run > {TRIGGER_HARNESS_MS} ms: {} ms ({})",
            t.harness_ms,
            status(Some(t.harness_ms > TRIGGER_HARNESS_MS))
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
    out
}

/// The text printed to the terminal: the tables of [`markdown`] plus the skipped reasons.
pub fn terminal(rows: &[DecisionRow], t: RunInfo) -> String {
    markdown(rows, t)
}

/// The whole run as JSON.
pub fn report_json(rows: &[DecisionRow], t: RunInfo) -> Value {
    let all = Counts::of(rows);
    json!({
        "decisions": all.measured,
        "skipped": all.skipped,
        "generated": all.generated,
        "hit": all.hit,
        "beam": t.k(),
        "recall_at_beam": all.recall(),
        "hit_over_all": all.recall_of_all(),
        "generation_rate": all.generation_rate(),
        "top3_rate": all.top3_rate(),
        "verdict": verdict(all.generated, all.recall()).id(),
        "harness_ms": t.harness_ms,
        "rss_kb": t.rss_kb,
        "triggers": trigger_lines(rows, t),
        "rows": rows.iter().map(DecisionRow::to_json).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryo_v2_core::app::search::{DepthStats, RankedNode};
    use ryo_v2_core::domain::{
        BlueprintId, Fingerprint, Infeasible, IntentKind, Prior, ProposalId, Reward, SearchTree,
        StateFp,
    };

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
            intent: IntentKind::RenameLocal,
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
                rss_kb: Some(1),
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
