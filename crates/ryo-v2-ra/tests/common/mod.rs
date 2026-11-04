//! Throwaway workspaces for the tests of the search ports (the corpus is never touched).
#![allow(dead_code)]

use ryo_v2_core::domain::RelPath;
use std::path::PathBuf;

/// A temp workspace that removes itself on drop.
pub struct Fixture {
    pub dir: PathBuf,
}

impl Fixture {
    /// Writes `files` (relative path, text) under a fresh temp dir named after `test`.
    pub fn new(test: &str, files: &[(&str, &str)]) -> Self {
        // Not CARGO_TARGET_TMPDIR: `session::load` skips every path with a `target` component,
        // so a workspace under target/ would have no indexed files.
        let dir = std::env::temp_dir().join(format!("ryo-v2-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (rel, text) in files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).unwrap();
            std::fs::write(path, text).unwrap();
        }
        Self { dir }
    }

    /// One package `probe` with `src/lib.rs` = `lib`.
    pub fn single(test: &str, lib: &str) -> Self {
        Self::new(
            test,
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
                ),
                ("src/lib.rs", lib),
            ],
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `RelPath::parse` for a literal.
pub fn rel(s: &str) -> RelPath {
    RelPath::parse(s).unwrap()
}

thread_local! {
    /// The backend of the forks the running test builds its analyzers with.
    static ENGINE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the running test is on the fork Engine (`RaConfig.engine`): the value for the
/// `engine` field of every analyzer a test of both backends builds.
pub fn engine() -> bool {
    ENGINE.with(std::cell::Cell::get)
}

/// The backends a test of both backends runs on: the undo log, then the Engine; with
/// `RYO_V2_BACKEND=undo-log` or `RYO_V2_BACKEND=engine`, that one only (to count the results of
/// one backend).
fn backends() -> Vec<bool> {
    match std::env::var("RYO_V2_BACKEND").as_deref() {
        Ok("undo-log") => vec![false],
        Ok("engine") => vec![true],
        _ => vec![false, true],
    }
}

/// Runs `test` on each backend of [`backends`] in turn (`engine()` tells it which); a failure
/// names the backend it failed on.
pub fn both(test: fn()) {
    for on_engine in backends() {
        on(on_engine, test);
    }
}

/// Runs `test` on the Engine only, whatever `RYO_V2_BACKEND` says (a test of what only the
/// Engine has).
pub fn engine_only(test: fn()) {
    on(true, test);
}

fn on(on_engine: bool, test: fn()) {
    ENGINE.with(|c| c.set(on_engine));
    let name = if on_engine { "engine" } else { "undo-log" };
    if let Err(p) = std::panic::catch_unwind(test) {
        eprintln!("failed on the {name} backend");
        std::panic::resume_unwind(p);
    }
}
