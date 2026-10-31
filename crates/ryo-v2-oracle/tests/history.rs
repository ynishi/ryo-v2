//! Precision on real history: the verifier is run on the `.rs` changes of a commit over its parent
//! (pairs written by `history-cases export`, expected files by `history-cases expect`). A commit
//! that builds cleanly has no expected error, so every error the verifier reports is a false
//! positive. Never runs cargo check itself, and has no floor: this is the first measurement.
//!
//! Env: `RYO_V2_HISTORY_DIR=<out>` (unset: the test prints that and returns),
//! `RYO_V2_HISTORY_LIMIT=<n>` (the first n pairs), `RYO_V2_HISTORY_REPORT=<file>` (per-pair JSON),
//! and the oracle's `RYO_V2_ORACLE_PROC_MACRO=off`, `RYO_V2_ORACLE_GAP_DETECTORS=off` (every gap detector; renamed from
//! `RYO_V2_ORACLE_UNRESOLVED`, which now stops the run),
//! `RYO_V2_ORACLE_CFG_TEST=off`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use ryo_v2_core::app::ports::{Analyzer, Event};
use ryo_v2_core::app::verify::verify;
use ryo_v2_core::domain::Scope;
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_core::infra::overlay::overlay_edits;
use ryo_v2_oracle::case::parse_expected;
use ryo_v2_oracle::groups::{Outcome, compare};
use ryo_v2_oracle::history::{HistoryCounts, HistoryPair, load_sets};
use ryo_v2_oracle::materials::Materials;
use ryo_v2_ra::{RaAnalyzer, RaConfig};

fn switch_on(name: &str) -> bool {
    std::env::var(name).map_or(true, |v| v != "off")
}

/// The outcome of one pair, or why it was not compared.
fn run_pair(pair: &HistoryPair, proc_macro: bool) -> Result<(Outcome, Option<u64>), String> {
    if let Some(why) = &pair.entry.skip {
        return Ok((Outcome::Skip(why.clone()), None));
    }
    let text = match std::fs::read_to_string(&pair.expected) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((
                Outcome::Skip("no expected; run history-cases expect".into()),
                None,
            ));
        }
        Err(e) => return Err(format!("read {}: {e}", pair.expected.display())),
    };
    let expected =
        parse_expected(&text).map_err(|e| format!("{}: {e}", pair.expected.display()))?;
    if !expected.is_empty() {
        return Ok((
            Outcome::Skip(format!(
                "child does not build cleanly: {} errors",
                expected.len()
            )),
            None,
        ));
    }
    let edits = overlay_edits(&pair.base, &pair.overlay).map_err(|e| format!("overlay: {e}"))?;

    let observer = Arc::new(RecordingObserver::default());
    let analyzer = RaAnalyzer::new(
        RaConfig {
            proc_macro,
            threads: None,
            assert_revert: true,
            gap_detectors: switch_on("RYO_V2_ORACLE_GAP_DETECTORS"),
            assert_revert_every: None,
            cfg_test: switch_on("RYO_V2_ORACLE_CFG_TEST"),
        },
        Arc::new(MonotonicClock::new()),
        observer.clone(),
    );
    // The session holds the whole project (about 5 GB on a large one): it ends with this call.
    let mut session = analyzer
        .load(&pair.base)
        .map_err(|e| format!("load: {e}"))?;
    let _ = observer.take();
    let verdict = verify(&analyzer, &mut session, &edits, Scope::Affected)
        .map_err(|e| format!("verify: {e}"))?;
    let mut check_ms = None;
    for ev in observer.take() {
        if let Event::Checked { check_ms: ms, .. } = ev {
            check_ms = Some(ms);
        }
    }
    Ok((compare(&[], &verdict.new_errors), check_ms))
}

