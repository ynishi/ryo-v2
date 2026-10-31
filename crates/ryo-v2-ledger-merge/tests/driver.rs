//! The `ledger-merge` binary as git runs it: `ledger-merge %O %A %B %L %P`, the merge written
//! into `%A`, the exit status saying whether conflicts are left.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// A fresh directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("ryo-v2-ledger-merge-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("scratch directory");
    dir
}

const BASE: &str = "# L\n\n| id | state |\n|---|---|\n| A-1 | open |\n| A-2 | open |\n";

/// Runs the driver on the three texts; the exit code and what `%A` holds afterwards.
fn drive(name: &str, ours: &str, theirs: &str, extra: &[&str]) -> (Option<i32>, String) {
    let dir = scratch(name);
    let (b, o, t) = (dir.join("base"), dir.join("ours"), dir.join("theirs"));
    fs::write(&b, BASE).expect("write base");
    fs::write(&o, ours).expect("write ours");
    fs::write(&t, theirs).expect("write theirs");
    let status = Command::new(env!("CARGO_BIN_EXE_ledger-merge"))
        .arg(&b)
        .arg(&o)
        .arg(&t)
        .args(extra)
        .status()
        .expect("run ledger-merge");
    let result = fs::read_to_string(&o).expect("read ours");
    let _ = fs::remove_dir_all(&dir);
    (status.code(), result)
}

#[test]
fn clean_merge_is_written_into_ours_and_exits_0() {
    let ours = BASE.replace("| A-1 | open |", "| A-1 | done |");
    let theirs = BASE.replace("| A-2 | open |", "| A-2 | done |");
    let (code, text) = drive("clean", &ours, &theirs, &["7", "CHECK_LEDGER.md"]);
    assert_eq!(code, Some(0));
    assert_eq!(text, BASE.replace("open", "done"));
}

#[test]
fn conflict_is_written_with_markers_of_the_given_size_and_exits_1() {
    let ours = BASE.replace("| A-1 | open |", "| A-1 | done |");
    let theirs = BASE.replace("| A-1 | open |", "| A-1 | deferred |");
    let (code, text) = drive("conflict", &ours, &theirs, &["9"]);
    assert_eq!(code, Some(1));
    assert!(
        text.contains(
            "<<<<<<<<< ours\n| A-1 | done |\n=========\n| A-1 | deferred |\n>>>>>>>>> theirs\n"
        ),
        "{text}"
    );
}

#[test]
fn bad_arguments_exit_2_and_leave_ours_unchanged() {
    let ours = BASE.replace("open", "done");
    let (code, text) = drive("bad-size", &ours, BASE, &["0"]);
    assert_eq!((code, text.as_str()), (Some(2), ours.as_str()));
    let status = Command::new(env!("CARGO_BIN_EXE_ledger-merge"))
        .arg("only-one")
        .status()
        .expect("run ledger-merge");
    assert_eq!(status.code(), Some(2));
    let (code, text) = drive("extra-args", &ours, BASE, &["7", "L.md", "surplus"]);
    assert_eq!((code, text.as_str()), (Some(2), ours.as_str()));
    let dir = scratch("missing");
    let o = dir.join("ours");
    fs::write(&o, &ours).expect("write ours");
    let status = Command::new(env!("CARGO_BIN_EXE_ledger-merge"))
        .arg(dir.join("no-base"))
        .arg(&o)
        .arg(dir.join("no-theirs"))
        .status()
        .expect("run ledger-merge");
    assert_eq!(status.code(), Some(2));
    assert_eq!(fs::read_to_string(&o).expect("read ours"), ours);
    let _ = fs::remove_dir_all(&dir);
}

/// The driver command `just ledger-merge-install` registers, with the binary at `bin`: the line
/// is taken from the `Justfile`, so what this test runs is what a checkout runs.
fn driver_command(bin: &std::path::Path) -> String {
    let justfile =
        fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Justfile"))
            .expect("Justfile");
    let line = justfile
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("git config merge.ledger.driver \""))
        .expect("the driver line of ledger-merge-install");
    let quoted = &line["git config merge.ledger.driver \"".len()..line.len() - 1];
    quoted
        .replace("\\$", "$")
        .replace("'$dest'", &format!("'{}'", bin.display()))
}

fn git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
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
        .unwrap_or_else(|e| panic!("git is needed on the path: {e}"))
}

fn ok(out: std::process::Output) {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

const TABLE: &str =
    "# L\n\n| id | state |\n|---|---|\n| A-1 | open |\n| A-2 | open |\n| A-3 | open |\n";

/// `git merge` of two branches that edit adjacent rows (a line conflict), with the driver line
/// pointing at `bin`; the exit status of the merge and the merged file.
fn git_merge(name: &str, bin: &std::path::Path) -> (bool, String) {
    let dir = scratch(name);
    ok(git(&dir, &["init", "-q", "-b", "main"]));
    fs::write(dir.join(".gitattributes"), "L.md merge=ledger\n").expect("attributes");
    fs::write(dir.join("L.md"), TABLE).expect("base");
    ok(git(&dir, &["add", "-A"]));
    ok(git(&dir, &["commit", "-q", "-m", "base"]));
    ok(git(&dir, &["checkout", "-q", "-b", "x"]));
    fs::write(
        dir.join("L.md"),
        TABLE.replace("| A-2 | open |", "| A-2 | done |"),
    )
    .expect("x");
    ok(git(&dir, &["commit", "-q", "-am", "x"]));
    ok(git(&dir, &["checkout", "-q", "main"]));
    fs::write(
        dir.join("L.md"),
        TABLE.replace("| A-3 | open |", "| A-3 | done |"),
    )
    .expect("main");
    ok(git(&dir, &["commit", "-q", "-am", "main"]));
    let driver = format!("merge.ledger.driver={}", driver_command(bin));
    let merged = git(&dir, &["-c", &driver, "merge", "-q", "--no-edit", "x"]);
    let text = fs::read_to_string(dir.join("L.md")).expect("merged");
    let _ = fs::remove_dir_all(&dir);
    (merged.status.success(), text)
}

#[test]
fn git_merges_adjacent_row_edits_through_the_installed_driver_line() {
    let (clean, text) = git_merge(
        "git-real",
        std::path::Path::new(env!("CARGO_BIN_EXE_ledger-merge")),
    );
    assert!(clean, "{text}");
    assert_eq!(
        text,
        TABLE
            .replace("| A-2 | open |", "| A-2 | done |")
            .replace("| A-3 | open |", "| A-3 | done |")
    );
}

#[cfg(unix)]
#[test]
fn a_missing_or_failing_driver_falls_back_to_the_line_merge_with_markers() {
    use std::os::unix::fs::PermissionsExt;
    let (clean, text) = git_merge(
        "git-missing",
        std::path::Path::new("/nonexistent/ledger-merge"),
    );
    assert!(!clean && text.contains("<<<<<<<"), "{text}");
    let dir = scratch("failing-bin");
    let bin = dir.join("ledger-merge");
    fs::write(&bin, "#!/bin/sh\nexit 2\n").expect("script");
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).expect("chmod");
    let (clean, text) = git_merge("git-failing", &bin);
    assert!(!clean && text.contains("<<<<<<<"), "{text}");
    let _ = fs::remove_dir_all(&dir);
}
