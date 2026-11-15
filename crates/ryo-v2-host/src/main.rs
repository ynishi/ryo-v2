//! The `ryo-v2` command: parses arguments, calls
//! `ryo-v2-core`, prints the result.
//!
//! The argument parser's definition (clap's derive types) is the only
//! description of the commands: help, the command list and validation come
//! from it, not from a second hand-kept table.
//!
//! The host runs on Linux only: `bench` reads its figures from `/proc` (`proc.rs`), and a read
//! that fails is an error of the run.

mod apply;
mod blueprint_json;
mod resolution_json;
mod search;
mod search_json;

use anyhow::Result;
use clap::{Parser, Subcommand};
use ryo_v2_core::app::search::RESERVED_RAM_GB;
use ryo_v2_core::domain::{RelPath, Scope, TextRange};
use ryo_v2_host::bench::{self, BenchArgs, DepthArgs};
use ryo_v2_host::forks_bench::{self, ForksArgs, WidthSpec};

/// What `bench --forks` and its options do not go with.
const FORKS_CONFLICTS: [&str; 4] = ["variants", "on", "large", "depth_sweep"];

/// A `--fork-paths` path: a workspace-relative path.
fn parse_rel(text: &str) -> Result<RelPath, String> {
    RelPath::parse(text).map_err(|e| format!("{text:?}: {e}"))
}
use std::path::PathBuf;
use std::process::ExitCode;

/// The command's help summary. Written out rather than taken from the package
/// description (clap's bare `about`), so that describing the crate does not
/// rewrite the command's help.
const ABOUT: &str = "Command-line interface of ryo-v2: Speculative refactoring engine for Rust: \
                     Blueprints explored and verified in-process on rust-analyzer, built once after commit";

