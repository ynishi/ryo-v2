//! The outcome of verifying edits.

use super::Diagnostics;

/// Pass or fail, with the errors the edits introduced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// new_errors.is_empty() と同値。
    pub passed: bool,
    /// baseline に無かった Error。
    pub new_errors: Diagnostics,
}

impl Verdict {
    /// Builds a verdict; `passed` iff there are no new errors.
    pub fn from_diagnostics(new_errors: Diagnostics) -> Self {
        Self {
            passed: new_errors.is_empty(),
            new_errors,
        }
    }
}
