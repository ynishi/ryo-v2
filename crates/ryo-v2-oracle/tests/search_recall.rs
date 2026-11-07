//! One planted case of `corpus/search/base/search-lab` through the real language server: the
//! planted state verifies, the search runs, and the row names what became of the best change.
//! Proc macros are off and the beam is 5, so it takes seconds. The full table is
//! `just search-recall`. Before the run it checks the probes of every committed case of every
//! workspace under `corpus/search` (each probe names a type of its workspace). A second test
//! compares every verdict the search forms on its forks with the reference check of the same
//! edits, and a third ties the reference check to a fresh load of the state it checks.

use std::collections::BTreeMap;
use std::sync::Arc;

use ryo_v2_core::app::memory::MemoryBudget;
use ryo_v2_core::app::ports::{Analyzer, CheckReach, Verifier};
use ryo_v2_core::app::search::{
    ASSERT_REVERT_EVERY, DEFAULT_SEARCH_BUDGET, Fate, SEARCH_FORKS, SearchError, SearchInput,
    SearchReport, search_decision,
};
use ryo_v2_core::app::verdict_of;
use ryo_v2_core::domain::{
    Blocking, BlueprintId, ChangeProbe, DEFAULT_WEIGHTS, Ident, Resolution, Scope, Verdict,
};
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::judge::MetricJudge;
use ryo_v2_core::infra::memory::ProcGauge;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_oracle::materials::Materials;
use ryo_v2_oracle::search_recall::{PlantedCase, RunInfo, markdown, run_case};
use ryo_v2_ra::{ForksConfig, RaAnalyzer, RaConfig, RaSession, SyntaxAssessor};
use std::num::{NonZeroU32, NonZeroUsize};

#[test]
fn one_planted_case_runs_end_to_end() {
    let m = Materials::find().expect("materials");
    let root = m.corpus.join("search/base/search-lab");
    let cases = PlantedCase::load_all(&root, &m.corpus).expect("load cases");
    let case = cases
        .iter()
        .find(|c| c.id == "obscure-name-0")
        .expect("obscure-name-0 is a case of search-lab");
    assert_eq!(case.depth, 1);
    assert_eq!(
        case.probes,
        vec![
            ChangeProbe::AddVariant {
                ty: Ident::parse("Shape").unwrap()
            },
            ChangeProbe::AddField {
                ty: Ident::parse("Report").unwrap()
            },
        ],
        "the probes of the case file reach the case"
    );
    let every = PlantedCase::load_all(&m.corpus.join("search"), &m.corpus)
        .expect("load every case under corpus/search");
    let with_cases = workspaces_with_cases(&m.corpus.join("search"));
    let names: Vec<&str> = with_cases
        .iter()
        .filter_map(|d| d.file_name().and_then(|n| n.to_str()))
        .collect();
    for lab in ["search-lab", "breakage-lab", "gen-lab"] {
        assert!(names.contains(&lab), "{lab} has cases: {with_cases:?}");
    }
    let files: usize = with_cases
        .iter()
        .map(|d| {
            std::fs::read_dir(d.join("cases"))
                .expect("read cases dir")
                .filter(|e| {
                    e.as_ref()
                        .is_ok_and(|e| e.path().extension().is_some_and(|x| x == "toml"))
                })
                .count()
        })
        .sum();
    assert_eq!(
        every.len(),
        files,
        "the probe check loads every case file under corpus/search"
    );
    for c in &every {
        assert!(
            !c.probes.is_empty(),
            "every committed case carries probes: {} has none",
            c.id
        );
        let src = workspace_sources(&c.workspace);
        for p in &c.probes {
            let (item, ty) = match p {
                ChangeProbe::AddVariant { ty } => ("enum", ty),
                ChangeProbe::AddField { ty } => ("struct", ty),
                ChangeProbe::AddMethod { ty } => ("trait", ty),
            };
            assert!(
                src.contains(&format!("pub {item} {} ", ty.as_str())),
                "probe {p:?} of {} names no `{item}` of its workspace",
                c.id
            );
        }
    }

    let analyzer = RaAnalyzer::new(
        RaConfig {
            proc_macro: false,
            threads: None,
            assert_revert: false,
            gap_detectors: true,
            assert_revert_every: NonZeroU32::new(ASSERT_REVERT_EVERY),
            cfg_test: true,
            ..Default::default()
        },
        Arc::new(MonotonicClock::new()),
        Arc::new(RecordingObserver::default()),
    )
    .with_check_timeout(Some(DEFAULT_SEARCH_BUDGET.per_candidate_ms));
    let mut session = analyzer.load(&case.workspace).expect("load search-lab");
    let mut budget = DEFAULT_SEARCH_BUDGET;
    budget.beam = 5;
    let clock = MonotonicClock::new();
    let run = run_case(
        &analyzer,
        &SyntaxAssessor,
        &MetricJudge,
        &clock,
        (&ProcGauge, MemoryBudget::unbounded()),
        &mut session,
        case,
        budget,
    )
    .expect("run the case");
    let row = run.row;
    println!(
        "{}",
        markdown(std::slice::from_ref(&row), RunInfo::default())
    );
    assert!(
        session.detector_panics().is_empty() && session.expander_panics().is_empty(),
        "no panic was contained: {:?} {:?}",
        session.detector_panics(),
        session.expander_panics()
    );
    assert_eq!(row.skipped, None, "the planted state verifies: {row:?}");
    assert!(
        !row.candidates.is_empty() && row.candidates[0] > 0,
        "the expander proposes children of the planted state: {row:?}"
    );
    assert!(row.generated, "the restore is among the children: {row:?}");
    assert!(
        row.hit,
        "and it survives a beam of 5 (fate {:?}): {row:?}",
        row.fate
    );
}

