//! Where the repo, the corpus and the proc-macro server are, found from the
//! environment.

use std::path::PathBuf;
use std::process::Command;

/// Paths and switches a run needs.
#[derive(Debug, Clone)]
pub struct Materials {
    /// The repository root.
    pub repo_root: PathBuf,
    /// The corpus directory.
    pub corpus: PathBuf,
    /// Whether rust-analyzer expands proc macros (`RYO_V2_ORACLE_PROC_MACRO` is not `off`).
    pub proc_macro: bool,
    /// The sysroot's proc-macro server, when it exists.
    pub proc_macro_srv: Option<PathBuf>,
}

/// Why the materials could not be found.
#[derive(Debug, thiserror::Error)]
pub enum MaterialsError {
    /// The repo root could not be resolved.
    #[error("resolve repo root {path}: {source}")]
    RepoRoot {
        /// The path tried.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The corpus directory does not exist.
    #[error("corpus directory not found: {0}")]
    Corpus(PathBuf),
}

impl Materials {
    /// Finds the materials. The corpus is `RYO_V2_CORPUS` or `<repo_root>/corpus`.
    /// The proc-macro server is looked up in the sysroot of the toolchain that
    /// `rustc` resolves to in the repo root; a failure to ask `rustc` is `None`.
    pub fn find() -> Result<Self, MaterialsError> {
        let raw = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
        let repo_root = raw
            .canonicalize()
            .map_err(|source| MaterialsError::RepoRoot { path: raw, source })?;
        let corpus = match std::env::var_os("RYO_V2_CORPUS") {
            Some(p) => PathBuf::from(p),
            None => repo_root.join("corpus"),
        };
        if !corpus.is_dir() {
            return Err(MaterialsError::Corpus(corpus));
        }
        let proc_macro = std::env::var("RYO_V2_ORACLE_PROC_MACRO").map_or(true, |v| v != "off");
        let proc_macro_srv = sysroot(&repo_root)
            .map(|s| s.join("libexec").join("rust-analyzer-proc-macro-srv"))
            .filter(|p| p.is_file());
        Ok(Self {
            repo_root,
            corpus,
            proc_macro,
            proc_macro_srv,
        })
    }
}

fn sysroot(cwd: &std::path::Path) -> Option<PathBuf> {
    let out = Command::new("rustc")
        .args(["--print", "sysroot"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| PathBuf::from(s))
}
