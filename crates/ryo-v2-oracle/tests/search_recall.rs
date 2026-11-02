//! One planted case of `corpus/search/base/search-lab` through the real language server: the
//! planted state verifies, the search runs, and the row names what became of the best change.
//! Proc macros are off and the beam is 5, so it takes seconds. The full table is
//! `just search-recall`. Before the run it checks the probes of every committed case of every
//! workspace under `corpus/search` (each probe names a type of its workspace). A second test
//! compares every verdict a stacked search forms with the flat check of the same edits.

use std::collections::BTreeMap;
use std::sync::Arc;

use ryo_v2_core::app::ports::{Analyzer, CheckReach, Verifier};
use ryo_v2_core::app::search::{
    ASSERT_REVERT_EVERY, DEFAULT_SEARCH_BUDGET, Fate, SearchError, SearchInput, SearchReport,
    search_decision,
};
use ryo_v2_core::app::verdict_of;
use ryo_v2_core::domain::{
    Blocking, BlueprintId, ChangeProbe, DEFAULT_WEIGHTS, Ident, Resolution, Scope, Verdict,
};
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::judge::MetricJudge;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_oracle::materials::Materials;
use ryo_v2_oracle::search_recall::{PlantedCase, RunInfo, markdown, run_case};
use ryo_v2_ra::{RaAnalyzer, RaConfig, RaSession, SyntaxAssessor};
use std::num::NonZeroU32;

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
        &analyzer,
        &SyntaxAssessor,
        &MetricJudge,
        &clock,
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

/// Every node a stacked search checked has the verdict `app::verdict_of` gives for its composed
/// edits, on the same session after the fork's release: the same new errors and unseen classes,
/// and its fate follows that verdict. The depth-2 cases of search-lab, which run every stacked
/// depth, proc macros off, beam 5, every write's revert re-asserted, no time limit.
///
/// The case list bounds its time. With the first case of gen-lab, shape-lab and
/// breakage-lab as well it took about 20 s, against about 3 s for the test above; each of those
/// labs added about 3.4 s, mostly its load. The ten depth-2 cases take about 9 s.
#[test]
fn stacked_verdicts_equal_the_flat_check() {
    let m = Materials::find().expect("materials");
    let cases: Vec<PlantedCase> =
        PlantedCase::load_all(&m.corpus.join("search/base/search-lab"), &m.corpus)
            .expect("load the cases of search-lab")
            .into_iter()
            .filter(|c| c.depth == 2 && c.skip.is_none())
            .collect();
    assert_eq!(cases.len(), 10, "the depth-2 cases of search-lab");

    let analyzer = RaAnalyzer::new(
        RaConfig {
            proc_macro: false,
            threads: None,
            assert_revert: false,
            gap_detectors: true,
            assert_revert_every: NonZeroU32::new(1),
            cfg_test: true,
        },
        Arc::new(MonotonicClock::new()),
        Arc::new(RecordingObserver::default()),
    );
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
            weights: DEFAULT_WEIGHTS,
            keep_trace_edits: true,
        };
        let r = match search_decision(
            &analyzer,
            &analyzer,
            &SyntaxAssessor,
            &MetricJudge,
            &clock,
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
            let flat = verdict_of(&analyzer, s, edits, Scope::Affected, &baseline);
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