/// The verdict the search formed for each Blueprint it checked: the evidence of a decided search,
/// or the blocking of a failed one (a failed search has no passing node).
fn search_verdicts(r: &SearchReport) -> BTreeMap<BlueprintId, Verdict> {
    match &r.resolution {
        Resolution::Decided { evidence, .. } => evidence.verdicts().iter().cloned().collect(),
        Resolution::Failed(i) => i
            .blocking()
            .iter()
            .filter_map(|b| match b {
                Blocking::NewErrors { blueprint, verdict } => Some((*blueprint, verdict.clone())),
                _ => None,
            })
            .collect(),
        Resolution::Stopped { .. } => BTreeMap::new(),
    }
}

/// Every node a search checked has the verdict `app::verdict_of` gives for its composed edits,
/// on the same session after the forks are closed: the same new errors and unseen classes, and
/// its fate follows that verdict. The depth-2 cases of search-lab, which run every depth on its
/// parent's Layer, proc macros off, beam 5, the base's isolation sampled at every job, every
/// reference check re-asserting its level, no time limit.
///
/// The case list bounds its time. With the first case of gen-lab, shape-lab and
/// breakage-lab as well it took about 20 s, against about 3 s for the test above; each of those
/// labs added about 3.4 s, mostly its load. The ten depth-2 cases take about 9 s per width.
///
/// It runs the search on the forks of the fork Engine (D11, stage 4) at width 1 and at width 4
/// (`SEARCH_FORKS`); the flat check is the reference check (`RaAnalyzer::reference_check`, which
/// runs no layer code).
#[test]
fn search_verdicts_equal_the_reference_check() {
    for width in [1, SEARCH_FORKS] {
        println!("width: {width}");
        search_verdicts_at(width);
    }
}

