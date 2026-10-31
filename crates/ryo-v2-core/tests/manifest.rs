//! Manifest rules: what the root `Cargo.toml` and every `crates/*/Cargo.toml` must keep. The
//! rules are stated here, once, and `cargo test -p ryo-v2-core` fails when a manifest breaks one.
//!
//! 1. `[workspace.package]` states `rust-version` and has no `authors` (deprecated in Cargo).
//! 2. The root sets `missing_docs` to `warn` or `deny` in `[workspace.lints.rust]`, and every
//!    member opts into the workspace lints with `[lints] workspace = true`; without it Cargo
//!    inherits none of `[workspace.lints]`.
//! 3. A crate that is not `publish = false` has a `description`, a `documentation` URL ending in
//!    `/<its own name>`, 1 to 5 `keywords` (each at most 20 characters, an ASCII letter first,
//!    then ASCII letters, digits, `_`, `-` or `+`), and 1 to 5 `categories`.
//! 4. Such a crate lists its package contents with `include`, which names `LICENSE-MIT` and
//!    `LICENSE-APACHE` and takes nothing under `tests/`: no entry starts with `tests` and no
//!    entry is a top-level glob (`*...`), which would take `tests/` too. The tests reach the
//!    corpus and the workspace root, which are outside the package, so they cannot run from it.
//!    Both licence files exist in the crate directory (as symlinks to the root copies, which
//!    `cargo package` flattens) with the content of the root copies; `include` skips a named
//!    file that is missing without an error.
//! 5. A dependency on another member (any table of dependencies, target-specific ones included)
//!    is `<name>.workspace = true`, so the version and path are written once, in the root.
//!
//! Each rule is a function returning its problems; the `workspace_*` tests run them on the real
//! manifests, the others show that each rule reports a broken manifest.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use toml::{Table, Value};

