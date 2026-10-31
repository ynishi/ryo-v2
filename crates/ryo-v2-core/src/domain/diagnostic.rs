//! Diagnostics as the oracle compares them.

use super::{ErrorCode, RelPath};

/// oracle が比べる単位: E-code + workspace 相対 path + 1 始まりの行。列と message は持たない。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Diagnostic {
    /// rustc error code.
    pub code: ErrorCode,
    /// Workspace-relative file.
    pub path: RelPath,
    /// 1-based line.
    pub line: u32,
}

/// Error severity だが rustc の E-code に写せない診断 (ra 固有 code: "unresolved-macro-call", "syntax-error", ...)。oracle は FP 側に数える。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnmappedDiagnostic {
    /// The analyzer's own code name.
    pub ra_code: String,
    /// Workspace-relative file.
    pub path: RelPath,
    /// 1-based line.
    pub line: u32,
}

/// Analyzer::diagnostics の返り値。両 Vec とも sort 済み (Ord 順)、重複は残す。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diagnostics {
    /// Errors with a rustc E-code.
    pub errors: Vec<Diagnostic>,
    /// Errors without one.
    pub unmapped: Vec<UnmappedDiagnostic>,
}

impl Diagnostics {
    /// errors と unmapped が両方空。
    pub fn is_empty(&self) -> bool {
        self.errors.is_empty() && self.unmapped.is_empty()
    }
}
