//! The engine probe: what the database of a loaded session holds and what one stacked check adds
//! to the heap, measured on the one stacked procedure of [`crate::stacked`].
//!
//! The `engine-probe` binary links this module with its counting allocator, which it hands in
//! as a [`HeapGauge`]; this module holds no allocator of its own, so the `ryo-v2` binary that
//! links the same library runs on the system allocator. Every `/proc` figure is read through
//! [`crate::proc`]: a failed read ends the run, and no report is written.
//!
//! No time figure of the probe stands for the engine's speed: the counting allocator adds two
//! atomic operations per allocation. The probe's check times are kept only to state that
//! overhead against `bench`, run back to back on the system allocator.

use crate::bench::{label_of, scope_name, summarize};
use crate::proc::{self, ProcError};
use crate::stacked::{self, DepthPoint, Point};
use anyhow::{Context, Result};
use ryo_v2_core::app::ports::{Analyzer, Event, Observer};
use ryo_v2_core::domain::Scope;
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::overlay::overlay_edits;
use ryo_v2_ra::{Backend, IngredientRow, MemoRow, MemoryReport, RaAnalyzer, RaConfig, RaSession};
use serde::Serialize;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

/// Bytes per GiB.
const GIB: f64 = (1u64 << 30) as f64;

/// `bytes` in GiB.
fn gib(bytes: u64) -> f64 {
    bytes as f64 / GIB
}

/// `kb` kB in GiB.
fn kb_gib(kb: u64) -> f64 {
    kb as f64 * 1024.0 / GIB
}

/// `to - from` bytes in GiB, negative when the heap shrank.
fn diff_gib(to: u64, from: u64) -> f64 {
    (to as f64 - from as f64) / GIB
}

/// The live heap of the process as the counting allocator of `engine-probe` keeps it.
pub trait HeapGauge: Sync {
    /// The bytes allocated and not yet freed.
    fn live_bytes(&self) -> u64;
    /// The most bytes that were live at once since the process started, or since the last
    /// [`HeapGauge::reset_peak`].
    fn peak_bytes(&self) -> u64;
    /// Starts the peak again from the bytes live now, so that the next `peak_bytes` is the most
    /// live at once from here on (the transient peak of one operation).
    fn reset_peak(&self);
}

/// Reads the resident set size in kB: `proc::vm_rss_kb`, or a failing reader in a test.
type RssReader = fn() -> Result<u64, ProcError>;

/// The bench's median of the warm checks of one variant and scope, to state the probe's
/// overhead against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchMedian {
    /// The variant as given on the command line of both runs.
    pub variant: String,
    /// The scope.
    pub scope: Scope,
    /// The median `check_ms` of the bench's warm runs (its summary's `warm_check_ms` median).
    pub ms: u64,
}

/// Arguments of one probe run.
#[derive(Debug, Clone)]
pub struct ProbeArgs {
    /// The workspace to load.
    pub base: PathBuf,
    /// The parent overlay: level 1 of every series.
    pub on: Option<PathBuf>,
    /// Overlay dirs; each one's edits that the parent does not make are level 2.
    pub variants: Vec<PathBuf>,
    /// Repeats per (variant, scope).
    pub repeat: u32,
    /// Scopes to diagnose, in order.
    pub scopes: Vec<Scope>,
    /// Whether the person running it kept the machine otherwise idle.
    pub idle: bool,
    /// The bench's medians to state the overhead against.
    pub bench_medians: Vec<BenchMedian>,
    /// The backend of the forks (`RaConfig.engine`); the report names it.
    pub backend: Backend,
}

/// What one probe run measured, written as JSON to `--report`.
///
/// # Contract
/// - C1: the report holds exactly the fields of `ProbeReport` (backend, machine, phases,
///   composition, checks, overhead), one `checks` entry per diagnosed level with h0, hW, h1, h2,
///   h3, delta, freed at write, freed at pop and the probe's wall ms; memory in GiB. backend,
///   the first key, is the run's [`ryo_v2_ra::Backend`] by name, `"undo-log"` or `"engine"`, so that two
///   runs can be told apart.
///   tested: engine_probe::tests::report_json_keeps_its_keys_and_values
/// - C2: a check's freed at write is h0 - hW (what the new revision of the level's write frees),
///   its delta h1 - hW (what the diagnosis allocates and keeps after that write) and its freed at
///   pop h2 - h3, all read from the counting allocator, never from the RSS.
///   tested: engine_probe::tests::delta_and_freed_read_the_allocator
/// - C3: a failed read of `/proc` ends the run with an error naming the file; no figure of the
///   report is left out or zero because of it.
///   tested: engine_probe::tests::a_failed_proc_read_is_an_error
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProbeReport {
    /// The backend the forks ran on (`--engine`).
    pub backend: Backend,
    /// The machine the run measured on.
    pub machine: Machine,
    /// P0 before load, P1 at `Event::Indexed`, P2 at `Event::Loaded`, P3 after the last check.
    pub phases: Vec<Phase>,
    /// The session's `memory_report` at P2 and at P3.
    pub composition: Vec<Composition>,
    /// One entry per diagnosed variant level, in the order they ran.
    pub checks: Vec<CheckRow>,
    /// One entry per (variant, scope) that ran.
    pub overhead: Vec<Overhead>,
}

/// The machine of a [`ProbeReport`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Machine {
    /// `MemTotal` of `/proc/meminfo`.
    pub ram_gib: f64,
    /// The available parallelism of the process.
    pub cores: usize,
    /// The session's diagnostic threads (`RaConfig.threads`, or the cores when unset).
    pub threads: usize,
    /// Whether the person running it kept the machine otherwise idle (`--idle`).
    pub idle: bool,
}

/// The memory of the process at one phase of a [`ProbeReport`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Phase {
    /// `P0`, `P1`, `P2` or `P3`.
    pub phase: &'static str,
    /// The resident set size.
    pub rss_gib: f64,
    /// The live heap, from the counting allocator.
    pub live_heap_gib: f64,
    /// The peak live heap so far, from the counting allocator.
    pub peak_heap_gib: f64,
}

/// The session's `memory_report` at one phase, with its sums.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Composition {
    /// `P2` or `P3`.
    pub phase: &'static str,
    /// Every ingredient row, its sizes in bytes as salsa reports them.
    pub ingredients: Vec<IngredientRow>,
    /// Every memo row, its sizes in bytes as salsa reports them.
    pub memos: Vec<MemoRow>,
    /// The sums of the ingredient rows.
    pub ingredient_sum: Sum,
    /// The sums of the memo rows.
    pub memo_sum: Sum,
}

