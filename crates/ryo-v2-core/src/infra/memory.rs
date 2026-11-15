//! The memory of this process and of the machine, read from `/proc`: the [`Gauge`] a search reads
//! its RSS stop from, and the figures the measurement commands read, without collapsing a failure
//! into a missing value.
//!
//! The reads are Linux reads: where `/proc` is absent, every read here is an error. The [`Gauge`]
//! port alone turns an error into `None`, as its contract asks.

use crate::app::memory::MemoryBudget;
use crate::app::ports::Gauge;
use std::fmt;
use std::path::{Path, PathBuf};

/// The status file of this process: `VmRSS` and the other `Vm*` lines.
pub const STATUS: &str = "/proc/self/status";

/// The memory of the machine: the `MemTotal` and `MemAvailable` lines.
pub const MEMINFO: &str = "/proc/meminfo";

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
    /// The file was read, but a field that is not a `<key> <n> kB` line is missing or not a
    /// number.
    Field {
        /// The file.
        file: PathBuf,
        /// The field, as `utime` or `AT_CLKTCK`.
        field: &'static str,
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
            ProcError::Field { file, field } => {
                write!(f, "{}: no number for `{field}`", file.display())
            }
        }
    }
}

impl std::error::Error for ProcError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ProcError::Read { source, .. } => Some(source),
            ProcError::Parse { .. } | ProcError::Field { .. } => None,
        }
    }
}

/// The text of `file`.
///
/// # Contract
/// - C1: a file that cannot be read is `ProcError::Read` naming the file.
///   tested: infra::memory::tests::an_unreadable_file_is_an_error_naming_it
pub fn read(file: &Path) -> Result<String, ProcError> {
    std::fs::read_to_string(file).map_err(|source| ProcError::Read {
        file: file.to_path_buf(),
        source,
    })
}

/// The number of the first `<key> <n> kB` line of `text`, the content of `file`.
///
/// # Contract
/// - C1: the number is the first whitespace-separated field after `key` on the first line that
///   starts with `key`, followed by the unit `kB` and nothing else.
///   tested: infra::memory::tests::vm_rss_reads_kb
/// - C2: no such line, a field that is not a number, or a unit that is missing or not `kB`, is
///   `ProcError::Parse` naming `file` and `key`; never a missing, zero or guessed value.
///   tested: infra::memory::tests::a_missing_or_malformed_line_is_an_error
fn parse_kb(file: &Path, text: &str, key: &'static str) -> Result<u64, ProcError> {
    let fields: Option<Vec<&str>> = text
        .lines()
        .find_map(|l| l.strip_prefix(key))
        .map(|rest| rest.split_whitespace().collect());
    let number = match fields.as_deref() {
        Some([n, "kB"]) => Some(n.parse::<u64>()),
        _ => None,
    };
    match number {
        Some(Ok(n)) => Ok(n),
        Some(Err(_)) | None => Err(ProcError::Parse {
            file: file.to_path_buf(),
            key,
        }),
    }
}

/// The number of the first `<key> <n> kB` line of `file`: the number followed by the unit `kB` and
/// nothing else, or `ProcError::Parse` naming `file` and `key`.
pub fn kb_line(file: &Path, key: &'static str) -> Result<u64, ProcError> {
    parse_kb(file, &read(file)?, key)
}

/// The resident set size of this process in kB, the `VmRSS` line of `/proc/self/status`.
///
/// # Contract
/// - C1: the figure is the resident set size line of `/proc/self/status`; a failed read or parse
///   is a `ProcError` naming that file, which the caller turns into an error of its run.
///   tested: infra::memory::tests::vm_rss_of_this_process_reads
pub fn vm_rss_kb() -> Result<u64, ProcError> {
    kb_line(Path::new(STATUS), "VmRSS:")
}

/// The memory of the machine in kB, the `MemTotal` line of `/proc/meminfo`.
///
/// # Contract
/// - C1: the figure is the `MemTotal:` line of `/proc/meminfo`; a failed read or parse is a
///   `ProcError` naming that file.
///   tested: infra::memory::tests::mem_total_and_available_of_this_machine_read
pub fn mem_total_kb() -> Result<u64, ProcError> {
    kb_line(Path::new(MEMINFO), "MemTotal:")
}

/// The memory the machine can still give without swapping in kB, the `MemAvailable` line of
/// `/proc/meminfo`: unlike `MemTotal` it counts what the other processes hold, the proc-macro
/// servers included, which are outside this process's `VmRSS`.
///
/// # Contract
/// - C1: the figure is the `MemAvailable:` line of `/proc/meminfo`, at most what
///   [`mem_total_kb`] reads; a failed read or parse is a `ProcError` naming that file.
///   tested: infra::memory::tests::mem_total_and_available_of_this_machine_read
pub fn mem_available_kb() -> Result<u64, ProcError> {
    kb_line(Path::new(MEMINFO), "MemAvailable:")
}

/// The memory budget of a search that starts now: its RSS may grow by what the machine has
/// available now less the reserve (`MemoryBudget::of_available`).
///
/// # Contract
/// - C1: it is `MemoryBudget::of_available` of [`mem_available_kb`]; a failed read is that
///   read's `ProcError`.
///   tested: infra::memory::tests::the_budget_at_start_is_the_available_memory_less_the_reserve
pub fn budget_at_start() -> Result<MemoryBudget, ProcError> {
    mem_available_kb().map(MemoryBudget::of_available)
}

/// The [`Gauge`] of this process: its `VmRSS`.
///
/// # Contract
/// - C1: `rss_kb` is [`vm_rss_kb`], and `None` where that read fails.
///   tested: infra::memory::tests::the_proc_gauge_reads_the_rss
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcGauge;

impl Gauge for ProcGauge {
    fn rss_kb(&self) -> Option<u64> {
        vm_rss_kb().ok()
    }
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
            // A unit that is missing or not kB is not guessed at.
            "Name:\tx\nVmRSS:\t 12\n",
            "Name:\tx\nVmRSS:\t 12 MB\n",
            "Name:\tx\nVmRSS:\t 12 kB 3\n",
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

    #[test]
    fn mem_total_and_available_of_this_machine_read() {
        let total = mem_total_kb().unwrap();
        assert!(total > vm_rss_kb().unwrap());
        let available = mem_available_kb().unwrap();
        assert!(
            available > 0 && available <= total,
            "{available} of {total}"
        );
    }

    #[test]
    fn the_budget_at_start_is_the_available_memory_less_the_reserve() {
        // MemAvailable moves with the other processes of the machine between two reads: the
        // budget is compared with the reads around it, within 256 MiB.
        let before = MemoryBudget::of_available(mem_available_kb().unwrap()).growth_kb();
        let budget = budget_at_start().unwrap().growth_kb();
        let after = MemoryBudget::of_available(mem_available_kb().unwrap()).growth_kb();
        let drift = 256 * 1024;
        assert!(
            budget + drift >= before.min(after) && budget <= before.max(after) + drift,
            "{budget} kB against {before} and {after}"
        );
    }

    #[test]
    fn the_proc_gauge_reads_the_rss() {
        let rss = ProcGauge.rss_kb().expect("the RSS of this process");
        let direct = vm_rss_kb().unwrap();
        // The RSS of a running test process moves a little between two reads.
        assert!(rss.abs_diff(direct) < 64 * 1024, "{rss} against {direct}");
    }
}
