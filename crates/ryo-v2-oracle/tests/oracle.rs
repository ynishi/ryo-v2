//! Compares the in-memory verifier (`ryo-v2-ra`) with the expected diagnostics
//! `cargo check` gave for each corpus case. Never runs cargo check itself.
//!
//! Env: `RYO_V2_ORACLE_ONLY=<group>,..`, `RYO_V2_ORACLE_LIMIT=<n>`,
//! `RYO_V2_ORACLE_FULL` (list every diagnostic), `RYO_V2_ORACLE_REPORT=<file>`,
//! `RYO_V2_ORACLE_PROC_MACRO=off` (print the table, ignore floors),
//! `RYO_V2_ORACLE_GAP_DETECTORS=off` (switch every gap detector off; ignore floors; renamed from
//! `RYO_V2_ORACLE_UNRESOLVED`, which now stops the run),
//! `RYO_V2_ORACLE_CFG_TEST=off` (ra without `cfg(test)`, as before the V2 decision; ignore floors).
//! The expected files are `cargo check --workspace --all-targets`, so the floors hold only with
//! proc-macro on, the gap detectors on and cfg-test on. Independent of those switches, an unfiltered
//! run also asserts that the primary denominator holds its floor and that no case is skipped or
//! misgrouped, and that no detector or expander panic was contained.

use std::collections::BTreeMap;
use std::sync::Arc;

use ryo_v2_core::app::ports::{Analyzer, Event};
use ryo_v2_core::app::verify::verify;
use ryo_v2_core::domain::Scope;
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_oracle::case::{Case, load_gap_tags};
use ryo_v2_oracle::groups::{CaseOutcome, Counts, Group, Outcome, Report, compare};
use ryo_v2_oracle::materials::Materials;
use ryo_v2_oracle::search_recall::ContainedPanics;
use ryo_v2_ra::{RaAnalyzer, RaConfig};

// Floors: the least number of recall hits (`hit`; `exact` for valid) a group
// must keep. Raised, never lowered; lowering one needs the id of its
// CHECK_LEDGER row written next to it.
const FLOOR_UNRESOLVED_IMPORT: usize = 7;
const FLOOR_UNRESOLVED_NAME: usize = 5;
const FLOOR_UNRESOLVED_TYPE: usize = 0;
const FLOOR_NO_METHOD: usize = 10;
const FLOOR_TYPE_MISMATCH: usize = 7;
const FLOOR_TRAIT_BOUND: usize = 3;
const FLOOR_ARITY: usize = 0;
const FLOOR_FIELD: usize = 1;
const FLOOR_BORROW: usize = 0;
const FLOOR_PRIVACY: usize = 4;
const FLOOR_CONST_EVAL: usize = 0;
const FLOOR_DUPLICATE_NAME: usize = 2;
const FLOOR_COPY_IMPL: usize = 6;
const FLOOR_CFG: usize = 1;
const FLOOR_CFG_TEST: usize = 5;
const FLOOR_MACRO_DERIVE: usize = 2;
const FLOOR_DERIVE_FIELD: usize = 13;
const FLOOR_MACRO_ATTR: usize = 2;
const FLOOR_MACRO_RULES: usize = 0;
const FLOOR_MACRO_TOKENS: usize = 2;
const FLOOR_PROC_MACRO_EDIT: usize = 0;
const FLOOR_VALID: usize = 11;
const PRIMARY_OF_FLOOR: usize = 32; // raised, never lowered (same rule as the floors)

