//! Error diagnostics as the analyzer reports them, before the baseline difference.

use super::{ErrorCode, RelPath};

/// The code of a [`Finding`]: a rustc E-code, or the analyzer's own name for an
/// Error that has none ("unresolved-macro-call", "syntax-error", ...).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FindingCode {
    /// A rustc error code.
    Rustc(ErrorCode),
    /// The analyzer's own code name.
    Analyzer(String),
}

/// One Error-severity diagnostic as the analyzer reported it, message included:
/// the input of the baseline difference. ([`super::Diagnostic`] is the output, without message.)
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Finding {
    /// What kind of error.
    pub code: FindingCode,
    /// The analyzer's message.
    pub message: String,
    /// Workspace-relative file.
    pub path: RelPath,
    /// 1-based line.
    pub line: u32,
}

/// What one diagnostics run looked at and what it found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopedFindings {
    /// The files diagnosed: created files included, deleted files excluded. Sorted, unique.
    pub files: Vec<RelPath>,
    /// Every Error in those files, sorted (Ord), duplicates kept.
    pub findings: Vec<Finding>,
}
