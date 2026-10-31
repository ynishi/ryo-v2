//! The rules and types of the problem, in its own words.
//!
//! Pure: nothing here touches the file system, a process, the environment, the
//! network, the clock or a random source. A value that comes from outside
//! (a file's contents, the current time) is passed in by the caller.
//! A value with rules is a type, not a `String`: an enum for a closed set,
//! a newtype that checks its input for anything else.

pub mod baseline;
pub mod diagnostic;
pub mod edit;
pub mod error;
pub mod error_code;
pub mod finding;
pub mod rel_path;
pub mod scope;
pub mod verdict;
pub use diagnostic::{Diagnostic, Diagnostics, UnmappedDiagnostic};
pub use edit::Edit;
pub use error::DomainError;
pub use error_code::ErrorCode;
pub use finding::{Finding, FindingCode, ScopedFindings};
pub use rel_path::RelPath;
pub use scope::Scope;
pub use verdict::Verdict;
