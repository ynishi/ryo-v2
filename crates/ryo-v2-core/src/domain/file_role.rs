//! What kind of file a path is, for the checks that cannot see it.

use super::RelPath;

/// How the analyzer can treat a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FileRole {
    /// An ordinary Rust source file.
    Source,
    /// A cargo build script (`build.rs`).
    BuildScript,
    /// Source of a proc-macro crate.
    ProcMacroSource,
    /// A manifest, lock file, cargo config or any non-Rust file: the crate graph, not code.
    Manifest,
}

impl std::fmt::Display for FileRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            FileRole::Source => "source",
            FileRole::BuildScript => "build-script",
            FileRole::ProcMacroSource => "proc-macro-source",
            FileRole::Manifest => "manifest",
        })
    }
}

/// What the workspace model says about a file, supplied by the adapter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FileFacts {
    /// The file is the root of a build-script target.
    pub is_build_script: bool,
    /// The file belongs to a proc-macro crate.
    pub in_proc_macro_crate: bool,
}

/// The role of `path` given the adapter's `facts`.
///
/// # Contract
/// - C1: `Cargo.toml`, `Cargo.lock`, anything under a `.cargo` component and any path not
///   ending in `.rs` are `Manifest`, whatever the facts say.
///   tested: domain::file_role::tests::manifest_paths_and_facts
/// - C2: for a `.rs` file `is_build_script` wins over `in_proc_macro_crate`; with neither the
///   role is `Source`.
///   tested: domain::file_role::tests::manifest_paths_and_facts
pub fn file_role(path: &RelPath, facts: &FileFacts) -> FileRole {
    let s = path.as_str();
    let last = s.rsplit('/').next().unwrap_or(s);
    let manifest = last == "Cargo.toml"
        || last == "Cargo.lock"
        || s.split('/').any(|c| c == ".cargo")
        || !s.ends_with(".rs");
    if manifest {
        FileRole::Manifest
    } else if facts.is_build_script {
        FileRole::BuildScript
    } else if facts.in_proc_macro_crate {
        FileRole::ProcMacroSource
    } else {
        FileRole::Source
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(p: &str, b: bool, m: bool) -> FileRole {
        file_role(
            &RelPath::parse(p).unwrap(),
            &FileFacts {
                is_build_script: b,
                in_proc_macro_crate: m,
            },
        )
    }

    #[test]
    fn manifest_paths_and_facts() {
        for p in [
            "Cargo.toml",
            "a/Cargo.toml",
            "Cargo.lock",
            ".cargo/config.toml",
            "a/.cargo/x.rs",
            "README.md",
            "a/b.toml",
        ] {
            assert_eq!(role(p, true, true), FileRole::Manifest, "{p}");
        }
        assert_eq!(role("a/src/lib.rs", false, false), FileRole::Source);
        assert_eq!(role("a/build.rs", true, true), FileRole::BuildScript);
        assert_eq!(role("m/src/lib.rs", false, true), FileRole::ProcMacroSource);
        assert_eq!(FileRole::ProcMacroSource.to_string(), "proc-macro-source");
    }
}
