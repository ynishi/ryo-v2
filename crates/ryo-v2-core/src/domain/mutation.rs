//! One textual change to one file, as a Blueprint states it.

use super::{DomainError, RelPath, TextRange};

/// What a [`MutationSpec`] does to its range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SpecKind {
    /// Replace a non-empty range with text.
    Replace,
    /// Insert text at an offset.
    Insert,
    /// Delete a non-empty range.
    Delete,
}

impl std::fmt::Display for SpecKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SpecKind::Replace => "replace",
            SpecKind::Insert => "insert",
            SpecKind::Delete => "delete",
        })
    }
}

/// A change of bytes `range` of `path`, against the text the Blueprint read.
///
/// The fields are private: every value is a change that does something.
///
/// # Contract
/// - C1 [invariant]: no spec is a no-op: a `Replace` or `Delete` has a non-empty `range` and an
///   `Insert` a non-empty `text`; the constructors refuse anything else with
///   [`DomainError::Mutation`].
///   tested: domain::mutation::tests::no_op_specs_rejected
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationSpec {
    path: RelPath,
    kind: SpecKind,
    range: TextRange,
    text: String,
}

impl MutationSpec {
    /// Replaces `range` with `text`.
    pub fn replace(
        path: RelPath,
        range: TextRange,
        text: impl Into<String>,
    ) -> Result<Self, DomainError> {
        if range.is_empty() {
            return Err(DomainError::Mutation(format!(
                "replace of an empty range in {path}"
            )));
        }
        Ok(Self {
            path,
            kind: SpecKind::Replace,
            range,
            text: text.into(),
        })
    }

    /// Inserts `text` at byte offset `at`.
    pub fn insert(path: RelPath, at: u32, text: impl Into<String>) -> Result<Self, DomainError> {
        let text = text.into();
        if text.is_empty() {
            return Err(DomainError::Mutation(format!(
                "insert of empty text in {path}"
            )));
        }
        Ok(Self {
            path,
            kind: SpecKind::Insert,
            range: TextRange::empty_at(at),
            text,
        })
    }

    /// Deletes `range`.
    pub fn delete(path: RelPath, range: TextRange) -> Result<Self, DomainError> {
        if range.is_empty() {
            return Err(DomainError::Mutation(format!(
                "delete of an empty range in {path}"
            )));
        }
        Ok(Self {
            path,
            kind: SpecKind::Delete,
            range,
            text: String::new(),
        })
    }

    /// The file.
    pub fn path(&self) -> &RelPath {
        &self.path
    }

    /// What it does.
    pub fn kind(&self) -> SpecKind {
        self.kind
    }

    /// The bytes it touches; for an insert, the empty range at the offset.
    pub fn range(&self) -> TextRange {
        self.range
    }

    /// The new text; empty for a delete.
    pub fn text(&self) -> &str {
        &self.text
    }
}

impl std::fmt::Display for MutationSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}:{}", self.kind, self.path, self.range)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel() -> RelPath {
        RelPath::parse("a.rs").unwrap()
    }

    #[test]
    fn no_op_specs_rejected() {
        let empty = TextRange::empty_at(3);
        assert!(MutationSpec::replace(rel(), empty, "x").is_err());
        assert!(MutationSpec::delete(rel(), empty).is_err());
        assert!(MutationSpec::insert(rel(), 3, "").is_err());

        let r = TextRange::new(1, 4).unwrap();
        let rep = MutationSpec::replace(rel(), r, "z").unwrap();
        assert_eq!(
            (rep.kind(), rep.range(), rep.text()),
            (SpecKind::Replace, r, "z")
        );
        let del = MutationSpec::delete(rel(), r).unwrap();
        assert_eq!((del.kind(), del.text()), (SpecKind::Delete, ""));
        let ins = MutationSpec::insert(rel(), 3, "q").unwrap();
        assert_eq!(
            (ins.kind(), ins.range(), ins.text()),
            (SpecKind::Insert, empty, "q")
        );
        assert_eq!(ins.path(), &rel());
        assert_eq!(ins.to_string(), "insert a.rs:3..3");
    }
}
