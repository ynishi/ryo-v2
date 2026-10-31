//! The `Resolution` as the JSON the `apply` command prints, and its exit code.

use ryo_v2_core::domain::{
    Blocking, Candidate, Diagnostics, Disposition, FileRole, Freshness, LogEntry, OuterCheckReason,
    Resolution, ReviewReason, RiskMark, Scope, Stage, Verdict,
};
use serde_json::{Value, json};

fn scope_str(s: Scope) -> &'static str {
    match s {
        Scope::Affected => "affected",
        Scope::Workspace => "workspace",
    }
}

fn mark_str(m: RiskMark) -> &'static str {
    match m {
        RiskMark::Borrow => "borrow",
        RiskMark::BuildScript => "build_script",
        RiskMark::ProcMacroCrate => "proc_macro_crate",
        RiskMark::Behaviour => "behaviour",
        RiskMark::Visibility => "visibility",
    }
}

fn reason_str(r: OuterCheckReason) -> &'static str {
    match r {
        OuterCheckReason::Borrow => "borrow",
        OuterCheckReason::BuildScript => "build_script",
        OuterCheckReason::ProcMacroCrate => "proc_macro_crate",
        OuterCheckReason::Visibility => "visibility",
    }
}

fn role_str(r: FileRole) -> &'static str {
    match r {
        FileRole::Source => "source",
        FileRole::BuildScript => "build_script",
        FileRole::ProcMacroSource => "proc_macro_source",
        FileRole::Manifest => "manifest",
    }
}

fn stage_str(s: Stage) -> &'static str {
    match s {
        Stage::FileRole => "file_role",
        Stage::Freshness => "freshness",
        Stage::Conformance => "conformance",
        Stage::Splice => "splice",
        Stage::Marks => "marks",
        Stage::Prune => "prune",
        Stage::Check => "check",
        Stage::Candidate => "candidate",
    }
}

fn disposition_json(d: Disposition) -> (&'static str, Value) {
    match d {
        Disposition::AutoApplicable { rule } => ("auto_applicable", json!(rule.to_string())),
        Disposition::AfterOuterCheck { rule } => ("after_outer_check", json!(rule.to_string())),
        Disposition::NeedsReview { reason } => (
            match reason {
                ReviewReason::NoRule => "needs_review/no_rule",
                ReviewReason::ProcMacroCrate => "needs_review/proc_macro_crate",
                ReviewReason::Behaviour => "needs_review/behaviour",
                ReviewReason::ScopeTooNarrow => "needs_review/scope_too_narrow",
            },
            Value::Null,
        ),
    }
}

fn diagnostics_json(d: &Diagnostics) -> (Value, Value) {
    let errors: Vec<Value> = d
        .errors
        .iter()
        .map(|e| json!({"code": e.code.to_string(), "path": e.path.to_string(), "line": e.line}))
        .collect();
    let unmapped: Vec<Value> = d
        .unmapped
        .iter()
        .map(|u| json!({"ra_code": u.ra_code, "path": u.path.to_string(), "line": u.line}))
        .collect();
    (Value::Array(errors), Value::Array(unmapped))
}

fn verdict_json(v: &Verdict) -> (Value, Value) {
    diagnostics_json(&v.new_errors)
}

fn candidate_json(c: &Candidate) -> Value {
    let (disposition, rule) = disposition_json(c.disposition());
    let outer_check = match c.outer_check() {
        Some(o) => Value::Array(o.reasons().iter().map(|r| json!(reason_str(*r))).collect()),
        None => Value::Null,
    };
    let marks: Vec<Value> = c.marks().iter().map(|m| json!(mark_str(*m))).collect();
    let edits: Vec<Value> = c
        .edits()
        .iter()
        .map(|e| json!({"path": e.path.to_string(), "new_text": e.new_text}))
        .collect();
    json!({
        "blueprint": c.blueprint().to_string(),
        "disposition": disposition,
        "rule": rule,
        "outer_check": outer_check,
        "marks": marks,
        "edits": edits,
    })
}

fn log_json(e: &LogEntry) -> Value {
    json!({
        "blueprint": e.blueprint.to_string(),
        "stage": stage_str(e.stage),
        "ok": e.ok,
        "note": e.note,
    })
}

