//! `engine-probe`: loads a workspace once, takes the memory of the process before the load, at
//! its indexing, after its baseline and after the last check, with the database's composition,
//! and runs each variant as a level on the `--on` parent's level through the one stacked
//! procedure of the host (jobs of `Forks` at W = 1), reading the counting allocator at every
//! point of each repeat. It writes a `ProbeReport` (`ryo_v2_host::engine_probe`) as JSON to
//! `--report`.
//!
//! The counting allocator makes every allocation slower, so no time figure of this binary stands
//! for the engine's speed; `bench`, on the system allocator, gives those.
//!
//! Every fork is a layer of the fork Engine (D11); the series of `--on` and `--depth` run on
//! `Forks`.

mod alloc;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use ryo_v2_core::domain::{RelPath, Scope};
use ryo_v2_host::engine_probe::{self, BenchMedian, DepthProbeArgs, ProbeArgs};
use ryo_v2_host::layer_cost::{self, EditClass, LayerCostArgs};
use std::path::PathBuf;
use std::process::ExitCode;

/// The allocator of this binary, and the gauge the probe reads.
#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting::new();

#[derive(Parser)]
#[command(
    name = "engine-probe",
    about = "Measure what a loaded session holds and what each stacked check adds to the heap",
    group(clap::ArgGroup::new("stacks").args(["depth", "layer_cost"]))
)]
struct Cli {
    /// The workspace to load.
    base: PathBuf,
    /// Overlay dirs; each one's edits that the --on parent does not make are written as a level
    /// on the parent's level.
    #[arg(requires = "on")]
    variants: Vec<PathBuf>,
    /// The parent overlay, written and diagnosed once as level 1 of every series.
    #[arg(long, value_name = "PARENT")]
    on: Option<PathBuf>,
    /// Repeats of each variant per scope.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
    repeat: u32,
    /// Which scopes to diagnose.
    #[arg(long, value_enum, default_value_t = ScopeArg::Affected)]
    scope: ScopeArg,
    /// The machine was kept otherwise idle during the run (recorded in the report).
    #[arg(long)]
    idle: bool,
    /// The bench's warm median check_ms of a variant and scope, as VARIANT:SCOPE=MS, from a
    /// bench run back to back with this one; the report states the probe's overhead against it.
    #[arg(long, value_name = "VARIANT:SCOPE=MS", value_parser = parse_bench_median)]
    bench_median: Vec<BenchMedian>,
    /// Where the JSON report is written.
    #[arg(long, value_name = "OUT")]
    report: PathBuf,
    /// Stack this many levels instead of the variants: level k appends a function to one of
    /// `--depth-files` workspace files (as `bench --depth-sweep`) and is diagnosed once before
    /// level k + 1 is written on it, the search's shape. The report is a `DepthReport`: the
    /// stack's live heap, per level the heap delta and the layer's counters.
    #[arg(
        long,
        value_parser = clap::value_parser!(u32).range(1..),
        conflicts_with_all = ["on", "variants", "bench_median"]
    )]
    depth: Option<u32>,
    /// With `--depth` or `--layer-cost`: the number of workspace files (in path order) the
    /// levels rotate over
    /// [default: 1].
    #[arg(long, requires = "stacks", value_parser = clap::value_parser!(u32).range(1..))]
    depth_files: Option<u32>,
    /// With `--depth`: the run fails (after its report is written) when the chain views of a
    /// stack take more than this percentage of its heap (gate M2-view)
    /// [default: `engine_probe::M2_VIEW_MAX_SHARE_PCT`, 15].
    #[arg(long, value_name = "PCT", requires = "depth", value_parser = parse_pct)]
    max_view_share: Option<f64>,
    /// Measure what one layer of the fork Engine costs (always on the Engine): per edit class of
    /// `--classes`, a stack one level deeper than the largest of `--view-depths`, each level
    /// written and diagnosed once before the next. The report is a `LayerCostReport`: per level
    /// the heap it keeps, its transient peak and the bytes of its chain view; per class the
    /// quantiles and the view bytes at each view depth.
    #[arg(
        long,
        conflicts_with_all = ["on", "variants", "bench_median", "depth"]
    )]
    layer_cost: bool,
    /// With `--layer-cost`: the edit classes [default: body,item].
    #[arg(long, value_enum, value_delimiter = ',', requires = "layer_cost")]
    classes: Vec<ClassArg>,
    /// With `--layer-cost`: the depths whose chain-view bytes are reported [default: 1,10,20].
    #[arg(
        long,
        value_delimiter = ',',
        requires = "layer_cost",
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    view_depths: Vec<u32>,
    /// With `--layer-cost`: `RaConfig.threads`, the threads of one diagnosis [default: the
    /// available parallelism].
    #[arg(long, requires = "layer_cost", value_parser = clap::value_parser!(u32).range(1..))]
    threads: Option<u32>,
    /// With `--layer-cost`: the workspace files (relative paths) the levels rotate over, in
    /// this order, instead of the first `--depth-files` in path order.
    #[arg(
        long,
        value_delimiter = ',',
        requires = "layer_cost",
        conflicts_with = "depth_files",
        value_parser = parse_rel
    )]
    layer_files: Vec<RelPath>,
}

