//! The unsafe ceiling: where the workspace may use `unsafe`, stated once, and `cargo test -p
//! ryo-v2-core` fails when the code and the ledger part.
//!
//! 1. The root `Cargo.toml` denies the lint ([`LINT`]) in `[workspace.lints.rust]` and excludes
//!    `vendor` from the workspace: the lint binds the members, every one of which opts into the
//!    workspace lints (`tests/manifest.rs` rule 2); a vendored crate keeps its own rules.
//! 2. A Rust file under `crates/` (build output skipped) that names the lint at all, in an
//!    allowance or anywhere else, is one of [`ALLOWED`]; each of those names it in an inner
//!    `allow` attribute, so the allowance covers that module and nothing else.
//! 3. `UNSAFE.tsv` at the root is the ledger of those allowances: a header of the seven columns
//!    of [`COLUMNS`], then exactly one row per allowed file, with a verdict that is neither empty
//!    nor `unknown`.
//!
//! Each rule is a function returning its problems; the `workspace_*` tests run them on the real
//! tree, the others show that each rule reports a broken one.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// The lint's name, built so that this file does not name it itself (rule 2).
const LINT: &str = concat!("unsafe", "_code");

/// The files that may allow the lint: the counting allocator of the engine probe.
const ALLOWED: [&str; 1] = ["crates/ryo-v2-host/src/bin/engine-probe/alloc.rs"];

/// The columns of `UNSAFE.tsv`, in order.
const COLUMNS: [&str; 7] = [
    "file",
    "enclosing item",
    "kind",
    "classification",
    "SAFETY",
    "verdict",
    "note",
];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Rule 1 on the text of the root manifest.
fn manifest_problems(text: &str) -> Vec<String> {
    let table: toml::Table = match toml::from_str(text) {
        Ok(t) => t,
        Err(e) => return vec![format!("Cargo.toml does not parse: {e}")],
    };
    let mut out = Vec::new();
    let workspace = table.get("workspace").and_then(|w| w.as_table());
    let level = workspace
        .and_then(|w| w.get("lints"))
        .and_then(|l| l.get("rust"))
        .and_then(|r| r.get(LINT));
    let denied = match level {
        Some(toml::Value::String(s)) => s == "deny" || s == "forbid",
        Some(toml::Value::Table(t)) => t
            .get("level")
            .and_then(|l| l.as_str())
            .is_some_and(|s| s == "deny" || s == "forbid"),
        _ => false,
    };
    if !denied {
        out.push(format!(
            "[workspace.lints.rust] does not deny {LINT}: {level:?}"
        ));
    }
    let excludes_vendor = workspace
        .and_then(|w| w.get("exclude"))
        .and_then(|e| e.as_array())
        .is_some_and(|e| e.iter().any(|v| v.as_str() == Some("vendor")));
    if !excludes_vendor {
        out.push("[workspace] exclude does not name vendor".to_owned());
    }
    out
}

/// Every `.rs` file under `dir`, `target` directories skipped.
fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries =
        fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry
            .unwrap_or_else(|e| panic!("cannot read an entry of {}: {e}", dir.display()))
            .path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Rule 2 on `(path relative to the root, text)` of every Rust file under `crates/`; returns the
/// problems and the files that name the lint.
fn allowance_problems(files: &[(String, String)]) -> (Vec<String>, BTreeSet<String>) {
    let inner_allow = format!("#![allow({LINT})]");
    let mut out = Vec::new();
    let mut naming = BTreeSet::new();
    for (rel, text) in files {
        if !text.contains(LINT) {
            continue;
        }
        naming.insert(rel.clone());
        if !ALLOWED.contains(&rel.as_str()) {
            out.push(format!(
                "{rel}: names {LINT}, and is not one of {ALLOWED:?}"
            ));
        } else if !text.lines().any(|l| l.trim() == inner_allow) {
            out.push(format!("{rel}: has no `{inner_allow}` line"));
        }
    }
    (out, naming)
}

