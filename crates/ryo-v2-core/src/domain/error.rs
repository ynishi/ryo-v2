//! Errors of the domain's value types.

/// A string did not satisfy the rules of a domain value type.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DomainError {
    /// Not `E` followed by exactly four ASCII digits.
    #[error("not a rustc error code (E followed by four digits): {0:?}")]
    ErrorCode(String),
    /// Not a workspace-relative `/`-separated path.
    #[error("not a workspace-relative path: {0:?}")]
    RelPath(String),
}
