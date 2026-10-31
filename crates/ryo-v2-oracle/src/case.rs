//! Corpus cases: one overlay directory over a base workspace, the group it
//! belongs to, and the expected diagnostics `cargo check` gave for it.

use std::path::{Path, PathBuf};

use ryo_v2_core::domain::{Diagnostic, Edit, ErrorCode, RelPath};
use ryo_v2_core::infra::overlay::{OverlayError, overlay_edits};

use crate::groups::Group;

/// The shape of a `cases/<id>.toml` file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseFile {
    /// Corpus-relative path of the base workspace (the parent of `cases/`).
    pub workspace: String,
    /// Corpus-relative path of the overlay directory.
    pub overlay: String,
    /// A [`Group`] id.
    pub group: String,
    /// Where the case came from (`hand`, ...).
    pub origin: String,
    /// When set, the case is not compared; the string says why.
    #[serde(default)]
    pub skip: Option<String>,
}

/// A loaded case.
#[derive(Debug, Clone)]
pub struct Case {
    /// File stem of the case file.
    pub id: String,
    /// Corpus-relative base workspace, `/`-separated.
    pub workspace_rel: String,
    /// Absolute base workspace.
    pub workspace: PathBuf,
    /// Absolute overlay directory.
    pub overlay: PathBuf,
    /// The group of the case.
    pub group: Group,
    /// Where the case came from.
    pub origin: String,
    /// Why the case is skipped, if it is.
    pub skip: Option<String>,
    /// `<corpus>/<workspace_rel>/expected/<id>.check.json`.
    pub expected_path: PathBuf,
}

/// Why a case could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum CaseError {
    /// A file or directory could not be read.
    #[error("read {path}: {source}")]
    Io {
        /// The path.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A case file is not valid TOML of the expected shape.
    #[error("parse {path}: {source}")]
    Toml {
        /// The path.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: toml::de::Error,
    },
    /// An expected file is not valid JSON.
    #[error("parse {path}: {source}")]
    Json {
        /// The path.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: serde_json::Error,
    },
    /// A case or expected file is well-formed but wrong.
    #[error("{path}: {reason}")]
    Invalid {
        /// The path.
        path: PathBuf,
        /// What is wrong.
        reason: String,
    },
    /// The overlay could not be turned into edits.
    #[error(transparent)]
    Overlay(#[from] OverlayError),
}

impl Case {
    /// Walks `corpus` (not `target` or `.git`) and loads every `cases/*.toml`,
    /// sorted by id. It is `Invalid` when the workspace is not the corpus-relative
    /// path of the parent of `cases/`, the group is unknown, or the overlay
    /// directory does not exist.
    pub fn load_all(corpus: &Path) -> Result<Vec<Case>, CaseError> {
        let mut cases_dirs = Vec::new();
        find_cases_dirs(corpus, &mut cases_dirs)?;
        let mut cases = Vec::new();
        for dir in cases_dirs {
            let ws_dir = dir.parent().unwrap_or(corpus);
            let workspace_rel = rel_slash(corpus, ws_dir);
            for entry in read_dir(&dir)? {
                if entry.extension().is_some_and(|e| e == "toml") {
                    cases.push(load_one(corpus, &entry, &workspace_rel)?);
                }
            }
        }
        cases.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(cases)
    }

    /// The edits the overlay makes over the base workspace.
    pub fn edits(&self) -> Result<Vec<Edit>, CaseError> {
        Ok(overlay_edits(&self.workspace, &self.overlay)?)
    }

    /// The expected diagnostics, or `None` when the expected file does not exist.
    pub fn expected(&self) -> Result<Option<Vec<Diagnostic>>, CaseError> {
        let text = match std::fs::read_to_string(&self.expected_path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(CaseError::Io {
                    path: self.expected_path.clone(),
                    source,
                });
            }
        };
        parse_expected(&text)
            .map(Some)
            .map_err(|reason| CaseError::Invalid {
                path: self.expected_path.clone(),
                reason,
            })
    }
}

