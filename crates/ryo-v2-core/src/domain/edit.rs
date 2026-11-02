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

    /// The paths `edits` delete, in their order: the `deleted` a level's `Verdict::of_level`
    /// takes.
    ///
    /// # Contract
    /// - C1: the path of every edit of `edits` with no new text, in order, and no other.
    ///   tested: domain::edit::tests::deleted_paths_are_those_with_no_text
    pub fn deleted_paths(edits: &[Edit]) -> Vec<RelPath> {
        edits
            .iter()
            .filter(|e| e.new_text.is_none())
            .map(|e| e.path.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deleted_paths_are_those_with_no_text() {
        let rel = |p: &str| RelPath::parse(p).unwrap();
        let edits = [
            Edit::delete(rel("b.rs")),
            Edit::write(rel("a.rs"), "x"),
            Edit::write(rel("e.rs"), ""),
            Edit::delete(rel("a.rs")),
        ];
        assert_eq!(Edit::deleted_paths(&edits), vec![rel("b.rs"), rel("a.rs")]);
        assert!(Edit::deleted_paths(&[]).is_empty());
    }
}
