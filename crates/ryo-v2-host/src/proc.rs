//! Reads figures of this process from `/proc`, without collapsing a failure into a missing value.
//!
//! The host runs on Linux only: where `/proc` is absent, every read here is an error, and an
//! error ends the run that asked for the figure.

use std::fmt;
use std::path::{Path, PathBuf};

/// The status file of this process: `VmRSS` and the other `Vm*` lines.
const STATUS: &str = "/proc/self/status";

/// The stat line of this process: its CPU times in clock ticks, every thread counted.
const STAT: &str = "/proc/self/stat";

/// The auxiliary vector of this process: `AT_CLKTCK`, the clock ticks per second of `STAT`.
const AUXV: &str = "/proc/self/auxv";

/// The memory of the machine: the `MemTotal` line.
const MEMINFO: &str = "/proc/meminfo";

/// The key of the clock ticks per second in the auxiliary vector (`AT_CLKTCK` of `<elf.h>`).
const AT_CLKTCK: u64 = 17;

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
///   starts with `key`, followed by the unit `kB` and nothing else.
///   tested: proc::tests::vm_rss_reads_kb
/// - C2: no such line, a field that is not a number, or a unit that is missing or not `kB`, is
///   `ProcError::Parse` naming `file` and `key`; never a missing, zero or guessed value.
///   tested: proc::tests::a_missing_or_malformed_line_is_an_error
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

/// The resident set size of this process in kB, the `VmRSS` line of `/proc/self/status`.
///
/// # Contract
/// - C1: the figure is the resident set size line of `/proc/self/status`; a failed read or parse
///   is a `ProcError` naming that file, which the caller turns into an error of its run.
///   tested: proc::tests::vm_rss_of_this_process_reads
pub fn vm_rss_kb() -> Result<u64, ProcError> {
    kb_line(Path::new(STATUS), "VmRSS:")
}

/// The number of the `<key> <n> kB` line of `file`, read and parsed as [`parse_kb`] does.
pub(crate) fn kb_line(file: &Path, key: &'static str) -> Result<u64, ProcError> {
    parse_kb(file, &read(file)?, key)
}

/// The memory of the machine in kB, the `MemTotal` line of `/proc/meminfo`.
///
/// # Contract
/// - C1: the figure is the `MemTotal:` line of `/proc/meminfo`; a failed read or parse is a
///   `ProcError` naming that file.
///   tested: proc::tests::mem_total_of_this_machine_reads
pub fn mem_total_kb() -> Result<u64, ProcError> {
    kb_line(Path::new(MEMINFO), "MemTotal:")
}

/// The CPU time of this process in ms, every thread counted: `utime + stime` of
/// `/proc/self/stat` in clock ticks, over the ticks per second of `/proc/self/auxv`.
///
/// # Contract
/// - C1: the figure is `(utime + stime) * 1000 / AT_CLKTCK`, the fields 14 and 15 of the stat
///   line counted after the parenthesised command name; a failed read, a missing field or one
///   that is not a number is a `ProcError` naming the file, never a zero.
///   tested: proc::tests::cpu_ms_reads_utime_and_stime
///   tested: proc::tests::a_malformed_stat_or_auxv_is_an_error
///   tested: proc::tests::cpu_ms_of_this_process_reads
pub fn cpu_ms() -> Result<u64, ProcError> {
    let stat = Path::new(STAT);
    let ticks = parse_stat_ticks(stat, &read(stat)?)?;
    let auxv = Path::new(AUXV);
    let bytes = std::fs::read(auxv).map_err(|source| ProcError::Read {
        file: auxv.to_path_buf(),
        source,
    })?;
    let per_sec = parse_clock_ticks(auxv, &bytes)?;
    Ok(ticks * 1000 / per_sec)
}

/// `utime + stime` of a `/proc/<pid>/stat` line, in clock ticks.
///
/// The command name is in parentheses and may hold spaces and parentheses itself, so the fields
/// are counted from the last `)`: the state is field 3, `utime` field 14 and `stime` field 15.
fn parse_stat_ticks(file: &Path, text: &str) -> Result<u64, ProcError> {
    let missing = |field| ProcError::Field {
        file: file.to_path_buf(),
        field,
    };
    let (_, rest) = text.rsplit_once(')').ok_or_else(|| missing("comm"))?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let number = |at: usize, field: &'static str| match fields.get(at).map(|f| f.parse::<u64>()) {
        Some(Ok(n)) => Ok(n),
        Some(Err(_)) | None => Err(missing(field)),
    };
    // Field n of the line is index n - 3 after the `)`.
    Ok(number(11, "utime")? + number(12, "stime")?)
}