/// One member manifest: the directory name under `crates/` and the parsed file.
struct Member {
    dir: String,
    manifest: Table,
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn parse(path: &Path) -> Table {
    let text =
        fs::read_to_string(path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    toml::from_str(&text).unwrap_or_else(|e| panic!("cannot parse {}: {e}", path.display()))
}

fn root_manifest() -> Table {
    parse(&root().join("Cargo.toml"))
}

/// Every `crates/*/Cargo.toml`, sorted by directory name.
fn members() -> Vec<Member> {
    let mut out = Vec::new();
    for entry in fs::read_dir(root().join("crates"))
        .expect("crates/ directory")
        .flatten()
    {
        let path = entry.path().join("Cargo.toml");
        if path.is_file() {
            out.push(Member {
                dir: entry.file_name().to_string_lossy().into_owned(),
                manifest: parse(&path),
            });
        }
    }
    out.sort_by(|a, b| a.dir.cmp(&b.dir));
    assert!(!out.is_empty(), "no crates/*/Cargo.toml found");
    out
}

fn package(m: &Member) -> &Table {
    m.manifest
        .get("package")
        .and_then(Value::as_table)
        .unwrap_or_else(|| panic!("crates/{}: no [package]", m.dir))
}

fn name(m: &Member) -> &str {
    package(m)
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("crates/{}: no package.name", m.dir))
}

fn is_published(m: &Member) -> bool {
    package(m).get("publish").and_then(Value::as_bool) != Some(false)
}

fn strings(v: Option<&Value>) -> Option<Vec<&str>> {
    v?.as_array()?.iter().map(Value::as_str).collect()
}

/// Rule 1.
fn workspace_package_problems(root: &Table) -> Vec<String> {
    let mut out = Vec::new();
    let Some(wp) = root
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(Value::as_table)
    else {
        return vec!["root: no [workspace.package]".to_owned()];
    };
    if !wp.get("rust-version").is_some_and(Value::is_str) {
        out.push("root: [workspace.package] has no rust-version".to_owned());
    }
    if wp.contains_key("authors") {
        out.push("root: [workspace.package] has authors (deprecated)".to_owned());
    }
    out
}

/// Rule 2, the root half.
fn workspace_lints_problems(root: &Table) -> Vec<String> {
    let level = root
        .get("workspace")
        .and_then(|w| w.get("lints"))
        .and_then(|l| l.get("rust"))
        .and_then(|r| r.get("missing_docs"))
        .and_then(|v| v.as_str().or_else(|| v.get("level")?.as_str()));
    match level {
        Some("warn" | "deny") => Vec::new(),
        other => vec![format!(
            "root: [workspace.lints.rust] missing_docs is {other:?}, not warn or deny"
        )],
    }
}

/// Rule 2, the member half.
fn lints_problems(m: &Member) -> Vec<String> {
    let ok = m
        .manifest
        .get("lints")
        .and_then(|l| l.get("workspace"))
        .and_then(Value::as_bool)
        == Some(true);
    if ok {
        Vec::new()
    } else {
        vec![format!("crates/{}: no `[lints] workspace = true`", m.dir)]
    }
}

fn keyword_ok(k: &str) -> bool {
    let mut chars = k.chars();
    k.len() <= 20
        && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+'))
}

/// Rule 3.
fn metadata_problems(m: &Member) -> Vec<String> {
    if !is_published(m) {
        return Vec::new();
    }
    let p = package(m);
    let n = name(m);
    let mut out = Vec::new();
    if !p
        .get("description")
        .and_then(Value::as_str)
        .is_some_and(|d| !d.trim().is_empty())
    {
        out.push(format!("{n}: no description"));
    }
    match p.get("documentation").and_then(Value::as_str) {
        Some(d) if d.ends_with(&format!("/{n}")) => {}
        Some(d) => out.push(format!("{n}: documentation `{d}` does not end with `/{n}`")),
        None => out.push(format!(
            "{n}: no documentation of its own (an inherited one is not checked here)"
        )),
    }
    match strings(p.get("keywords")) {
        Some(ks) if (1..=5).contains(&ks.len()) => {
            for k in ks.into_iter().filter(|k| !keyword_ok(k)) {
                out.push(format!(
                    "{n}: keyword `{k}` is not a valid crates.io keyword"
                ));
            }
        }
        _ => out.push(format!("{n}: keywords must be a list of 1 to 5 strings")),
    }
    match strings(p.get("categories")) {
        Some(cs) if (1..=5).contains(&cs.len()) => {}
        _ => out.push(format!("{n}: categories must be a list of 1 to 5 strings")),
    }
    out
}

/// Rule 4.
fn include_problems(m: &Member) -> Vec<String> {
    if !is_published(m) {
        return Vec::new();
    }
    let n = name(m);
    let Some(include) = strings(package(m).get("include")) else {
        return vec![format!("{n}: no `include` list")];
    };
    let mut out = Vec::new();
    for licence in ["LICENSE-MIT", "LICENSE-APACHE"] {
        if !include.contains(&licence) {
            out.push(format!("{n}: include does not name {licence}"));
        }
    }
    for e in include {
        let e = e.trim_start_matches("./").trim_start_matches('/');
        if e.starts_with("tests") || e.starts_with('*') {
            out.push(format!("{n}: include entry `{e}` takes tests/"));
        }
    }
    out
}

/// Rule 4, the files: each licence named in `include` is in `crate_dir` and equals the copy in
/// `root_dir`.
fn licence_file_problems(m: &Member, crate_dir: &Path, root_dir: &Path) -> Vec<String> {
    if !is_published(m) {
        return Vec::new();
    }
    let mut out = Vec::new();
    for licence in ["LICENSE-MIT", "LICENSE-APACHE"] {
        let here = crate_dir.join(licence);
        match (fs::read(&here), fs::read(root_dir.join(licence))) {
            (Ok(a), Ok(b)) if a == b => {}
            (Ok(_), Ok(_)) => out.push(format!(
                "crates/{}: {licence} differs from the root copy",
                m.dir
            )),
            (Err(e), _) => out.push(format!("crates/{}: cannot read {licence}: {e}", m.dir)),
            (_, Err(e)) => out.push(format!("root: cannot read {licence}: {e}")),
        }
    }
    out
}

/// Rule 5. `internal` is the set of member package names.
fn internal_dependency_problems(m: &Member, internal: &BTreeSet<String>) -> Vec<String> {
    const KINDS: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];
    let mut tables: Vec<(String, &Table)> = Vec::new();
    for kind in KINDS {
        if let Some(t) = m.manifest.get(kind).and_then(Value::as_table) {
            tables.push((kind.to_owned(), t));
        }
    }
    if let Some(targets) = m.manifest.get("target").and_then(Value::as_table) {
        for (cfg, t) in targets {
            for kind in KINDS {
                if let Some(t) = t.get(kind).and_then(Value::as_table) {
                    tables.push((format!("target.{cfg}.{kind}"), t));
                }
            }
        }
    }
    let mut out = Vec::new();
    for (kind, deps) in tables {
        for (dep, spec) in deps {
            let real = spec
                .get("package")
                .and_then(Value::as_str)
                .unwrap_or(dep.as_str());
            if !internal.contains(real) {
                continue;
            }
            if spec.get("workspace").and_then(Value::as_bool) != Some(true) {
                out.push(format!(
                    "crates/{}: [{kind}] {dep} is not `.workspace = true`",
                    m.dir
                ));
            }
        }
    }
    out
}

fn assert_none(problems: Vec<String>) {
    assert!(
        problems.is_empty(),
        "manifest problems:\n{}",
        problems.join("\n")
    );
}

#[test]
fn workspace_package_states_rust_version_and_no_authors() {
    assert_none(workspace_package_problems(&root_manifest()));
}

#[test]
fn workspace_members_inherit_the_workspace_lints() {
    let mut problems = workspace_lints_problems(&root_manifest());
    problems.extend(members().iter().flat_map(lints_problems));
    assert_none(problems);
}

#[test]
fn workspace_published_crates_carry_their_metadata() {
    let ms = members();
    assert!(ms.iter().any(is_published), "no publishable crate found");
    assert_none(ms.iter().flat_map(metadata_problems).collect());
}

