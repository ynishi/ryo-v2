//! The memory of a search on N live forks (D11, stage 4): how many Layers a machine holds
//! (`max_forks`, from Table M4 of SEARCH_LEDGER.md), and the RSS stop of one search, relative to
//! the RSS at its start (`MemoryBudget`, `RssStop`).
//!
//! The figures are read through the [`Gauge`] port; the `/proc` reads are in
//! [`crate::infra::memory`].

use crate::app::ports::Gauge;
use crate::app::search::{RESERVED_RAM_GB, SESSION_RAM_GB};

/// Table M4 (SEARCH_LEDGER.md): what one Layer keeps, in MiB, the worst one measured: a wide4 item
/// level at 1 thread, 52.17 MB kept plus 1.52 MB of view, 51.2 MiB, rounded up.
pub const LAYER_MIB: u64 = 52;

/// Table M4 (SEARCH_LEDGER.md): what one running job holds besides what it keeps, in MiB, the worst
/// one measured: 9.87 MB, rounded up.
pub const JOB_TRANSIENT_MIB: u64 = 10;

/// KiB in a GiB: the `GB` of D3's constants are GiB, as `TRIGGER_RSS_KB` reads them.
const KIB_PER_GIB: u64 = 1024 * 1024;

/// How many Layers fit on a machine of `ram_gib` with `width` jobs running at once: the memory
/// past D3's reserve and one session, less the transient of `width` jobs, over the worst Layer of
/// Table M4.
///
/// # Contract
/// - C1: `floor(((ram_gib - RESERVED_RAM_GB - SESSION_RAM_GB) * 1024 - width * JOB_TRANSIENT_MIB)
///   / LAYER_MIB)`, every subtraction saturating at zero, and at least 1: 30 GiB at W = 4 gives
///   314, and no machine gives fewer than 1.
///   tested: app::memory::tests::max_forks_follows_table_m4
pub fn max_forks(ram_gib: u64, width: u64) -> u64 {
    let free_mib = ram_gib
        .saturating_sub(RESERVED_RAM_GB)
        .saturating_sub(SESSION_RAM_GB)
        .saturating_mul(1024);
    let layers_mib = free_mib.saturating_sub(width.saturating_mul(JOB_TRANSIENT_MIB));
    (layers_mib / LAYER_MIB).max(1)
}

/// How far the RSS of one search may grow past its RSS at the start.
///
/// A budget is relative to the start of each search, so a long-lived session that once grew (an
/// allocator keeps its peak) is not stopped at its next search's first job.
///
/// # Contract
/// - C1: `of_available(kb)` allows `kb` less D3's reserve (`RESERVED_RAM_GB` GiB), saturating at
///   zero; `unbounded()` allows any growth.
///   tested: app::memory::tests::a_budget_is_the_available_memory_less_the_reserve
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct MemoryBudget {
    growth_kb: u64,
}

impl MemoryBudget {
    /// The budget of a search that starts with `mem_available_kb` available on the machine
    /// (`MemAvailable`, which counts the other processes and the proc-macro servers).
    pub fn of_available(mem_available_kb: u64) -> MemoryBudget {
        MemoryBudget {
            growth_kb: mem_available_kb.saturating_sub(RESERVED_RAM_GB * KIB_PER_GIB),
        }
    }

    /// No stop: for tests and for callers that read no gauge.
    pub fn unbounded() -> MemoryBudget {
        MemoryBudget {
            growth_kb: u64::MAX,
        }
    }

    /// The growth allowed, in kB.
    pub fn growth_kb(&self) -> u64 {
        self.growth_kb
    }
}

/// The RSS stop of one search: its RSS at the start, and the budget of its growth.
///
/// # Contract
/// - C1: `reached` is true exactly when the gauge reads an RSS more than `growth_kb` above the RSS
///   it read at `start`; an RSS at or below the start, or a growth at the budget itself, is not.
///   tested: app::memory::tests::the_rss_stop_is_relative_to_its_start
/// - C2: a gauge that cannot read (`None`), at the start or later, never stops: the stop is a
///   backstop, not a reason to fail the search.
///   tested: app::memory::tests::an_unreadable_gauge_never_stops
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct RssStop {
    start_kb: Option<u64>,
    budget: MemoryBudget,
}