fn search_verdicts_at(width: usize) {
    let m = Materials::find().expect("materials");
    let cases: Vec<PlantedCase> =
        PlantedCase::load_all(&m.corpus.join("search/base/search-lab"), &m.corpus)
            .expect("load the cases of search-lab")
            .into_iter()
            .filter(|c| c.depth == 2 && c.skip.is_none())
            .collect();
    assert_eq!(cases.len(), 10, "the depth-2 cases of search-lab");

    let analyzer = reference_analyzer(width);
    let mut budget = DEFAULT_SEARCH_BUDGET;
    budget.beam = 5;
    budget.soft_ms = u64::MAX;
    budget.hard_ms = u64::MAX;
    let clock = MonotonicClock::new();
    let mut nodes = 0;
    let mut loaded: Option<(std::path::PathBuf, RaSession)> = None;
    for case in &cases {
        if loaded.as_ref().is_none_or(|(ws, _)| *ws != case.workspace) {
            let s = analyzer
                .load(&case.workspace)
                .unwrap_or_else(|e| panic!("{}: load: {e}", case.id));
            loaded = Some((case.workspace.clone(), s));
        }
        let Some((_, s)) = loaded.as_mut() else {
            unreachable!("loaded above");
        };
        let edits = case.edits().expect("the planted edits");
        let baseline = Verifier::baseline(&analyzer, s);
        let planted = verdict_of(&analyzer, s, &edits, Scope::Affected, &baseline)
            .unwrap_or_else(|e| panic!("{}: the planted check: {e}", case.id));
        assert!(planted.passed(), "{}: the planted state verifies", case.id);
        let mut budget = budget;
        budget.max_depth = case.depth.max(1);
        let input = SearchInput {
            start: &edits,
            roots: &[],
            targets: &case.targets,
            probes: &case.probes,
            scope: Scope::Affected,
            budget,
            memory: MemoryBudget::unbounded(),
            weights: DEFAULT_WEIGHTS,
            keep_trace_edits: true,
        };
        let r = match search_decision(
            &analyzer,
            &SyntaxAssessor,
            &MetricJudge,
            &clock,
            &ProcGauge,
            s,
            &input,
        ) {
            Ok(r) => r,
            Err(SearchError::NoCandidates) => continue,
            Err(e) => panic!("{}: the search: {e}", case.id),
        };
        assert!(!r.budget_exhausted && !r.soft_stopped, "{}", case.id);
        let by_search = search_verdicts(&r);
        for t in r.trace.iter().filter(|t| t.node.is_some()) {
            let at = format!("{}: {} at depth {}", case.id, t.blueprint, t.depth);
            let edits = t.edits.as_ref().expect("the trace keeps the edits");
            let flat = analyzer
                .reference_check(s, edits, Scope::Affected)
                .map(|checked| Verdict::of_check(&baseline, &checked));
            match (&t.fate, flat) {
                (Fate::Passed | Fate::Failed { .. }, Ok(flat)) => {
                    let stacked = by_search
                        .get(&t.blueprint)
                        .unwrap_or_else(|| panic!("{at}: the search's verdict"));
                    assert_eq!(stacked.new_errors, flat.new_errors, "{at}: new errors");
                    assert_eq!(stacked.unseen, flat.unseen, "{at}: unseen classes");
                    let fate = if flat.passed() {
                        Fate::Passed
                    } else {
                        Fate::Failed {
                            new_errors: flat.new_errors.len(),
                        }
                    };
                    assert_eq!(t.fate, fate, "{at}: the fate");
                }
                // A diagnosis the search could not finish fails the flat check for the same
                // Blueprint reason.
                (Fate::Blocked(_), Err(e)) => {
                    assert_eq!(e.reach(), CheckReach::Blueprint, "{at}: {e}");
                }
                (fate, flat) => panic!("{at}: fate {fate:?}, the flat check {flat:?}"),
            }
            nodes += 1;
        }
    }
    println!("{nodes} nodes of {} cases compared", cases.len());
    assert!(nodes > 0, "some node was checked");
}

/// The environment variable that points the fresh-load anchor at another directory of planted
/// cases (the ryo-rs decision corpus of SEARCH_LEDGER "Large runs", step 1) instead of the
/// depth-2 cases of search-lab; it is read by that test only.
const ANCHOR_CASES: &str = "RYO_V2_ANCHOR_CASES";

