//! Reads figures of this process from `/proc`, without collapsing a failure into a missing value.
//!
//! The host runs on Linux only: where `/proc` is absent, every read here is an error, and an
//! error ends the run that asked for the figure.

use std::fmt;
use std::path::{Path, PathBuf};

/// The status file of this process: `VmRSS` and the other `Vm*` lines.
const STATUS: &str = "/proc/self/status";

/// A read of a `/proc` file that failed or did not give the figure.
#[derive(Debug)]
pub enum ProcError {
    /// The file could not be read.
    Read {
        /// The file.
        file: PathBuf,
        /// What the read returned.
        source: std::io::Error,
    },
    /// The file was read, but the figure is missing or not a number.
    Parse {
        /// The file.
        file: PathBuf,
        /// The key of the line, as `VmRSS:`.
        key: &'static str,
    },
}

impl fmt::Display for ProcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProcError::Read { file, source } => {
                write!(f, "read {}: {source}", file.display())
            }
            ProcError::Parse { file, key } => {
                write!(f, "{}: no `{key} <n> kB` line", file.display())
            }
        }
    }
}

impl std::error::Error for ProcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ProcError::Read { source, .. } => Some(source),
            ProcError::Parse { .. } => None,
        }
    }
}

/// The text of `file`.
///
/// # Contract
/// - C1: a file that cannot be read is `ProcError::Read` naming the file.
///   tested: proc::tests::an_unreadable_file_is_an_error_naming_it
fn read(file: &Path) -> Result<String, ProcError> {
    std::fs::read_to_string(file).map_err(|source| ProcError::Read {
        file: file.to_path_buf(),
        source,
    })
}

/// The number of the first `<key> <n> kB` line of `text`, the content of `file`.
///
/// # Contract
/// - C1: the number is the first whitespace-separated field after `key` on the first line that
///   starts with `key`, read as kB.
///   tested: proc::tests::vm_rss_reads_kb
/// - C2: no such line, or a field that is not a number, is `ProcError::Parse` naming `file` and
///   `key`; never a missing or zero value.
///   tested: proc::tests::a_missing_or_malformed_line_is_an_error
fn parse_kb(file: &Path, text: &str, key: &'static str) -> Result<u64, ProcError> {
    let field = text
        .lines()
        .find_map(|l| l.strip_prefix(key))
        .and_then(|rest| rest.split_whitespace().next());
    match field.map(str::parse::<u64>) {
        Some(Ok(n)) => Ok(n),
        Some(Err(_)) | None => Err(ProcError::Parse {
            file: file.to_path_buf(),
            key,
        }),
    }
}

/// The resident set size of this process in kB, the `VmRSS` line of `/proc/self/status`.
///
/// # Contract
/// - C1: the figure is the resident set size line of `/proc/self/status`; a failed read or parse
///   is a `ProcError` naming that file, which the caller turns into an error of its run.
///   tested: proc::tests::vm_rss_of_this_process_reads
pub fn vm_rss_kb() -> Result<u64, ProcError> {
    let file = Path::new(STATUS);
    parse_kb(file, &read(file)?, "VmRSS:")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_rss_reads_kb() {
        let f = Path::new(STATUS);
        let text = "Name:\tx\nVmRSS:\t  12345 kB\nVmRSS:\t 1 kB\n";
        assert_eq!(parse_kb(f, text, "VmRSS:").unwrap(), 12345);
    }

    #[test]
    fn a_missing_or_malformed_line_is_an_error() {
        let f = Path::new(STATUS);
        for text in [
            "Name:\tx\nVmSize:\t 1 kB\n",
            "Name:\tx\nVmRSS:\n",
            "Name:\tx\nVmRSS:\t twelve kB\n",
            "",
        ] {
            match parse_kb(f, text, "VmRSS:") {
                Err(e @ ProcError::Parse { .. }) => {
                    let shown = e.to_string();
                    assert!(shown.contains(STATUS), "{shown}");
                    assert!(shown.contains("VmRSS:"), "{shown}");
                }
                other => panic!("{text:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn an_unreadable_file_is_an_error_naming_it() {
        let f = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/no-such-proc-file"));
        match read(f) {
            Err(e @ ProcError::Read { .. }) => {
                assert!(e.to_string().contains("no-such-proc-file"), "{e}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn vm_rss_of_this_process_reads() {
        assert!(vm_rss_kb().unwrap() > 0);
    }
}
