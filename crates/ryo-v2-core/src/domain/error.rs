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
    /// A text range whose start is past its end.
    #[error("invalid text range: {0}")]
    Range(String),
    /// Not `fnv1a64:` followed by 16 lowercase hex digits.
    #[error("not a fingerprint (fnv1a64:<16 lowercase hex>): {0:?}")]
    Fingerprint(String),
    /// Not a usable Rust identifier.
    #[error("not a usable identifier: {0:?}")]
    Ident(String),
    /// Not a `::`-separated path of identifiers.
    #[error("not a derive name: {0:?}")]
    DeriveName(String),
    /// Not a UUID version 7.
    #[error("not a version 7 uuid: {0:?}")]
    Uuid(String),
    /// A mutation spec that changes nothing or is malformed.
    #[error("invalid mutation: {0}")]
    Mutation(String),
    /// Not `<doc>#<item>`.
    #[error("not a spec reference (<doc>#<item>): {0:?}")]
    SpecRef(String),
    /// A required text was empty.
    #[error("empty value: {0}")]
    Empty(String),
}
