//! Precision on real history: the verifier is run on a commit's `.rs` changes over its parent, and
//! every error it reports is a false positive when the commit builds cleanly.
//!
//! `history-cases export` writes the pairs (parent tree, child overlay) outside the repo, and
//! `history-cases expect` runs `cargo check --all-targets` on each child. The pure planning of a
//! pair lives here so that it is tested without git.

use std::path::{Path, PathBuf};

use ryo_v2_core::domain::RelPath;

use crate::groups::Outcome;

/// How a path changed between a parent and a child (git `--name-status`; a copy is `Added`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// `A` (and `C`).
    Added,
    /// `M` (and `T`).
    Modified,
    /// `D`.
    Deleted,
    /// `R`.
    Renamed,
}

/// What to do with one (parent, child) pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairPlan {
    /// The pair is a case.
    Use {
        /// `.rs` files under a member dir that the child adds or modifies.
        overlay: Vec<RelPath>,
        /// Non-`.rs` files taken from the child into the base.
        base_from_child: Vec<RelPath>,
        /// Non-`.rs` files the child deletes from the base.
        base_removed: Vec<RelPath>,
        /// A manifest, lock file, `.cargo/` file or toolchain file changed.
        manifest_from_child: bool,
        /// `.rs` changes outside every member dir, dropped because ra does not index them.
        ignored_rs: usize,
    },
    /// The pair is not a case; the string says why.
    Skip(String),
}

/// The reason of a pair whose `.rs` changes are all outside the member dirs.
pub const NO_OVERLAY: &str = "no member .rs change";

fn is_rs(p: &RelPath) -> bool {
    p.as_str().ends_with(".rs")
}

fn file_name(p: &RelPath) -> &str {
    p.as_str().rsplit('/').next().unwrap_or("")
}

fn is_manifest_like(p: &RelPath) -> bool {
    let s = p.as_str();
    let name = file_name(p);
    name == "Cargo.toml"
        || name == "Cargo.lock"
        || s.starts_with(".cargo/")
        || s.contains("/.cargo/")
        || (!s.contains('/') && name.starts_with("rust-toolchain"))
}

fn in_member(p: &RelPath, member_dirs: &[RelPath]) -> bool {
    member_dirs
        .iter()
        .any(|d| p.as_str().starts_with(&format!("{}/", d.as_str())))
}

/// Plans a pair from its changes and the member dirs of the workspace (relative to the root).
///
/// A package at the workspace root has no `RelPath`; the caller passes its target dirs
/// (`src`, `tests`, `examples`, `benches`) instead.
///
/// # Contract
/// - C1: a deleted or renamed `.rs` file is a `Skip`, because an overlay cannot delete.
///   tested: history::tests::deleted_rs_is_skipped
/// - C2: an added or deleted `Cargo.toml` at the root or inside a member dir is a `Skip`,
///   because the crate graph changed; one elsewhere (a fixture workspace) is an ordinary
///   non-`.rs` change.
///   tested: history::tests::added_cargo_toml_is_skipped
/// - C3: an added or modified `.rs` file outside every member dir is counted in `ignored_rs` and
///   not part of the overlay.
///   tested: history::tests::non_member_rs_is_ignored
/// - C4: non-`.rs` changes go to the base, and a manifest-like one sets `manifest_from_child`.
///   tested: history::tests::manifest_change_goes_to_base
/// - C5: an empty overlay is a `Skip`.
///   tested: history::tests::empty_overlay_is_skipped
pub fn plan_pair(changes: &[(ChangeKind, RelPath)], member_dirs: &[RelPath]) -> PairPlan {
    if changes
        .iter()
        .any(|(k, p)| is_rs(p) && matches!(k, ChangeKind::Deleted | ChangeKind::Renamed))
    {
        return PairPlan::Skip("deletes or renames a .rs file".into());
    }
    if changes.iter().any(|(k, p)| {
        matches!(k, ChangeKind::Added | ChangeKind::Deleted)
            && file_name(p) == "Cargo.toml"
            && (!p.as_str().contains('/') || in_member(p, member_dirs))
    }) {
        return PairPlan::Skip("crate graph changed: Cargo.toml added or removed".into());
    }
    let mut overlay = Vec::new();
    let mut base_from_child = Vec::new();
    let mut base_removed = Vec::new();
    let mut ignored_rs = 0;
    for (kind, path) in changes {
        if is_rs(path) {
            if in_member(path, member_dirs) {
                overlay.push(path.clone());
            } else {
                ignored_rs += 1;
            }
        } else if *kind == ChangeKind::Deleted {
            base_removed.push(path.clone());
        } else {
            base_from_child.push(path.clone());
        }
    }
    if overlay.is_empty() {
        return PairPlan::Skip(NO_OVERLAY.into());
    }
    for v in [&mut overlay, &mut base_from_child, &mut base_removed] {
        v.sort();
        v.dedup();
    }
    let manifest_from_child = base_from_child
        .iter()
        .chain(&base_removed)
        .any(is_manifest_like);
    PairPlan::Use {
        overlay,
        base_from_child,
        base_removed,
        manifest_from_child,
        ignored_rs,
    }
}