#[test]
fn workspace_published_crates_ship_licences_and_no_tests() {
    let root = root();
    let mut problems = Vec::new();
    for m in members() {
        problems.extend(include_problems(&m));
        problems.extend(licence_file_problems(
            &m,
            &root.join("crates").join(&m.dir),
            &root,
        ));
    }
    assert_none(problems);
}

#[test]
fn workspace_internal_dependencies_come_from_the_workspace() {
    let ms = members();
    let internal: BTreeSet<String> = ms.iter().map(|m| name(m).to_owned()).collect();
    assert_none(
        ms.iter()
            .flat_map(|m| internal_dependency_problems(m, &internal))
            .collect(),
    );
}

fn member(text: &str) -> Member {
    Member {
        dir: "x".to_owned(),
        manifest: toml::from_str(text).expect("test manifest parses"),
    }
}

const GOOD: &str = r#"
[package]
name = "x"
description = "d"
documentation = "https://docs.rs/x"
keywords = ["a", "b-c"]
categories = ["development-tools"]
include = ["src/**", "Cargo.toml", "LICENSE-MIT", "LICENSE-APACHE"]

[lints]
workspace = true

[dependencies]
y.workspace = true
"#;

fn internal() -> BTreeSet<String> {
    ["x", "y"].map(str::to_owned).into()
}

#[test]
fn good_manifest_has_no_problems() {
    let m = member(GOOD);
    assert_none(lints_problems(&m));
    assert_none(metadata_problems(&m));
    assert_none(include_problems(&m));
    assert_none(internal_dependency_problems(&m, &internal()));
}

#[test]
fn missing_rust_version_and_authors_are_problems() {
    let root: Table = toml::from_str("[workspace.package]\nauthors = [\"a\"]\n").expect("parses");
    assert_eq!(workspace_package_problems(&root).len(), 2);
}

#[test]
fn missing_docs_not_set_in_the_workspace_is_a_problem() {
    let set: Table =
        toml::from_str("[workspace.lints.rust]\nmissing_docs = { level = \"deny\" }\n")
            .expect("parses");
    assert_none(workspace_lints_problems(&set));
    let allowed: Table =
        toml::from_str("[workspace.lints.rust]\nmissing_docs = \"allow\"\n").expect("parses");
    assert_eq!(workspace_lints_problems(&allowed).len(), 1);
    assert_eq!(workspace_lints_problems(&Table::new()).len(), 1);
}

#[test]
fn missing_licence_file_is_a_problem() {
    let root = root();
    let m = member(GOOD);
    assert_none(licence_file_problems(&m, &root, &root));
    let absent = root.join("crates").join("no-such-crate");
    assert_eq!(licence_file_problems(&m, &absent, &root).len(), 2);
}

#[test]
fn member_without_workspace_lints_is_a_problem() {
    let m = member(&GOOD.replace("workspace = true\n\n[dependencies]", "\n[dependencies]"));
    assert_eq!(lints_problems(&m).len(), 1);
}

#[test]
fn wrong_documentation_and_keywords_are_problems() {
    let m = member(
        &GOOD
            .replace("docs.rs/x", "docs.rs/umbrella")
            .replace(r#"["a", "b-c"]"#, r#"["a", "b", "c", "d", "e", "f"]"#)
            .replace(r#"description = "d""#, r#"description = """#),
    );
    assert_eq!(
        metadata_problems(&m).len(),
        3,
        "{:?}",
        metadata_problems(&m)
    );
    let m = member(&GOOD.replace(r#""b-c""#, r#""9lives""#));
    assert_eq!(metadata_problems(&m).len(), 1);
    let m = member(&GOOD.replace(r#"categories = ["development-tools"]"#, ""));
    assert_eq!(metadata_problems(&m).len(), 1);
}

#[test]
fn unpublished_crate_needs_no_metadata() {
    let m = member("[package]\nname = \"x\"\npublish = false\n");
    assert_none(metadata_problems(&m));
    assert_none(include_problems(&m));
}

#[test]
fn include_without_licence_or_with_tests_is_a_problem() {
    let m = member(&GOOD.replace(r#", "LICENSE-APACHE""#, r#", "tests/**""#));
    assert_eq!(include_problems(&m).len(), 2, "{:?}", include_problems(&m));
    let m = member(&GOOD.replace(r#""src/**""#, r#""**/*.rs""#));
    assert_eq!(include_problems(&m).len(), 1);
    let m = member(&GOOD.replace("include = ", "exclude = "));
    assert_eq!(include_problems(&m).len(), 1);
}

#[test]
fn internal_dependency_by_path_is_a_problem() {
    let m = member(
        &(GOOD.replace("y.workspace = true", r#"y = { path = "../y" }"#)
            + "\n[target.'cfg(unix)'.dev-dependencies]\nz = { package = \"x\", version = \"0.1\" }\n"),
    );
    assert_eq!(
        internal_dependency_problems(&m, &internal()).len(),
        2,
        "{:?}",
        internal_dependency_problems(&m, &internal())
    );
}