/// The value of `AT_CLKTCK` in an auxiliary vector: pairs of native-endian words (key, value),
/// ended by a zero key.
fn parse_clock_ticks(file: &Path, bytes: &[u8]) -> Result<u64, ProcError> {
    const WORD: usize = std::mem::size_of::<usize>();
    let (pairs, _) = bytes.as_chunks::<{ 2 * WORD }>();
    let word = |b: &[u8]| {
        let mut w = [0u8; WORD];
        w.copy_from_slice(b);
        usize::from_ne_bytes(w) as u64
    };
    pairs
        .iter()
        .map(|pair| (word(&pair[..WORD]), word(&pair[WORD..])))
        .take_while(|&(key, _)| key != 0)
        .find_map(|(key, value)| (key == AT_CLKTCK && value > 0).then_some(value))
        .ok_or_else(|| ProcError::Field {
            file: file.to_path_buf(),
            field: "AT_CLKTCK",
        })
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
    fn mem_total_of_this_machine_reads() {
        assert!(mem_total_kb().unwrap() > vm_rss_kb().unwrap());
    }

    /// An auxiliary vector of the pairs `(key, value)`, ended by a zero key.
    fn auxv(pairs: &[(usize, usize)]) -> Vec<u8> {
        let mut out = Vec::new();
        for &(k, v) in pairs.iter().chain([(0, 0)].iter()) {
            out.extend_from_slice(&k.to_ne_bytes());
            out.extend_from_slice(&v.to_ne_bytes());
        }
        out
    }

    #[test]
    fn cpu_ms_reads_utime_and_stime() {
        let f = Path::new(STAT);
        // A command name with a space and a parenthesis; utime 250, stime 50.
        let line = "42 (a b) c) S 1 2 3 4 5 6 7 8 9 10 250 50 0 0 20 0 9 0 100 2000 300\n";
        assert_eq!(parse_stat_ticks(f, line).unwrap(), 300);
        let a = Path::new(AUXV);
        assert_eq!(
            parse_clock_ticks(a, &auxv(&[(6, 4096), (17, 100)])).unwrap(),
            100
        );
    }

    #[test]
    fn a_malformed_stat_or_auxv_is_an_error() {
        let f = Path::new(STAT);
        for line in [
            "",
            "42 no-parenthesis S 1",
            "42 (x) S 1 2 3 4 5 6 7 8 9 10\n",
            "42 (x) S 1 2 3 4 5 6 7 8 9 10 many 50 0\n",
        ] {
            match parse_stat_ticks(f, line) {
                Err(e @ ProcError::Field { .. }) => {
                    assert!(e.to_string().contains(STAT), "{e}");
                }
                other => panic!("{line:?}: {other:?}"),
            }
        }
        let a = Path::new(AUXV);
        // No AT_CLKTCK, one after the end marker, a zero one, and a cut word.
        let mut after_end = auxv(&[(6, 4096)]);
        after_end.extend_from_slice(&auxv(&[(17, 100)]));
        let mut cut = auxv(&[(17, 100)]);
        cut.truncate(std::mem::size_of::<usize>() + 1);
        for bytes in [auxv(&[(6, 4096)]), after_end, auxv(&[(17, 0)]), cut] {
            match parse_clock_ticks(a, &bytes) {
                Err(e @ ProcError::Field { .. }) => {
                    let shown = e.to_string();
                    assert!(
                        shown.contains(AUXV) && shown.contains("AT_CLKTCK"),
                        "{shown}"
                    );
                }
                other => panic!("{bytes:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn cpu_ms_of_this_process_reads() {
        // Spin until the process has used CPU time; the figure never goes back.
        let first = cpu_ms().unwrap();
        let mut x = 0u64;
        while cpu_ms().unwrap() <= first {
            x = std::hint::black_box(x.wrapping_add(1));
        }
        assert!(cpu_ms().unwrap() > first);
    }
}
