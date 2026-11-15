//! `RaSession::memory_report` on a corpus base through the real rust-analyzer. Needs a sysroot
//! and `cargo metadata` (no network).

use ryo_v2_core::app::ports::{Analyzer, CheckError, Event, Verifier};
use ryo_v2_core::domain::{Edit, RelPath, Scope};
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_ra::{RaAnalyzer, RaConfig, RaSession};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn corpus(rel: &str) -> PathBuf {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../corpus"))
        .join(rel)
        .canonicalize()
        .expect("fixture exists")
}

fn rel(s: &str) -> RelPath {
    RelPath::parse(s).unwrap()
}

/// The small corpus base, loaded; the number of local files its `Event::Loaded` reported.
fn loaded() -> (RaAnalyzer, RaSession, usize) {
    let observer = Arc::new(RecordingObserver::default());
    let a = RaAnalyzer::new(
        RaConfig {
            cfg_test: false,
            ..RaConfig::default()
        },
        Arc::new(MonotonicClock::new()),
        observer.clone(),
    );
    let s = a
        .load(&corpus("base/extract-trait-multicrate"))
        .expect("load fixture");
    let files = observer
        .take()
        .into_iter()
        .find_map(|e| match e {
            Event::Loaded { files, .. } => Some(files),
            _ => None,
        })
        .expect("a load reports Event::Loaded");
    (a, s, files)
}

/// The edit of broken-01: the lib's method removed, so the user crate's caller breaks.
fn broken01() -> Vec<Edit> {
    let text = std::fs::read_to_string(corpus("broken-01").join("lib/src/lib.rs")).unwrap();
    vec![Edit::write(rel("lib/src/lib.rs"), text)]
}

#[test]
fn memory_report_lists_ingredients_and_memos() {
    let (_a, mut s, files) = loaded();
    assert!(files > 0);
    let report = s.memory_report().expect("no fork is live");
    assert!(
        report.ingredients.iter().all(|r| r.count > 0),
        "{:?}",
        report.ingredients
    );
    assert!(
        report.memos.iter().all(|r| r.count > 0),
        "{:?}",
        report.memos
    );
    let file_text = report
        .ingredients
        .iter()
        .find(|r| r.name == "FileText")
        .unwrap_or_else(|| panic!("no FileText row in {:?}", report.ingredients));
    assert!(
        file_text.count >= files as u64,
        "{} FileText slots for {files} local files",
        file_text.count
    );
    assert!(file_text.size_of_fields > 0 && file_text.size_of_metadata > 0);
    // The baseline parsed the local files: salsa keys the memos of `EditionedFileId::parse` by
    // its tracked method's name.
    let parse = report
        .memos
        .iter()
        .find(|r| r.query == "EditionedFileId::parse_")
        .unwrap_or_else(|| {
            let names: Vec<&str> = report.memos.iter().map(|r| r.query.as_str()).collect();
            panic!("no parse memo row in {names:?}")
        });
    assert!(parse.count > 0);
    // rust-analyzer declares no heap size.
    assert!(
        report
            .ingredients
            .iter()
            .all(|r| r.heap_size_of_fields.is_none())
    );
    assert!(report.memos.iter().all(|r| r.heap_size_of_fields.is_none()));
    // Sorted, so that two reports compare row by row.
    assert!(
        report
            .ingredients
            .windows(2)
            .all(|w| w[0].name <= w[1].name)
    );
    assert!(report.memos.windows(2).all(|w| w[0].query <= w[1].query));
}

#[test]
fn memory_report_changes_nothing() {
    let (a, mut s, _) = loaded();
    let edits = broken01();
    let path = rel("lib/src/lib.rs");
    let baseline = Verifier::baseline(&a, &s);
    let text = a.text(&s, &path);
    let before = a.check(&mut s, &edits, Scope::Affected).expect("check");
    s.memory_report().expect("no fork is live");
    let after = a.check(&mut s, &edits, Scope::Affected).expect("check");
    assert_eq!(after, before);
    assert!(!before.findings.is_empty(), "broken-01 breaks the caller");
    assert_eq!(Verifier::baseline(&a, &s), baseline);
    assert_eq!(a.text(&s, &path), text);
}

#[test]
fn memory_report_refuses_while_a_fork_is_live() {
    let (a, mut s, _) = loaded();
    let edits = broken01();
    let path = rel("lib/src/lib.rs");
    let text = a.text(&s, &path);
    let mut f = Analyzer::fork(&a, &mut s).expect("fork");
    Analyzer::apply(&a, &mut s, &mut f, &edits).expect("apply");
    let before = Analyzer::diagnostics(&a, &s, &f, Scope::Affected).expect("diagnostics");
    assert_eq!(s.memory_report(), Err(CheckError::ForkInUse));
    // Nothing changed: the fork's level diagnoses as before, and the fork goes on.
    let after = Analyzer::diagnostics(&a, &s, &f, Scope::Affected).expect("diagnostics");
    assert_eq!(after, before);
    Analyzer::discard(&a, &mut s, f).expect("discard");
    assert_eq!(a.text(&s, &path), text);
    assert!(s.memory_report().is_ok());

    s.poison("for the test");
    assert!(matches!(s.memory_report(), Err(CheckError::Poisoned(_))));
}
