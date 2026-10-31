//! `ryo-v2 apply` and `ryo-v2 fingerprint` through the real binary, on throwaway workspaces.

use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::{Command, Output};

const LIB: &str = "pub fn total(xs: &[u32]) -> u32 {\n    let mut acc = 0;\n    for x in xs {\n        acc += x;\n    }\n    acc\n}\n\npub fn twice(n: u32) -> u32 {\n    n * 2\n}\n\npub fn uses() -> u32 {\n    twice(3)\n}\n";

/// A temp workspace that removes itself on drop.
struct Ws {
    dir: PathBuf,
}

impl Ws {
    fn new(test: &str, extra: &[(&str, &str)]) -> Self {
        // Not CARGO_TARGET_TMPDIR: the loader skips every path with a `target` component.
        let dir = std::env::temp_dir().join(format!("ryo-v2-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let files = [
            (
                "Cargo.toml",
                "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
            ),
            ("src/lib.rs", LIB),
        ];
        for (rel, text) in files.iter().chain(extra.iter()) {
            std::fs::write(dir.join(rel), text).unwrap();
        }
        Self { dir }
    }

    fn write(&self, rel: &str, text: &str) {
        std::fs::write(self.dir.join(rel), text).unwrap();
    }

    fn json_file(&self, name: &str, v: &Value) -> PathBuf {
        let p = self.dir.join(name);
        std::fs::write(&p, serde_json::to_string(v).unwrap()).unwrap();
        p
    }
}

impl Drop for Ws {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn ryo(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ryo-v2"))
        .args(args)
        .output()
        .expect("run ryo-v2")
}

/// Byte range `[start, end)` of the `n`-th (0-based) `needle` in `text`, plus `skip`/`len` trimming.
fn at(text: &str, needle: &str, nth: usize, skip: usize, len: usize) -> (usize, usize) {
    let start = text
        .match_indices(needle)
        .nth(nth)
        .unwrap_or_else(|| panic!("{needle:?} #{nth} not found"))
        .0
        + skip;
    (start, start + len)
}

/// A `reads` entry from the `fingerprint` subcommand (the disk at call time).
fn read(ws: &Ws, path: &str, range: (usize, usize)) -> Value {
    let r = format!("{}..{}", range.0, range.1);
    let out = ryo(&["fingerprint", ws.dir.to_str().unwrap(), path, &r]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("fingerprint line")
}

/// A `bases` entry from the `fingerprint` subcommand without a range (the disk at call time).
fn base(ws: &Ws, path: &str) -> Value {
    let out = ryo(&["fingerprint", ws.dir.to_str().unwrap(), path]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("fingerprint line")
}

fn replace(path: &str, range: (usize, usize), text: &str) -> Value {
    json!({"op": "replace", "path": path, "start": range.0, "end": range.1, "text": text})
}

fn blueprint(intent: Value, specs: Vec<Value>, reads: Vec<Value>, bases: Vec<Value>) -> Value {
    json!({"blueprints": [{
        "intent": intent, "specs": specs, "reads": reads, "bases": bases,
        "record": {"proposer": "test", "rationale": "test"},
    }]})
}

fn rules(intent: &str) -> (Value, String) {
    let id = uuid::Uuid::now_v7().to_string();
    let v = json!({"rules": [{
        "id": id, "intent": intent,
        "review": {"reviewer": "me", "reviewed_at_ms": 1, "note": "n"},
        "min_scope": "affected",
    }]});
    (v, id)
}

fn apply(ws: &Ws, bp: &Value, rules: Option<&Value>) -> (Option<i32>, Value, String) {
    let bp_file = ws.json_file("bp.json", bp);
    let mut args = vec![
        "apply".to_owned(),
        ws.dir.to_str().unwrap().to_owned(),
        bp_file.to_str().unwrap().to_owned(),
        "--no-proc-macro".to_owned(),
    ];
    if let Some(r) = rules {
        let f = ws.json_file("rules.json", r);
        args.push("--rules".to_owned());
        args.push(f.to_str().unwrap().to_owned());
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = ryo(&refs);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let stdout = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    (out.status.code(), stdout, stderr)
}

fn rename_acc(ws: &Ws) -> Value {
    let total = at(
        LIB,
        "pub fn total",
        0,
        0,
        LIB.find("\n\npub fn twice").unwrap(),
    );
    let specs = vec![
        replace("src/lib.rs", at(LIB, "acc", 0, 0, 3), "sum"),
        replace("src/lib.rs", at(LIB, "acc", 1, 0, 3), "sum"),
        replace("src/lib.rs", at(LIB, "acc", 2, 0, 3), "sum"),
    ];
    blueprint(
        json!({"kind": "rename-local", "from": "acc", "to": "sum"}),
        specs,
        vec![read(ws, "src/lib.rs", total)],
        vec![base(ws, "src/lib.rs")],
    )
}

#[test]
fn apply_rename_local_is_decided() {
    let ws = Ws::new("apply_rename", &[]);
    let bp = rename_acc(&ws);
    let (code, out, err) = apply(&ws, &bp, None);
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
    assert_eq!(code, Some(0), "{err}");
    assert!(err.contains("generated id"), "{err}");
    assert_eq!(out["resolution"], "decided");
    let c = &out["candidates"][0];
    assert_eq!(c["disposition"], "needs_review/no_rule");
    assert_eq!(c["outer_check"], Value::Null);
    let new_text = c["edits"][0]["new_text"].as_str().unwrap();
    assert!(
        !new_text.contains("acc") && new_text.contains("sum"),
        "{new_text}"
    );
    // The dry run wrote nothing.
    assert_eq!(
        std::fs::read_to_string(ws.dir.join("src/lib.rs")).unwrap(),
        LIB
    );

    // A rename is checked by text match only, so no rule may cover it.
    let (rv, _) = rules("rename-local");
    let (code, _, err) = apply(&ws, &bp, Some(&rv));
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("text match"), "{err}");
}

#[test]
fn apply_breaking_rename_is_failed_new_errors() {
    let ws = Ws::new("apply_breaking", &[]);
    let name = at(LIB, "pub fn twice", 0, 7, 5);
    let def = at(
        LIB,
        "pub fn twice",
        0,
        0,
        "pub fn twice(n: u32) -> u32 {\n    n * 2\n}".len(),
    );
    let bp = blueprint(
        json!({"kind": "rename-local", "from": "twice", "to": "thrice"}),
        vec![replace("src/lib.rs", name, "thrice")],
        vec![read(&ws, "src/lib.rs", def)],
        vec![base(&ws, "src/lib.rs")],
    );
    let (code, out, err) = apply(&ws, &bp, None);
    assert_eq!(code, Some(2), "{err}\n{out}");
    assert_eq!(out["resolution"], "failed");
    let blockers = out["blocking"].as_array().unwrap();
    assert_eq!(blockers.len(), 1, "{out}");
    assert_eq!(blockers[0]["kind"], "new_errors");
    let errors = blockers[0]["errors"].as_array().unwrap();
    assert!(
        errors
            .iter()
            .any(|e| e["code"] == "E0425" && e["line"] == 14),
        "{errors:?}"
    );
    assert_eq!(out["near_miss"].as_array().unwrap().len(), 1, "{out}");
}

#[test]
fn apply_after_disk_change_is_failed_stale() {
    let ws = Ws::new("apply_stale", &[]);
    let bp = rename_acc(&ws);
    // One byte inside the read range changes (same length): the base drifted, and the read of
    // an edited file is covered by its base, so no read is listed.
    ws.write("src/lib.rs", &LIB.replacen("= 0;", "= 1;", 1));
    let (code, out, err) = apply(&ws, &bp, None);
    assert_eq!(code, Some(2), "{err}\n{out}");
    assert_eq!(out["blocking"][0]["kind"], "stale", "{out}");
    assert_eq!(
        out["blocking"][0]["bases"][0]["path"], "src/lib.rs",
        "{out}"
    );
    assert_eq!(
        out["blocking"][0]["bases"][0]["freshness"], "stale",
        "{out}"
    );
    assert!(
        out["blocking"][0]["reads"].as_array().unwrap().is_empty(),
        "{out}"
    );
}

#[test]
fn apply_after_insert_above_is_failed_stale() {
    let ws = Ws::new("apply_insert_above", &[]);
    // The read covers exactly the line `    let mut acc = 0;\n`; the spec renames its `acc`.
    let line = "    let mut acc = 0;\n";
    let line_at = at(LIB, line, 0, 0, line.len());
    let bp = blueprint(
        json!({"kind": "rename-local", "from": "acc", "to": "sum"}),
        vec![replace("src/lib.rs", at(LIB, "acc", 0, 0, 3), "sum")],
        vec![read(&ws, "src/lib.rs", line_at)],
        vec![base(&ws, "src/lib.rs")],
    );
    // An identical copy of the line goes directly above it: the bytes at the read's old offsets
    // are unchanged, but everything after them has moved.
    let moved = format!("{}{line}{}", &LIB[..line_at.0], &LIB[line_at.0..]);
    assert_eq!(&moved[line_at.0..line_at.1], line, "the bug is exercised");
    ws.write("src/lib.rs", &moved);
    let (code, out, err) = apply(&ws, &bp, None);
    assert_eq!(code, Some(2), "{err}\n{out}");
    assert_eq!(out["blocking"][0]["kind"], "stale", "{out}");
    assert_eq!(
        out["blocking"][0]["bases"][0]["path"], "src/lib.rs",
        "{out}"
    );
    assert_eq!(
        out["blocking"][0]["bases"][0]["freshness"], "stale",
        "{out}"
    );
}

#[test]
fn apply_cargo_toml_edit_is_unverifiable() {
    let ws = Ws::new("apply_manifest", &[]);
    let manifest = std::fs::read_to_string(ws.dir.join("Cargo.toml")).unwrap();
    let name = at(&manifest, "probe", 0, 0, 5);
    let bp = blueprint(
        json!({"kind": "rename-local", "from": "probe", "to": "probe2"}),
        vec![replace("Cargo.toml", name, "probe2")],
        vec![read(&ws, "Cargo.toml", name)],
        vec![base(&ws, "Cargo.toml")],
    );
    let (code, out, err) = apply(&ws, &bp, None);
    assert_eq!(code, Some(2), "{err}\n{out}");
    assert_eq!(out["blocking"][0]["kind"], "unverifiable", "{out}");
    assert_eq!(out["blocking"][0]["role"], "manifest", "{out}");
}

#[test]
fn apply_build_rs_edit_needs_outer_check() {
    let build = "fn main() {\n    let x = 1;\n    let _ = x;\n}\n";
    let ws = Ws::new("apply_build_rs", &[("build.rs", build)]);
    let bp = blueprint(
        json!({"kind": "rename-local", "from": "x", "to": "y"}),
        vec![
            replace("build.rs", at(build, "let x", 0, 4, 1), "y"),
            replace("build.rs", at(build, "= x;", 0, 2, 1), "y"),
        ],
        vec![read(&ws, "build.rs", (0, build.len()))],
        vec![base(&ws, "build.rs")],
    );
    let (code, out, err) = apply(&ws, &bp, None);
    assert_eq!(code, Some(0), "{err}\n{out}");
    let c = &out["candidates"][0];
    assert_eq!(c["outer_check"], json!(["build_script"]), "{out}");
    assert_eq!(c["disposition"], "needs_review/no_rule");
}

#[test]
fn apply_extract_variable_needs_review_behaviour() {
    let ws = Ws::new("apply_extract", &[]);
    let uses = at(
        LIB,
        "pub fn uses",
        0,
        0,
        LIB.len() - LIB.find("pub fn uses").unwrap(),
    );
    let at_twice = LIB.find("twice(3)").unwrap();
    let bp = blueprint(
        json!({"kind": "extract-variable", "name": "n"}),
        vec![
            json!({"op": "insert", "path": "src/lib.rs", "at": at_twice, "text": "let n = 3;\n    "}),
            replace("src/lib.rs", at(LIB, "(3)", 0, 1, 1), "n"),
        ],
        vec![read(&ws, "src/lib.rs", uses)],
        vec![base(&ws, "src/lib.rs")],
    );
    let (code, out, err) = apply(&ws, &bp, None);
    assert_eq!(code, Some(0), "{err}\n{out}");
    let c = &out["candidates"][0];
    assert_eq!(c["outer_check"], json!(["borrow"]), "{out}");
    assert_eq!(c["marks"], json!(["borrow", "behaviour"]), "{out}");
    assert_eq!(c["disposition"], "needs_review/behaviour");

    let (rv, _) = rules("extract-variable");
    let (code, _, err) = apply(&ws, &bp, Some(&rv));
    assert_eq!(code, Some(1));
    assert!(err.contains("behaviour"), "{err}");
}

#[test]
fn fingerprint_prints_shared_vector() {
    let ws = Ws::new("fingerprint_vector", &[]);
    ws.write("src/lib.rs", "a = 1");
    let out = ryo(&[
        "fingerprint",
        ws.dir.to_str().unwrap(),
        "src/lib.rs",
        "0..5",
    ]);
    assert!(out.status.success());
    let line = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        line.trim(),
        r#"{"path":"src/lib.rs","start":0,"end":5,"fingerprint":"fnv1a64:c4f954ac18a041ff"}"#
    );
}

#[test]
fn fingerprint_without_range_is_whole_file() {
    let ws = Ws::new("fingerprint_whole", &[]);
    ws.write("src/lib.rs", "a = 1");
    let out = ryo(&["fingerprint", ws.dir.to_str().unwrap(), "src/lib.rs"]);
    assert!(out.status.success());
    let line = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        line.trim(),
        r#"{"path":"src/lib.rs","fingerprint":"fnv1a64:c4f954ac18a041ff"}"#
    );
}
