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