/// The anchor of the reference check (R1, D11 stage 4): `RaAnalyzer::reference_check` of
/// a state gives what a fresh load of that state gives, so the flat side of every differential
/// test is itself tied to rust-analyzer's own load. For each depth-2 case of search-lab, two
/// states: the planted state, and the state of one node the search checked (a failed one when
/// there is one). Each state's reference check, at `Scope::Workspace` on the shared session of
/// the base, must name the files and the findings of a fresh session loaded from a copy of the
/// workspace with the state written to disk. The reference checks run with the re-assert of
/// their level (`assert_revert`), as every reference check of the oracle does.
///
/// With `RYO_V2_ANCHOR_CASES=<dir>` it runs every case under `<dir>` instead (the run on ryo-rs,
/// one of SEARCH_LEDGER's large runs; a release build, and minutes per case).
#[test]
fn the_reference_check_is_a_fresh_load() {
    let m = Materials::find().expect("materials");
    let cases: Vec<PlantedCase> = match std::env::var_os(ANCHOR_CASES) {
        Some(dir) => PlantedCase::load_all(std::path::Path::new(&dir), &m.corpus)
            .expect("load the cases of RYO_V2_ANCHOR_CASES")
            .into_iter()
            .filter(|c| c.skip.is_none())
            .collect(),
        None => {
            let cases: Vec<PlantedCase> =
                PlantedCase::load_all(&m.corpus.join("search/base/search-lab"), &m.corpus)
                    .expect("load the cases of search-lab")
                    .into_iter()
                    .filter(|c| c.depth == 2 && c.skip.is_none())
                    .collect();
            assert_eq!(cases.len(), 10, "the depth-2 cases of search-lab");
            cases
        }
    };
    assert!(!cases.is_empty(), "some case to anchor");

    let analyzer = reference_analyzer(SEARCH_FORKS);
    let mut budget = DEFAULT_SEARCH_BUDGET;
    budget.beam = 5;
    budget.max_depth = 1;
    budget.soft_ms = u64::MAX;
    budget.hard_ms = u64::MAX;
    let clock = MonotonicClock::new();
    let tmp = std::env::temp_dir().join(format!("ryo-v2-anchor-{}", std::process::id()));
    let mut loaded: Option<(std::path::PathBuf, RaSession)> = None;
    let mut states = Vec::new();
    for case in &cases {
        if loaded.as_ref().is_none_or(|(ws, _)| *ws != case.workspace) {
            let s = analyzer
                .load(&case.workspace)
                .unwrap_or_else(|e| panic!("{}: load: {e}", case.id));
            loaded = Some((case.workspace.clone(), s));
        }
        let Some((_, s)) = loaded.as_mut() else {
            unreachable!("loaded above");
        };
        let planted = case.edits().expect("the planted edits");
        let input = SearchInput {
            start: &planted,
            roots: &[],
            targets: &case.targets,
            probes: &case.probes,
            scope: Scope::Affected,
            budget,
            memory: MemoryBudget::unbounded(),
            weights: DEFAULT_WEIGHTS,
            keep_trace_edits: true,
        };
        let r = search_decision(
            &analyzer,
            &SyntaxAssessor,
            &MetricJudge,
            &clock,
            &ProcGauge,
            s,
            &input,
        )
        .unwrap_or_else(|e| panic!("{}: the search: {e}", case.id));
        let checked = |t: &&ryo_v2_core::app::search::TraceEntry| {
            t.node.is_some() && matches!(t.fate, Fate::Passed | Fate::Failed { .. })
        };
        let node = r
            .trace
            .iter()
            .filter(checked)
            .find(|t| matches!(t.fate, Fate::Failed { .. }))
            .or_else(|| r.trace.iter().find(checked))
            .unwrap_or_else(|| panic!("{}: the search checked a node", case.id));
        let node_edits = node.edits.clone().expect("the trace keeps the edits");
        for (what, state) in [
            ("the planted state".to_owned(), planted.clone()),
            (format!("node {}", node.blueprint), node_edits),
        ] {
            let at = format!("{}: {what}", case.id);
            let reference = analyzer
                .reference_check(s, &state, Scope::Workspace)
                .unwrap_or_else(|e| panic!("{at}: the reference check: {e}"));
            states.push((at, case.workspace.clone(), state, reference));
        }
    }
    drop(loaded);
    // The fresh loads are independent sessions: a few at a time on the small bases, each on its
    // own thread (a load of a small base is mostly one thread's work); one at a time on another
    // corpus, whose sessions are large.
    let at_once = if std::env::var_os(ANCHOR_CASES).is_some() {
        1
    } else {
        FRESH_LOADS_AT_ONCE
    };
    for (n, chunk) in states.chunks(at_once).enumerate() {
        std::thread::scope(|scope| {
            for (i, (at, workspace, state, reference)) in chunk.iter().enumerate() {
                let dir = tmp.join(format!("state-{n}-{i}"));
                let analyzer = &analyzer;
                scope.spawn(move || {
                    let (files, findings) = fresh_load(analyzer, workspace, state, &dir);
                    assert_eq!(reference.files, files, "{at}: the files of the state");
                    let mut by_reference = reference.findings.clone();
                    by_reference.sort();
                    assert_eq!(by_reference, findings, "{at}: the findings of the state");
                });
            }
        });
    }
    let _ = std::fs::remove_dir_all(&tmp);
    println!("{} states of {} cases anchored", states.len(), cases.len());
    assert_eq!(states.len(), 2 * cases.len(), "two states per case");
}

