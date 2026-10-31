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