/// One row of `index.toml` (`[[pair]]`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PairEntry {
    /// `NN-<child sha, 8 hex>`, the directory name of the pair.
    pub id: String,
    /// The parent commit.
    pub parent: String,
    /// The child commit.
    pub child: String,
    /// See [`PairPlan::Use`].
    pub manifest_from_child: bool,
    /// Number of overlay files.
    pub overlay_files: usize,
    /// See [`PairPlan::Use`].
    pub ignored_rs: usize,
    /// Why the pair is not a case, if it is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip: Option<String>,
}

/// A pair on disk.
#[derive(Debug, Clone)]
pub struct HistoryPair {
    /// The set name (directory under the out dir).
    pub set: String,
    /// The index row.
    pub entry: PairEntry,
    /// `<out>/<set>/<id>/base`.
    pub base: PathBuf,
    /// `<out>/<set>/<id>/overlay`.
    pub overlay: PathBuf,
    /// `<out>/<set>/<id>/expected.check.json`.
    pub expected: PathBuf,
}

/// Why a set could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    /// A file or directory could not be read or written.
    #[error("{path}: {source}")]
    Io {
        /// The path.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// `index.toml` is not valid.
    #[error("parse {path}: {source}")]
    Toml {
        /// The path.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: toml::de::Error,
    },
    /// The index could not be rendered.
    #[error("render index.toml: {0}")]
    Render(#[from] toml::ser::Error),
}

#[derive(serde::Serialize, serde::Deserialize)]
struct IndexFile {
    #[serde(default)]
    pair: Vec<PairEntry>,
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> HistoryError + '_ {
    move |source| HistoryError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Writes `<set_dir>/index.toml` with one `[[pair]]` per entry.
///
/// # Contract
/// - C1: what is written is what [`load_sets`] reads back, in the same order.
///   tested: history::tests::index_round_trips
pub fn write_index(set_dir: &Path, entries: &[PairEntry]) -> Result<(), HistoryError> {
    let text = toml::to_string(&IndexFile {
        pair: entries.to_vec(),
    })?;
    std::fs::create_dir_all(set_dir).map_err(io(set_dir))?;
    let path = set_dir.join("index.toml");
    std::fs::write(&path, text).map_err(io(&path))
}

/// Every pair of one set dir (`<set_dir>/index.toml`), sorted by id.
pub fn load_set(set_dir: &Path) -> Result<Vec<HistoryPair>, HistoryError> {
    let path = set_dir.join("index.toml");
    let text = std::fs::read_to_string(&path).map_err(io(&path))?;
    let mut file: IndexFile = toml::from_str(&text).map_err(|source| HistoryError::Toml {
        path: path.clone(),
        source,
    })?;
    file.pair.sort_by(|a, b| a.id.cmp(&b.id));
    let set = set_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(file
        .pair
        .into_iter()
        .map(|entry| {
            let dir = set_dir.join(&entry.id);
            HistoryPair {
                set: set.clone(),
                base: dir.join("base"),
                overlay: dir.join("overlay"),
                expected: dir.join("expected.check.json"),
                entry,
            }
        })
        .collect())
}

/// Every pair of every set under `out` (a set is a directory with an `index.toml`).
///
/// # Contract
/// - C1: sets are ordered by name and the pairs of a set by id.
///   tested: history::tests::index_round_trips
pub fn load_sets(out: &Path) -> Result<Vec<HistoryPair>, HistoryError> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(out).map_err(io(out))? {
        let path = entry.map_err(io(out))?.path();
        if path.join("index.toml").is_file() {
            dirs.push(path);
        }
    }
    dirs.sort();
    let mut pairs = Vec::new();
    for dir in dirs {
        pairs.extend(load_set(&dir)?);
    }
    Ok(pairs)
}

/// Counts of one set of pairs. `valid` pairs are the ones whose child builds cleanly, so every
/// error the verifier reports on them is a false positive.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HistoryCounts {
    /// Pairs seen.
    pub pairs: usize,
    /// Pairs not compared.
    pub skip: usize,
    /// Pairs compared.
    pub valid: usize,
    /// Compared pairs where the verifier reported something.
    pub fp_cases: usize,
    /// Reported diagnostics with a rustc code, over all FP cases.
    pub extra_diags: usize,
    /// Reported diagnostics without a rustc code, over all FP cases.
    pub unmapped_diags: usize,
}

