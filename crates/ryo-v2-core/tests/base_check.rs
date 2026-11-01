//! `just base-check` run for real: the recipe of the workspace `Justfile`, copied into throwaway
//! git repositories, in each situation it decides. The text of the recipe is pinned by rule 8 of
//! `tests/manifest.rs`; this file checks what it does. It needs `git` and `just` on the path, as
//! `just check` itself does.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn justfile() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Justfile")
}

/// A fresh directory holding a copy of the Justfile.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ryo-v2-base-check-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch directory");
    fs::copy(justfile(), dir.join("Justfile")).expect("copy Justfile");
    dir
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .output()
        .unwrap_or_else(|e| panic!("git is needed on the path: {e}"));
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A repository with `main` at commit 2 of 2 (`old` tags commit 1), checked out on `main`.
fn repo(name: &str) -> PathBuf {
    let dir = scratch(name);
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["add", "Justfile"]);
    git(&dir, &["commit", "-q", "-m", "one"]);
    git(&dir, &["tag", "old"]);
    git(&dir, &["commit", "-q", "--allow-empty", "-m", "two"]);
    dir
}

/// `just base-check` in `dir`: the exit code and the output.
fn base_check(dir: &Path) -> (Option<i32>, String) {
    let out = Command::new("just")
        .current_dir(dir)
        .arg("base-check")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap_or_else(|e| panic!("just is needed on the path: {e}"));
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code(), text)
}

fn expect(dir: &Path, code: i32, line: &str) {
    let (got, text) = base_check(dir);
    assert_eq!(got, Some(code), "{text}");
    assert!(text.contains(line), "expected {line:?} in: {text}");
}

#[test]
fn main_and_a_branch_containing_it_pass_and_a_branch_of_an_older_main_is_refused() {
    let dir = repo("local");
    expect(&dir, 0, "base-check: on main");
    git(&dir, &["checkout", "-q", "-b", "fresh", "main"]);
    expect(&dir, 0, "base-check: fresh contains main");
    git(&dir, &["checkout", "-q", "-b", "stale", "old"]);
    expect(&dir, 1, "base-check: stale does not contain main");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn origin_main_is_the_base_where_it_exists() {
    let dir = repo("origin");
    // origin/main at the older commit: a branch from it passes although it lacks the local main.
    git(&dir, &["update-ref", "refs/remotes/origin/main", "old"]);
    git(&dir, &["checkout", "-q", "-b", "stale", "old"]);
    expect(&dir, 0, "base-check: stale contains origin/main");
    // origin/main ahead of the branch: refused.
    git(&dir, &["update-ref", "refs/remotes/origin/main", "main"]);
    expect(&dir, 1, "base-check: stale does not contain origin/main");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn it_skips_and_passes_where_it_cannot_tell() {
    let copy = scratch("copy");
    let (code, text) = base_check(&copy);
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(text.trim(), "base-check: skipped (not a git checkout)");
    let _ = fs::remove_dir_all(&copy);

    let dir = repo("noref");
    git(&dir, &["checkout", "-q", "-b", "work"]);
    git(&dir, &["branch", "-q", "-D", "main"]);
    expect(&dir, 0, "base-check: skipped (no main or origin/main)");

    let shallow =
        std::env::temp_dir().join(format!("ryo-v2-base-check-{}-shallow", std::process::id()));
    let _ = fs::remove_dir_all(&shallow);
    let url = format!("file://{}", dir.display());
    git(
        &std::env::temp_dir(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            "--branch",
            "work",
            &url,
            &shallow.display().to_string(),
        ],
    );
    expect(&shallow, 0, "base-check: skipped (shallow clone)");
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&shallow);
}