impl RssStop {
    /// The stop of a search that starts now: reads the RSS at its start from `gauge`.
    pub fn start(gauge: &dyn Gauge, budget: MemoryBudget) -> RssStop {
        RssStop {
            start_kb: gauge.rss_kb(),
            budget,
        }
    }

    /// The RSS read at the start; `None` when the gauge could not read it.
    pub fn start_kb(&self) -> Option<u64> {
        self.start_kb
    }

    /// Whether the RSS `gauge` reads now has grown past the budget.
    pub fn reached(&self, gauge: &dyn Gauge) -> bool {
        match (self.start_kb, gauge.rss_kb()) {
            (Some(start), Some(now)) => now.saturating_sub(start) > self.budget.growth_kb,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A gauge that reads what the test sets.
    struct FakeGauge(Mutex<Option<u64>>);

    impl FakeGauge {
        fn new(rss_kb: Option<u64>) -> FakeGauge {
            FakeGauge(Mutex::new(rss_kb))
        }

        fn set(&self, rss_kb: Option<u64>) {
            *self.0.lock().unwrap() = rss_kb;
        }
    }

    impl Gauge for FakeGauge {
        fn rss_kb(&self) -> Option<u64> {
            *self.0.lock().unwrap()
        }
    }

    #[test]
    fn max_forks_follows_table_m4() {
        assert_eq!((LAYER_MIB, JOB_TRANSIENT_MIB), (52, 10));
        // (30 - 8 - 6) * 1024 = 16,384 MiB; less 4 jobs of 10 MiB: 16,344 / 52 = 314.
        assert_eq!(max_forks(30, 4), 314);
        assert_eq!(max_forks(30, 1), 314);
        assert_eq!(max_forks(30, 12), 312);
        assert_eq!(max_forks(64, 4), 983);
        // No room past the reserve and the session: still one.
        assert_eq!(max_forks(14, 4), 1);
        assert_eq!(max_forks(0, 0), 1);
        assert_eq!(max_forks(15, 200), 1);
        assert_eq!(max_forks(u64::MAX, u64::MAX), 1);
    }

    #[test]
    fn a_budget_is_the_available_memory_less_the_reserve() {
        let gib = 1024 * 1024;
        assert_eq!(MemoryBudget::of_available(20 * gib).growth_kb(), 12 * gib);
        assert_eq!(MemoryBudget::of_available(8 * gib).growth_kb(), 0);
        assert_eq!(MemoryBudget::of_available(gib).growth_kb(), 0);
        assert_eq!(MemoryBudget::unbounded().growth_kb(), u64::MAX);
    }

    #[test]
    fn the_rss_stop_is_relative_to_its_start() {
        // A session that once grew to 9 GiB starts its next search there.
        let gauge = FakeGauge::new(Some(9_000_000));
        let budget = MemoryBudget {
            growth_kb: 1_000_000,
        };
        let stop = RssStop::start(&gauge, budget);
        assert_eq!(stop.start_kb(), Some(9_000_000));
        assert!(!stop.reached(&gauge), "the RSS at the start is no stop");
        gauge.set(Some(10_000_000));
        assert!(!stop.reached(&gauge), "the budget itself is no stop");
        gauge.set(Some(10_000_001));
        assert!(stop.reached(&gauge), "one kB past the budget stops");
        gauge.set(Some(4_000_000));
        assert!(!stop.reached(&gauge), "an RSS below the start is no stop");
        // Unbounded: nothing stops.
        gauge.set(Some(9_000_000));
        let free = RssStop::start(&gauge, MemoryBudget::unbounded());
        gauge.set(Some(u64::MAX));
        assert!(!free.reached(&gauge));
    }

    #[test]
    fn an_unreadable_gauge_never_stops() {
        let budget = MemoryBudget { growth_kb: 0 };
        let gauge = FakeGauge::new(None);
        let blind = RssStop::start(&gauge, budget);
        gauge.set(Some(u64::MAX));
        assert!(!blind.reached(&gauge), "no RSS at the start");
        gauge.set(Some(5));
        let stop = RssStop::start(&gauge, budget);
        gauge.set(None);
        assert!(!stop.reached(&gauge), "no RSS now");
    }
}
