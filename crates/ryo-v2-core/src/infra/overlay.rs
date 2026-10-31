//! Turns an overlay directory into [`Edit`]s.

use crate::domain::{DomainError, Edit, RelPath};
use std::path::{Path, PathBuf};

/// Failure while reading an overlay.
#[derive(Debug, thiserror::Error)]
pub enum OverlayError {
    /// Reading a file or directory failed.
    #[error("read {path}: {source}")]
    Io {
        /// The path being read.
        path: PathBuf,
        /// The cause.
        #[source]
        source: std::io::Error,
    },
    /// A file's relative path is not a valid [`RelPath`].
    #[error(transparent)]
    Path(#[from] DomainError),
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> OverlayError + '_ {
    move |source| OverlayError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// overlay dir の .rs file を base と比べ、内容が違う file と base に無い file を Edit::write にする (overlay dir の中身を変更として読む)。
/// `target` dir は辿らない。.rs 以外は見ない。削除は表せない。返り値は path 順に sort。
pub fn overlay_edits(base: &Path, overlay: &Path) -> Result<Vec<Edit>, OverlayError> {
    let mut edits = Vec::new();
    let mut stack = vec![overlay.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).map_err(io(&dir))? {
            let path = entry.map_err(io(&dir))?.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let Ok(rel) = path.strip_prefix(overlay) else {
                    continue;
                };
                let rel_str = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                let rel_path = RelPath::parse(&rel_str)?;
                let new_text = std::fs::read_to_string(&path).map_err(io(&path))?;
                let base_path = base.join(rel);
                if base_path.exists() {
                    let base_text = std::fs::read_to_string(&base_path).map_err(io(&base_path))?;
                    if new_text == base_text {
                        continue;
                    }
                }
                edits.push(Edit::write(rel_path, new_text));
            }
        }
    }
    edits.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(edits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../corpus")
            .join(name)
    }

    fn paths(variant: &str) -> Vec<String> {
        overlay_edits(&corpus("base/extract-trait-multicrate"), &corpus(variant))
            .unwrap()
            .iter()
            .map(|e| e.path.as_str().to_owned())
            .collect()
    }

    #[test]
    fn overlay_edits_of_broken_01_is_lib_rs_only() {
        assert_eq!(paths("broken-01"), ["lib/src/lib.rs"]);
        assert_eq!(paths("valid-02").len(), 2);
        assert_eq!(
            paths("created-broken-disk"),
            ["lib/src/hircheck_probe_mod.rs", "lib/src/lib.rs"]
        );
    }
}