/// The sums of the rows of one kind of a [`Composition`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sum {
    /// The slots or memos.
    pub count: u64,
    /// The shallow sizes of the fields or values.
    pub size_of_fields_gib: f64,
    /// salsa's metadata.
    pub size_of_metadata_gib: f64,
}

impl Sum {
    fn of(rows: impl Iterator<Item = (u64, u64, u64)>) -> Sum {
        let (count, fields, metadata) = rows.fold((0, 0, 0), |(c, f, m), (rc, rf, rm)| {
            (c + rc, f + rf, m + rm)
        });
        Sum {
            count,
            size_of_fields_gib: gib(fields),
            size_of_metadata_gib: gib(metadata),
        }
    }
}

impl Composition {
    fn of(phase: &'static str, report: MemoryReport) -> Composition {
        let ingredient_sum = Sum::of(
            report
                .ingredients
                .iter()
                .map(|r| (r.count, r.size_of_fields, r.size_of_metadata)),
        );
        let memo_sum = Sum::of(
            report
                .memos
                .iter()
                .map(|r| (r.count, r.size_of_fields, r.size_of_metadata)),
        );
        Composition {
            phase,
            ingredients: report.ingredients,
            memos: report.memos,
            ingredient_sum,
            memo_sum,
        }
    }
}

/// One repeat of a series of a [`ProbeReport`]: the variant's level written on the parent's,
/// diagnosed and popped.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CheckRow {
    /// The variant as given.
    pub variant: String,
    /// `affected` or `workspace`.
    pub scope: &'static str,
    /// The repeat, from 1.
    pub repeat: u32,
    /// `PASS`, `FAIL` or `REVERT-MISMATCH`, as `bench` labels a run.
    pub label: &'static str,
    /// The `check_ms` of the level's `Event::Checked`, under the counting allocator.
    pub wall_ms: u64,
    /// The live heap just before the write of the level (h0).
    pub h0_gib: f64,
    /// The live heap just after the write, before the diagnosis (hW).
    pub hw_gib: f64,
    /// The live heap just after its diagnosis (h1).
    pub h1_gib: f64,
    /// The live heap just before its pop (h2).
    pub h2_gib: f64,
    /// The live heap just after its pop (h3).
    pub h3_gib: f64,
    /// `h0 - hW`: what the new revision of the level's write freed (the predecessors parked in
    /// `deleted_entries` and the LRU evictions due since the revision before).
    pub freed_at_write_gib: f64,
    /// `h1 - hW`: what the diagnosis allocated and kept after the level's own write.
    pub delta_gib: f64,
    /// `h2 - h3`: what the pop, the database write of the level's revert, freed.
    pub freed_at_pop_gib: f64,
    /// The resident set size just after the pop.
    pub rss_gib: f64,
}

/// The probe's time against the bench's for one (variant, scope) of a [`ProbeReport`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Overhead {
    /// The variant as given.
    pub variant: String,
    /// `affected` or `workspace`.
    pub scope: &'static str,
    /// The median `wall_ms` of the warm checks (from the second on; the first when it is alone),
    /// taken as `bench` takes its `warm_check_ms` median.
    pub probe_warm_median_ms: u64,
    /// The bench's median given with `--bench-median`; `None` when none was given.
    pub bench_warm_median_ms: Option<u64>,
    /// `probe_warm_median_ms / bench_warm_median_ms`; `None` without a bench median.
    pub ratio: Option<f64>,
}

/// The memory at `phase`: VmRSS through `rss` and the live and peak heap from `gauge`.
fn phase(phase: &'static str, gauge: &dyn HeapGauge, rss: RssReader) -> Result<Phase, ProcError> {
    let live = gauge.live_bytes();
    let peak = gauge.peak_bytes();
    Ok(Phase {
        phase,
        rss_gib: kb_gib(rss()?),
        live_heap_gib: gib(live),
        peak_heap_gib: gib(peak),
    })
}

/// The observer of the probe's analyzer: it keeps the events, and takes P1 at `Event::Indexed`
/// and P2 at `Event::Loaded`, as they are emitted inside the load.
struct ProbeObserver {
    gauge: &'static dyn HeapGauge,
    rss: RssReader,
    events: Mutex<Vec<Event>>,
    /// The phases taken inside the load, a failed read included, in the order taken.
    phases: Mutex<Vec<Result<Phase, ProcError>>>,
}

impl ProbeObserver {
    fn new(gauge: &'static dyn HeapGauge, rss: RssReader) -> ProbeObserver {
        ProbeObserver {
            gauge,
            rss,
            events: Mutex::new(Vec::new()),
            phases: Mutex::new(Vec::new()),
        }
    }

    /// The events since the last take.
    fn take(&self) -> Vec<Event> {
        std::mem::take(&mut *self.events.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// The phases taken inside the load; the first failed read is the error.
    fn take_phases(&self) -> Result<Vec<Phase>, ProcError> {
        std::mem::take(&mut *self.phases.lock().unwrap_or_else(PoisonError::into_inner))
            .into_iter()
            .collect()
    }
}

impl Observer for ProbeObserver {
    fn observe(&self, event: Event) {
        let at = match event {
            Event::Indexed { .. } => Some("P1"),
            Event::Loaded { .. } => Some("P2"),
            Event::Checked { .. } | Event::Reverted { .. } | Event::Memory { .. } => None,
        };
        if let Some(name) = at {
            let taken = phase(name, self.gauge, self.rss);
            self.phases
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(taken);
        }
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event);
    }
}

/// A check row before its series is known to have ended well: its wall ms is `None` until the
/// level's `Event::Checked` is seen.
struct Pending {
    row: CheckRow,
    wall_ms: Option<u64>,
}

/// The probe's hook on `stacked::stacked_series` for one (variant, scope): the live heap at
/// every point of a repeat (h0, hW, h1, h2, h3) and VmRSS after the pop.
struct ProbeHook<'a> {
    gauge: &'a dyn HeapGauge,
    observer: &'a ProbeObserver,
    rss: RssReader,
    variant: &'a str,
    scope: &'static str,
    /// h0, hW, h1 and h2 of the current repeat.
    h: [u64; 4],
    /// The `check_ms` of the current repeat's level.
    wall_ms: Option<u64>,
    rows: Vec<Pending>,
}