/// The fresh loads of the anchor that run at once on the small bases (about 26 s in a debug
/// build at 4, against 73 s one at a time; 12 threads).
const FRESH_LOADS_AT_ONCE: usize = 4;

/// The analyzer of the oracle's reference checks: proc macros off, the base's isolation sampled
/// at every job, and the re-assert of every reference check's level against the baseline
/// (`assert_revert`), so a write-back that leaves a trace the texts do not show poisons the
/// shared session instead of shifting its base for the next case. `width` is the search's.
fn reference_analyzer(width: usize) -> RaAnalyzer {
    RaAnalyzer::new(
        RaConfig {
            proc_macro: false,
            threads: None,
            assert_revert: true,
            gap_detectors: true,
            assert_revert_every: NonZeroU32::new(1),
            cfg_test: true,
            forks: ForksConfig {
                width: NonZeroUsize::new(width),
                caller_works: false,
            },
            ..Default::default()
        },
        Arc::new(MonotonicClock::new()),
        Arc::new(RecordingObserver::default()),
    )
}

/// The `.rs` files and the sorted findings of a session that `a` loads fresh from a copy of
/// `workspace` (made in `dir`, removed after) with `state` written to disk.
fn fresh_load(
    a: &RaAnalyzer,
    workspace: &std::path::Path,
    state: &[ryo_v2_core::domain::Edit],
    dir: &std::path::Path,
) -> (
    Vec<ryo_v2_core::domain::RelPath>,
    Vec<ryo_v2_core::domain::Finding>,
) {
    let _ = std::fs::remove_dir_all(dir);
    copy_workspace(workspace, dir);
    for e in state {
        let path = dir.join(e.path.as_str());
        match &e.new_text {
            Some(text) => {
                std::fs::create_dir_all(path.parent().expect("a parent")).expect("create dir");
                std::fs::write(&path, text).expect("write a file of the state");
            }
            None => std::fs::remove_file(&path).expect("delete a file of the state"),
        }
    }
    let fresh = a.load(dir).expect("the fresh load of the state");
    let files: Vec<ryo_v2_core::domain::RelPath> = Verifier::provenance(a, &fresh)
        .files()
        .keys()
        .filter(|p| p.as_str().ends_with(".rs"))
        .cloned()
        .collect();
    let mut findings = Analyzer::baseline(a, &fresh);
    findings.sort();
    drop(fresh);
    let _ = std::fs::remove_dir_all(dir);
    (files, findings)
}

/// Copies `from` to `to`, outside `target` and `.git`.
fn copy_workspace(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).expect("create the copy");
    for entry in std::fs::read_dir(from).expect("read the workspace") {
        let entry = entry.expect("dir entry");
        let name = entry.file_name();
        if name == "target" || name == ".git" {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            copy_workspace(&path, &to.join(&name));
        } else {
            std::fs::copy(&path, to.join(&name)).expect("copy a file");
        }
    }
}

/// Every directory under `dir` that has a `cases` directory, outside `target` and `planted`.
fn workspaces_with_cases(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if dir.join("cases").is_dir() {
        out.push(dir.to_path_buf());
    }
    for entry in std::fs::read_dir(dir).expect("read corpus dir") {
        let path = entry.expect("dir entry").path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() && !matches!(name, "target" | "planted" | "cases" | ".git") {
            out.extend(workspaces_with_cases(&path));
        }
    }
    out.sort();
    out
}

/// The text of every `.rs` file under `dir`, outside `target` and the planted overlays.
fn workspace_sources(dir: &std::path::Path) -> String {
    let mut out = String::new();
    for entry in std::fs::read_dir(dir).expect("read workspace dir") {
        let path = entry.expect("dir entry").path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            if !matches!(name, "target" | "planted" | ".git") {
                out.push_str(&workspace_sources(&path));
            }
        } else if name.ends_with(".rs") {
            out.push_str(&std::fs::read_to_string(&path).expect("read source"));
        }
    }
    out
}