fn floor(g: Group) -> usize {
    match g {
        Group::UnresolvedImport => FLOOR_UNRESOLVED_IMPORT,
        Group::UnresolvedName => FLOOR_UNRESOLVED_NAME,
        Group::UnresolvedType => FLOOR_UNRESOLVED_TYPE,
        Group::NoMethod => FLOOR_NO_METHOD,
        Group::TypeMismatch => FLOOR_TYPE_MISMATCH,
        Group::TraitBound => FLOOR_TRAIT_BOUND,
        Group::Arity => FLOOR_ARITY,
        Group::Field => FLOOR_FIELD,
        Group::Borrow => FLOOR_BORROW,
        Group::Privacy => FLOOR_PRIVACY,
        Group::ConstEval => FLOOR_CONST_EVAL,
        Group::DuplicateName => FLOOR_DUPLICATE_NAME,
        Group::CopyImpl => FLOOR_COPY_IMPL,
        Group::Cfg => FLOOR_CFG,
        Group::CfgTest => FLOOR_CFG_TEST,
        Group::MacroDerive => FLOOR_MACRO_DERIVE,
        Group::DeriveField => FLOOR_DERIVE_FIELD,
        Group::MacroAttr => FLOOR_MACRO_ATTR,
        Group::MacroRules => FLOOR_MACRO_RULES,
        Group::MacroTokens => FLOOR_MACRO_TOKENS,
        Group::ProcMacroEdit => FLOOR_PROC_MACRO_EDIT,
        Group::Valid => FLOOR_VALID,
    }
}

/// The metric a floor is checked against: `exact` for valid, `hit` otherwise.
fn floor_metric(g: Group, c: &Counts) -> usize {
    if g == Group::Valid { c.exact } else { c.hit }
}

fn ratio_text(r: Option<f64>) -> String {
    r.map_or_else(|| "-".to_string(), |v| format!("{v:.2}"))
}

fn listed<T: std::fmt::Debug>(items: &[T], full: bool) -> String {
    let shown: Vec<String> = items
        .iter()
        .take(if full { usize::MAX } else { 5 })
        .map(|i| format!("{i:?}"))
        .collect();
    let more = if items.len() > shown.len() {
        format!(" (+{} more)", items.len() - shown.len())
    } else {
        String::new()
    };
    format!("[{}]{more}", shown.join(", "))
}