/// A `--layer-files` path: a workspace-relative path.
fn parse_rel(text: &str) -> Result<RelPath, String> {
    RelPath::parse(text).map_err(|e| format!("{text:?}: {e}"))
}

#[derive(Clone, Copy, ValueEnum)]
enum ClassArg {
    Body,
    Item,
}

/// A `--max-view-share` value: a percentage from 0 to 100.
fn parse_pct(text: &str) -> Result<f64, String> {
    let pct: f64 = text.parse().map_err(|e| format!("{text:?}: {e}"))?;
    if (0.0..=100.0).contains(&pct) {
        Ok(pct)
    } else {
        Err(format!("{text:?} is not a percentage from 0 to 100"))
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum ScopeArg {
    Affected,
    Workspace,
    Both,
}

/// A `--bench-median` value: `VARIANT:SCOPE=MS`, the variant as given to both runs.
fn parse_bench_median(text: &str) -> Result<BenchMedian, String> {
    let shape = || format!("{text:?}: not VARIANT:SCOPE=MS");
    let (head, ms) = text.rsplit_once('=').ok_or_else(shape)?;
    let (variant, scope) = head.rsplit_once(':').ok_or_else(shape)?;
    let scope = match scope {
        "affected" => Scope::Affected,
        "workspace" => Scope::Workspace,
        other => {
            return Err(format!(
                "{text:?}: scope {other:?} is not affected or workspace"
            ));
        }
    };
    let ms = ms
        .parse::<u64>()
        .map_err(|e| format!("{text:?}: MS {ms:?}: {e}"))?;
    if variant.is_empty() {
        return Err(shape());
    }
    Ok(BenchMedian {
        variant: variant.to_owned(),
        scope,
        ms,
    })
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let scopes = match cli.scope {
        ScopeArg::Affected => vec![Scope::Affected],
        ScopeArg::Workspace => vec![Scope::Workspace],
        ScopeArg::Both => vec![Scope::Affected, Scope::Workspace],
    };
    if cli.layer_cost {
        let classes = if cli.classes.is_empty() {
            vec![EditClass::Body, EditClass::Item]
        } else {
            cli.classes
                .iter()
                .map(|c| match c {
                    ClassArg::Body => EditClass::Body,
                    ClassArg::Item => EditClass::Item,
                })
                .collect()
        };
        let args = LayerCostArgs {
            base: cli.base,
            classes,
            view_depths: if cli.view_depths.is_empty() {
                vec![1, 10, 20]
            } else {
                cli.view_depths
            },
            files: cli.depth_files.unwrap_or(1),
            paths: cli.layer_files,
            scopes,
            threads: cli.threads.map(|t| t as usize),
            idle: cli.idle,
        };
        let report = layer_cost::run_layer_cost(&args, &COUNTING)?;
        engine_probe::write_report(&report, &cli.report)
            .with_context(|| format!("the report {}", cli.report.display()))?;
        eprintln!(
            "[probe] layer cost, depth {}, {} series, report {}",
            report.depth,
            report.series.len(),
            cli.report.display()
        );
        return Ok(());
    }
    if let Some(depth) = cli.depth {
        let args = DepthProbeArgs {
            base: cli.base,
            depth,
            files: cli.depth_files.unwrap_or(1),
            scopes,
            idle: cli.idle,
        };
        let report = engine_probe::run_depth(&args, &COUNTING)?;
        engine_probe::write_report(&report, &cli.report)
            .with_context(|| format!("the report {}", cli.report.display()))?;
        eprintln!(
            "[probe] depth {}, {} series, report {}",
            report.depth,
            report.series.len(),
            cli.report.display()
        );
        return engine_probe::check_view_share(
            &report,
            cli.max_view_share
                .unwrap_or(engine_probe::M2_VIEW_MAX_SHARE_PCT),
        );
    }
    let args = ProbeArgs {
        base: cli.base,
        on: cli.on,
        variants: cli.variants,
        repeat: cli.repeat,
        scopes,
        idle: cli.idle,
        bench_medians: cli.bench_median,
    };
    let report = engine_probe::run(&args, &COUNTING)?;
    engine_probe::write_report(&report, &cli.report)
        .with_context(|| format!("the report {}", cli.report.display()))?;
    eprintln!(
        "[probe] {} phases, {} checks, report {}",
        report.phases.len(),
        report.checks.len(),
        cli.report.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_the_probe_line() {
        let cli = Cli::try_parse_from([
            "engine-probe",
            "/b",
            "--on",
            "/p",
            "/v1",
            "/v2",
            "--repeat",
            "20",
            "--scope",
            "both",
            "--idle",
            "--bench-median",
            "/v1:affected=120",
            "--report",
            "/o/probe.json",
        ])
        .unwrap();
        assert_eq!(cli.variants, [PathBuf::from("/v1"), PathBuf::from("/v2")]);
        assert_eq!(cli.on, Some(PathBuf::from("/p")));
        assert_eq!((cli.repeat, cli.idle), (20, true));
        assert_eq!(
            cli.bench_median,
            [BenchMedian {
                variant: "/v1".to_owned(),
                scope: Scope::Affected,
                ms: 120
            }]
        );
        // A variant needs the parent, and the report is required.
        assert!(Cli::try_parse_from(["engine-probe", "/b", "/v", "--report", "/o"]).is_err());
        assert!(Cli::try_parse_from(["engine-probe", "/b"]).is_err());
        assert!(Cli::try_parse_from(["engine-probe", "/b", "--report", "/o"]).is_ok());
        for bad in [
            "v:affected",
            "v=1",
            "v:all=1",
            "v:affected=x",
            ":affected=1",
        ] {
            assert!(parse_bench_median(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn cli_parses_depth() {
        let cli = Cli::try_parse_from([
            "engine-probe",
            "/b",
            "--depth",
            "20",
            "--depth-files",
            "4",
            "--report",
            "/o/depth.json",
        ])
        .unwrap();
        assert_eq!((cli.depth, cli.depth_files), (Some(20), Some(4)));
        // A stack of levels, not a parent and variants; at least one level and one file.
        let parse = |args: &[&str]| Cli::try_parse_from(args).map(|_| ());
        assert!(
            parse(&[
                "engine-probe",
                "/b",
                "--depth",
                "2",
                "--on",
                "/p",
                "--report",
                "/o"
            ])
            .is_err()
        );
        assert!(parse(&["engine-probe", "/b", "--depth", "0", "--report", "/o"]).is_err());
        assert!(parse(&["engine-probe", "/b", "--depth-files", "2", "--report", "/o"]).is_err());
        assert!(
            parse(&[
                "engine-probe",
                "/b",
                "--depth",
                "1",
                "--depth-files",
                "0",
                "--report",
                "/o"
            ])
            .is_err()
        );
        // The M2-view ceiling: a percentage, only with `--depth`.
        let max = |args: &[&str]| Cli::try_parse_from(args).map(|c| c.max_view_share);
        let depth = ["engine-probe", "/b", "--depth", "2", "--report", "/o"];
        assert_eq!(max(&depth).unwrap(), None);
        let with = |pct: &str| max(&[&depth[..], &["--max-view-share", pct]].concat());
        assert_eq!(with("12.5").unwrap(), Some(12.5));
        assert!(with("101").is_err() && with("x").is_err());
        assert!(
            parse(&[
                "engine-probe",
                "/b",
                "--max-view-share",
                "5",
                "--report",
                "/o"
            ])
            .is_err()
        );
    }

    #[test]
    fn cli_parses_layer_cost() {
        let cli = Cli::try_parse_from([
            "engine-probe",
            "/b",
            "--layer-cost",
            "--classes",
            "body,item",
            "--view-depths",
            "1,10,20",
            "--depth-files",
            "4",
            "--threads",
            "1",
            "--report",
            "/o/layer-cost.json",
        ])
        .unwrap();
        assert!(cli.layer_cost);
        assert_eq!(cli.classes.len(), 2);
        assert_eq!(cli.view_depths, [1, 10, 20]);
        assert_eq!((cli.depth_files, cli.threads), (Some(4), Some(1)));
        assert!(cli.layer_files.is_empty());
        let chosen = Cli::try_parse_from([
            "engine-probe",
            "/b",
            "--layer-cost",
            "--layer-files",
            "a/src/lib.rs,b/src/x.rs",
            "--report",
            "/o",
        ])
        .unwrap();
        assert_eq!(
            chosen.layer_files,
            [
                RelPath::parse("a/src/lib.rs").unwrap(),
                RelPath::parse("b/src/x.rs").unwrap()
            ]
        );
        let parse = |args: &[&str]| Cli::try_parse_from(args).map(|_| ());
        // Not with another mode.
        for other in [&["--depth", "2"][..], &["--on", "/p", "/v"]] {
            let args = [
                &["engine-probe", "/b", "--layer-cost", "--report", "/o"][..],
                other,
            ]
            .concat();
            assert!(parse(&args).is_err(), "{other:?}");
        }
        // Its options need it; a depth is at least 1.
        assert!(parse(&["engine-probe", "/b", "--classes", "body", "--report", "/o"]).is_err());
        assert!(parse(&["engine-probe", "/b", "--threads", "2", "--report", "/o"]).is_err());
        assert!(
            parse(&[
                "engine-probe",
                "/b",
                "--layer-files",
                "a.rs",
                "--report",
                "/o"
            ])
            .is_err()
        );
        assert!(
            parse(&[
                "engine-probe",
                "/b",
                "--layer-cost",
                "--layer-files",
                "a.rs",
                "--depth-files",
                "2",
                "--report",
                "/o"
            ])
            .is_err()
        );
        assert!(
            parse(&[
                "engine-probe",
                "/b",
                "--layer-cost",
                "--view-depths",
                "0",
                "--report",
                "/o"
            ])
            .is_err()
        );
    }
}