impl HistoryCounts {
    /// Adds the outcome of one pair, compared against an empty expectation.
    pub fn add(&mut self, o: &Outcome) {
        self.pairs += 1;
        match o {
            Outcome::Skip(_) => self.skip += 1,
            Outcome::Pass => self.valid += 1,
            Outcome::Fail {
                extra, unmapped, ..
            } => {
                self.valid += 1;
                self.fp_cases += 1;
                self.extra_diags += extra.len();
                self.unmapped_diags += unmapped.len();
            }
        }
    }

    /// `fp_cases / valid`: the false-positive rate per case, `None` when no pair was compared.
    ///
    /// # Contract
    /// - C1: it counts cases, not diagnostics: a pair with three extra errors is one.
    ///   tested: history::tests::fp_rate_counts_cases_not_diagnostics
    pub fn fp_rate(&self) -> Option<f64> {
        (self.valid > 0).then(|| self.fp_cases as f64 / self.valid as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryo_v2_core::domain::{Diagnostic, ErrorCode, UnmappedDiagnostic};

    fn rp(s: &str) -> RelPath {
        RelPath::parse(s).expect("rel path")
    }

    fn members() -> Vec<RelPath> {
        vec![rp("crates/a"), rp("crates/b")]
    }

    #[test]
    fn deleted_rs_is_skipped() {
        for kind in [ChangeKind::Deleted, ChangeKind::Renamed] {
            let plan = plan_pair(
                &[
                    (ChangeKind::Modified, rp("crates/a/src/lib.rs")),
                    (kind, rp("crates/a/src/old.rs")),
                ],
                &members(),
            );
            assert_eq!(plan, PairPlan::Skip("deletes or renames a .rs file".into()));
        }
    }

    #[test]
    fn added_cargo_toml_is_skipped() {
        for kind in [ChangeKind::Added, ChangeKind::Deleted] {
            let plan = plan_pair(
                &[
                    (ChangeKind::Modified, rp("crates/a/src/lib.rs")),
                    (kind, rp("crates/a/inner/Cargo.toml")),
                ],
                &members(),
            );
            assert_eq!(
                plan,
                PairPlan::Skip("crate graph changed: Cargo.toml added or removed".into())
            );
        }
        let root = plan_pair(
            &[
                (ChangeKind::Modified, rp("crates/a/src/lib.rs")),
                (ChangeKind::Added, rp("Cargo.toml")),
            ],
            &members(),
        );
        assert!(matches!(root, PairPlan::Skip(_)));
        // A fixture workspace outside the member dirs is not part of the crate graph.
        let fixture = plan_pair(
            &[
                (ChangeKind::Modified, rp("crates/a/src/lib.rs")),
                (ChangeKind::Added, rp("corpus/lab/Cargo.toml")),
            ],
            &members(),
        );
        assert!(matches!(
            fixture,
            PairPlan::Use {
                manifest_from_child: true,
                ..
            }
        ));
    }

    #[test]
    fn non_member_rs_is_ignored() {
        let plan = plan_pair(
            &[
                (ChangeKind::Modified, rp("crates/a/src/lib.rs")),
                (ChangeKind::Added, rp("crates/b/tests/it.rs")),
                (ChangeKind::Modified, rp("corpus/x/src/lib.rs")),
                (ChangeKind::Added, rp("build.rs")),
            ],
            &members(),
        );
        assert_eq!(
            plan,
            PairPlan::Use {
                overlay: vec![rp("crates/a/src/lib.rs"), rp("crates/b/tests/it.rs")],
                base_from_child: vec![],
                base_removed: vec![],
                manifest_from_child: false,
                ignored_rs: 2,
            }
        );
    }

    #[test]
    fn manifest_change_goes_to_base() {
        let plan = plan_pair(
            &[
                (ChangeKind::Modified, rp("crates/a/src/lib.rs")),
                (ChangeKind::Modified, rp("Cargo.toml")),
                (ChangeKind::Modified, rp("Cargo.lock")),
                (ChangeKind::Deleted, rp(".cargo/config.toml")),
                (ChangeKind::Modified, rp("README.md")),
            ],
            &members(),
        );
        assert_eq!(
            plan,
            PairPlan::Use {
                overlay: vec![rp("crates/a/src/lib.rs")],
                base_from_child: vec![rp("Cargo.lock"), rp("Cargo.toml"), rp("README.md")],
                base_removed: vec![rp(".cargo/config.toml")],
                manifest_from_child: true,
                ignored_rs: 0,
            }
        );
        let docs_only = plan_pair(
            &[
                (ChangeKind::Modified, rp("crates/a/src/lib.rs")),
                (ChangeKind::Modified, rp("README.md")),
            ],
            &members(),
        );
        assert!(matches!(
            docs_only,
            PairPlan::Use {
                manifest_from_child: false,
                ..
            }
        ));
    }

    #[test]
    fn empty_overlay_is_skipped() {
        assert_eq!(
            plan_pair(&[(ChangeKind::Modified, rp("README.md"))], &members()),
            PairPlan::Skip(NO_OVERLAY.into())
        );
        assert_eq!(
            plan_pair(
                &[(ChangeKind::Modified, rp("corpus/x/src/lib.rs"))],
                &members()
            ),
            PairPlan::Skip(NO_OVERLAY.into())
        );
    }

    fn fail(extra: usize, unmapped: usize) -> Outcome {
        let d = |line| Diagnostic {
            code: ErrorCode::parse("E0425").expect("code"),
            path: rp("crates/a/src/lib.rs"),
            line,
        };
        Outcome::Fail {
            missed: vec![],
            extra: (1..=extra as u32).map(d).collect(),
            unmapped: (0..unmapped)
                .map(|i| UnmappedDiagnostic {
                    ra_code: format!("ra-{i}"),
                    path: rp("crates/a/src/lib.rs"),
                    line: 1,
                })
                .collect(),
        }
    }

    #[test]
    fn fp_rate_counts_cases_not_diagnostics() {
        let mut c = HistoryCounts::default();
        assert_eq!(c.fp_rate(), None);
        c.add(&Outcome::Skip("x".into()));
        assert_eq!(c.fp_rate(), None);
        c.add(&Outcome::Pass);
        c.add(&fail(3, 1));
        c.add(&Outcome::Pass);
        c.add(&Outcome::Pass);
        assert_eq!(
            c,
            HistoryCounts {
                pairs: 5,
                skip: 1,
                valid: 4,
                fp_cases: 1,
                extra_diags: 3,
                unmapped_diags: 1,
            }
        );
        assert_eq!(c.fp_rate(), Some(0.25));
    }

    #[test]
    fn index_round_trips() {
        let out = std::env::temp_dir().join(format!("ryo-v2-history-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        let entry = |id: &str, skip: Option<&str>| PairEntry {
            id: id.into(),
            parent: "a".repeat(40),
            child: "b".repeat(40),
            manifest_from_child: id.ends_with('2'),
            overlay_files: 3,
            ignored_rs: 1,
            skip: skip.map(str::to_string),
        };
        // Written out of order: the loader sorts.
        let rows = vec![
            entry("02-bbbbbbbb", None),
            entry("01-aaaaaaaa", Some("why")),
        ];
        write_index(&out.join("zeta"), &rows).expect("write zeta");
        write_index(&out.join("alpha"), &rows[..1]).expect("write alpha");
        std::fs::create_dir_all(out.join("not-a-set")).expect("mkdir");
        let pairs = load_sets(&out).expect("load");
        let ids: Vec<String> = pairs
            .iter()
            .map(|p| format!("{}/{}", p.set, p.entry.id))
            .collect();
        assert_eq!(
            ids,
            ["alpha/02-bbbbbbbb", "zeta/01-aaaaaaaa", "zeta/02-bbbbbbbb"]
        );
        assert_eq!(pairs[1].entry, rows[1]);
        assert_eq!(pairs[2].entry, rows[0]);
        assert!(pairs[1].base.ends_with("zeta/01-aaaaaaaa/base"));
        assert!(
            pairs[1]
                .expected
                .ends_with("zeta/01-aaaaaaaa/expected.check.json")
        );
        let _ = std::fs::remove_dir_all(&out);
    }
}
