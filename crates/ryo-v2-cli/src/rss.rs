//! Reads the resident set size of this process.

/// The VmRSS of this process in kB; None off Linux or when unreadable.
pub fn current_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    parse_vmrss(&status)
}

/// Parses the `VmRSS:   12345 kB` line.
pub fn parse_vmrss(status: &str) -> Option<u64> {
    status
        .lines()
        .find(|l| l.starts_with("VmRSS:"))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_vmrss_reads_kb() {
        assert_eq!(parse_vmrss("Name:\tx\nVmRSS:\t  12345 kB\n"), Some(12345));
    }

    #[test]
    fn parse_vmrss_none_without_line() {
        assert_eq!(parse_vmrss("Name:\tx\nVmSize:\t 1 kB\n"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn current_kb_is_some_on_linux() {
        assert!(current_kb().is_some());
    }
}