#[derive(Parser)]
#[command(name = "ryo-v2", version, about = ABOUT)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Load BASE once, then verify each VARIANT dir's .rs diff against it (apply, diagnose, revert).
    Bench {
        /// The workspace to load.
        #[arg(required_unless_present = "large")]
        base: Option<PathBuf>,
        /// Overlay dirs; each .rs that differs from (or is missing in) BASE is an edit.
        #[arg(required_unless_present_any = ["large", "depth_sweep", "forks"])]
        variants: Vec<PathBuf>,
        /// Large mode: BASE = PATH, VARIANTS = corpus/body-01 corpus/body-02 corpus/body-03 (relative to the cwd).
        #[arg(long, value_name = "PATH", conflicts_with_all = ["base", "variants"])]
        large: Option<PathBuf>,
        /// fork/apply/diagnose/discard each variant this many times per scope.
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
        repeat: u32,
        /// Which scopes to diagnose.
        #[arg(long, value_enum, default_value_t = ScopeArg::Affected)]
        scope: ScopeArg,
        /// Do not start the proc-macro server.
        #[arg(long)]
        no_proc_macro: bool,
        /// Skip the post-revert re-diagnosis (revert_ms then excludes it; revert_ok prints `-`).
        #[arg(long)]
        no_assert_revert: bool,
        /// Turn ryo-v2's gap detectors off (`RaConfig.gap_detectors`): each check is
        /// rust-analyzer's own diagnosis alone.
        #[arg(long, conflicts_with_all = ["depth_sweep", "forks"])]
        no_gap_detectors: bool,
        /// Print under each run the wall time of each file's diagnosis: the total, the files and
        /// the slowest five (`  file_ms total <ms> files <n> top <path>=<ms> ...`).
        #[arg(long, conflicts_with_all = ["depth_sweep", "forks"])]
        file_times: bool,
        /// Measure each VARIANT as a child of the PARENT overlay, two ways: `rebuilt` (the
        /// parent's and the variant's edits composed and verified from BASE) and `stacked` (the
        /// variant submitted as a job on the parent's Layer of `Forks` at W = 1).
        #[arg(long, value_name = "PARENT")]
        on: Option<PathBuf>,
        /// Measure stacks of these depths instead of VARIANTS: per depth K, K levels, each a job
        /// of `Forks` at W = 1 on the Layer of the one below (level k appends `fn
        /// __ryo_depth_<k>() -> u32 { <k> }` to a workspace file), diagnose the top level
        /// `--repeat` times (the later times the top level run again on the level below) and drop
        /// the Layers. A comma-separated list, each >= 1.
        #[arg(
            long,
            value_name = "DEPTHS",
            value_delimiter = ',',
            num_args = 1,
            value_parser = clap::value_parser!(u32).range(1..),
            conflicts_with_all = ["variants", "on", "large"],
        )]
        depth_sweep: Vec<u32>,
        /// With `--depth-sweep`: the number of workspace files (in path order) the levels rotate
        /// over, so that depth is not the growth of one file [default: 1].
        #[arg(
            long,
            value_name = "N",
            requires = "depth_sweep",
            // clap waives a missing `requires` target that conflicts with a present argument,
            // so the conflicts of `--depth-sweep` are repeated here.
            conflicts_with_all = ["variants", "on", "large"],
            value_parser = clap::value_parser!(u32).range(1..),
        )]
        depth_files: Option<u32>,
        /// Run a job tree through the `Forks` port of the fork Engine at each of these widths
        /// instead of VARIANTS (D11, stage 3b): a seed job, then `--fork-depth` levels of
        /// `--fork-jobs` sibling jobs, each level on the sealed layer of job 0 of the level
        /// below. A comma-separated list; `<n>c` counts the caller's thread out of the workers'
        /// share (`ForksConfig.caller_works`, stage 4's shape). Always on the Engine.
        #[arg(
            long,
            value_name = "WIDTHS",
            value_delimiter = ',',
            num_args = 1,
            value_parser = WidthSpec::parse,
            conflicts_with_all = FORKS_CONFLICTS,
        )]
        forks: Vec<WidthSpec>,
        // The options of `--forks` repeat its conflicts: clap waives a missing `requires`
        // target that conflicts with a present argument.
        /// With `--forks`: the levels of the job tree.
        #[arg(
            long,
            value_name = "D",
            default_value_t = 4,
            requires = "forks",
            conflicts_with_all = FORKS_CONFLICTS,
            value_parser = clap::value_parser!(u32).range(1..),
        )]
        fork_depth: u32,
        /// With `--forks`: the sibling jobs of each level.
        #[arg(
            long,
            value_name = "J",
            default_value_t = 12,
            requires = "forks",
            conflicts_with_all = FORKS_CONFLICTS,
            value_parser = clap::value_parser!(u32).range(1..),
        )]
        fork_jobs: u32,
        /// With `--forks`: the number of workspace files (in path order) the jobs rotate over.
        #[arg(
            long,
            value_name = "N",
            default_value_t = 4,
            requires = "forks",
            conflicts_with_all = FORKS_CONFLICTS,
            conflicts_with = "fork_paths",
            value_parser = clap::value_parser!(u32).range(1..),
        )]
        fork_files: u32,
        /// With `--forks`: the workspace files (relative paths) the jobs rotate over, in this
        /// order, instead of the first `--fork-files`.
        #[arg(
            long,
            value_name = "PATHS",
            value_delimiter = ',',
            requires = "forks",
            conflicts_with_all = FORKS_CONFLICTS,
            value_parser = parse_rel,
        )]
        fork_paths: Vec<RelPath>,
        /// With `--forks`: `RaConfig.threads`, T, the threads the widths share [default: the
        /// available parallelism]; above the cores a width runs oversubscribed.
        #[arg(
            long,
            value_name = "T",
            requires = "forks",
            conflicts_with_all = FORKS_CONFLICTS,
            value_parser = clap::value_parser!(u32).range(1..),
        )]
        fork_threads: Option<u32>,
        /// With `--forks`: the cut-off of a job's check in ms (D1); without it no job is cut
        /// off, as W4 runs.
        #[arg(
            long,
            value_name = "MS",
            requires = "forks",
            conflicts_with_all = FORKS_CONFLICTS,
            value_parser = clap::value_parser!(u64).range(1..),
        )]
        fork_cut_off_ms: Option<u64>,
        /// With `--forks`: the RAM in GiB the run leaves free; it withdraws its jobs and stops
        /// when the RSS exceeds the machine's RAM less this.
        #[arg(
            long,
            value_name = "GIB",
            default_value_t = RESERVED_RAM_GB as f64,
            requires = "forks",
            conflicts_with_all = FORKS_CONFLICTS,
        )]
        rss_stop_gib: f64,
        /// With `--forks`: also run the tree serially, one job at a time at W = 1, per repeat.
        #[arg(long, requires = "forks", conflicts_with_all = FORKS_CONFLICTS)]
        fork_serial: bool,
    },
    /// Verify BLUEPRINTS against the workspace and print the Resolution as JSON (a dry run: nothing is written).
    Apply {
        /// The workspace to load.
        workspace: PathBuf,
        /// The blueprints JSON file.
        blueprints: PathBuf,
        /// How far the check reaches.
        #[arg(long, value_enum, default_value_t = ApplyScopeArg::Affected)]
        scope: ApplyScopeArg,
        /// Do not start the proc-macro server.
        #[arg(long)]
        no_proc_macro: bool,
    },
    /// Search for the best verified changes of one decision and print the report as JSON (a dry run: nothing is written).
    Search {
        /// The workspace to load.
        workspace: PathBuf,
        /// The request JSON file: places, probes, roots, budget, judge.
        request: PathBuf,
        /// How far each check reaches.
        #[arg(long, value_enum, default_value_t = ApplyScopeArg::Affected)]
        scope: ApplyScopeArg,
        /// Do not start the proc-macro server.
        #[arg(long)]
        no_proc_macro: bool,
    },
    /// Print the fingerprint of a file, read from disk: the `reads` entry (path, range,
    /// fingerprint) of a byte range `START..END`, or without a range the `bases` entry (path,
    /// fingerprint) of the whole file.
    Fingerprint {
        /// The workspace root.
        workspace: PathBuf,
        /// The file, relative to the workspace.
        path: String,
        /// Byte range `START..END`; without it the whole file.
        #[arg(value_parser = parse_range)]
        range: Option<TextRange>,
    },
}