impl ProbeHook<'_> {
    fn observe(&mut self, point: Point<'_>) -> Result<(), ProcError> {
        match point {
            Point::ParentDiagnosed => {
                self.observer.take();
            }
            Point::BeforeWrite => {
                // The events of earlier points are dropped first, so that h0 is read last.
                self.observer.take();
                self.wall_ms = None;
                self.h[0] = self.gauge.live_bytes();
            }
            Point::AfterWrite => {
                self.h[1] = self.gauge.live_bytes();
            }
            Point::AfterDiagnose => {
                self.h[2] = self.gauge.live_bytes();
                self.wall_ms = self.observer.take().into_iter().find_map(|e| match e {
                    Event::Checked { check_ms, .. } => Some(check_ms),
                    _ => None,
                });
            }
            Point::BeforePop => {
                self.h[3] = self.gauge.live_bytes();
            }
            Point::AfterPop(verdict) => {
                let h3 = self.gauge.live_bytes();
                let rss_kb = (self.rss)()?;
                let [h0, hw, h1, h2] = self.h;
                self.rows.push(Pending {
                    row: CheckRow {
                        variant: self.variant.to_owned(),
                        scope: self.scope,
                        repeat: self.rows.len() as u32 + 1,
                        label: label_of(verdict),
                        wall_ms: 0,
                        h0_gib: gib(h0),
                        hw_gib: gib(hw),
                        h1_gib: gib(h1),
                        h2_gib: gib(h2),
                        h3_gib: gib(h3),
                        freed_at_write_gib: diff_gib(h0, hw),
                        delta_gib: diff_gib(h1, hw),
                        freed_at_pop_gib: diff_gib(h2, h3),
                        rss_gib: kb_gib(rss_kb),
                    },
                    wall_ms: self.wall_ms,
                });
            }
        }
        Ok(())
    }

    /// The rows of a series that ended well; a row without its `Event::Checked` is an error.
    fn into_rows(self) -> Result<Vec<CheckRow>> {
        self.rows
            .into_iter()
            .map(|p| match p.wall_ms {
                Some(wall_ms) => Ok(CheckRow { wall_ms, ..p.row }),
                None => Err(anyhow::anyhow!(
                    "{} [{}] repeat {}: the level's diagnosis reported no Event::Checked",
                    p.row.variant,
                    p.row.scope,
                    p.row.repeat
                )),
            })
            .collect()
    }
}

/// The overhead row of the checks of one series against the bench's median, if one was given.
fn overhead(
    variant: &str,
    scope: Scope,
    rows: &[CheckRow],
    bench: &[BenchMedian],
) -> Result<Overhead> {
    let walls: Vec<u64> = rows.iter().map(|r| r.wall_ms).collect();
    let warm = if walls.len() > 1 {
        &walls[1..]
    } else {
        &walls[..]
    };
    let (_, median, _) =
        summarize(warm).with_context(|| format!("{variant}: the series diagnosed no level"))?;
    let bench_ms = bench
        .iter()
        .find(|b| b.variant == variant && b.scope == scope)
        .map(|b| b.ms);
    Ok(Overhead {
        variant: variant.to_owned(),
        scope: scope_name(scope),
        probe_warm_median_ms: median,
        bench_warm_median_ms: bench_ms,
        ratio: bench_ms.map(|b| median as f64 / b as f64),
    })
}

/// The composition of `session` at `phase`.
fn composition(session: &mut RaSession, phase: &'static str) -> Result<Composition> {
    let report = session
        .memory_report()
        .with_context(|| format!("the memory report at {phase}"))?;
    Ok(Composition::of(phase, report))
}

/// Loads `args.base`, runs every (variant, scope) series on top of the `--on` parent and returns
/// what it measured; `gauge` is the counting allocator of the process.
pub fn run(args: &ProbeArgs, gauge: &'static dyn HeapGauge) -> Result<ProbeReport> {
    let rss: RssReader = proc::vm_rss_kb;
    let ram_gib = kb_gib(proc::mem_total_kb()?);
    let cores = std::thread::available_parallelism()
        .context("the available parallelism")?
        .get();
    let mut phases = vec![phase("P0", gauge, rss)?];

    let observer = Arc::new(ProbeObserver::new(gauge, rss));
    // The configuration of `bench`, so that the probe and the bench run the same check.
    let analyzer = RaAnalyzer::new(
        RaConfig {
            proc_macro: true,
            assert_revert: true,
            ..RaConfig::default()
        }
        .with_backend(args.backend),
        Arc::new(MonotonicClock::new()),
        observer.clone(),
    );
    let loaded = analyzer.load(&args.base);
    let in_load = observer.take_phases();
    let mut session = loaded.with_context(|| format!("load {}", args.base.display()))?;
    let in_load = in_load?;
    let names: Vec<&str> = in_load.iter().map(|p| p.phase).collect();
    anyhow::ensure!(
        names == ["P1", "P2"],
        "the load reported the phases {names:?}, not Event::Indexed then Event::Loaded"
    );
    phases.extend(in_load);
    let mut compositions = vec![composition(&mut session, "P2")?];

    let mut checks = Vec::new();
    let mut overheads = Vec::new();
    if let Some(on) = &args.on {
        let parent =
            overlay_edits(&args.base, on).with_context(|| format!("overlay {}", on.display()))?;
        anyhow::ensure!(
            !parent.is_empty(),
            "--on {}: no edits vs base",
            on.display()
        );
        for variant in &args.variants {
            let edits = overlay_edits(&args.base, variant)
                .with_context(|| format!("overlay {}", variant.display()))?;
            let name = variant.display().to_string();
            let own = stacked::own_edits(&parent, &edits);
            if own.is_empty() {
                eprintln!("{name}: SKIP (no edits vs the --on parent)");
                continue;
            }
            for &scope in &args.scopes {
                let mut hook = ProbeHook {
                    gauge,
                    observer: &observer,
                    rss,
                    variant: &name,
                    scope: scope_name(scope),
                    h: [0; 4],
                    wall_ms: None,
                    rows: Vec::new(),
                };
                let reverted = stacked::stacked_series(
                    &analyzer,
                    &mut session,
                    &name,
                    &parent,
                    &own,
                    scope,
                    args.repeat,
                    &mut |point| hook.observe(point),
                )?;
                // A revert that failed poisoned the session: nothing after it can be measured.
                anyhow::ensure!(
                    reverted,
                    "{name} [{}]: a level did not revert",
                    scope_name(scope)
                );
                let rows = hook.into_rows()?;
                overheads.push(overhead(&name, scope, &rows, &args.bench_medians)?);
                checks.extend(rows);
            }
        }
    }

    phases.push(phase("P3", gauge, rss)?);
    compositions.push(composition(&mut session, "P3")?);
    Ok(ProbeReport {
        backend: args.backend,
        machine: Machine {
            ram_gib,
            cores,
            threads: session.threads(),
            idle: args.idle,
        },
        phases,
        composition: compositions,
        checks,
        overhead: overheads,
    })
}

