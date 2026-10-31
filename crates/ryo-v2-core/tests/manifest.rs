//! Manifest rules: what the root `Cargo.toml` and every `crates/*/Cargo.toml` must keep. The
//! rules are stated here, once, and `cargo test -p ryo-v2-core` fails when a manifest breaks one.
//!
//! 1. `[workspace.package]` states `rust-version`, equal to the major.minor of the channel in
//!    `rust-toolchain.toml`, and has no `authors` (deprecated in Cargo). Every member inherits
//!    it with `rust-version.workspace = true`; a member without it publishes no minimum version.
//! 2. The root sets `missing_docs` to `warn` or `deny` in `[workspace.lints.rust]`, and every
//!    member opts into the workspace lints with `[lints] workspace = true`; without it Cargo
//!    inherits none of `[workspace.lints]`.
//! 3. The crates that are not `publish = false` are exactly those in [`PUBLISHED`]. Each has a `description`, a `documentation` URL ending in
//!    `/<its own name>`, 1 to 5 `keywords` (each at most 20 characters, an ASCII letter first,
//!    then ASCII letters, digits, `_`, `-` or `+`), and 1 to 5 non-empty `categories`.
//! 4. Such a crate sets `readme` (Cargo copies the file into the package root) and lists its
//!    package contents with `include`, which has an entry `src/**` or `src`, names `LICENSE-MIT`
//!    and `LICENSE-APACHE`, and takes nothing under `tests/`: no entry starts with `tests` and no
//!    entry is a top-level glob (`*...`), which would take `tests/` too. The tests reach the
//!    corpus and the workspace root, which are outside the package, so they cannot run from it.
//!    Both licence files are symlinks in the crate directory (`cargo package` flattens them)
//!    whose content is that of the root copies, so the copies cannot drift; `include` skips a
//!    named file that is missing without an error.
//! 5. A dependency on another member (any table of dependencies, target-specific ones and the
//!    underscore spellings included) is `<name>.workspace = true`, so the version and path are
//!    written once, in the root.
//! 6. No Rust file under `crates/` (sources, tests, build scripts; `target` skipped) sets the
//!    level of `missing_docs` in an attribute (`#![warn(missing_docs)]`, `#[allow(missing_docs)]`,
//!    one spread over several lines, ...); the level is set once, by rule 2.
//! 7. The gate recipes in the `Justfile`: `rust-clippy` runs with `CARGO_BUILD_WARNINGS=deny`
//!    (warnings fail the gate) and `--locked`, `rust-test` with `--locked`, and `check` runs
//!    both (comment lines in a recipe do not count). `rust-clippy` runs before `rust-test` in
//!    `check`, so without its own `--locked` it would rewrite a stale `Cargo.lock` before
//!    `rust-test` could refuse it.
//! 8. The base and the ledgers in the `Justfile`: `check` runs `base-check` (as does
//!    `branch-check`), and `base-check` refuses a branch that `origin/main`, or the local `main`,
//!    is no ancestor of (`merge-base --is-ancestor`), saying in one line that it skipped outside
//!    a git checkout; `ledger-merge-install` sets `merge.ledger.driver` to the driver with git's
//!    arguments `%O %A %B %L %P`, falling back to `git merge-file` when it is missing or fails. The lines that carry these
//!    decisions are pinned verbatim and in order ([`BASE_CHECK_LINES`], [`LEDGER_INSTALL_LINES`]),
//!    so a negated or reversed check fails the rule, not only a missing word.
//!
//! Each rule is a function returning its problems; the `workspace_*` tests run them on the real
//! manifests, the others show that each rule reports a broken manifest.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use toml::{Table, Value};

/// The crates meant for crates.io; every other member is `publish = false`.
const PUBLISHED: [&str; 4] = ["ryo-v2", "ryo-v2-core", "ryo-v2-host", "ryo-v2-ra"];

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