fn load_one(corpus: &Path, file: &Path, workspace_rel: &str) -> Result<Case, CaseError> {
    let invalid = |reason: String| CaseError::Invalid {
        path: file.to_path_buf(),
        reason,
    };
    let text = std::fs::read_to_string(file).map_err(|source| CaseError::Io {
        path: file.to_path_buf(),
        source,
    })?;
    let cf: CaseFile = toml::from_str(&text).map_err(|source| CaseError::Toml {
        path: file.to_path_buf(),
        source,
    })?;
    if cf.workspace != workspace_rel {
        return Err(invalid(format!(
            "workspace {:?} is not the parent of cases/ ({workspace_rel:?})",
            cf.workspace
        )));
    }
    let group =
        Group::parse(&cf.group).ok_or_else(|| invalid(format!("unknown group {:?}", cf.group)))?;
    let overlay = corpus.join(&cf.overlay);
    if !overlay.is_dir() {
        return Err(invalid(format!(
            "overlay directory not found: {}",
            overlay.display()
        )));
    }
    let id = file
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| invalid("file name is not UTF-8".into()))?
        .to_string();
    let workspace = corpus.join(workspace_rel);
    let expected_path = workspace.join("expected").join(format!("{id}.check.json"));
    Ok(Case {
        id,
        workspace_rel: workspace_rel.to_string(),
        workspace,
        overlay,
        group,
        origin: cf.origin,
        skip: cf.skip,
        expected_path,
    })
}

fn read_dir(dir: &Path) -> Result<Vec<PathBuf>, CaseError> {
    let io = |source| CaseError::Io {
        path: dir.to_path_buf(),
        source,
    };
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(io)? {
        out.push(entry.map_err(io)?.path());
    }
    out.sort();
    Ok(out)
}

fn find_cases_dirs(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), CaseError> {
    for path in read_dir(dir)? {
        if !path.is_dir() {
            continue;
        }
        match path.file_name().and_then(|n| n.to_str()) {
            Some("target" | ".git") => {}
            Some("cases") => out.push(path),
            _ => find_cases_dirs(&path, out)?,
        }
    }
    Ok(())
}