/// Writes `report` as pretty JSON to `out`.
pub fn write_report<R: Serialize>(report: &R, out: &std::path::Path) -> Result<()> {
    let file = std::fs::File::create(out).with_context(|| format!("create {}", out.display()))?;
    let mut w = std::io::BufWriter::new(file);
    serde_json::to_writer_pretty(&mut w, report)
        .with_context(|| format!("write {}", out.display()))?;
    w.flush()
        .with_context(|| format!("write {}", out.display()))
}

/// Arguments of one `engine-probe --depth` run.
#[derive(Debug, Clone)]
pub struct DepthProbeArgs {
    /// The workspace to load.
    pub base: PathBuf,
    /// The levels of the stack.
    pub depth: u32,
    /// The number of workspace files the levels rotate over (as `bench --depth-files`).
    pub files: u32,
    /// Scopes to diagnose, in order: one stack per scope.
    pub scopes: Vec<Scope>,
    /// Whether the person running it kept the machine otherwise idle.
    pub idle: bool,
    /// The backend of the forks; only the Engine has layer counters.
    pub backend: Backend,
}

/// What one `engine-probe --depth` run measured, written as JSON to `--report`: the memory of a
/// stack of `depth` levels, each written on the one below and diagnosed once before the next is
/// written (the search's shape, `stacked::DepthShape::EveryLevel`), with the counters of each
/// level's layer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DepthReport {
    /// The backend the forks ran on.
    pub backend: Backend,
    /// The machine the run measured on.
    pub machine: Machine,
    /// The levels of each stack.
    pub depth: u32,
    /// The files the levels rotate over.
    pub files: Vec<String>,
    /// One stack per scope, in order.
    pub series: Vec<DepthSeries>,
}

/// One stack of a [`DepthReport`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DepthSeries {
    /// The scope its levels were diagnosed at.
    pub scope: &'static str,
    /// The live heap once every level is written and diagnosed, less the live heap before the
    /// fork: what the stack holds.
    pub stack_heap_bytes: i64,
    /// One entry per level, the lowest first.
    pub levels: Vec<DepthLevel>,
    /// Under `fork-probe`: the bytes a flattened chain view of each sealed level would take
    /// (the chain-view design, one view per sealed level that holds a memo, a tracked entry or a tombstone;
    /// the others share their parent's), from the chain's distinct keys
    /// ([`projected_view_bytes`]), summed over the stack. `None` without the feature or the
    /// Engine.
    pub projected_view_bytes: Option<u64>,
    /// `projected_view_bytes` as a percentage of `stack_heap_bytes`.
    pub projected_view_share_pct: Option<f64>,
    /// The bytes of the chain views the stack's layers built (`LayerStats::flat_bytes`, a shared
    /// view counted once), summed over the stack: the cost gate M2-view bounds. `None` on the
    /// undo log.
    pub view_bytes: Option<u64>,
    /// `view_bytes` as a percentage of `stack_heap_bytes` (gate M2-view:
    /// at most [`M2_VIEW_MAX_SHARE_PCT`] at depth 20).
    pub view_share_pct: Option<f64>,
}

/// Gate M2-view of the chain views (D11): the chain views of a depth-20 stack on ryo-rs take at most this
/// percentage of the stack's live heap. Fixed with stage 1 from stage 0's measurement (a
/// projected 4.2 %, from about 9.2k distinct memo keys per chain at every depth; stage 1
/// measured 4.05 %, 6.45 MB of 159 MB); a larger share asks for the K = 2 checkpoint view
/// instead. `engine-probe --depth` fails a run above it.
pub const M2_VIEW_MAX_SHARE_PCT: f64 = 15.0;

/// One level of a [`DepthSeries`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DepthLevel {
    /// The level, from 1.
    pub level: usize,
    /// The live heap after its diagnosis less the live heap before its write.
    pub heap_delta_bytes: i64,
    /// The wall ms of its diagnosis (with the counting allocator's overhead).
    pub check_ms: f64,
    /// Its layer's counters, read once every level is written and diagnosed; `None` on the undo
    /// log.
    pub stats: Option<LevelCounters>,
}

/// The counters of one level's layer ([`ryo_v2_ra::LayerStats`]).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LevelCounters {
    /// Whether the layer is sealed (every level but the top).
    pub sealed: bool,
    /// Memos inserted into the layer.
    pub memos: usize,
    /// Stamps it put on memos below it.
    pub stamps: usize,
    /// Tracked-struct entries it holds.
    pub tracked_entries: usize,
    /// Tracked structs it tombstoned.
    pub tombstones: usize,
    /// Deep verifications run under it.
    pub deep_verifies: usize,
    /// The memo keys of its chain view (0 without a view: the top level).
    pub flat_memos: usize,
    /// The tracked ids of its chain view.
    pub flat_tracked: usize,
    /// The hidden ids of its chain view.
    pub flat_hidden: usize,
    /// The bytes of the view it built (0 when it shares its parent's, or has none).
    pub flat_bytes: usize,
    /// Under `fork-probe`: the distinct memo keys, tracked ids and tombstoned ids of its chain,
    /// and the bytes a view of them would take.
    pub chain: Option<ChainCounts>,
}

/// The `fork-probe` counts of one level's chain.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChainCounts {
    /// Distinct memo keys the chain's layers hold.
    pub unique_memo_keys: usize,
    /// Distinct tracked-struct ids the chain's layers hold an entry or tombstone for.
    pub unique_tracked_ids: usize,
    /// Distinct ids the chain's layers tombstoned.
    pub unique_tombstoned_ids: usize,
    /// [`projected_view_bytes`] of the counts above when the level is sealed and holds an
    /// entry of its own, else 0 (no view, or its parent's shared).
    pub projected_view_bytes: u64,
}

/// The bytes of a `hashbrown` table (std's `HashMap`/`HashSet`) of `n` entries of `entry` bytes
/// grown by insertion: its buckets are the least power of two from 4 whose capacity (one less
/// than the buckets below 8, 7/8 of them from 8) holds `n`; one entry and one control byte per
/// bucket, plus 16 trailing control bytes. 0 for no entry (no allocation).
pub fn table_bytes(n: usize, entry: usize) -> u64 {
    if n == 0 {
        return 0;
    }
    let capacity = |buckets: usize| {
        if buckets < 8 {
            buckets - 1
        } else {
            buckets / 8 * 7
        }
    };
    let mut buckets = 4usize;
    while capacity(buckets) < n {
        buckets *= 2;
    }
    (buckets * entry + buckets + 16) as u64
}