/// Rule 3 on the text of `UNSAFE.tsv` and the files that name the lint.
fn ledger_problems(tsv: &str, naming: &BTreeSet<String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut lines = tsv.lines();
    let header: Vec<&str> = lines.next().unwrap_or_default().split('\t').collect();
    if header != COLUMNS {
        out.push(format!("UNSAFE.tsv header {header:?}, not {COLUMNS:?}"));
    }
    let mut listed = BTreeSet::new();
    for (i, line) in lines.enumerate() {
        let cells: Vec<&str> = line.split('\t').collect();
        if cells.len() != COLUMNS.len() {
            out.push(format!(
                "UNSAFE.tsv row {}: {} cells, not {}",
                i + 1,
                cells.len(),
                COLUMNS.len()
            ));
            continue;
        }
        let verdict = cells[5].trim();
        if verdict.is_empty() || verdict == "unknown" {
            out.push(format!("UNSAFE.tsv row {}: verdict {verdict:?}", i + 1));
        }
        if !listed.insert(cells[0].to_owned()) {
            out.push(format!("UNSAFE.tsv: {} has two rows", cells[0]));
        }
    }
    if &listed != naming {
        out.push(format!(
            "UNSAFE.tsv lists {listed:?}; the files that allow {LINT} are {naming:?}"
        ));
    }
    out
}

fn workspace_files() -> Vec<(String, String)> {
    let root = root();
    let mut paths = Vec::new();
    rust_files(&root.join("crates"), &mut paths);
    paths
        .into_iter()
        .map(|p| {
            let rel = p
                .strip_prefix(&root)
                .unwrap_or_else(|_| panic!("{} is under the root", p.display()))
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            let text = fs::read_to_string(&p)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()));
            (rel, text)
        })
        .collect()
}

fn assert_none(problems: Vec<String>) {
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn workspace_denies_unsafe_and_excludes_vendor() {
    let text = fs::read_to_string(root().join("Cargo.toml")).expect("root Cargo.toml");
    assert_none(manifest_problems(&text));
}

#[test]
fn workspace_allows_unsafe_only_where_the_ledger_says() {
    let (problems, naming) = allowance_problems(&workspace_files());
    assert_none(problems);
    let expected: BTreeSet<String> = ALLOWED.iter().map(|s| (*s).to_owned()).collect();
    assert_eq!(naming, expected, "the allowed files must each allow it");
    let tsv = fs::read_to_string(root().join("UNSAFE.tsv")).expect("UNSAFE.tsv at the root");
    assert_none(ledger_problems(&tsv, &naming));
}

#[test]
fn a_manifest_without_the_lint_or_the_exclusion_is_a_problem() {
    let lint = format!("[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.lints.rust]\n{LINT}");
    let good = format!("{lint} = \"deny\"\n").replace("members", "exclude = [\"vendor\"]\nmembers");
    assert_none(manifest_problems(&good));
    let warned = good.replace("\"deny\"", "\"warn\"");
    assert_eq!(manifest_problems(&warned).len(), 1);
    let unexcluded = good.replace("exclude = [\"vendor\"]\n", "");
    assert_eq!(manifest_problems(&unexcluded).len(), 1);
    assert_eq!(manifest_problems("[workspace]\n").len(), 2);
}

#[test]
fn an_allowance_elsewhere_or_off_the_ledger_is_a_problem() {
    let allow = format!("//! x\n#![allow({LINT})]\n");
    let stray = (
        "crates/ryo-v2-core/src/x.rs".to_owned(),
        format!("#[allow({LINT})]\nfn f() {{}}\n"),
    );
    let alloc = (ALLOWED[0].to_owned(), allow.clone());
    let (problems, naming) = allowance_problems(&[alloc.clone(), stray]);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("crates/ryo-v2-core/src/x.rs"));
    assert_eq!(naming.len(), 2);
    // The allowed file without its inner attribute.
    let outer = (
        ALLOWED[0].to_owned(),
        format!("#[allow({LINT})]\nmod m {{}}\n"),
    );
    assert_eq!(allowance_problems(&[outer]).0.len(), 1);

    let (_, naming) = allowance_problems(&[alloc]);
    let header = COLUMNS.join("\t");
    let row = |verdict: &str| format!("{}\ti\tk\tc\ts\t{verdict}\tn", ALLOWED[0]);
    assert_none(ledger_problems(
        &format!("{header}\n{}\n", row("keep")),
        &naming,
    ));
    for bad in [
        format!("{header}\n{}\n", row("unknown")),
        format!("{header}\n{}\n", row("")),
        format!("{header}\n"),
        format!("{header}\n{}\n{}\n", row("keep"), row("keep")),
        format!("file\tverdict\n{}\n", row("keep")),
        format!("{header}\n{}\tx\n", row("keep")),
    ] {
        assert!(!ledger_problems(&bad, &naming).is_empty(), "{bad}");
    }
}
