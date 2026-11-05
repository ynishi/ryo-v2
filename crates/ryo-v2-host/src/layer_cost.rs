//! `engine-probe --layer-cost`: what one layer of the fork Engine costs in memory, the source of
//! the memory term of stage 3b's `Forks` (D11; the bound stage 4 states as `max_forks`):
//!
//! ```text
//! W × transient_peak + Σ over live layers of (kept(class) + view_bytes(depth)) ≤ A
//! ```
//!
//! Per edit class it stacks levels as jobs of `Forks` at W = 1, each a job on the Layer of the one
//! below, written and diagnosed once before the next is submitted (the search's shape), and
//! reads per level:
//! - the heap the level keeps once its write and diagnosis are done (`kept`);
//! - the most heap live at once during them above what it keeps (`transient`);
//! - the bytes of the chain view the level builds once it is sealed and has a child
//!   (`LayerStats::flat_bytes`), which grows with the depth of the chain (F14).
//!
//! Every figure is read from the counting allocator of `engine-probe` (a [`HeapGauge`]); no time
//! figure is reported.

use crate::bench::scope_name;
use crate::engine_probe::{HeapGauge, Machine};
use crate::proc;
use crate::stacked::{self, DepthPoint};
use anyhow::{Context, Result};
use ryo_v2_core::domain::{Edit, RelPath, Scope};
use ryo_v2_ra::Backend;
use serde::Serialize;
use std::path::PathBuf;

/// An edit class of a layer-cost series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EditClass {
    /// Each level changes the body of one function and no item: the literal a function returns.
    Body,
    /// Each level adds an item: a function appended to a file (`stacked::depth_levels`).
    Item,
}

impl EditClass {
    /// `body` or `item`.
    pub fn name(self) -> &'static str {
        match self {
            EditClass::Body => "body",
            EditClass::Item => "item",
        }
    }
}

/// Arguments of one `engine-probe --layer-cost` run.
#[derive(Debug, Clone)]
pub struct LayerCostArgs {
    /// The workspace to load.
    pub base: PathBuf,
    /// The edit classes, one series per class and scope.
    pub classes: Vec<EditClass>,
    /// The depths whose chain-view bytes are reported; the stack is one level deeper than the
    /// largest, so that the level at that depth is sealed and has a child.
    pub view_depths: Vec<u32>,
    /// The number of workspace files (in path order) the levels rotate over, when `paths` is
    /// empty.
    pub files: u32,
    /// The workspace files the levels rotate over, in this order; empty: the first `files`.
    pub paths: Vec<RelPath>,
    /// Scopes to diagnose, in order.
    pub scopes: Vec<Scope>,
    /// `RaConfig.threads`: the threads of one diagnosis (`None`: the available parallelism).
    pub threads: Option<usize>,
    /// Whether the person running it kept the machine otherwise idle.
    pub idle: bool,
}

/// What one `engine-probe --layer-cost` run measured, written as JSON to `--report`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LayerCostReport {
    /// Always the Engine: only its layers are measured.
    pub backend: Backend,
    /// The machine the run measured on; `threads` is the threads of one diagnosis.
    pub machine: Machine,
    /// The files the levels rotate over.
    pub files: Vec<String>,
    /// The levels of each stack.
    pub depth: u32,
    /// One series per class and scope, in that order.
    pub series: Vec<ClassCost>,
}

/// One stack of a [`LayerCostReport`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClassCost {
    /// The edit class of its levels.
    pub class: EditClass,
    /// The scope its levels were diagnosed at.
    pub scope: &'static str,
    /// One entry per level, the lowest first.
    pub levels: Vec<LayerCostLevel>,
    /// `kept_bytes` over the levels of the class (`counted`): min, p50, p90, max.
    pub kept_bytes: Quantiles,
    /// `transient_bytes` over the same levels.
    pub transient_bytes: Quantiles,
    /// `flat_bytes` of the level at each of the run's view depths, as (depth, bytes).
    pub view_bytes_at: Vec<(u32, u64)>,
    /// The live heap once every level is written and diagnosed, less the live heap before the
    /// fork: what the stack holds.
    pub stack_heap_bytes: i64,
}

/// One level of a [`ClassCost`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LayerCostLevel {
    /// The level, from 1.
    pub level: usize,
    /// Whether it is an edit of the series' class and counts in its quantiles: a body series'
    /// first level on each file adds the function whose body the later levels change, an item.
    pub counted: bool,
    /// The files its diagnosis covered.
    pub scope_files: usize,
    /// The live heap after its diagnosis less the live heap before its write.
    pub kept_bytes: i64,
    /// The most heap live at once during its write and diagnosis less the live heap after its
    /// diagnosis.
    pub transient_bytes: i64,
    /// The bytes of the chain view its layer built (0 for the top level, which has no child).
    pub flat_bytes: u64,
}