/// Rule 1, the toolchain: `rust-version` is the major.minor of the toolchain `channel`.
fn rust_version_matches_toolchain(root: &Table, toolchain: &Table) -> Vec<String> {
    let rv = root
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("rust-version"))
        .and_then(Value::as_str);
    let channel = toolchain
        .get("toolchain")
        .and_then(|t| t.get("channel"))
        .and_then(Value::as_str);
    match (rv, channel) {
        (Some(rv), Some(ch)) if ch.splitn(3, '.').take(2).collect::<Vec<_>>().join(".") == rv => {
            Vec::new()
        }
        (rv, ch) => vec![format!(
            "root: rust-version {rv:?} is not the major.minor of the toolchain channel {ch:?}"
        )],
    }
}

/// Rule 1, the member half.
fn member_rust_version_problems(m: &Member) -> Vec<String> {
    let inherited = package(m)
        .get("rust-version")
        .and_then(|v| v.get("workspace"))
        .and_then(Value::as_bool)
        == Some(true);
    if inherited {
        Vec::new()
    } else {
        vec![format!(
            "crates/{}: no `rust-version.workspace = true`",
            m.dir
        )]
    }
}

/// Rule 3, the set: the publishable crates are exactly `expected`.
fn published_set_problems(ms: &[Member], expected: &[&str]) -> Vec<String> {
    let found: BTreeSet<&str> = ms.iter().filter(|m| is_published(m)).map(name).collect();
    let expected: BTreeSet<&str> = expected.iter().copied().collect();
    if found == expected {
        Vec::new()
    } else {
        vec![format!(
            "publishable crates are {found:?}, expected {expected:?} (`PUBLISHED`)"
        )]
    }
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
        Some(cs) if (1..=5).contains(&cs.len()) && cs.iter().all(|c| !c.trim().is_empty()) => {}
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
    let mut out = Vec::new();
    let readme = package(m).get("readme");
    if !readme.is_some_and(|r| {
        r.as_str().is_some_and(|r| !r.is_empty())
            || r.get("workspace").and_then(Value::as_bool) == Some(true)
    }) {
        out.push(format!("{n}: no readme"));
    }
    let Some(include) = strings(package(m).get("include")) else {
        out.push(format!("{n}: no `include` list"));
        return out;
    };
    if !include.iter().any(|e| matches!(*e, "src/**" | "src")) {
        out.push(format!("{n}: include has no `src/**` entry"));
    }
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
        if !fs::symlink_metadata(&here).is_ok_and(|md| md.file_type().is_symlink()) {
            out.push(format!("crates/{}: {licence} is not a symlink", m.dir));
        }
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
    const KINDS: [&str; 5] = [
        "dependencies",
        "dev-dependencies",
        "build-dependencies",
        "dev_dependencies",
        "build_dependencies",
    ];
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

/// Rule 6: the attribute lines of `text` that set a level for `missing_docs`.
fn missing_docs_attribute_problems(rel_path: &str, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_attribute = false;
    for (i, line) in text.lines().enumerate() {
        let l = line.trim_start();
        if l.starts_with("#![") || l.starts_with("#[") {
            in_attribute = true;
        }
        if in_attribute && l.contains("missing_docs") {
            out.push(format!(
                "{rel_path}:{}: sets missing_docs in an attribute",
                i + 1
            ));
        }
        if l.contains(']') {
            in_attribute = false;
        }
    }
    out
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            if entry.file_name() != "target" {
                rust_files(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The body lines of recipe `name` in a Justfile: the indented lines after `name:` or
/// `name <params>:`; for `check` the dependency list on the header line is returned too.
fn recipe<'a>(justfile: &'a str, name: &str) -> Vec<&'a str> {
    let mut lines = justfile.lines();
    let Some(header) = lines.by_ref().find(|l| {
        l.strip_prefix(name)
            .is_some_and(|rest| rest.starts_with(':') || rest.starts_with(' '))
    }) else {
        return Vec::new();
    };
    let mut out = vec![header];
    out.extend(
        lines
            .take_while(|l| l.starts_with(' ') || l.starts_with('\t'))
            .filter(|l| !l.trim_start().starts_with('#')),
    );
    out
}

/// Rule 7.
fn gate_recipe_problems(justfile: &str) -> Vec<String> {
    let mut out = Vec::new();
    let clippy = recipe(justfile, "rust-clippy").join("\n");
    for needle in ["CARGO_BUILD_WARNINGS=deny", "cargo clippy", "--locked"] {
        if !clippy.contains(needle) {
            out.push(format!("Justfile: rust-clippy does not contain `{needle}`"));
        }
    }
    let test = recipe(justfile, "rust-test").join("\n");
    for needle in ["cargo test", "--locked"] {
        if !test.contains(needle) {
            out.push(format!("Justfile: rust-test does not contain `{needle}`"));
        }
    }
    let check = recipe(justfile, "check");
    let deps = check.first().copied().unwrap_or("");
    for dep in ["rust-clippy", "rust-test"] {
        if !deps.split_whitespace().any(|d| d == dep) {
            out.push(format!("Justfile: check does not run {dep}"));
        }
    }
    out
}

/// Rule 8, the lines it pins: per recipe, groups of consecutive lines (trimmed) that must appear
/// in this order. The decisions are in the exact text: which ref wins, the direction of the
/// ancestry check, which situations skip and pass, and the driver line with its fallback.
const BASE_CHECK_LINES: [&[&str]; 5] = [
    &[
        r#"echo "base-check: skipped (not a git checkout)""#,
        "exit 0",
    ],
    &[r#"echo "base-check: skipped (shallow clone)""#, "exit 0"],
    &[
        r#"if [ "$branch" = "main" ]; then"#,
        r#"echo "base-check: on main""#,
        "exit 0",
    ],
    &[
        "if git rev-parse --verify --quiet refs/remotes/origin/main >/dev/null; then",
        r#"base="origin/main""#,
        "elif git rev-parse --verify --quiet refs/heads/main >/dev/null; then",
        r#"base="main""#,
        "else",
        r#"echo "base-check: skipped (no main or origin/main)""#,
        "exit 0",
    ],
    &[
        r#"if ! git merge-base --is-ancestor "$base" HEAD; then"#,
        r#"echo "base-check: $branch does not contain $base; merge $base into it and check again" >&2"#,
        "exit 1",
    ],
];

/// Rule 8, the driver lines of `ledger-merge-install`.
const LEDGER_INSTALL_LINES: [&[&str]; 2] = [
    &[
        r#"dest="$(cd "$(git rev-parse --git-common-dir)" && pwd -P)/ledger-merge""#,
        r#"install -m 755 "$built" "$dest""#,
    ],
    &[
        r#"git config merge.ledger.driver "if [ -x '$dest' ]; then '$dest' %O %A %B %L %P; s=\$?; [ \$s -le 1 ] && exit \$s; fi; git merge-file --marker-size=%L %A %O %B""#,
    ],
];

/// The groups of `groups` that are not found in `body`, in order, each after the previous one.
fn missing_line_groups(name: &str, body: &[&str], groups: &[&[&str]]) -> Vec<String> {
    let lines: Vec<&str> = body.iter().map(|l| l.trim()).collect();
    let mut from = 0;
    let mut out = Vec::new();
    for group in groups {
        let found = (from..lines.len()).find(|&i| lines[i..].starts_with(group));
        match found {
            Some(i) => from = i + group.len(),
            None => out.push(format!(
                "Justfile: {name} does not have, in order, the lines {group:?}"
            )),
        }
    }
    out
}

/// Rule 8.
fn base_and_ledger_recipe_problems(justfile: &str) -> Vec<String> {
    let mut out = Vec::new();
    for runner in ["check", "branch-check"] {
        let header = recipe(justfile, runner).first().copied().unwrap_or("");
        let deps = header.split_once(':').map_or("", |(_, d)| d);
        if !deps.split_whitespace().any(|d| d == "base-check") {
            out.push(format!("Justfile: {runner} does not run base-check"));
        }
    }
    out.extend(missing_line_groups(
        "base-check",
        &recipe(justfile, "base-check"),
        &BASE_CHECK_LINES,
    ));
    out.extend(missing_line_groups(
        "ledger-merge-install",
        &recipe(justfile, "ledger-merge-install"),
        &LEDGER_INSTALL_LINES,
    ));
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
    let root_manifest = root_manifest();
    let mut problems = workspace_package_problems(&root_manifest);
    problems.extend(rust_version_matches_toolchain(
        &root_manifest,
        &parse(&root().join("rust-toolchain.toml")),
    ));
    problems.extend(members().iter().flat_map(member_rust_version_problems));
    assert_none(problems);
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
    let mut problems = published_set_problems(&ms, &PUBLISHED);
    problems.extend(ms.iter().flat_map(metadata_problems));
    assert_none(problems);
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
fn workspace_rust_files_set_no_missing_docs_level() {
    let root = root();
    let mut files = Vec::new();
    for entry in fs::read_dir(root.join("crates"))
        .expect("crates/")
        .flatten()
    {
        rust_files(&entry.path(), &mut files);
    }
    assert!(!files.is_empty(), "no source files found");
    let mut problems = Vec::new();
    for f in files {
        let text = fs::read_to_string(&f).unwrap_or_else(|e| panic!("{}: {e}", f.display()));
        let rel = f.strip_prefix(&root).unwrap_or(&f).display().to_string();
        problems.extend(missing_docs_attribute_problems(&rel, &text));
    }
    assert_none(problems);
}

#[test]
fn workspace_gate_recipes_deny_warnings_and_lock() {
    let justfile = fs::read_to_string(root().join("Justfile")).expect("Justfile");
    assert_none(gate_recipe_problems(&justfile));
}

#[test]
fn workspace_base_check_and_ledger_driver_recipes_are_in_place() {
    let justfile = fs::read_to_string(root().join("Justfile")).expect("Justfile");
    assert_none(base_and_ledger_recipe_problems(&justfile));
}

#[test]
fn mutations_of_base_check_and_the_ledger_driver_are_problems() {
    let justfile = fs::read_to_string(root().join("Justfile")).expect("Justfile");
    assert_none(base_and_ledger_recipe_problems(&justfile));
    let mutations: [(&str, &str, &str); 9] = [
        ("check without base-check", "check: base-check ", "check: "),
        (
            "branch-check without it",
            "branch-check: base-check",
            "branch-check:",
        ),
        (
            "ancestry negated",
            "if ! git merge-base",
            "if git merge-base",
        ),
        (
            "ancestry reversed",
            r#"--is-ancestor "$base" HEAD"#,
            r#"--is-ancestor HEAD "$base""#,
        ),
        (
            "main refused",
            "echo \"base-check: on main\"\n        exit 0",
            "echo \"base-check: on main\"\n        exit 1",
        ),
        (
            "local main preferred",
            "refs/remotes/origin/main >/dev/null; then\n        base=\"origin/main\"",
            "refs/heads/main >/dev/null; then\n        base=\"main\"",
        ),
        (
            "shallow skip removed",
            "echo \"base-check: skipped (shallow clone)\"",
            "true",
        ),
        (
            "driver fallback inverted",
            "if [ -x '$dest' ]",
            "if [ ! -x '$dest' ]",
        ),
        (
            "driver line commented out",
            "    git config merge.ledger.driver",
            "    # git config merge.ledger.driver",
        ),
    ];
    for (what, from, to) in mutations {
        assert!(
            justfile.contains(from),
            "{what}: the Justfile has no {from:?}"
        );
        let broken = justfile.replacen(from, to, 1);
        assert!(
            !base_and_ledger_recipe_problems(&broken).is_empty(),
            "{what}: not reported"
        );
    }
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
rust-version.workspace = true
readme.workspace = true
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
    assert_none(member_rust_version_problems(&m));
    assert_none(lints_problems(&m));
    assert_none(metadata_problems(&m));
    assert_none(include_problems(&m));
    assert_none(internal_dependency_problems(&m, &internal()));
}

#[test]
fn missing_rust_version_and_authors_are_problems() {
    let root: Table = toml::from_str("[workspace.package]\nauthors = [\"a\"]\n").expect("parses");
    assert_eq!(workspace_package_problems(&root).len(), 2);
    let m = member(&GOOD.replace("rust-version.workspace = true\n", ""));
    assert_eq!(member_rust_version_problems(&m).len(), 1);
}

#[test]
fn rust_version_off_the_toolchain_is_a_problem() {
    let root = |rv: &str| -> Table {
        toml::from_str(&format!("[workspace.package]\nrust-version = \"{rv}\"\n")).expect("parses")
    };
    let tc: Table = toml::from_str("[toolchain]\nchannel = \"1.98.1\"\n").expect("parses");
    assert_none(rust_version_matches_toolchain(&root("1.98"), &tc));
    assert_eq!(rust_version_matches_toolchain(&root("1.97"), &tc).len(), 1);
    assert_eq!(
        rust_version_matches_toolchain(&root("1.98.1"), &tc).len(),
        1
    );
}

#[test]
fn publishable_set_other_than_expected_is_a_problem() {
    let ms = [
        member(GOOD),
        member("[package]\nname = \"o\"\npublish = false\n"),
    ];
    assert_none(published_set_problems(&ms, &["x"]));
    assert_eq!(published_set_problems(&ms, &["x", "o"]).len(), 1);
    assert_eq!(published_set_problems(&ms, &[]).len(), 1);
}

#[test]
fn missing_docs_attribute_in_source_is_a_problem() {
    let text = "//! Doc.\n#![warn(missing_docs)]\n\n#[allow(missing_docs)]\npub fn f() {}\n// missing_docs in a comment\n#[allow(\n    dead_code,\n    missing_docs\n)]\nfn g() {}\n";
    assert_eq!(missing_docs_attribute_problems("lib.rs", text).len(), 3);
}

#[test]
fn gate_recipes_without_deny_or_lock_are_problems() {
    let good = "check: rust-fmt-check rust-clippy rust-test deny\n\nrust-clippy:\n    CARGO_BUILD_WARNINGS=deny cargo clippy --workspace --all-targets --locked\n\nrust-test:\n    cargo test --workspace --locked\n";
    assert_none(gate_recipe_problems(good));
    let bad = good
        .replace("CARGO_BUILD_WARNINGS=deny ", "")
        .replace("--all-targets --locked", "--all-targets")
        .replace("--workspace --locked", "--workspace")
        .replace(" rust-test deny", " deny");
    assert_eq!(
        gate_recipe_problems(&bad).len(),
        4,
        "{:?}",
        gate_recipe_problems(&bad)
    );
    let commented = good.replace("    CARGO_BUILD_WARNINGS", "    # CARGO_BUILD_WARNINGS");
    assert_eq!(gate_recipe_problems(&commented).len(), 3);
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
    assert_none(licence_file_problems(
        &m,
        &root.join("crates/ryo-v2-core"),
        &root,
    ));
    // The root copies are regular files, not symlinks.
    assert_eq!(licence_file_problems(&m, &root, &root).len(), 2);
    let absent = root.join("crates").join("no-such-crate");
    // Neither a symlink nor readable.
    assert_eq!(licence_file_problems(&m, &absent, &root).len(), 4);
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
    let m = member(&GOOD.replace(r#"["development-tools"]"#, r#"[" "]"#));
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
    // A top-level glob takes tests/, and the list then has no `src/**` entry.
    let m = member(&GOOD.replace(r#""src/**""#, r#""**/*.rs""#));
    assert_eq!(include_problems(&m).len(), 2);
    let m = member(&GOOD.replace(r#""src/**", "#, ""));
    assert_eq!(include_problems(&m).len(), 1);
    let m = member(&GOOD.replace("include = ", "exclude = "));
    assert_eq!(include_problems(&m).len(), 1);
    let m = member(&GOOD.replace("readme.workspace = true\n", ""));
    assert_eq!(include_problems(&m).len(), 1);
}

#[test]
fn internal_dependency_by_path_is_a_problem() {
    let m = member(
        &(GOOD.replace("y.workspace = true", r#"y = { path = "../y" }"#)
            + "\n[target.'cfg(unix)'.dev-dependencies]\nz = { package = \"x\", version = \"0.1\" }\n"
            + "\n[build_dependencies]\ny = \"0.1\"\n"),
    );
    assert_eq!(
        internal_dependency_problems(&m, &internal()).len(),
        3,
        "{:?}",
        internal_dependency_problems(&m, &internal())
    );
}
