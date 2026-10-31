//! A file edit.

use super::RelPath;

/// 1 file 分の編集: 編集後の全文、None は削除。path が workspace に無く new_text が Some なら新規 file (Created)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    /// Workspace-relative file.
    pub path: RelPath,
    /// Whole new text, `None` to delete.
    pub new_text: Option<String>,
}

impl Edit {
    /// Replace (or create) `path` with `new_text`.
    pub fn write(path: RelPath, new_text: impl Into<String>) -> Self {
        Self {
            path,
            new_text: Some(new_text.into()),
        }
    }

    /// Delete `path`.
    pub fn delete(path: RelPath) -> Self {
        Self {
            path,
            new_text: None,
        }
    }
}