/// The entry sizes of the chain view (`fork/flat.rs`): a memo key `(IngredientIndex, Id)` of
/// 12 bytes with an 8-byte reference, a tracked id (8 bytes) with a reference, a hidden id.
pub const VIEW_MEMO_ENTRY: usize = 20;
/// See [`VIEW_MEMO_ENTRY`].
pub const VIEW_TRACKED_ENTRY: usize = 16;
/// See [`VIEW_MEMO_ENTRY`].
pub const VIEW_HIDDEN_ENTRY: usize = 8;

/// The bytes of a chain view holding `memo_keys`, `tracked_ids` and `hidden_ids` entries.
pub fn projected_view_bytes(memo_keys: usize, tracked_ids: usize, hidden_ids: usize) -> u64 {
    table_bytes(memo_keys, VIEW_MEMO_ENTRY)
        + table_bytes(tracked_ids, VIEW_TRACKED_ENTRY)
        + table_bytes(hidden_ids, VIEW_HIDDEN_ENTRY)
}

/// `part` as a percentage of `whole`, `None` when `whole` is not positive.
fn share_pct(part: u64, whole: i64) -> Option<f64> {
    (whole > 0).then(|| part as f64 * 100.0 / whole as f64)
}

/// The counters of the layer of level `level` of `of` levels, as a report row.
fn level_counters(level: usize, of: usize, stats: &ryo_v2_ra::LayerStats) -> LevelCounters {
    let sealed = level < of;
    #[cfg(feature = "fork-probe")]
    let chain = {
        let own = stats.memos + stats.tracked_entries + stats.tombstones > 0;
        Some(ChainCounts {
            unique_memo_keys: stats.chain_unique_memo_keys,
            unique_tracked_ids: stats.chain_unique_tracked_ids,
            unique_tombstoned_ids: stats.chain_unique_tombstoned_ids,
            projected_view_bytes: if sealed && own {
                projected_view_bytes(
                    stats.chain_unique_memo_keys,
                    stats.chain_unique_tracked_ids,
                    stats.chain_unique_tombstoned_ids,
                )
            } else {
                0
            },
        })
    };
    #[cfg(not(feature = "fork-probe"))]
    let chain = None;
    LevelCounters {
        sealed,
        memos: stats.memos,
        stamps: stats.stamps,
        tracked_entries: stats.tracked_entries,
        tombstones: stats.tombstones,
        deep_verifies: stats.deep_verifies,
        flat_memos: stats.flat_memos,
        flat_tracked: stats.flat_tracked,
        flat_hidden: stats.flat_hidden,
        flat_bytes: stats.flat_bytes,
        chain,
    }
}

/// The series of one stack from its readings: `heap` per level the live heap before its write
/// and after its diagnosis, `check_ms` per level, `stack_heap_bytes` and the level counters.
///
/// # Contract
/// - C1: a level's `heap_delta_bytes` is its after-diagnosis reading less its before-write one;
///   `projected_view_bytes` is the sum of the levels' `chain.projected_view_bytes`, which is 0
///   for the top (open) level and for a sealed level with no own memo, tracked entry or
///   tombstone; its share is of `stack_heap_bytes`; with no chain counts both are `None`.
///   `view_bytes` is the sum of the levels' `flat_bytes`, with its share of `stack_heap_bytes`;
///   with no layer counters (the undo log) both are `None`.
///   tested: engine_probe::tests::a_depth_series_sums_the_views_of_sealed_levels
pub fn depth_series_of(
    scope: &'static str,
    heap: &[(u64, u64)],
    check_ms: &[f64],
    stack_heap_bytes: i64,
    stats: &[ryo_v2_ra::LayerStats],
) -> DepthSeries {
    let of = heap.len();
    let levels: Vec<DepthLevel> = heap
        .iter()
        .enumerate()
        .map(|(i, &(before, after))| DepthLevel {
            level: i + 1,
            heap_delta_bytes: after as i64 - before as i64,
            check_ms: check_ms.get(i).copied().unwrap_or(0.0),
            stats: stats.get(i).map(|s| level_counters(i + 1, of, s)),
        })
        .collect();
    let projected: Option<u64> = levels
        .iter()
        .map(|l| {
            l.stats
                .as_ref()
                .and_then(|s| s.chain.as_ref())
                .map(|c| c.projected_view_bytes)
        })
        .sum();
    let view: Option<u64> = (!stats.is_empty()).then(|| {
        levels
            .iter()
            .filter_map(|l| l.stats.as_ref())
            .map(|s| s.flat_bytes as u64)
            .sum()
    });
    DepthSeries {
        scope,
        stack_heap_bytes,
        projected_view_bytes: projected,
        projected_view_share_pct: projected.and_then(|p| share_pct(p, stack_heap_bytes)),
        view_bytes: view,
        view_share_pct: view.and_then(|v| share_pct(v, stack_heap_bytes)),
        levels,
    }
}

/// Gate M2-view on a [`DepthReport`]: every series whose views were measured keeps their share of
/// the stack's heap at or below `max_pct`.
///
/// # Contract
/// - C1: `Err` naming the scope, the share and `max_pct` when a series' `view_share_pct` exceeds
///   `max_pct`; `Ok` when every measured share is at or below it, and for a series without a
///   measure (the undo log).
///   tested: engine_probe::tests::the_view_share_gate_fails_above_its_ceiling
pub fn check_view_share(report: &DepthReport, max_pct: f64) -> Result<()> {
    for series in &report.series {
        if let Some(share) = series.view_share_pct {
            anyhow::ensure!(
                share <= max_pct,
                "gate M2-view: the chain views of the depth-{} stack [{}] take {share:.2} % of its \
                 heap, above {max_pct} %",
                report.depth,
                series.scope
            );
        }
    }
    Ok(())
}

