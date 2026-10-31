//! One planted case of `corpus/search/base/search-lab` through the real language server: the
//! planted state verifies, the search runs, and the row names what became of the best change.
//! Proc macros are off and the beam is 5, so it takes seconds. The full table is
//! `just search-recall`. Before the run it checks the probes of every committed case of every
//! workspace under `corpus/search` (each probe names a type of its workspace).

use std::sync::Arc;

use ryo_v2_core::app::ports::Analyzer;
use ryo_v2_core::app::search::{ASSERT_REVERT_EVERY, DEFAULT_SEARCH_BUDGET};
use ryo_v2_core::domain::{ChangeProbe, Ident};
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::judge::MetricJudge;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_oracle::materials::Materials;
use ryo_v2_oracle::search_recall::{PlantedCase, RunInfo, markdown, run_case};
use ryo_v2_ra::{RaAnalyzer, RaConfig, SyntaxAssessor};
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