/// Min, nearest-rank p50 and p90, and max of a set of byte figures; all 0 for none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Quantiles {
    /// How many figures.
    pub n: usize,
    /// The smallest.
    pub min: i64,
    /// The nearest-rank median.
    pub p50: i64,
    /// The nearest-rank 90th percentile.
    pub p90: i64,
    /// The largest.
    pub max: i64,
}

impl Quantiles {
    /// The quantiles of `xs`, p50 and p90 by nearest rank (`ceil(q · n)`-th smallest).
    ///
    /// # Contract
    /// - C1: with n figures sorted, p50 is figure `ceil(n / 2)` and p90 figure `ceil(9n / 10)`
    ///   (from 1), min the first and max the last; no figure gives all zeros.
    ///   tested: layer_cost::tests::quantiles_are_nearest_rank
    pub fn of(xs: &[i64]) -> Quantiles {
        let mut sorted = xs.to_vec();
        sorted.sort_unstable();
        let n = sorted.len();
        let rank = |num: usize, den: usize| -> i64 {
            let r = (num * n).div_ceil(den).max(1);
            sorted[r - 1]
        };
        if n == 0 {
            return Quantiles {
                n,
                min: 0,
                p50: 0,
                p90: 0,
                max: 0,
            };
        }
        Quantiles {
            n,
            min: sorted[0],
            p50: rank(1, 2),
            p90: rank(9, 10),
            max: sorted[n - 1],
        }
    }
}

/// The function a body series changes, appended to a file at its first level there.
const BODY_FN: &str = "__ryo_layer_cost_body";

/// The levels of a series of `class`, `depth` levels over `files` (each a path and its base
/// text), and per level whether it counts as an edit of the class. Every level is one write of
/// one file: file `(k - 1) % files.len()` at level k. An item level appends a function
/// (`stacked::depth_levels`); a body level changes the literal that [`BODY_FN`] returns, so its
/// first level on a file appends that function and is not counted.
///
/// # Contract
/// - C1: a body series' level k writes its file's base text plus `BODY_FN` returning k; the
///   first level on each file is not counted and every later one is; two levels on one file
///   differ only inside the function's body. An item series is `stacked::depth_levels`, every
///   level counted.
///   tested: layer_cost::tests::body_levels_change_only_a_body
pub fn class_levels(
    class: EditClass,
    files: &[(RelPath, String)],
    depth: usize,
) -> Vec<(Vec<Edit>, bool)> {
    match class {
        EditClass::Item => stacked::depth_levels(files, depth)
            .into_iter()
            .map(|edits| (edits, true))
            .collect(),
        EditClass::Body => {
            if files.is_empty() {
                return Vec::new();
            }
            (1..=depth)
                .map(|k| {
                    let i = (k - 1) % files.len();
                    let (path, base) = &files[i];
                    let mut text = base.clone();
                    if !text.is_empty() && !text.ends_with('\n') {
                        text.push('\n');
                    }
                    text.push_str(&format!("\nfn {BODY_FN}() -> u32 {{\n    {k}\n}}\n"));
                    (vec![Edit::write(path.clone(), text)], k > files.len())
                })
                .collect()
        }
    }
}

/// The readings of one level: the live heap before its write, after its diagnosis, the peak
/// during them, and the files its diagnosis covered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reading {
    /// The live heap before the level's write (the peak is reset there).
    pub before: u64,
    /// The live heap after its diagnosis.
    pub after: u64,
    /// The most heap live at once from `before` to `after`.
    pub peak: u64,
    /// The files its diagnosis covered.
    pub scope_files: usize,
}

/// The series of one stack from its readings: per level (with `counted` from [`class_levels`])
/// its kept and transient bytes and its layer's `flat_bytes`, the quantiles over the counted
/// levels and the view bytes at `view_depths`.
///
/// # Contract
/// - C1: a level's `kept_bytes` is after - before, its `transient_bytes` peak - after; the
///   quantiles take the counted levels only; `view_bytes_at` holds, for each view depth d of at
///   most the stack's levels, the `flat_bytes` of level d.
///   tested: layer_cost::tests::a_class_cost_reads_kept_transient_and_views
pub fn class_cost_of(
    class: EditClass,
    scope: &'static str,
    readings: &[Reading],
    counted: &[bool],
    flat_bytes: &[u64],
    view_depths: &[u32],
    stack_heap_bytes: i64,
) -> ClassCost {
    let levels: Vec<LayerCostLevel> = readings
        .iter()
        .enumerate()
        .map(|(i, r)| LayerCostLevel {
            level: i + 1,
            counted: counted.get(i).copied().unwrap_or(false),
            scope_files: r.scope_files,
            kept_bytes: r.after as i64 - r.before as i64,
            transient_bytes: r.peak as i64 - r.after as i64,
            flat_bytes: flat_bytes.get(i).copied().unwrap_or(0),
        })
        .collect();
    let of = |f: fn(&LayerCostLevel) -> i64| {
        let xs: Vec<i64> = levels.iter().filter(|l| l.counted).map(f).collect();
        Quantiles::of(&xs)
    };
    ClassCost {
        class,
        scope,
        kept_bytes: of(|l| l.kept_bytes),
        transient_bytes: of(|l| l.transient_bytes),
        view_bytes_at: view_depths
            .iter()
            .filter_map(|&d| {
                let level = levels.get((d as usize).checked_sub(1)?)?;
                Some((d, level.flat_bytes))
            })
            .collect(),
        stack_heap_bytes,
        levels,
    }
}