fn parse_range(s: &str) -> Result<TextRange, String> {
    let (a, b) = s
        .split_once("..")
        .ok_or_else(|| format!("{s:?}: expected START..END"))?;
    let start: u32 = a.parse().map_err(|e| format!("start {a:?}: {e}"))?;
    let end: u32 = b.parse().map_err(|e| format!("end {b:?}: {e}"))?;
    TextRange::new(start, end).map_err(|e| e.to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum ApplyScopeArg {
    Affected,
    Workspace,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum ScopeArg {
    Affected,
    Workspace,
    Both,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<ExitCode> {
    match Cli::parse().command {
        Command::Apply {
            workspace,
            blueprints,
            scope,
            no_proc_macro,
        } => apply::run_apply(&apply::ApplyArgs {
            workspace,
            blueprints,
            scope: match scope {
                ApplyScopeArg::Affected => Scope::Affected,
                ApplyScopeArg::Workspace => Scope::Workspace,
            },
            proc_macro: !no_proc_macro,
        }),
        Command::Search {
            workspace,
            request,
            scope,
            no_proc_macro,
        } => search::run_search(&search::SearchArgs {
            workspace,
            request,
            scope: match scope {
                ApplyScopeArg::Affected => Scope::Affected,
                ApplyScopeArg::Workspace => Scope::Workspace,
            },
            proc_macro: !no_proc_macro,
        }),
        Command::Fingerprint {
            workspace,
            path,
            range,
        } => apply::run_fingerprint(&workspace, &path, range),
        Command::Bench {
            base,
            variants,
            large,
            repeat,
            scope,
            no_proc_macro,
            no_assert_revert,
            no_gap_detectors,
            file_times,
            on,
            depth_sweep,
            depth_files,
            forks,
            fork_depth,
            fork_jobs,
            fork_files,
            fork_paths,
            fork_threads,
            fork_cut_off_ms,
            rss_stop_gib,
            fork_serial,
        } => {
            let scopes = match scope {
                ScopeArg::Affected => vec![Scope::Affected],
                ScopeArg::Workspace => vec![Scope::Workspace],
                ScopeArg::Both => vec![Scope::Affected, Scope::Workspace],
            };
            if !forks.is_empty() {
                return forks_bench::run_forks(&ForksArgs {
                    base: base.ok_or_else(|| anyhow::anyhow!("BASE is required with --forks"))?,
                    widths: forks,
                    depth: fork_depth as usize,
                    jobs: fork_jobs as usize,
                    files: fork_files,
                    paths: fork_paths,
                    scopes,
                    repeat,
                    threads: fork_threads.map(|t| t as usize),
                    cut_off_ms: fork_cut_off_ms,
                    rss_reserve_gib: rss_stop_gib,
                    proc_macro: !no_proc_macro,
                    serial: fork_serial,
                })
                .map(|()| ExitCode::SUCCESS);
            }
            if !depth_sweep.is_empty() {
                return bench::run_depth_sweep(&DepthArgs {
                    base: base
                        .ok_or_else(|| anyhow::anyhow!("BASE is required with --depth-sweep"))?,
                    depths: depth_sweep,
                    files: depth_files.unwrap_or(1),
                    repeat,
                    scopes,
                    proc_macro: !no_proc_macro,
                    assert_revert: !no_assert_revert,
                })
                .map(|()| ExitCode::SUCCESS);
            }
            let (base, variants) = match large {
                Some(path) => {
                    let variants: Vec<PathBuf> =
                        ["corpus/body-01", "corpus/body-02", "corpus/body-03"]
                            .iter()
                            .map(PathBuf::from)
                            .collect();
                    for v in &variants {
                        if !v.is_dir() {
                            anyhow::bail!(
                                "--large: {} not found; run from the ryo-v2 repo root",
                                v.display()
                            );
                        }
                    }
                    (path, variants)
                }
                None => (
                    base.ok_or_else(|| anyhow::anyhow!("BASE is required without --large"))?,
                    variants,
                ),
            };
            bench::run(&BenchArgs {
                base,
                variants,
                on,
                repeat,
                scopes,
                proc_macro: !no_proc_macro,
                assert_revert: !no_assert_revert,
                gap_detectors: !no_gap_detectors,
                file_times,
            })
            .map(|()| ExitCode::SUCCESS)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn help_summary_is_the_command_line_text() {
        let help = Cli::command().render_help().to_string();
        assert!(
            help.starts_with(
                "Command-line interface of ryo-v2: Speculative refactoring engine for Rust: \
                 Blueprints explored and verified in-process on rust-analyzer, built once after commit\n"
            ),
            "{help}"
        );
    }

    #[test]
    fn cli_parses_large_and_repeat() {
        let r = Cli::try_parse_from([
            "ryo-v2", "bench", "--large", "/x", "--repeat", "3", "--scope", "both",
        ]);
        assert!(r.is_ok());
    }

    #[test]
    fn cli_parses_bench_on() {
        let ok = Cli::try_parse_from(["ryo-v2", "bench", "/b", "/v1", "/v2", "--on", "/p"])
            .map_err(|e| e.to_string())
            .expect("parses");
        match ok.command {
            Command::Bench { variants, on, .. } => {
                assert_eq!(on, Some(PathBuf::from("/p")));
                assert_eq!(variants, vec![PathBuf::from("/v1"), PathBuf::from("/v2")]);
            }
            _ => panic!("not bench"),
        }
        let without = Cli::try_parse_from(["ryo-v2", "bench", "/b", "/v"])
            .map_err(|e| e.to_string())
            .expect("parses without --on");
        match without.command {
            Command::Bench { on, .. } => assert!(on.is_none()),
            _ => panic!("not bench"),
        }
        assert!(Cli::try_parse_from(["ryo-v2", "bench", "/b", "/v", "--on"]).is_err());
    }

    /// The controls of gate RS (SEARCH_LEDGER.md, "The search on N forks", gate RS, runs 4 and 5).
    #[test]
    fn cli_parses_bench_gap_detectors_and_file_times() {
        let flags = |args: &[&str]| match Cli::try_parse_from(args)
            .map_err(|e| e.to_string())
            .expect("parses")
            .command
        {
            Command::Bench {
                no_gap_detectors,
                file_times,
                ..
            } => (no_gap_detectors, file_times),
            _ => panic!("not bench"),
        };
        assert_eq!(flags(&["ryo-v2", "bench", "/b", "/v"]), (false, false));
        assert_eq!(
            flags(&["ryo-v2", "bench", "/b", "/v", "--no-gap-detectors"]),
            (true, false)
        );
        assert_eq!(
            flags(&[
                "ryo-v2",
                "bench",
                "/b",
                "/v",
                "--file-times",
                "--no-gap-detectors"
            ]),
            (true, true)
        );
        // Not with the series that load their own analyzer.
        for other in [["--depth-sweep", "1"], ["--forks", "1"]] {
            for flag in ["--no-gap-detectors", "--file-times"] {
                let args = ["ryo-v2", "bench", "/b", other[0], other[1], flag];
                assert!(Cli::try_parse_from(args).is_err(), "{args:?}");
            }
        }
    }

    #[test]
    fn cli_parses_bench_depth_sweep() {
        let parse = |args: &[&str]| match Cli::try_parse_from(args)
            .map_err(|e| e.to_string())
            .expect("parses")
            .command
        {
            Command::Bench {
                base,
                variants,
                depth_sweep,
                depth_files,
                repeat,
                ..
            } => (base, variants, depth_sweep, depth_files, repeat),
            _ => panic!("not bench"),
        };
        let (base, variants, depths, files, repeat) = parse(&[
            "ryo-v2",
            "bench",
            "/b",
            "--depth-sweep",
            "1,5,10,20",
            "--repeat",
            "3",
        ]);
        assert_eq!(base, Some(PathBuf::from("/b")));
        assert!(variants.is_empty());
        assert_eq!(depths, vec![1, 5, 10, 20]);
        assert_eq!((files, repeat), (None, 3));
        let (_, _, depths, files, _) = parse(&[
            "ryo-v2",
            "bench",
            "/b",
            "--depth-sweep",
            "2",
            "--depth-files",
            "3",
        ]);
        assert_eq!((depths, files), (vec![2], Some(3)));
        // Without it a bench still needs its variants, and keeps parsing as before.
        let (_, variants, depths, files, _) = parse(&["ryo-v2", "bench", "/b", "/v"]);
        assert_eq!((variants.len(), depths.len(), files), (1, 0, None));
        assert!(Cli::try_parse_from(["ryo-v2", "bench", "/b"]).is_err());
        // A depth is at least 1; a list, not a range.
        for bad in ["0", "1,0", "x", "1-5"] {
            assert!(
                Cli::try_parse_from(["ryo-v2", "bench", "/b", "--depth-sweep", bad]).is_err(),
                "{bad}"
            );
        }
        // Not with variants, `--on` or `--large`; `--depth-files` only with it, and >= 1.
        assert!(
            Cli::try_parse_from(["ryo-v2", "bench", "/b", "/v", "--depth-sweep", "1"]).is_err()
        );
        assert!(
            Cli::try_parse_from(["ryo-v2", "bench", "/b", "--depth-sweep", "1", "--on", "/p"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from(["ryo-v2", "bench", "--large", "/x", "--depth-sweep", "1"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from(["ryo-v2", "bench", "/b", "/v", "--depth-files", "2"]).is_err()
        );
        assert!(Cli::try_parse_from(["ryo-v2", "bench", "/b", "--depth-files", "2"]).is_err());
        assert!(
            Cli::try_parse_from([
                "ryo-v2",
                "bench",
                "/b",
                "--depth-sweep",
                "1",
                "--depth-files",
                "0"
            ])
            .is_err()
        );
    }

    #[test]
    fn cli_parses_bench_forks() {
        let parse = |args: &[&str]| match Cli::try_parse_from(args)
            .map_err(|e| e.to_string())
            .expect("parses")
            .command
        {
            Command::Bench {
                base,
                variants,
                forks,
                fork_depth,
                fork_jobs,
                fork_files,
                fork_paths,
                fork_threads,
                fork_cut_off_ms,
                rss_stop_gib,
                fork_serial,
                repeat,
                ..
            } => (
                (base, variants, forks),
                (fork_depth, fork_jobs, fork_files, fork_paths),
                (
                    fork_threads,
                    fork_cut_off_ms,
                    rss_stop_gib,
                    fork_serial,
                    repeat,
                ),
            ),
            _ => panic!("not bench"),
        };
        let ((base, variants, forks), shape, rest) =
            parse(&["ryo-v2", "bench", "/b", "--forks", "1,4,8,11c,12,16"]);
        assert_eq!(base, Some(PathBuf::from("/b")));
        assert!(variants.is_empty());
        let labels: Vec<String> = forks.iter().map(|w| w.label()).collect();
        assert_eq!(labels, ["1", "4", "8", "11c", "12", "16"]);
        assert!(forks[3].caller_works && !forks[4].caller_works);
        // The defaults: depth 4, 12 jobs a level, the first 4 files, no cut-off, the reserve of
        // D3 (`RESERVED_RAM_GB`), no serial run.
        assert_eq!(shape, (4, 12, 4, Vec::new()));
        assert_eq!(rest, (None, None, 8.0, false, 1));
        let (_, shape, rest) = parse(&[
            "ryo-v2",
            "bench",
            "/b",
            "--forks",
            "12",
            "--fork-depth",
            "2",
            "--fork-jobs",
            "24",
            "--fork-paths",
            "crates/a/src/lib.rs,crates/b/src/main.rs",
            "--fork-threads",
            "16",
            "--fork-cut-off-ms",
            "5000",
            "--rss-stop-gib",
            "10.5",
            "--fork-serial",
            "--repeat",
            "2",
        ]);
        assert_eq!((shape.0, shape.1, shape.3.len()), (2, 24, 2));
        assert_eq!(rest, (Some(16), Some(5000), 10.5, true, 2));
        // A width is at least 1, a number with an optional `c`.
        for bad in ["0", "4,0", "x", "1-4", "4d"] {
            assert!(
                Cli::try_parse_from(["ryo-v2", "bench", "/b", "--forks", bad]).is_err(),
                "{bad}"
            );
        }
        // Not with variants, `--on`, `--large` or `--depth-sweep`.
        for extra in [&["/v"][..], &["--on", "/p"], &["--depth-sweep", "1"]] {
            let mut args = vec!["ryo-v2", "bench", "/b", "--forks", "4"];
            args.extend_from_slice(extra);
            assert!(Cli::try_parse_from(&args).is_err(), "{extra:?}");
        }
        assert!(Cli::try_parse_from(["ryo-v2", "bench", "--large", "/x", "--forks", "4"]).is_err());
        // Its options only with it; `--fork-files` and `--fork-paths` not together.
        for opt in [
            &["--fork-depth", "2"][..],
            &["--fork-jobs", "2"],
            &["--fork-files", "2"],
            &["--fork-paths", "a.rs"],
            &["--fork-threads", "2"],
            &["--fork-cut-off-ms", "5"],
            &["--rss-stop-gib", "4"],
            &["--fork-serial"],
        ] {
            let mut with_variants = vec!["ryo-v2", "bench", "/b", "/v"];
            with_variants.extend_from_slice(opt);
            assert!(Cli::try_parse_from(&with_variants).is_err(), "{opt:?}");
            let mut alone = vec!["ryo-v2", "bench", "/b"];
            alone.extend_from_slice(opt);
            assert!(Cli::try_parse_from(&alone).is_err(), "{opt:?}");
        }
        assert!(
            Cli::try_parse_from([
                "ryo-v2",
                "bench",
                "/b",
                "--forks",
                "4",
                "--fork-files",
                "2",
                "--fork-paths",
                "a.rs"
            ])
            .is_err()
        );
        for bad in [&["--fork-depth", "0"][..], &["--fork-jobs", "0"]] {
            let mut args = vec!["ryo-v2", "bench", "/b", "--forks", "4"];
            args.extend_from_slice(bad);
            assert!(Cli::try_parse_from(&args).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn cli_rejects_large_with_base() {
        assert!(Cli::try_parse_from(["ryo-v2", "bench", "b", "v", "--large", "/x"]).is_err());
    }

    #[test]
    fn cli_rejects_repeat_zero() {
        assert!(Cli::try_parse_from(["ryo-v2", "bench", "b", "v", "--repeat", "0"]).is_err());
    }

    #[test]
    fn cli_parses_apply() {
        let r = Cli::try_parse_from([
            "ryo-v2",
            "apply",
            "/ws",
            "b.json",
            "--scope",
            "workspace",
            "--no-proc-macro",
        ]);
        assert!(r.is_ok());
        assert!(
            Cli::try_parse_from(["ryo-v2", "apply", "/ws", "b.json", "--scope", "both"]).is_err()
        );
        assert!(Cli::try_parse_from(["ryo-v2", "apply", "/ws"]).is_err());
        // SEARCH_LEDGER D6: no registry of Rules, so no `--rules`.
        assert!(
            Cli::try_parse_from(["ryo-v2", "apply", "/ws", "b.json", "--rules", "r.json"]).is_err()
        );
    }

    #[test]
    fn cli_parses_search() {
        let r = Cli::try_parse_from([
            "ryo-v2",
            "search",
            "/ws",
            "req.json",
            "--scope",
            "workspace",
            "--no-proc-macro",
        ]);
        assert!(r.is_ok());
        assert!(
            Cli::try_parse_from(["ryo-v2", "search", "/ws", "r.json", "--scope", "both"]).is_err()
        );
        assert!(Cli::try_parse_from(["ryo-v2", "search", "/ws"]).is_err());
        // SEARCH_LEDGER D6: no registry of Rules, so no `--rules`.
        assert!(
            Cli::try_parse_from(["ryo-v2", "search", "/ws", "r.json", "--rules", "x.json"])
                .is_err()
        );
    }

    #[test]
    fn cli_parses_fingerprint_range() {
        let ok = Cli::try_parse_from(["ryo-v2", "fingerprint", "/ws", "src/lib.rs", "3..9"])
            .map_err(|e| e.to_string())
            .expect("parses");
        match ok.command {
            Command::Fingerprint { range, .. } => {
                let range = range.expect("a range");
                assert_eq!((range.start(), range.end()), (3, 9));
            }
            _ => panic!("not fingerprint"),
        }
        assert!(Cli::try_parse_from(["ryo-v2", "fingerprint", "/ws", "p", "3-9"]).is_err());
        let whole = Cli::try_parse_from(["ryo-v2", "fingerprint", "/ws", "src/lib.rs"])
            .map_err(|e| e.to_string())
            .expect("parses without a range");
        match whole.command {
            Command::Fingerprint { range, .. } => assert!(range.is_none()),
            _ => panic!("not fingerprint"),
        }
    }

    #[test]
    fn cli_rejects_reversed_range() {
        assert!(Cli::try_parse_from(["ryo-v2", "fingerprint", "/ws", "p", "9..3"]).is_err());
    }

    #[test]
    fn cli_requires_base_without_large() {
        assert!(Cli::try_parse_from(["ryo-v2", "bench"]).is_err());
    }
}