fn rel_slash(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// One expected diagnostic of one case, tagged with the CHECK_LEDGER Gaps row
/// it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GapTag {
    /// Gaps row id, e.g. "G-B-4".
    pub row: String,
    /// `CaseOutcome::id` form: "<last component of workspace_rel>/<case id>".
    pub case: String,
    /// The tagged expected diagnostic.
    pub diagnostic: Diagnostic,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct GapTagRow {
    row: String,
    case: String,
    code: String,
    path: String,
    line: u32,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct GapTagFile {
    #[serde(default)]
    tag: Vec<GapTagRow>,
}

/// Reads `<corpus>/gap-tags.toml`; a missing file is an empty list.
/// Shape: `[[tag]]` tables with `row`, `case`, `code`, `path`, `line`
/// (unknown fields are refused).
pub fn load_gap_tags(corpus: &Path) -> Result<Vec<GapTag>, CaseError> {
    let file = corpus.join("gap-tags.toml");
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(CaseError::Io { path: file, source }),
    };
    let parsed: GapTagFile = toml::from_str(&text).map_err(|source| CaseError::Toml {
        path: file.clone(),
        source,
    })?;
    let invalid = |reason: String| CaseError::Invalid {
        path: file.clone(),
        reason,
    };
    parsed
        .tag
        .into_iter()
        .map(|r| {
            Ok(GapTag {
                row: r.row,
                case: r.case,
                diagnostic: Diagnostic {
                    code: ErrorCode::parse(&r.code).map_err(|e| invalid(e.to_string()))?,
                    path: RelPath::parse(&r.path).map_err(|e| invalid(e.to_string()))?,
                    line: r.line,
                },
            })
        })
        .collect()
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedRow {
    code: String,
    path: String,
    line: u32,
}

/// Reads `[{"code","path","line"}]`.
pub fn parse_expected(json: &str) -> Result<Vec<Diagnostic>, String> {
    let rows: Vec<ExpectedRow> = serde_json::from_str(json).map_err(|e| e.to_string())?;
    rows.into_iter()
        .map(|r| {
            Ok(Diagnostic {
                code: ErrorCode::parse(&r.code).map_err(|e| e.to_string())?,
                path: RelPath::parse(&r.path).map_err(|e| e.to_string())?,
                line: r.line,
            })
        })
        .collect()
}

/// Writes the diagnostics as pretty JSON (fields `code`, `path`, `line`; sorted;
/// trailing newline).
pub fn render_expected(diags: &[Diagnostic]) -> String {
    let mut sorted: Vec<&Diagnostic> = diags.iter().collect();
    sorted.sort();
    let rows: Vec<ExpectedRow> = sorted
        .into_iter()
        .map(|d| ExpectedRow {
            code: d.code.as_str().to_string(),
            path: d.path.as_str().to_string(),
            line: d.line,
        })
        .collect();
    // Serializing plain strings and integers cannot fail.
    let mut s = serde_json::to_string_pretty(&rows).unwrap_or_else(|_| "[]".to_string());
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(code: &str, path: &str, line: u32) -> Diagnostic {
        Diagnostic {
            code: ErrorCode::parse(code).unwrap(),
            path: RelPath::parse(path).unwrap(),
            line,
        }
    }

    #[test]
    fn parse_expected_round_trips() {
        let diags = vec![
            d("E0308", "lib/src/lib.rs", 4),
            d("E0599", "user/src/lib.rs", 20),
        ];
        let text = render_expected(&diags);
        assert_eq!(parse_expected(&text).unwrap(), diags);
        let code = text.find("\"code\"").unwrap();
        let path = text.find("\"path\"").unwrap();
        let line = text.find("\"line\"").unwrap();
        assert!(code < path && path < line, "field order: {text}");
        assert!(text.ends_with("]\n"));
        assert_eq!(render_expected(&[]), "[]\n");
        assert!(parse_expected("[{\"code\":\"x\",\"path\":\"a\",\"line\":1}]").is_err());
    }

    fn real_corpus() -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../corpus"))
    }

    #[test]
    fn load_all_finds_the_hand_cases() {
        let cases = Case::load_all(&real_corpus()).unwrap();
        let ids: Vec<&str> = cases
            .iter()
            .filter(|c| c.origin == "hand" && c.workspace_rel == "base/extract-trait-multicrate")
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(
            ids,
            [
                "broken-01",
                "broken-02",
                "broken-03",
                "broken-04",
                "broken-05",
                "broken-06",
                "created-broken-disk",
                "valid-02"
            ]
        );
    }

    #[test]
    fn load_all_finds_the_macro_lab_cases() {
        let cases = Case::load_all(&real_corpus()).unwrap();
        let lab: Vec<&Case> = cases
            .iter()
            .filter(|c| c.workspace_rel == "base/macro-lab")
            .collect();
        let ids: Vec<&str> = lab.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "attr-keep-0",
                "attr-reparse-0",
                "derive-removed-0",
                "derive-removed-1",
                "macro-valid-0",
                "pm-edit-0",
                "rules-body-0",
                "rules-body-1",
                "tokens-format-0",
                "tokens-rules-0",
                "tokens-vec-0"
            ]
        );
        assert!(lab.iter().all(|c| c.origin == "hand"));
    }

    #[test]
    fn load_gap_tags_reads_the_corpus_file() {
        let tags = load_gap_tags(&real_corpus()).unwrap();
        assert!(tags.iter().all(|t| t.case.contains('/')), "{tags:?}");
        // A missing file is an empty list.
        let none = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../target/oracle-test-tmp"
        ))
        .join(format!("no-tags-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&none);
        std::fs::create_dir_all(&none).unwrap();
        assert!(load_gap_tags(&none).unwrap().is_empty());
        // An unknown field is refused.
        std::fs::write(
            none.join("gap-tags.toml"),
            "[[tag]]\nrow = \"G-B-4\"\ncase = \"a/b\"\ncode = \"E0425\"\npath = \"x/src/lib.rs\"\nline = 1\nextra = 2\n",
        )
        .unwrap();
        let err = load_gap_tags(&none).unwrap_err();
        assert!(
            matches!(err, CaseError::Invalid { .. } | CaseError::Toml { .. }),
            "{err}"
        );
        // A bad code is Invalid.
        std::fs::write(
            none.join("gap-tags.toml"),
            "[[tag]]\nrow = \"G-B-4\"\ncase = \"a/b\"\ncode = \"nope\"\npath = \"x/src/lib.rs\"\nline = 1\n",
        )
        .unwrap();
        let err = load_gap_tags(&none).unwrap_err();
        let _ = std::fs::remove_dir_all(&none);
        assert!(matches!(err, CaseError::Invalid { .. }), "{err}");
    }

    #[test]
    fn case_ids_unique_per_workspace() {
        let cases = Case::load_all(&real_corpus()).unwrap();
        let mut seen = std::collections::BTreeSet::new();
        for c in &cases {
            assert!(
                seen.insert((c.workspace_rel.clone(), c.id.clone())),
                "duplicate case {} in {}",
                c.id,
                c.workspace_rel
            );
        }
    }

    #[test]
    fn load_all_rejects_unknown_group() {
        let root = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../target/oracle-test-tmp"
        ))
        .join(format!("unknown-group-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("ws/cases")).unwrap();
        std::fs::create_dir_all(root.join("ov")).unwrap();
        std::fs::write(
            root.join("ws/cases/x.toml"),
            "workspace = \"ws\"\noverlay = \"ov\"\ngroup = \"bogus\"\norigin = \"hand\"\n",
        )
        .unwrap();
        let err = Case::load_all(&root).unwrap_err();
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            matches!(&err, CaseError::Invalid { reason, .. } if reason.contains("unknown group")),
            "{err}"
        );
    }
}
