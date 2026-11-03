//! `engine-probe`: loads a workspace once, takes the memory of the process before the load, at
//! its indexing, after its baseline and after the last check, with the database's composition,
//! and runs each variant as a level on the `--on` parent's level through the one stacked
//! procedure of the host, reading the counting allocator at every point of each repeat. It writes
//! a `ProbeReport` (`ryo_v2_host::engine_probe`) as JSON to `--report`.
//!
//! The counting allocator makes every allocation slower, so no time figure of this binary stands
//! for the engine's speed; `bench`, on the system allocator, gives those.

mod alloc;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use ryo_v2_core::domain::Scope;
use ryo_v2_host::engine_probe::{self, BenchMedian, ProbeArgs};
use std::path::PathBuf;
use std::process::ExitCode;

/// The allocator of this binary, and the gauge the probe reads.
#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting::new();

#[derive(Parser)]
#[command(
    name = "engine-probe",
    about = "Measure what a loaded session holds and what each stacked check adds to the heap"
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
    let args = ProbeArgs {
        base: cli.base,
        on: cli.on,
        variants: cli.variants,
        repeat: cli.repeat,
        scopes: match cli.scope {
            ScopeArg::Affected => vec![Scope::Affected],
            ScopeArg::Workspace => vec![Scope::Workspace],
            ScopeArg::Both => vec![Scope::Affected, Scope::Workspace],
        },
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
}
