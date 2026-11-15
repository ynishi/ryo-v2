//! `ryo-v2 search` through the real binary, on throwaway workspaces.

use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::{Command, Output};

const LIB: &str = "pub fn total(xs: &[u32]) -> u32 {\n    let mut acc = 0;\n    for x in xs {\n        acc += x;\n    }\n    acc\n}\n\npub fn twice(n: u32) -> u32 {\n    n * 2\n}\n\npub fn uses() -> u32 {\n    twice(3)\n}\n";

/// A temp workspace that removes itself on drop.
struct Ws {
    dir: PathBuf,
}

impl Ws {
    fn new(test: &str) -> Self {
        // Not CARGO_TARGET_TMPDIR: the loader skips every path with a `target` component.
        let dir = std::env::temp_dir().join(format!("ryo-v2-search-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let files = [
            (
                "Cargo.toml",
                "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
            ),
            ("src/lib.rs", LIB),
        ];
        for (rel, text) in files {
            std::fs::write(dir.join(rel), text).unwrap();
        }
        Self { dir }
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

/// The `acc` to `sum` rename of `total`, as a root blueprint with a fixed id.
fn rename_acc(ws: &Ws, id: &str) -> Value {
    let total = at(
        LIB,
        "pub fn total",
        0,
        0,
        LIB.find("\n\npub fn twice").unwrap(),
    );
    json!({
        "id": id,
        "command": {"kind": "rename-local", "from": "acc", "to": "sum"},
        "splices": [
            replace("src/lib.rs", at(LIB, "acc", 0, 0, 3), "sum"),
            replace("src/lib.rs", at(LIB, "acc", 1, 0, 3), "sum"),
            replace("src/lib.rs", at(LIB, "acc", 2, 0, 3), "sum"),
        ],
        "reads": [read(ws, "src/lib.rs", total)],
        "bases": [base(ws, "src/lib.rs")],
        "record": {"proposer": "test", "rationale": "test"},
    })
}

fn search(ws: &Ws, request: &Value) -> (Option<i32>, Value, String) {
    let file = ws.json_file("request.json", request);
    let out = ryo(&[
        "search",
        ws.dir.to_str().unwrap(),
        file.to_str().unwrap(),
        "--no-proc-macro",
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let stdout = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    (out.status.code(), stdout, stderr)
}

#[test]
fn search_returns_decided_with_rank() {
    let ws = Ws::new("decided");
    let id = uuid::Uuid::now_v7().to_string();
    let request = json!({
        "roots": [rename_acc(&ws, &id)],
        "budget": {"beam": 5, "max_depth": 1},
    });
    let (code, out, stderr) = search(&ws, &request);
    assert_eq!(code, Some(0), "{stderr}\n{out}");
    assert_eq!(out["resolution"]["resolution"], "decided", "{out}");
    let first = &out["ranking"][0];
    assert_eq!(first["rank"], 1, "{out}");
    assert_eq!(first["depth"], 1, "{out}");
    assert_eq!(first["path"][0], id.as_str(), "{out}");
    assert_eq!(first["reward"]["passed"], true, "{out}");
    // The judge is asked about expanded children only, never about the roots.
    assert!(out["judge"]["providers"].is_array(), "{out}");
    assert!(
        out["judge"]["errors"].as_array().unwrap().is_empty(),
        "{out}"
    );
    assert!(out["depths"][0]["checked"].as_u64().unwrap() >= 1, "{out}");
    assert_eq!(out["budget_exhausted"], false, "{out}");
}

#[test]
fn search_without_places_or_roots_is_error() {
    let ws = Ws::new("empty");
    let (code, out, stderr) = search(&ws, &json!({}));
    assert_eq!(code, Some(1), "{stderr}\n{out}");
    assert!(out.is_null(), "no JSON on stdout: {out}");
    assert!(stderr.contains("no places and no roots"), "{stderr}");
    // Unknown fields are refused before the workspace is loaded.
    let (code, _, stderr) = search(&ws, &json!({"roots": [], "places": [], "extra": 1}));
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("extra"), "{stderr}");
}
