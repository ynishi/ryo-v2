//! The `Resolution` as the JSON the `apply` command prints, and its exit code.

use ryo_v2_core::domain::{
    BaseFp, Basis, Blocking, Candidate, Diagnostics, Disposition, Drift, FileRole, Freshness,
    LogEntry, OuterCheckReason, ReadRange, Resolution, ReviewReason, RiskMark, Scope, Stage,
    Verdict,
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

fn file_json(b: &BaseFp) -> Value {
    json!({"path": b.path().to_string(), "fingerprint": b.fingerprint().to_string()})
}

fn read_json(r: &ReadRange) -> Value {
    json!({
        "path": r.path().to_string(),
        "start": r.range().start(),
        "end": r.range().end(),
        "fingerprint": r.fingerprint().to_string(),
    })
}

fn basis_json(b: &Basis) -> Value {
    json!({
        "files": b.files().iter().map(file_json).collect::<Vec<_>>(),
        "reads": b.reads().iter().map(read_json).collect::<Vec<_>>(),
    })
}

fn drift_json(d: &Drift) -> Value {
    match d {
        Drift::File { base, now } => {
            let mut o = file_json(base);
            o["kind"] = json!("file");
            add_freshness(&mut o, now);
            o
        }
        Drift::Read { read, now } => {
            let mut o = read_json(read);
            o["kind"] = json!("read");
            add_freshness(&mut o, now);
            o
        }
        Drift::Missing(path) => json!({"kind": "missing", "path": path.to_string()}),
    }
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
    let path: Vec<Value> = c
        .path()
        .iter()
        .map(|b| {
            json!({
                "blueprint": b.id().to_string(),
                "intent": b.intent().to_string(),
                "proposer": b.record().proposer(),
                "rationale": b.record().rationale(),
                "spec_refs": b.spec_refs().iter().map(ToString::to_string).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "blueprint": c.blueprint().to_string(),
        "path": path,
        "disposition": disposition,
        "rule": rule,
        "outer_check": outer_check,
        "marks": marks,
        "edits": edits,
        "basis": basis_json(c.basis()),
    })
}

fn log_json(e: &LogEntry) -> Value {
    json!({
        "blueprint": e.blueprint().to_string(),
        "stage": stage_str(e.stage()),
        "ok": e.ok(),
        "note": e.note(),
    })
}

/// Adds the `freshness` fields (`actual` for a stale entry, `len` for an out-of-range one) of a
/// file or read drift of a `stale` blocking.
fn add_freshness(o: &mut Value, f: &Freshness) {
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
}

fn blocking_json(b: &Blocking) -> Value {
    let blueprint = b.blueprint().to_string();
    match b {
        Blocking::Unverifiable { path, role, .. } => json!({
            "kind": "unverifiable", "blueprint": blueprint,
            "path": path.to_string(), "role": role_str(*role),
        }),
        Blocking::Stale { drift, .. } => json!({
            "kind": "stale", "blueprint": blueprint,
            "drift": drift.iter().map(drift_json).collect::<Vec<_>>(),
        }),
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
///   `outer_check` reasons (or `null`), marks and edits, in that vocabulary and sorted order, and
///   its `basis`: the `files` it edits (path, `fingerprint`) and the `reads` of files it only
///   reads (path, `start`, `end`, `fingerprint`).
///   tested: resolution_json::tests::outer_check_lists_reasons
///   tested: resolution_json::tests::decided_lists_the_rule_id
///   tested: tests/apply.rs apply_rename_local_is_decided
///   tested: tests/apply.rs decided_candidate_lists_its_basis
///   tested: resolution_json::tests::decided_lists_the_reads_of_files_only_read
/// - C2: a `Failed` resolution lists each blocker with its `kind` and the blueprint; a stale
///   blocking lists its `drift`, each entry a `file` (path, expected `fingerprint`, `freshness`,
///   `actual`), a `read` (path, range, `fingerprint`, freshness) or a `missing` path.
///   tested: resolution_json::tests::failed_lists_stale_reads
///   tested: tests/apply.rs apply_after_disk_change_is_failed_stale
///   tested: tests/apply.rs apply_after_insert_above_is_failed_stale
///   tested: resolution_json::tests::budget_exhausted_blocking_is_listed
/// - C3: a `Decided` resolution lists, per candidate, the Blueprints of its path with intent,
///   proposer, rationale and spec refs, and the evidence lists the provenance: analyzer,
///   version, config and workspace fingerprint.
///   tested: resolution_json::tests::decided_lists_record_and_provenance
pub(crate) fn resolution_to_json(r: &Resolution) -> Value {
    match r {
        Resolution::Decided {
            blueprints,
            decision_log,
            evidence,
        } => {
            let provenance = evidence.provenance();
            let verdicts: Vec<Value> = evidence
                .verdicts()
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
                    "scope": scope_str(evidence.scope()),
                    "fingerprints_checked": evidence.fingerprints_checked(),
                    "baseline_size": evidence.baseline_size(),
                    "budget": {
                        "candidates_per_depth": evidence.budget().candidates_per_depth,
                        "max_depth": evidence.budget().max_depth,
                        "per_candidate_ms": evidence.budget().per_candidate_ms,
                    },
                    "provenance": {
                        "analyzer": provenance.analyzer(),
                        "version": provenance.version(),
                        "config": provenance.config(),
                        "workspace": provenance.workspace().to_string(),
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
                "blueprint": n.blueprint().to_string(),
                "new_errors": n.new_errors(),
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
    use ryo_v2_core::app::apply::apply_blueprints;
    use ryo_v2_core::app::ports::{CheckError, FileFacts, Verifier};
    use ryo_v2_core::domain::{
        BaseFp, Blueprint, BlueprintId, BlueprintParts, BlueprintSet, DecisionRecord, Edit,
        Finding, Fingerprint, Ident, Infeasible, IntentCall, IntentKind, MutationSpec, Provenance,
        ReadRange, RelPath, ReviewRecord, Rule, RuleCatalog, RuleId, ScopedFindings, SpecRef,
        TextRange,
    };
    use std::collections::{BTreeMap, BTreeSet};

    fn id() -> BlueprintId {
        BlueprintId::from_uuid(uuid::Uuid::now_v7()).expect("v7")
    }
    fn rel(s: &str) -> RelPath {
        RelPath::parse(s).expect("a relative path")
    }
    fn tr(a: u32, b: u32) -> TextRange {
        TextRange::new(a, b).expect("a range")
    }

    /// The provenance every `Fake` gives.
    fn prov() -> Provenance {
        Provenance::new(
            "fake-analyzer",
            "9.9",
            BTreeMap::from([("proc_macro".to_owned(), "true".to_owned())]),
            BTreeMap::from([(rel("src/lib.rs"), Fingerprint::of(b"x"))]),
        )
        .expect("a provenance")
    }

    /// A verifier over fixed texts whose checks always pass.
    #[derive(Default)]
    struct Fake {
        texts: BTreeMap<RelPath, String>,
        build_scripts: BTreeSet<RelPath>,
    }
    impl Fake {
        fn with(files: &[(&str, &str)]) -> Self {
            Self {
                texts: files
                    .iter()
                    .map(|(p, t)| (rel(p), (*t).to_owned()))
                    .collect(),
                build_scripts: BTreeSet::new(),
            }
        }
        fn build_script(mut self, path: &str) -> Self {
            self.build_scripts.insert(rel(path));
            self
        }
    }
    impl Verifier for Fake {
        type Session = ();
        fn text(&self, _: &(), path: &RelPath) -> Option<String> {
            self.texts.get(path).cloned()
        }
        fn facts(&self, _: &(), path: &RelPath) -> FileFacts {
            FileFacts {
                is_build_script: self.build_scripts.contains(path),
                in_proc_macro_crate: false,
            }
        }
        fn baseline(&self, _: &()) -> Vec<Finding> {
            vec![]
        }
        fn provenance(&self, _: &()) -> Provenance {
            prov()
        }
        fn check(&self, _: &mut (), _: &[Edit], _: Scope) -> Result<ScopedFindings, CheckError> {
            Ok(ScopedFindings::default())
        }
    }

    /// Every read here covers the whole file, so its fingerprint is the base of the file.
    fn blueprint(
        intent: IntentCall,
        specs: Vec<MutationSpec>,
        reads: Vec<ReadRange>,
        refs: &[&str],
    ) -> Blueprint {
        let bases = reads
            .iter()
            .map(|r| BaseFp::new(r.path().clone(), r.fingerprint()))
            .collect();
        Blueprint::new(BlueprintParts {
            id: id(),
            intent,
            specs,
            reads,
            bases,
            spec_refs: refs
                .iter()
                .map(|r| SpecRef::parse(r).expect("a spec ref"))
                .collect(),
            record: DecisionRecord::new("agent", "because").expect("a record"),
        })
        .expect("a blueprint")
    }
    /// Renames `a` to `b` in `let a = 1;` at `path`.
    fn rename(path: &str) -> Blueprint {
        let text = "let a = 1;";
        blueprint(
            IntentCall::RenameLocal {
                from: Ident::parse("a").expect("ident"),
                to: Ident::parse("b").expect("ident"),
            },
            vec![MutationSpec::replace(rel(path), tr(4, 5), "b").expect("a spec")],
            vec![ReadRange::capture(rel(path), text, tr(0, 10)).expect("a read")],
            &["docs/m3.md#4"],
        )
    }
    /// Extracts `1 + 2` of `f(1 + 2);` at `path`.
    fn extract(path: &str) -> Blueprint {
        let text = "f(1 + 2);";
        blueprint(
            IntentCall::ExtractVariable {
                name: Ident::parse("t").expect("ident"),
            },
            vec![
                MutationSpec::insert(rel(path), 0, "let t = 1 + 2;\n").expect("a spec"),
                MutationSpec::replace(rel(path), tr(2, 7), "t").expect("a spec"),
            ],
            vec![ReadRange::capture(rel(path), text, tr(0, 9)).expect("a read")],
            &[],
        )
    }
    /// Moves `foo` from `src/a.rs` to `src/b.rs`.
    fn move_foo() -> Blueprint {
        let a = "pub fn foo() {}\n";
        blueprint(
            IntentCall::MoveItem {
                item: Ident::parse("foo").expect("ident"),
                to: rel("src/b.rs"),
            },
            vec![
                MutationSpec::replace(rel("src/a.rs"), tr(0, 15), "pub use crate::b::foo;")
                    .expect("a spec"),
                MutationSpec::insert(rel("src/b.rs"), 0, a).expect("a spec"),
            ],
            vec![
                ReadRange::capture(rel("src/a.rs"), a, tr(0, 16)).expect("a read"),
                ReadRange::capture(rel("src/b.rs"), "", tr(0, 0)).expect("a read"),
            ],
            &[],
        )
    }
    fn decided(fake: &Fake, bps: Vec<Blueprint>, rules: &RuleCatalog) -> Resolution {
        let set = BlueprintSet::new(bps).expect("a set");
        let r = apply_blueprints(fake, &mut (), &set, rules, Scope::Affected).expect("a run");
        assert!(matches!(r, Resolution::Decided { .. }), "{r:?}");
        r
    }

    #[test]
    fn decided_lists_the_rule_id() {
        let rule_id = RuleId::from_uuid(uuid::Uuid::now_v7()).expect("v7");
        // The vocabulary, straight from the dispositions.
        assert_eq!(
            disposition_json(Disposition::AutoApplicable { rule: rule_id }),
            ("auto_applicable", json!(rule_id.to_string()))
        );
        assert_eq!(
            disposition_json(Disposition::AfterOuterCheck { rule: rule_id }),
            ("after_outer_check", json!(rule_id.to_string()))
        );
        assert_eq!(
            disposition_json(Disposition::NeedsReview {
                reason: ReviewReason::ScopeTooNarrow
            }),
            ("needs_review/scope_too_narrow", Value::Null)
        );
        // And through a run: the move-item Rule covers a move, which owes an outer check.
        let review = ReviewRecord {
            reviewer: "me".to_owned(),
            reviewed_at_ms: 1,
            note: "n".to_owned(),
        };
        let rule = Rule::register(rule_id, IntentKind::MoveItem, review, Scope::Affected)
            .expect("move-item is registrable");
        let rules = RuleCatalog::new(vec![rule]).expect("a catalog");
        let fake = Fake::with(&[("src/a.rs", "pub fn foo() {}\n"), ("src/b.rs", "")]);
        let v = resolution_to_json(&decided(&fake, vec![move_foo()], &rules));
        let c = &v["candidates"][0];
        assert_eq!(c["disposition"], "after_outer_check");
        assert_eq!(c["rule"], rule_id.to_string());
        assert_eq!(c["marks"], json!(["visibility"]));
        assert_eq!(c["outer_check"], json!(["visibility"]));
    }

    #[test]
    fn outer_check_lists_reasons() {
        let fake = Fake::with(&[("build.rs", "f(1 + 2);")]).build_script("build.rs");
        let v = resolution_to_json(&decided(
            &fake,
            vec![extract("build.rs")],
            &RuleCatalog::default(),
        ));
        assert_eq!(v["resolution"], "decided");
        let c = &v["candidates"][0];
        assert_eq!(c["outer_check"], json!(["borrow", "build_script"]));
        assert_eq!(c["marks"], json!(["borrow", "build_script", "behaviour"]));
        assert_eq!(c["disposition"], "needs_review/behaviour");
        assert_eq!(c["rule"], Value::Null);
        assert_eq!(c["edits"][0]["path"], "build.rs");
        assert_eq!(c["edits"][0]["new_text"], "let t = 1 + 2;\nf(t);");
        assert_eq!(v["evidence"]["scope"], "affected");
        let fake = Fake::with(&[("src/lib.rs", "let a = 1;")]);
        let none = resolution_to_json(&decided(
            &fake,
            vec![rename("src/lib.rs")],
            &RuleCatalog::default(),
        ));
        assert_eq!(none["candidates"][0]["outer_check"], Value::Null);
        assert_eq!(none["candidates"][0]["marks"], json!([]));
    }

    #[test]
    fn decided_lists_the_reads_of_files_only_read() {
        let (lib, other) = (rel("src/lib.rs"), rel("src/o.rs"));
        let text = "let a = 1;";
        let read = ReadRange::capture(other.clone(), "abcdef", tr(1, 4)).expect("a read");
        let bp = Blueprint::new(BlueprintParts {
            id: id(),
            intent: IntentCall::RenameLocal {
                from: Ident::parse("a").expect("ident"),
                to: Ident::parse("b").expect("ident"),
            },
            specs: vec![MutationSpec::replace(lib.clone(), tr(4, 5), "b").expect("a spec")],
            // the read of the edited file is covered by its base and is not in the basis
            reads: vec![
                ReadRange::capture(lib.clone(), text, tr(0, 10)).expect("a read"),
                read.clone(),
            ],
            bases: vec![BaseFp::capture(lib, text)],
            spec_refs: vec![],
            record: DecisionRecord::new("agent", "because").expect("a record"),
        })
        .expect("a blueprint");
        let fake = Fake::with(&[("src/lib.rs", text), ("src/o.rs", "abcdef")]);
        let v = resolution_to_json(&decided(&fake, vec![bp], &RuleCatalog::default()));
        assert_eq!(
            v["candidates"][0]["basis"],
            json!({
                "files": [{
                    "path": "src/lib.rs",
                    "fingerprint": Fingerprint::of(text.as_bytes()).to_string(),
                }],
                "reads": [{
                    "path": "src/o.rs", "start": 1, "end": 4,
                    "fingerprint": read.fingerprint().to_string(),
                }],
            })
        );
    }

    #[test]
    fn decided_lists_record_and_provenance() {
        let fake = Fake::with(&[("src/lib.rs", "let a = 1;")]);
        let bp = rename("src/lib.rs");
        let bp_id = bp.id();
        let v = resolution_to_json(&decided(&fake, vec![bp], &RuleCatalog::default()));
        let path = &v["candidates"][0]["path"];
        assert_eq!(path.as_array().map(Vec::len), Some(1));
        assert_eq!(
            path[0],
            json!({
                "blueprint": bp_id.to_string(),
                "intent": "rename-local a -> b",
                "proposer": "agent",
                "rationale": "because",
                "spec_refs": ["docs/m3.md#4"],
            })
        );
        assert_eq!(
            v["evidence"]["provenance"],
            json!({
                "analyzer": "fake-analyzer",
                "version": "9.9",
                "config": {"proc_macro": "true"},
                "workspace": prov().workspace().to_string(),
            })
        );
        // The decision log goes through the getters of its entries.
        assert_eq!(v["decision_log"][0]["blueprint"], bp_id.to_string());
        assert_eq!(v["decision_log"][0]["stage"], "file_role");
        assert_eq!(v["decision_log"][0]["ok"], true);
    }

    #[test]
    fn failed_lists_stale_reads() {
        let b = id();
        let read = ReadRange::new(
            RelPath::parse("src/lib.rs").unwrap(),
            TextRange::new(1, 4).unwrap(),
            Fingerprint::of(b"abc"),
        );
        let base = BaseFp::new(
            RelPath::parse("src/lib.rs").unwrap(),
            Fingerprint::of(b"abc"),
        );
        let stale = Blocking::Stale {
            blueprint: b,
            drift: vec![
                Drift::File {
                    base,
                    now: Freshness::Stale {
                        expected: Fingerprint::of(b"abc"),
                        actual: Fingerprint::of(b"abcd"),
                    },
                },
                Drift::Read {
                    read: read.clone(),
                    now: Freshness::Stale {
                        expected: Fingerprint::of(b"abc"),
                        actual: Fingerprint::of(b"abd"),
                    },
                },
                Drift::Missing(RelPath::parse("src/gone.rs").unwrap()),
            ],
        };
        let oor = Blocking::Stale {
            blueprint: b,
            drift: vec![Drift::Read {
                read,
                now: Freshness::OutOfRange { len: 2 },
            }],
        };
        let inf = Infeasible::new(vec![stale, oor], vec![], false).expect("blocking");
        let v = resolution_to_json(&Resolution::Failed(inf));
        assert_eq!(v["resolution"], "failed");
        assert_eq!(v["budget_exhausted"], false);
        let first = &v["blocking"][0];
        assert_eq!(first["kind"], "stale");
        assert_eq!(first["blueprint"], b.to_string());
        assert!(first.get("bases").is_none() && first.get("reads").is_none());
        let file = &first["drift"][0];
        assert_eq!(file["kind"], "file");
        assert_eq!(file["path"], "src/lib.rs");
        assert_eq!(file["fingerprint"], Fingerprint::of(b"abc").to_string());
        assert_eq!(file["freshness"], "stale");
        assert_eq!(file["actual"], Fingerprint::of(b"abcd").to_string());
        let read = &first["drift"][1];
        assert_eq!(read["kind"], "read");
        assert_eq!(read["start"], 1);
        assert_eq!(read["end"], 4);
        assert_eq!(read["freshness"], "stale");
        assert_eq!(read["actual"], Fingerprint::of(b"abd").to_string());
        assert_eq!(
            first["drift"][2],
            serde_json::json!({"kind": "missing", "path": "src/gone.rs"})
        );
        let oor = &v["blocking"][1]["drift"][0];
        assert_eq!(oor["kind"], "read");
        assert_eq!(oor["freshness"], "out_of_range");
        assert_eq!(oor["len"], 2);
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
        let fake = Fake::with(&[("src/lib.rs", "let a = 1;")]);
        let r = decided(&fake, vec![rename("src/lib.rs")], &RuleCatalog::default());
        assert_eq!(exit_code(&r), 0);
        let inf = Infeasible::new(vec![], vec![], true).expect("budget exhausted");
        assert_eq!(exit_code(&Resolution::Failed(inf)), 2);
    }
}