#[test]
fn oracle() {
    assert!(
        std::env::var_os("RYO_V2_ORACLE_UNRESOLVED").is_none(),
        "RYO_V2_ORACLE_UNRESOLVED was renamed to RYO_V2_ORACLE_GAP_DETECTORS (=off switches every gap detector off)"
    );
    let m = Materials::find().expect("materials");
    let mut cases = Case::load_all(&m.corpus).expect("load cases");
    let mut report = Report::default();

    if let Ok(only) = std::env::var("RYO_V2_ORACLE_ONLY") {
        let groups: Vec<Group> = only
            .split(',')
            .map(|s| {
                Group::parse(s.trim())
                    .unwrap_or_else(|| panic!("RYO_V2_ORACLE_ONLY: unknown group {s:?}"))
            })
            .collect();
        cases.retain(|c| {
            let keep = groups.contains(&c.group);
            if !keep {
                report.excluded.push((c.id.clone(), "only".into()));
            }
            keep
        });
    }
    if let Ok(limit) = std::env::var("RYO_V2_ORACLE_LIMIT") {
        let n: usize = limit.parse().expect("RYO_V2_ORACLE_LIMIT is a number");
        for c in cases.iter().skip(n) {
            report.excluded.push((c.id.clone(), "limit".into()));
        }
        cases.truncate(n);
    }

    if m.proc_macro && m.proc_macro_srv.is_none() {
        println!("warning: proc-macro srv not found; results equal proc-macro off");
    }

    let gap_detectors_on =
        std::env::var("RYO_V2_ORACLE_GAP_DETECTORS").map_or(true, |v| v != "off");
    println!(
        "gap-detectors: {}",
        if gap_detectors_on { "on" } else { "off" }
    );
    let cfg_test_on = std::env::var("RYO_V2_ORACLE_CFG_TEST").map_or(true, |v| v != "off");
    println!("cfg-test: {}", if cfg_test_on { "on" } else { "off" });
    let observer = Arc::new(RecordingObserver::default());
    let mut infra_errors: Vec<String> = Vec::new();
    let mut panics = ContainedPanics::default();

    let mut by_ws: BTreeMap<String, Vec<&Case>> = BTreeMap::new();
    for c in &cases {
        by_ws.entry(c.workspace_rel.clone()).or_default().push(c);
    }
    for (ws, ws_cases) in &by_ws {
        let analyzer = RaAnalyzer::new(
            RaConfig {
                proc_macro: m.proc_macro,
                threads: None,
                assert_revert: true,
                gap_detectors: gap_detectors_on,
                assert_revert_every: None,
                cfg_test: cfg_test_on,
                ..Default::default()
            },
            Arc::new(MonotonicClock::new()),
            observer.clone(),
        );
        let mut session = match analyzer.load(&ws_cases[0].workspace) {
            Ok(s) => s,
            Err(e) => {
                infra_errors.push(format!("load {ws}: {e}"));
                continue;
            }
        };
        let _ = observer.take();
        for case in ws_cases {
            let mut check_ms = None;
            let mut expected_diags = Vec::new();
            let outcome = if let Some(why) = &case.skip {
                Outcome::Skip(why.clone())
            } else {
                match case.expected() {
                    Err(e) => {
                        infra_errors.push(format!("{}: {e}", case.id));
                        continue;
                    }
                    Ok(None) => {
                        Outcome::Skip(format!("no expected file; run just oracle-refresh {ws}"))
                    }
                    Ok(Some(expected)) => {
                        let edits = match case.edits() {
                            Ok(e) => e,
                            Err(e) => {
                                infra_errors.push(format!("{}: {e}", case.id));
                                continue;
                            }
                        };
                        match verify(&analyzer, &mut session, &edits, Scope::Affected) {
                            Ok(verdict) => {
                                for ev in observer.take() {
                                    if let Event::Checked { check_ms: ms, .. } = ev {
                                        check_ms = Some(ms);
                                    }
                                }
                                let out = compare(&expected, &verdict.new_errors);
                                expected_diags = expected;
                                out
                            }
                            Err(e) => {
                                infra_errors.push(format!("{}: {e}", case.id));
                                continue;
                            }
                        }
                    }
                }
            };
            let ws_name = case.workspace_rel.rsplit('/').next().unwrap_or(ws);
            report.outcomes.push(CaseOutcome {
                id: format!("{ws_name}/{}", case.id),
                group: case.group,
                expected: expected_diags,
                outcome,
                check_ms,
            });
        }
        panics.absorb(ws, &session.detector_panics(), &session.expander_panics());
    }

    let filtered = !report.excluded.is_empty();
    let tags = match load_gap_tags(&m.corpus) {
        Ok(t) => t,
        Err(e) => {
            infra_errors.push(format!("gap tags: {e}"));
            Vec::new()
        }
    };
    if !filtered {
        for t in &tags {
            let found = report
                .outcomes
                .iter()
                .find(|o| o.id == t.case)
                .is_some_and(|o| o.expected.contains(&t.diagnostic));
            if !found {
                infra_errors.push(format!(
                    "stale gap tag: {} {} {} {}:{} (case not in the run, or diagnostic not expected)",
                    t.row, t.case, t.diagnostic.code, t.diagnostic.path, t.diagnostic.line
                ));
            }
        }
    }

    let counts = report.counts();
    println!("group | cases | skip | of | hit | FN | exact | FP | floor | recall | precision");
    for g in Group::ALL {
        let Some(c) = counts.get(&g) else { continue };
        println!(
            "{} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {}",
            g.id(),
            c.cases,
            c.skip,
            c.of,
            c.hit,
            c.fn_,
            c.exact,
            c.fp,
            floor(g),
            ratio_text(c.recall()),
            ratio_text(c.precision())
        );
    }
    println!("other code | missed | extra | cases");
    for (code, c) in report.other_codes() {
        println!("{code} | {} | {} | {}", c.missed, c.extra, c.cases);
    }
    match report.primary_recall() {
        Some((hit, of)) => println!(
            "H7 primary recall: {hit}/{of} = {:.2} (adopt >= 0.80, discard < 0.50)",
            hit as f64 / of as f64
        ),
        None => println!("H7 primary recall: - (no primary case compared)"),
    }
    let without = report.primary_recall_without(&tags, &["G-B-3", "G-B-4"]);
    match without {
        Some((hit, of)) => println!(
            "H7 primary recall without G-B-3, G-B-4: {hit}/{of} = {:.2}",
            hit as f64 / of as f64
        ),
        None => println!("H7 primary recall without G-B-3, G-B-4: - (no primary case compared)"),
    }
    for o in report.misgrouped() {
        let codes: Vec<&str> = o.expected.iter().map(|d| d.code.as_str()).collect();
        println!(
            "MISGROUPED {} [{}] expected codes: {}",
            o.id,
            o.group.id(),
            codes.join(",")
        );
    }
    for line in panics.lines() {
        println!("{line}");
    }
    let full = std::env::var_os("RYO_V2_ORACLE_FULL").is_some();
    for o in &report.outcomes {
        if let Outcome::Fail {
            missed,
            extra,
            unmapped,
        } = &o.outcome
        {
            println!(
                "FAIL {} [{}] missed: {} | extra: {} | unmapped: {}",
                o.id,
                o.group.id(),
                listed(missed, full),
                listed(extra, full),
                listed(unmapped, full)
            );
        }
        if let Outcome::Skip(why) = &o.outcome {
            println!("SKIP {} [{}] {why}", o.id, o.group.id());
        }
    }

    if let Ok(path) = std::env::var("RYO_V2_ORACLE_REPORT") {
        // A relative path is relative to the repo root, not to the test's cwd.
        let dest = m.repo_root.join(&path);
        let mut value = report.to_json();
        value["primary_recall_without_gb34"] = match without {
            Some((hit, of)) => serde_json::json!({"hit": hit, "of": of}),
            None => serde_json::Value::Null,
        };
        let json = serde_json::to_string_pretty(&value).expect("report json");
        std::fs::write(&dest, json + "\n")
            .unwrap_or_else(|e| panic!("write {}: {e}", dest.display()));
    }

    // Independent of the switches and of filtering: a contained panic is a defect to fix.
    assert!(
        panics.is_empty(),
        "contained panics, each a defect to fix:\n{}",
        panics.lines().join("\n")
    );
    assert!(
        infra_errors.is_empty(),
        "infrastructure errors:\n{}",
        infra_errors.join("\n")
    );

    // These quantities depend on the expected files only, so this holds with proc-macro, the
    // detector or cfg-test off too. A filtered run sees fewer cases, so it skips the check.
    if report.excluded.is_empty() {
        let problems = report.ratchet_problems(PRIMARY_OF_FLOOR);
        assert!(
            problems.is_empty(),
            "the run does not stand as the H7 measurement:\n{}",
            problems.join("\n")
        );
    }

    // A filtered run sees fewer cases than the floors count, so it skips them.
    if m.proc_macro && gap_detectors_on && cfg_test_on && report.excluded.is_empty() {
        let below: Vec<String> = Group::ALL
            .into_iter()
            .filter_map(|g| {
                let got = counts.get(&g).map_or(0, |c| floor_metric(g, c));
                let name = if g == Group::Valid { "exact" } else { "hit" };
                (got < floor(g)).then(|| format!("{}: {name} {got} < floor {}", g.id(), floor(g)))
            })
            .collect();
        assert!(below.is_empty(), "below floor:\n{}", below.join("\n"));
        // Ceiling: an edit of a proc-macro crate must stay a false negative.
        assert!(
            counts.get(&Group::ProcMacroEdit).is_none_or(|c| c.hit == 0),
            "proc-macro-edit: ra reported an edit of a proc-macro crate; the load-time dylib assumption (CHECK_LEDGER) no longer holds"
        );
    }
}
