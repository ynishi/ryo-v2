//! Reads figures of this process from `/proc`, without collapsing a failure into a missing value.
//!
//! The host runs on Linux only: where `/proc` is absent, every read here is an error, and an
//! error ends the run that asked for the figure. The memory reads (`VmRSS`, `MemTotal`,
//! `MemAvailable`) and the error type are `ryo-v2-core`'s (`infra::memory`), which the search's
//! RSS stop reads too; this module adds the threads and the CPU time.

use std::path::Path;

pub use ryo_v2_core::infra::memory::{
    ProcError, kb_line, mem_available_kb, mem_total_kb, vm_rss_kb,
};
use ryo_v2_core::infra::memory::{STATUS, read};

/// The stat line of this process: its CPU times in clock ticks, every thread counted.
const STAT: &str = "/proc/self/stat";

/// The auxiliary vector of this process: `AT_CLKTCK`, the clock ticks per second of `STAT`.
const AUXV: &str = "/proc/self/auxv";

/// The key of the clock ticks per second in the auxiliary vector (`AT_CLKTCK` of `<elf.h>`).
const AT_CLKTCK: u64 = 17;

/// The threads of this process, the `Threads` line of `/proc/self/status`: rust-analyzer's
/// dropper threads and the job pool's workers included.
///
/// # Contract
/// - C1: the figure is the one number after `Threads:` on the first line that starts with it; no
///   such line, or a field that is not a number or is followed by anything, is
///   `ProcError::Field` naming the file and the key.
///   tested: proc::tests::threads_reads_the_count
pub fn threads() -> Result<u64, ProcError> {
    let file = Path::new(STATUS);
    parse_count(file, &read(file)?, "Threads:")
}

/// The number of the first `<key> <n>` line of `text`, a count without a unit.
fn parse_count(file: &Path, text: &str, key: &'static str) -> Result<u64, ProcError> {
    let fields: Option<Vec<&str>> = text
        .lines()
        .find_map(|l| l.strip_prefix(key))
        .map(|rest| rest.split_whitespace().collect());
    match fields.as_deref() {
        Some([n]) => n.parse::<u64>().ok(),
        _ => None,
    }
    .ok_or_else(|| ProcError::Field {
        file: file.to_path_buf(),
        field: key,
    })
}

/// The CPU time of this process in ms, every thread counted: `utime + stime` of
/// `/proc/self/stat` in clock ticks, over the ticks per second of `/proc/self/auxv`.
///
/// # Contract
/// - C1: the figure is `(utime + stime) * 1000 / AT_CLKTCK`, the fields 14 and 15 of the stat
///   line counted after the parenthesised command name; a failed read, a missing field or one
///   that is not a number is a `memory::ProcError` naming the file, never a zero.
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
    fn threads_reads_the_count() {
        let f = Path::new(STATUS);
        assert_eq!(
            parse_count(f, "Name:\tx\nThreads:\t17\nThreads:\t2\n", "Threads:").unwrap(),
            17
        );
        for text in [
            "Name:\tx\n",
            "Threads:\n",
            "Threads:\tmany\n",
            "Threads:\t3 kB\n",
        ] {
            match parse_count(f, text, "Threads:") {
                Err(e @ ProcError::Field { .. }) => {
                    assert!(e.to_string().contains("Threads:"), "{e}");
                }
                other => panic!("{text:?}: {other:?}"),
            }
        }
        assert!(threads().unwrap() >= 1);
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
