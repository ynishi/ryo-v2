//! One planted case of `corpus/search/base/search-lab` through the real language server: the
//! planted state verifies, the search runs, and the row names what became of the best change.
//! Proc macros are off and the beam is 5, so it takes seconds. The full table is
//! `just search-recall`.

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
    let breakage = PlantedCase::load_all(&m.corpus.join("search/base/breakage-lab"), &m.corpus)
        .expect("load breakage-lab cases");
    for c in cases.iter().chain(&breakage) {
        assert!(
            !c.probes.is_empty(),
            "every committed case carries probes: {} has none",
            c.id
        );
    }

    let analyzer = RaAnalyzer::new(
        RaConfig {
            proc_macro: false,
            threads: None,
            assert_revert: false,
            unresolved_paths: true,
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