/// Loads `args.base` and, per scope, stacks `args.depth` levels on one fork (the levels of
/// `stacked::depth_levels` over the first `args.files` indexed files), each diagnosed once right
/// after its write, reads the counting allocator at every point and the layers' counters before
/// the pops, and pops back.
pub fn run_depth(args: &DepthProbeArgs, gauge: &'static dyn HeapGauge) -> Result<DepthReport> {
    let ram_gib = kb_gib(proc::mem_total_kb()?);
    let cores = std::thread::available_parallelism()
        .context("the available parallelism")?
        .get();
    let (analyzer, _observer, mut session) =
        crate::bench::load(&args.base, true, false, args.backend)?;
    let files = crate::bench::depth_files(&analyzer, &session, &args.base, args.files)?;
    let names: Vec<String> = files.iter().map(|(p, _)| p.to_string()).collect();
    eprintln!("[depth-probe] levels rotate over {}", names.join(", "));
    let levels = stacked::depth_levels(&files, args.depth as usize);
    let mut series = Vec::new();
    for &scope in &args.scopes {
        let mut heap: Vec<(u64, u64)> = Vec::new();
        let mut check_ms: Vec<f64> = Vec::new();
        let mut started: Option<std::time::Instant> = None;
        let mut stacked_at: u64 = 0;
        let mut stats = Vec::new();
        let before_fork = gauge.live_bytes();
        let reverted = stacked::depth_series(
            &analyzer,
            &mut session,
            &format!("depth {}", args.depth),
            &levels,
            scope,
            stacked::DepthShape::EveryLevel,
            &mut |point| {
                match point {
                    DepthPoint::BeforeWrite(_) => heap.push((gauge.live_bytes(), 0)),
                    DepthPoint::BeforeDiagnose(_) => started = Some(std::time::Instant::now()),
                    DepthPoint::AfterDiagnose(..) => {
                        check_ms.push(
                            started
                                .take()
                                .map_or(0.0, |t| t.elapsed().as_secs_f64() * 1000.0),
                        );
                        if let Some(last) = heap.last_mut() {
                            last.1 = gauge.live_bytes();
                        }
                    }
                    DepthPoint::AfterWrite(_)
                    | DepthPoint::BeforePop(_)
                    | DepthPoint::AfterPop(_) => {}
                }
                Ok(())
            },
            &mut |s| {
                stacked_at = gauge.live_bytes();
                stats = s.level_stats();
            },
        )?;
        anyhow::ensure!(
            reverted,
            "depth {} [{}]: a level did not revert",
            args.depth,
            scope_name(scope)
        );
        let one = depth_series_of(
            scope_name(scope),
            &heap,
            &check_ms,
            stacked_at as i64 - before_fork as i64,
            &stats,
        );
        eprintln!(
            "[depth-probe] [{}] depth {} stack_heap_bytes {} view_bytes {} view_share_pct {} \
             projected_view_bytes {} projected_view_share_pct {}",
            one.scope,
            args.depth,
            one.stack_heap_bytes,
            one.view_bytes.map_or("-".to_owned(), |b| b.to_string()),
            one.view_share_pct
                .map_or("-".to_owned(), |p| format!("{p:.2}")),
            one.projected_view_bytes
                .map_or("-".to_owned(), |b| b.to_string()),
            one.projected_view_share_pct
                .map_or("-".to_owned(), |p| format!("{p:.2}")),
        );
        series.push(one);
    }
    Ok(DepthReport {
        backend: args.backend,
        machine: Machine {
            ram_gib,
            cores,
            threads: session.threads(),
            idle: args.idle,
        },
        depth: args.depth,
        files: names,
        series,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::collections::BTreeSet;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A gauge whose readings the test sets.
    struct FakeGauge {
        live: AtomicU64,
        peak: AtomicU64,
    }
    impl HeapGauge for FakeGauge {
        fn live_bytes(&self) -> u64 {
            self.live.load(Ordering::SeqCst)
        }
        fn peak_bytes(&self) -> u64 {
            self.peak.load(Ordering::SeqCst)
        }
        fn reset_peak(&self) {
            self.peak.store(self.live_bytes(), Ordering::SeqCst);
        }
    }

    /// One per test, so that tests running at once do not share readings.
    static DELTA_GAUGE: FakeGauge = FakeGauge {
        live: AtomicU64::new(0),
        peak: AtomicU64::new(0),
    };
    static FAIL_GAUGE: FakeGauge = FakeGauge {
        live: AtomicU64::new(0),
        peak: AtomicU64::new(0),
    };

    /// `table_bytes`: the buckets std's `HashMap` takes when grown by insertion.
    #[test]
    fn table_bytes_follows_the_growth_of_a_std_map() {
        assert_eq!(table_bytes(0, 20), 0);
        for n in [1usize, 3, 4, 7, 8, 14, 15, 28, 29, 1000, 9000] {
            let mut map: std::collections::HashMap<u64, u64> = Default::default();
            for i in 0..n as u64 {
                map.insert(i, i);
            }
            let capacity = map.capacity();
            let buckets = if capacity < 8 {
                capacity + 1
            } else {
                capacity / 7 * 8
            };
            assert_eq!(
                table_bytes(n, 20),
                (buckets * 20 + buckets + 16) as u64,
                "{n} entries"
            );
        }
        assert_eq!(
            projected_view_bytes(1, 1, 1),
            (4 * 20 + 20) + (4 * 16 + 20) + (4 * 8 + 20)
        );
    }

    /// `depth_series_of` C1.
    #[test]
    fn a_depth_series_sums_the_views_of_sealed_levels() {
        let level = |memos: usize, keys: usize, flat_bytes: usize| {
            let mut s = ryo_v2_ra::LayerStats::default();
            s.memos = memos;
            s.flat_memos = if flat_bytes > 0 { keys } else { 0 };
            s.flat_bytes = flat_bytes;
            #[cfg(feature = "fork-probe")]
            {
                s.chain_unique_memo_keys = keys;
            }
            s
        };
        // Level 1 holds memos; level 2 none (it shares level 1's view); level 3 is the open top.
        let stats = [level(5, 5, 120), level(0, 5, 0), level(9, 12, 0)];
        let heap = [(100, 300), (300, 310), (310, 1000)];
        let series = depth_series_of("affected", &heap, &[1.0, 2.0, 3.0], 1000, &stats);
        let deltas: Vec<i64> = series.levels.iter().map(|l| l.heap_delta_bytes).collect();
        assert_eq!(deltas, [200, 10, 690]);
        let sealed: Vec<bool> = series
            .levels
            .iter()
            .map(|l| l.stats.as_ref().unwrap().sealed)
            .collect();
        assert_eq!(sealed, [true, true, false]);
        #[cfg(feature = "fork-probe")]
        {
            let one = projected_view_bytes(5, 0, 0);
            assert_eq!(series.projected_view_bytes, Some(one));
            assert_eq!(
                series.projected_view_share_pct,
                Some(one as f64 * 100.0 / 1000.0)
            );
        }
        #[cfg(not(feature = "fork-probe"))]
        assert_eq!(
            (series.projected_view_bytes, series.projected_view_share_pct),
            (None, None)
        );
        // The views measured: the one level 1 built.
        assert_eq!(
            (series.view_bytes, series.view_share_pct),
            (Some(120), Some(12.0))
        );
        // On the undo log there are no counters.
        let undo = depth_series_of("affected", &heap, &[1.0, 2.0, 3.0], 1000, &[]);
        assert!(undo.levels.iter().all(|l| l.stats.is_none()));
        assert_eq!(undo.projected_view_bytes, None);
        assert_eq!((undo.view_bytes, undo.view_share_pct), (None, None));
    }

    /// `check_view_share` C1, and the ceiling fixed with stage 1 of the chain views.
    #[test]
    fn the_view_share_gate_fails_above_its_ceiling() {
        assert_eq!(M2_VIEW_MAX_SHARE_PCT, 15.0);
        let with = |share: Option<f64>| DepthReport {
            backend: Backend::Engine,
            machine: Machine {
                ram_gib: 1.0,
                cores: 1,
                threads: 1,
                idle: false,
            },
            depth: 20,
            files: Vec::new(),
            series: vec![DepthSeries {
                scope: "affected",
                stack_heap_bytes: 100,
                levels: Vec::new(),
                projected_view_bytes: None,
                projected_view_share_pct: None,
                view_bytes: share.map(|s| s as u64),
                view_share_pct: share,
            }],
        };
        assert!(check_view_share(&with(Some(15.0)), M2_VIEW_MAX_SHARE_PCT).is_ok());
        assert!(check_view_share(&with(None), M2_VIEW_MAX_SHARE_PCT).is_ok());
        let e = check_view_share(&with(Some(15.5)), M2_VIEW_MAX_SHARE_PCT).unwrap_err();
        let e = e.to_string();
        assert!(
            e.contains("M2-view") && e.contains("[affected]") && e.contains("15.50 %"),
            "{e}"
        );
    }

    /// 5 GiB of RSS, far from every heap reading of the tests.
    fn fake_rss() -> Result<u64, ProcError> {
        Ok(5 * 1024 * 1024)
    }

    /// A VmRSS read that fails as `proc::vm_rss_kb` does where `/proc` is absent.
    fn failed_rss() -> Result<u64, ProcError> {
        proc::kb_line(Path::new("/no-such-proc/self/status"), "VmRSS:")
    }

    fn keys(v: &Value) -> BTreeSet<String> {
        v.as_object()
            .unwrap_or_else(|| panic!("not an object: {v}"))
            .keys()
            .cloned()
            .collect()
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    fn row() -> CheckRow {
        CheckRow {
            variant: "corpus/broken-01".to_owned(),
            scope: "affected",
            repeat: 1,
            label: "FAIL",
            wall_ms: 12,
            h0_gib: 1.0,
            hw_gib: 0.5,
            h1_gib: 1.25,
            h2_gib: 1.25,
            h3_gib: 1.125,
            freed_at_write_gib: 0.5,
            delta_gib: 0.75,
            freed_at_pop_gib: 0.125,
            rss_gib: 2.5,
        }
    }

    #[test]
    fn report_json_keeps_its_keys_and_values() {
        let report = ProbeReport {
            backend: Backend::UndoLog,
            machine: Machine {
                ram_gib: 64.0,
                cores: 16,
                threads: 16,
                idle: true,
            },
            phases: vec![Phase {
                phase: "P0",
                rss_gib: 0.5,
                live_heap_gib: 0.25,
                peak_heap_gib: 0.375,
            }],
            composition: vec![Composition::of(
                "P2",
                MemoryReport {
                    ingredients: vec![IngredientRow {
                        name: "FileText".to_owned(),
                        count: 3,
                        size_of_fields: 1 << 29,
                        size_of_metadata: 1 << 28,
                        heap_size_of_fields: None,
                    }],
                    memos: vec![MemoRow {
                        query: "EditionedFileId::parse_".to_owned(),
                        count: 2,
                        size_of_fields: 1 << 30,
                        size_of_metadata: 0,
                        heap_size_of_fields: None,
                    }],
                },
            )],
            checks: vec![row()],
            overhead: vec![Overhead {
                variant: "corpus/broken-01".to_owned(),
                scope: "affected",
                probe_warm_median_ms: 12,
                bench_warm_median_ms: Some(10),
                ratio: Some(1.2),
            }],
        };
        let v = serde_json::to_value(&report).unwrap();
        assert_eq!(
            keys(&v),
            set(&[
                "backend",
                "machine",
                "phases",
                "composition",
                "checks",
                "overhead"
            ])
        );
        assert_eq!(v["backend"], json!("undo-log"));
        let text = serde_json::to_string(&report).unwrap();
        assert!(text.starts_with(r#"{"backend":"undo-log","#), "{text}");
        // A run on the Engine differs in that key alone.
        let mut engine = serde_json::to_value(ProbeReport {
            backend: Backend::Engine,
            ..report.clone()
        })
        .unwrap();
        assert_eq!(engine["backend"], json!("engine"));
        engine["backend"] = json!("undo-log");
        assert_eq!(engine, v);
        assert_eq!(
            v["machine"],
            json!({"ram_gib": 64.0, "cores": 16, "threads": 16, "idle": true})
        );
        assert_eq!(
            v["phases"][0],
            json!({"phase": "P0", "rss_gib": 0.5, "live_heap_gib": 0.25, "peak_heap_gib": 0.375})
        );
        assert_eq!(
            v["checks"][0],
            json!({
                "variant": "corpus/broken-01", "scope": "affected", "repeat": 1, "label": "FAIL",
                "wall_ms": 12, "h0_gib": 1.0, "hw_gib": 0.5, "h1_gib": 1.25, "h2_gib": 1.25,
                "h3_gib": 1.125, "freed_at_write_gib": 0.5, "delta_gib": 0.75,
                "freed_at_pop_gib": 0.125, "rss_gib": 2.5
            })
        );
        let c = &v["composition"][0];
        assert_eq!(
            keys(c),
            set(&[
                "phase",
                "ingredients",
                "memos",
                "ingredient_sum",
                "memo_sum"
            ])
        );
        assert_eq!(
            c["ingredients"][0],
            json!({"name": "FileText", "count": 3, "size_of_fields": 1u64 << 29,
                   "size_of_metadata": 1u64 << 28, "heap_size_of_fields": null})
        );
        assert_eq!(c["memos"][0]["query"], "EditionedFileId::parse_");
        assert_eq!(
            c["ingredient_sum"],
            json!({"count": 3, "size_of_fields_gib": 0.5, "size_of_metadata_gib": 0.25})
        );
        assert_eq!(
            c["memo_sum"],
            json!({"count": 2, "size_of_fields_gib": 1.0, "size_of_metadata_gib": 0.0})
        );
        assert_eq!(
            v["overhead"][0],
            json!({"variant": "corpus/broken-01", "scope": "affected", "probe_warm_median_ms": 12,
                   "bench_warm_median_ms": 10, "ratio": 1.2})
        );
    }

    #[test]
    fn delta_and_freed_read_the_allocator() {
        let gauge = &DELTA_GAUGE;
        let observer = ProbeObserver::new(gauge, fake_rss);
        let mut hook = ProbeHook {
            gauge,
            observer: &observer,
            rss: fake_rss,
            variant: "v",
            scope: "affected",
            h: [0; 4],
            wall_ms: None,
            rows: Vec::new(),
        };
        // h0, hW, h1, h2, h3 in bytes: the write frees 5 MiB, the diagnosis then keeps 3 MiB,
        // the pop frees 2 MiB.
        const MIB: u64 = 1 << 20;
        let at = |live: u64| gauge.live.store(live, Ordering::SeqCst);
        hook.observe(Point::ParentDiagnosed).unwrap();
        at(100 * MIB);
        hook.observe(Point::BeforeWrite).unwrap();
        at(95 * MIB);
        hook.observe(Point::AfterWrite).unwrap();
        observer.observe(Event::Checked {
            scope_files: 1,
            total_files: 2,
            check_ms: 9,
        });
        at(98 * MIB);
        hook.observe(Point::AfterDiagnose).unwrap();
        at(99 * MIB);
        hook.observe(Point::BeforePop).unwrap();
        at(97 * MIB);
        hook.observe(Point::AfterPop(None)).unwrap();
        let rows = hook.into_rows().unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.freed_at_write_gib, gib(5 * MIB));
        assert_eq!(r.freed_at_write_gib, r.h0_gib - r.hw_gib);
        assert_eq!(r.delta_gib, gib(3 * MIB));
        assert_eq!(r.delta_gib, r.h1_gib - r.hw_gib);
        assert_eq!(r.freed_at_pop_gib, gib(2 * MIB));
        assert_eq!(r.freed_at_pop_gib, r.h2_gib - r.h3_gib);
        assert_eq!(
            (r.h0_gib, r.hw_gib, r.h1_gib, r.h2_gib, r.h3_gib),
            (
                gib(100 * MIB),
                gib(95 * MIB),
                gib(98 * MIB),
                gib(99 * MIB),
                gib(97 * MIB)
            )
        );
        // The RSS is read beside them and enters none of them.
        assert_eq!(r.rss_gib, 5.0);
        assert_eq!(r.wall_ms, 9);
        assert_eq!(r.label, "REVERT-MISMATCH");

        // A heap that shrank during the diagnosis gives a negative delta, and one that grew at
        // the write a negative freed at write, neither wrapped.
        let mut hook = hook_like(gauge, &observer);
        at(50 * MIB);
        hook.observe(Point::BeforeWrite).unwrap();
        at(51 * MIB);
        hook.observe(Point::AfterWrite).unwrap();
        observer.observe(Event::Checked {
            scope_files: 1,
            total_files: 2,
            check_ms: 1,
        });
        at(49 * MIB);
        hook.observe(Point::AfterDiagnose).unwrap();
        hook.observe(Point::BeforePop).unwrap();
        hook.observe(Point::AfterPop(None)).unwrap();
        let rows = hook.into_rows().unwrap();
        assert_eq!(rows[0].freed_at_write_gib, -gib(MIB));
        assert_eq!(rows[0].delta_gib, -gib(2 * MIB));
        assert_eq!(rows[0].freed_at_pop_gib, 0.0);

        // A level whose diagnosis reported no Event::Checked has no wall ms: an error, not a 0.
        let mut hook = hook_like(gauge, &observer);
        for point in [
            Point::BeforeWrite,
            Point::AfterWrite,
            Point::AfterDiagnose,
            Point::BeforePop,
            Point::AfterPop(None),
        ] {
            hook.observe(point).unwrap();
        }
        let e = hook.into_rows().unwrap_err().to_string();
        assert!(e.contains("Event::Checked"), "{e}");
    }

    fn hook_like<'a>(gauge: &'a dyn HeapGauge, observer: &'a ProbeObserver) -> ProbeHook<'a> {
        ProbeHook {
            gauge,
            observer,
            rss: fake_rss,
            variant: "v",
            scope: "affected",
            h: [0; 4],
            wall_ms: None,
            rows: Vec::new(),
        }
    }

    #[test]
    fn a_failed_proc_read_is_an_error() {
        // The parser given an unreadable path: the error names the file.
        match failed_rss() {
            Err(e @ ProcError::Read { .. }) => {
                assert!(e.to_string().contains("/no-such-proc/self/status"), "{e}");
            }
            other => panic!("{other:?}"),
        }
        // The parser given a malformed line: the error names the file and the key.
        let file = std::env::temp_dir().join(format!(
            "ryo-v2-engine-probe-meminfo-{}",
            std::process::id()
        ));
        std::fs::write(&file, "MemFree: 1 kB\nMemTotal: lots kB\n").unwrap();
        let malformed = proc::kb_line(&file, "MemTotal:");
        std::fs::remove_file(&file).unwrap();
        match malformed {
            Err(e @ ProcError::Parse { .. }) => {
                let shown = e.to_string();
                assert!(shown.contains(&file.display().to_string()), "{shown}");
                assert!(shown.contains("MemTotal:"), "{shown}");
            }
            other => panic!("{other:?}"),
        }

        // A phase whose VmRSS read fails is that error, never a phase with a zero RSS.
        let gauge = &FAIL_GAUGE;
        gauge.live.store(7, Ordering::SeqCst);
        assert!(matches!(
            phase("P0", gauge, failed_rss),
            Err(ProcError::Read { .. })
        ));
        // Inside the load: the observer keeps the failed read, and the phases are that error.
        let observer = ProbeObserver::new(gauge, failed_rss);
        observer.observe(Event::Indexed {
            load_ms: 1,
            files: 1,
        });
        assert!(matches!(
            observer.take_phases(),
            Err(ProcError::Read { .. })
        ));
        // After a pop: the hook's error, which ends the series; no row is recorded.
        let mut hook = ProbeHook {
            rss: failed_rss,
            ..hook_like(gauge, &observer)
        };
        for point in [
            Point::BeforeWrite,
            Point::AfterWrite,
            Point::AfterDiagnose,
            Point::BeforePop,
        ] {
            hook.observe(point).unwrap();
        }
        assert!(matches!(
            hook.observe(Point::AfterPop(None)),
            Err(ProcError::Read { .. })
        ));
        assert!(hook.rows.is_empty());
    }
}