/// Loads `args.base` on the Engine, runs one throwaway level as a job and drops it, and then,
/// per class and scope, stacks one level more than the largest view depth as jobs of `Forks` at
/// W = 1, each a job on the Layer of the one below and so written and diagnosed once before the
/// next, reading the counting allocator around every level and the Layers' counters once every
/// level is diagnosed; then drops them.
pub fn run_layer_cost(
    args: &LayerCostArgs,
    gauge: &'static dyn HeapGauge,
) -> Result<LayerCostReport> {
    let ram_gib = proc::mem_total_kb()? as f64 * 1024.0 / (1u64 << 30) as f64;
    let cores = std::thread::available_parallelism()
        .context("the available parallelism")?
        .get();
    let (analyzer, _observer, mut session) =
        crate::bench::load_on_threads(&args.base, true, false, Backend::Engine, args.threads)?;
    let files = if args.paths.is_empty() {
        crate::bench::depth_files(&analyzer, &session, &args.base, args.files)?
    } else {
        args.paths
            .iter()
            .map(|p| {
                ryo_v2_core::app::ports::Verifier::text(&analyzer, &session, p)
                    .map(|t| (p.clone(), t))
                    .with_context(|| format!("--layer-files: {p} is not an indexed file"))
            })
            .collect::<Result<Vec<_>>>()?
    };
    let names: Vec<String> = files.iter().map(|(p, _)| p.to_string()).collect();
    eprintln!("[layer-cost] levels rotate over {}", names.join(", "));
    let depth = args.view_depths.iter().copied().max().unwrap_or(0) as usize + 1;
    // A throwaway level first: the first diagnosis after the load frees what the load left
    // (about 0.1 GiB on ryo-rs at one thread), which would otherwise land in the first series.
    if let (Some(&class), Some(&scope)) = (args.classes.first(), args.scopes.first()) {
        let warm: Vec<Vec<Edit>> = class_levels(class, &files, 1)
            .into_iter()
            .map(|(edits, _)| edits)
            .collect();
        let reverted = stacked::depth_series(
            &analyzer,
            &mut session,
            "layer-cost warm-up",
            &warm,
            scope,
            stacked::DepthShape::EveryLevel,
            &mut |_| Ok(()),
            &mut |_, _| {},
        )?;
        anyhow::ensure!(
            reverted,
            "layer-cost warm-up: the base's isolation failed at the close (I2)"
        );
    }
    let mut series = Vec::new();
    for &class in &args.classes {
        let levels = class_levels(class, &files, depth);
        let (edits, counted): (Vec<Vec<Edit>>, Vec<bool>) = levels.into_iter().unzip();
        for &scope in &args.scopes {
            let mut readings: Vec<Reading> = Vec::new();
            let mut stacked_at: u64 = 0;
            let mut flat: Vec<u64> = Vec::new();
            let before_fork = gauge.live_bytes();
            let reverted = stacked::depth_series(
                &analyzer,
                &mut session,
                &format!("layer-cost {} depth {depth}", class.name()),
                &edits,
                scope,
                stacked::DepthShape::EveryLevel,
                &mut |point| {
                    match point {
                        DepthPoint::BeforeWrite(_) => {
                            gauge.reset_peak();
                            readings.push(Reading {
                                before: gauge.live_bytes(),
                                ..Reading::default()
                            });
                        }
                        DepthPoint::AfterDiagnose(_, found) => {
                            if let Some(last) = readings.last_mut() {
                                last.after = gauge.live_bytes();
                                last.peak = gauge.peak_bytes();
                                last.scope_files = found.files.len();
                            }
                        }
                        DepthPoint::AfterWrite(_)
                        | DepthPoint::BeforeDiagnose(_)
                        | DepthPoint::BeforePop(_)
                        | DepthPoint::AfterPop(_) => {}
                    }
                    Ok(())
                },
                &mut |_, layers| {
                    stacked_at = gauge.live_bytes();
                    flat = layers.iter().map(|l| l.stats().flat_bytes as u64).collect();
                },
            )?;
            anyhow::ensure!(
                reverted,
                "layer-cost {} [{}]: the base's isolation failed at the close (I2)",
                class.name(),
                scope_name(scope)
            );
            let one = class_cost_of(
                class,
                scope_name(scope),
                &readings,
                &counted,
                &flat,
                &args.view_depths,
                stacked_at as i64 - before_fork as i64,
            );
            eprintln!(
                "[layer-cost] {} [{}] kept p50/p90/max {}/{}/{} transient p50/p90/max {}/{}/{} \
                 view_bytes_at {:?} stack_heap_bytes {}",
                class.name(),
                one.scope,
                one.kept_bytes.p50,
                one.kept_bytes.p90,
                one.kept_bytes.max,
                one.transient_bytes.p50,
                one.transient_bytes.p90,
                one.transient_bytes.max,
                one.view_bytes_at,
                one.stack_heap_bytes,
            );
            series.push(one);
        }
    }
    Ok(LayerCostReport {
        backend: Backend::Engine,
        machine: Machine {
            ram_gib,
            cores,
            threads: session.threads(),
            idle: args.idle,
        },
        files: names,
        depth: depth as u32,
        series,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantiles_are_nearest_rank() {
        let q = Quantiles::of(&[5, 1, 4, 2, 3, 10, 9, 8, 7, 6]);
        assert_eq!(
            q,
            Quantiles {
                n: 10,
                min: 1,
                p50: 5,
                p90: 9,
                max: 10
            }
        );
        let one = Quantiles::of(&[7]);
        assert_eq!((one.min, one.p50, one.p90, one.max), (7, 7, 7, 7));
        let three = Quantiles::of(&[3, 1, 2]);
        assert_eq!((three.p50, three.p90), (2, 3));
        assert_eq!(Quantiles::of(&[]).n, 0);
        assert_eq!(Quantiles::of(&[]).max, 0);
    }

    fn files() -> Vec<(RelPath, String)> {
        vec![
            (RelPath::parse("a.rs").unwrap(), "fn a() {}".to_owned()),
            (RelPath::parse("b.rs").unwrap(), "fn b() {}\n".to_owned()),
        ]
    }

    #[test]
    fn body_levels_change_only_a_body() {
        let levels = class_levels(EditClass::Body, &files(), 5);
        assert_eq!(levels.len(), 5);
        let counted: Vec<bool> = levels.iter().map(|(_, c)| *c).collect();
        assert_eq!(counted, vec![false, false, true, true, true]);
        let text = |k: usize| levels[k - 1].0[0].new_text.clone().unwrap();
        assert_eq!(levels[0].0.len(), 1);
        assert_eq!(levels[0].0[0].path.as_str(), "a.rs");
        assert_eq!(levels[1].0[0].path.as_str(), "b.rs");
        assert_eq!(levels[2].0[0].path.as_str(), "a.rs");
        assert_eq!(
            text(1),
            format!("fn a() {{}}\n\nfn {BODY_FN}() -> u32 {{\n    1\n}}\n")
        );
        assert_eq!(text(3), text(1).replace("    1\n", "    3\n"));
        assert_eq!(text(4), text(2).replace("    2\n", "    4\n"));
        let items = class_levels(EditClass::Item, &files(), 3);
        let expected: Vec<(Vec<Edit>, bool)> = stacked::depth_levels(&files(), 3)
            .into_iter()
            .map(|e| (e, true))
            .collect();
        assert_eq!(items, expected);
    }

    #[test]
    fn a_class_cost_reads_kept_transient_and_views() {
        let readings = [
            Reading {
                before: 100,
                after: 150,
                peak: 400,
                scope_files: 3,
            },
            Reading {
                before: 150,
                after: 170,
                peak: 300,
                scope_files: 3,
            },
            Reading {
                before: 170,
                after: 200,
                peak: 260,
                scope_files: 3,
            },
        ];
        let cost = class_cost_of(
            EditClass::Body,
            "affected",
            &readings,
            &[false, true, true],
            &[11, 22],
            &[1, 2, 3, 4],
            100,
        );
        let kept: Vec<i64> = cost.levels.iter().map(|l| l.kept_bytes).collect();
        let transient: Vec<i64> = cost.levels.iter().map(|l| l.transient_bytes).collect();
        assert_eq!(kept, vec![50, 20, 30]);
        assert_eq!(transient, vec![250, 130, 60]);
        assert_eq!(
            (cost.kept_bytes.n, cost.kept_bytes.min, cost.kept_bytes.max),
            (2, 20, 30)
        );
        assert_eq!(
            (cost.transient_bytes.min, cost.transient_bytes.max),
            (60, 130)
        );
        assert_eq!(cost.view_bytes_at, vec![(1, 11), (2, 22), (3, 0)]);
        assert_eq!(cost.stack_heap_bytes, 100);
    }
}