fn blocking_json(b: &Blocking) -> Value {
    let blueprint = b.blueprint().to_string();
    match b {
        Blocking::Unverifiable { path, role, .. } => json!({
            "kind": "unverifiable", "blueprint": blueprint,
            "path": path.to_string(), "role": role_str(*role),
        }),
        Blocking::Stale { reads, .. } => {
            let reads: Vec<Value> = reads
                .iter()
                .map(|(r, f)| {
                    let mut o = json!({
                        "path": r.path().to_string(),
                        "start": r.range().start(),
                        "end": r.range().end(),
                        "fingerprint": r.fingerprint().to_string(),
                    });
                    match f {
                        Freshness::Fresh => o["freshness"] = json!("fresh"),
                        Freshness::Stale { actual, .. } => {
                            o["freshness"] = json!("stale");
                            o["actual"] = json!(actual.to_string());
                        }
                        Freshness::OutOfRange { len } => {
                            o["freshness"] = json!("out_of_range");
                            o["len"] = json!(len);
                        }
                    }
                    o
                })
                .collect();
            json!({"kind": "stale", "blueprint": blueprint, "reads": reads})
        }
        Blocking::MissingFile { path, .. } => json!({
            "kind": "missing_file", "blueprint": blueprint, "path": path.to_string(),
        }),
        Blocking::Nonconforming { why, .. } => json!({
            "kind": "nonconforming", "blueprint": blueprint, "why": why.to_string(),
        }),
        Blocking::Splice { err, .. } => json!({
            "kind": "splice", "blueprint": blueprint, "error": err.to_string(),
        }),
        Blocking::NewErrors { verdict, .. } => {
            let (errors, unmapped) = verdict_json(verdict);
            json!({
                "kind": "new_errors", "blueprint": blueprint,
                "errors": errors, "unmapped": unmapped,
            })
        }
        Blocking::BudgetExhausted { limit_ms, .. } => json!({
            "kind": "budget_exhausted", "blueprint": blueprint, "limit_ms": limit_ms,
        }),
    }
}

/// The JSON document `apply` prints for a `Resolution`.
///
/// # Contract
/// - C1: a `Decided` resolution lists each candidate's disposition, rule id (or `null`),
///   `outer_check` reasons (or `null`), marks and edits, in that vocabulary and sorted order.
///   tested: resolution_json::tests::outer_check_lists_reasons
///   tested: resolution_json::tests::decided_lists_the_rule_id
///   tested: tests/apply.rs apply_rename_local_is_decided
/// - C2: a `Failed` resolution lists each blocker with its `kind` and the blueprint; a stale
///   read carries its freshness.
///   tested: resolution_json::tests::failed_lists_stale_reads
///   tested: tests/apply.rs apply_after_disk_change_is_failed_stale
///   tested: resolution_json::tests::budget_exhausted_blocking_is_listed
pub(crate) fn resolution_to_json(r: &Resolution) -> Value {
    match r {
        Resolution::Decided {
            blueprints,
            decision_log,
            evidence,
        } => {
            let verdicts: Vec<Value> = evidence
                .verdicts
                .iter()
                .map(|(id, v)| {
                    let (errors, unmapped) = verdict_json(v);
                    json!({"blueprint": id.to_string(), "errors": errors, "unmapped": unmapped})
                })
                .collect();
            json!({
                "resolution": "decided",
                "candidates": blueprints.iter().map(candidate_json).collect::<Vec<_>>(),
                "decision_log": decision_log.entries().iter().map(log_json).collect::<Vec<_>>(),
                "evidence": {
                    "scope": scope_str(evidence.scope),
                    "fingerprints_checked": evidence.fingerprints_checked,
                    "baseline_size": evidence.baseline_size,
                    "budget": {
                        "candidates_per_depth": evidence.budget.candidates_per_depth,
                        "max_depth": evidence.budget.max_depth,
                        "per_candidate_ms": evidence.budget.per_candidate_ms,
                    },
                    "verdicts": verdicts,
                },
            })
        }
        Resolution::Stopped(m) => match *m {},
        Resolution::Failed(inf) => json!({
            "resolution": "failed",
            "blocking": inf.blocking().iter().map(blocking_json).collect::<Vec<_>>(),
            "near_miss": inf.near_miss().iter().map(|n| json!({
                "blueprint": n.blueprint.to_string(),
                "new_errors": n.new_errors,
            })).collect::<Vec<_>>(),
            "budget_exhausted": inf.budget_exhausted(),
        }),
    }
}

