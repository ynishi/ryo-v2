//! The files merged by the ledger driver: `.gitattributes` names every `*_LEDGER.md` of the
//! root with `merge=ledger`, and none of those files holds a conflict marker (a merge resolved by hand once
//! left one in a ledger).

use std::fs;
use std::path::{Path, PathBuf};

use ryo_v2_ledger_merge::conflict_markers;

/// The files that must be merged by the driver: every `*_LEDGER.md` at the root.
fn ledgers() -> Vec<String> {
    let mut out: Vec<String> = fs::read_dir(root())
        .expect("workspace root")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with("_LEDGER.md"))
        .collect();
    out.sort();
    out
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The patterns `.gitattributes` gives `merge=ledger` (comment lines skipped).
fn ledger_patterns(attributes: &str) -> Vec<&str> {
    attributes
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let mut words = l.split_whitespace();
            let pattern = words.next()?;
            words.any(|w| w == "merge=ledger").then_some(pattern)
        })
        .collect()
}

#[test]
fn gitattributes_route_both_ledgers_to_the_driver() {
    let text = fs::read_to_string(root().join(".gitattributes")).expect(".gitattributes");
    let patterns = ledger_patterns(&text);
    let ledgers = ledgers();
    assert!(!ledgers.is_empty(), "no *_LEDGER.md at the root");
    for ledger in &ledgers {
        assert!(
            patterns.contains(&ledger.as_str()),
            ".gitattributes does not give {ledger} `merge=ledger` (it has {patterns:?})"
        );
    }
    assert_eq!(
        ledger_patterns("# a.md merge=ledger\nb.md   text merge=ledger\nc.md merge=union\n"),
        ["b.md"]
    );
}

#[test]
fn ledgers_hold_no_conflict_marker() {
    let text = fs::read_to_string(root().join(".gitattributes")).expect(".gitattributes");
    let mut problems = Vec::new();
    for pattern in ledger_patterns(&text) {
        let path = root().join(pattern);
        let body = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{pattern} (from .gitattributes): {e}"));
        for line in conflict_markers(&body) {
            problems.push(format!("{pattern}:{line}: conflict marker"));
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