#[test]
fn history_precision() {
    assert!(
        std::env::var_os("RYO_V2_ORACLE_UNRESOLVED").is_none(),
        "RYO_V2_ORACLE_UNRESOLVED was renamed to RYO_V2_ORACLE_GAP_DETECTORS (=off switches every gap detector off)"
    );
    let Some(dir) = std::env::var_os("RYO_V2_HISTORY_DIR") else {
        println!("history: RYO_V2_HISTORY_DIR not set; skipped");
        return;
    };
    let m = Materials::find().expect("materials");
    let mut pairs = load_sets(&PathBuf::from(dir)).expect("load history sets");
    if let Ok(limit) = std::env::var("RYO_V2_HISTORY_LIMIT") {
        let n: usize = limit.parse().expect("RYO_V2_HISTORY_LIMIT is a number");
        pairs.truncate(n);
    }
    println!(
        "gap-detectors: {}, cfg-test: {}, proc-macro: {}",
        if switch_on("RYO_V2_ORACLE_GAP_DETECTORS") {
            "on"
        } else {
            "off"
        },
        if switch_on("RYO_V2_ORACLE_CFG_TEST") {
            "on"
        } else {
            "off"
        },
        if m.proc_macro { "on" } else { "off" },
    );

    let mut by_set: BTreeMap<String, HistoryCounts> = BTreeMap::new();
    let mut rows: Vec<serde_json::Value> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for pair in &pairs {
        let id = format!("{}/{}", pair.set, pair.entry.id);
        let (outcome, check_ms) = match run_pair(pair, m.proc_macro) {
            Ok(r) => r,
            Err(e) => {
                println!("HISTORY {id} error {e}");
                errors.push(format!("{id}: {e}"));
                continue;
            }
        };
        by_set.entry(pair.set.clone()).or_default().add(&outcome);
        let ms = check_ms.map_or("-".to_string(), |v| v.to_string());
        let mut row = serde_json::json!({
            "set": pair.set,
            "id": pair.entry.id,
            "manifest_from_child": pair.entry.manifest_from_child,
            "overlay_files": pair.entry.overlay_files,
            "check_ms": check_ms,
        });
        match &outcome {
            Outcome::Skip(why) => {
                println!("HISTORY {id} skip {why}");
                row["outcome"] = "skip".into();
                row["reason"] = why.clone().into();
            }
            Outcome::Pass => {
                println!("HISTORY {id} pass check_ms={ms}");
                row["outcome"] = "pass".into();
            }
            Outcome::Fail {
                extra, unmapped, ..
            } => {
                let extra_txt: Vec<String> = extra
                    .iter()
                    .map(|d| format!("{} {}:{}", d.code, d.path, d.line))
                    .collect();
                let unmapped_txt: Vec<String> = unmapped
                    .iter()
                    .map(|d| format!("{} {}:{}", d.ra_code, d.path, d.line))
                    .collect();
                println!(
                    "HISTORY {id} FP extra:[{}] unmapped:[{}] check_ms={ms}",
                    extra_txt.join(", "),
                    unmapped_txt.join(", ")
                );
                row["outcome"] = "fp".into();
                row["extra"] = extra_txt.into();
                row["unmapped"] = unmapped_txt.into();
            }
        }
        rows.push(row);
    }
    for (set, c) in &by_set {
        let rate = c
            .fp_rate()
            .map_or_else(|| "-".to_string(), |r| format!("{r:.2}"));
        println!(
            "precision on history {set}: pairs {} skip {} valid {} FP {} rate {rate} (extra {}, unmapped {})",
            c.pairs, c.skip, c.valid, c.fp_cases, c.extra_diags, c.unmapped_diags
        );
    }

    if let Ok(path) = std::env::var("RYO_V2_HISTORY_REPORT") {
        let dest = m.repo_root.join(&path);
        let json = serde_json::to_string_pretty(&rows).expect("report json");
        std::fs::write(&dest, json + "\n")
            .unwrap_or_else(|e| panic!("write {}: {e}", dest.display()));
    }
    assert!(
        errors.is_empty(),
        "infrastructure errors:\n{}",
        errors.join("\n")
    );
}