/// The process exit code of a resolution.
///
/// # Contract
/// - C1: `Decided` is 0 and `Failed` is 2.
///   tested: resolution_json::tests::exit_codes_follow_the_resolution
pub(crate) fn exit_code(r: &Resolution) -> u8 {
    match r {
        Resolution::Decided { .. } => 0,
        Resolution::Failed(_) => 2,
        Resolution::Stopped(m) => match *m {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryo_v2_core::domain::{
        BlueprintId, DEFAULT_BUDGET, DecisionLog, Edit, Evidence, Fingerprint, Infeasible,
        IntentKind, NonEmpty, ReadRange, RelPath, ReviewRecord, Rule, RuleId, TextRange,
    };
    use std::collections::BTreeSet;

    fn id() -> BlueprintId {
        BlueprintId::from_uuid(uuid::Uuid::now_v7()).expect("v7")
    }

    fn decided(marks: BTreeSet<RiskMark>) -> Resolution {
        decided_with(marks, None)
    }

    fn decided_with(marks: BTreeSet<RiskMark>, rule: Option<&Rule>) -> Resolution {
        let b = id();
        let c = Candidate::new(
            b,
            vec![Edit::write(RelPath::parse("src/lib.rs").unwrap(), "x")],
            Verdict::from_diagnostics(Diagnostics::default()),
            marks,
            rule,
            Scope::Affected,
        )
        .expect("a passing verdict");
        Resolution::Decided {
            blueprints: NonEmpty::new(vec![c]).unwrap(),
            decision_log: DecisionLog::default(),
            evidence: Evidence {
                scope: Scope::Affected,
                fingerprints_checked: 1,
                baseline_size: 0,
                verdicts: vec![(b, Verdict::from_diagnostics(Diagnostics::default()))],
                budget: DEFAULT_BUDGET,
            },
        }
    }

    #[test]
    fn decided_lists_the_rule_id() {
        let rule_id = RuleId::from_uuid(uuid::Uuid::now_v7()).expect("v7");
        let review = ReviewRecord {
            reviewer: "me".to_owned(),
            reviewed_at_ms: 1,
            note: "n".to_owned(),
        };
        let rule = Rule::register(rule_id, IntentKind::MoveItem, review, Scope::Affected)
            .expect("move-item is registrable");
        let v = resolution_to_json(&decided_with(BTreeSet::new(), Some(&rule)));
        let c = &v["candidates"][0];
        assert_eq!(c["disposition"], "auto_applicable");
        assert_eq!(c["rule"], rule_id.to_string());
        let marks: BTreeSet<RiskMark> = [RiskMark::BuildScript].into();
        let v = resolution_to_json(&decided_with(marks, Some(&rule)));
        let c = &v["candidates"][0];
        assert_eq!(c["disposition"], "after_outer_check");
        assert_eq!(c["rule"], rule_id.to_string());
    }

    #[test]
    fn outer_check_lists_reasons() {
        let marks: BTreeSet<RiskMark> = [RiskMark::Borrow, RiskMark::BuildScript].into();
        let v = resolution_to_json(&decided(marks));
        assert_eq!(v["resolution"], "decided");
        let c = &v["candidates"][0];
        assert_eq!(c["outer_check"], json!(["borrow", "build_script"]));
        assert_eq!(c["marks"], json!(["borrow", "build_script"]));
        assert_eq!(c["disposition"], "needs_review/no_rule");
        assert_eq!(c["rule"], Value::Null);
        assert_eq!(c["edits"][0]["new_text"], "x");
        assert_eq!(v["evidence"]["scope"], "affected");
        let none = resolution_to_json(&decided(BTreeSet::new()));
        assert_eq!(none["candidates"][0]["outer_check"], Value::Null);
    }

    #[test]
    fn failed_lists_stale_reads() {
        let b = id();
        let read = ReadRange::new(
            RelPath::parse("src/lib.rs").unwrap(),
            TextRange::new(1, 4).unwrap(),
            Fingerprint::of(b"abc"),
        );
        let stale = Blocking::Stale {
            blueprint: b,
            reads: vec![(
                read.clone(),
                Freshness::Stale {
                    expected: Fingerprint::of(b"abc"),
                    actual: Fingerprint::of(b"abd"),
                },
            )],
        };
        let oor = Blocking::Stale {
            blueprint: b,
            reads: vec![(read, Freshness::OutOfRange { len: 2 })],
        };
        let inf = Infeasible::new(vec![stale, oor], vec![], false).expect("blocking");
        let v = resolution_to_json(&Resolution::Failed(inf));
        assert_eq!(v["resolution"], "failed");
        assert_eq!(v["budget_exhausted"], false);
        let first = &v["blocking"][0];
        assert_eq!(first["kind"], "stale");
        assert_eq!(first["blueprint"], b.to_string());
        assert_eq!(first["reads"][0]["freshness"], "stale");
        assert_eq!(
            first["reads"][0]["actual"],
            Fingerprint::of(b"abd").to_string()
        );
        assert_eq!(v["blocking"][1]["reads"][0]["freshness"], "out_of_range");
        assert_eq!(v["blocking"][1]["reads"][0]["len"], 2);
    }

    #[test]
    fn budget_exhausted_blocking_is_listed() {
        let b = id();
        let cut = Blocking::BudgetExhausted {
            blueprint: b,
            limit_ms: 6_000,
        };
        let inf = Infeasible::new(vec![cut], vec![], false).expect("blocking");
        let v = resolution_to_json(&Resolution::Failed(inf));
        let first = &v["blocking"][0];
        assert_eq!(first["kind"], "budget_exhausted");
        assert_eq!(first["blueprint"], b.to_string());
        assert_eq!(first["limit_ms"], 6_000);
        assert_eq!(stage_str(Stage::Prune), "prune");
        assert_eq!(mark_str(RiskMark::Visibility), "visibility");
        assert_eq!(reason_str(OuterCheckReason::Visibility), "visibility");
    }

    #[test]
    fn exit_codes_follow_the_resolution() {
        assert_eq!(exit_code(&decided(BTreeSet::new())), 0);
        let inf = Infeasible::new(vec![], vec![], true).expect("budget exhausted");
        assert_eq!(exit_code(&Resolution::Failed(inf)), 2);
    }
}
