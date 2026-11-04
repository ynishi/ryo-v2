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
use ryo_v2_core::domain::{Scope, TextRange};
use ryo_v2_host::bench::{self, BenchArgs, DepthArgs};
use ryo_v2_ra::Backend;
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
        #[arg(required_unless_present_any = ["large", "depth_sweep"])]
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
        /// Measure each VARIANT as a child of the PARENT overlay, two ways: `rebuilt` (the
        /// parent's and the variant's edits composed and verified from BASE) and `stacked` (the
        /// variant written as a level on top of the parent's level of one fork).
        #[arg(long, value_name = "PARENT")]
        on: Option<PathBuf>,
        /// Run the forks on layers of the fork Engine instead of the stacked undo log; every
        /// summary line ends with ` backend engine` (` backend undo-log` without it).
        #[arg(long)]
        engine: bool,
        /// Measure stacks of these depths instead of VARIANTS: per depth K, write K levels on one
        /// fork (level k appends `fn __ryo_depth_<k>() -> u32 { <k> }` to a workspace file),
        /// diagnose the top level `--repeat` times and pop back. A comma-separated list, each >= 1.
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
        /// The request JSON file: targets, probes, roots, budget, judge.
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
            on,
            engine,
            depth_sweep,
            depth_files,
        } => {
            let scopes = match scope {
                ScopeArg::Affected => vec![Scope::Affected],
                ScopeArg::Workspace => vec![Scope::Workspace],
                ScopeArg::Both => vec![Scope::Affected, Scope::Workspace],
            };
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
                    backend: Backend::of_engine(engine),
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
                backend: Backend::of_engine(engine),
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

    #[test]
    fn cli_parses_bench_engine() {
        let engine = |args: &[&str]| match Cli::try_parse_from(args)
            .map_err(|e| e.to_string())
            .expect("parses")
            .command
        {
            Command::Bench { engine, .. } => engine,
            _ => panic!("not bench"),
        };
        assert!(!engine(&["ryo-v2", "bench", "/b", "/v"]));
        assert!(engine(&["ryo-v2", "bench", "/b", "/v", "--engine"]));
        assert!(engine(&[
            "ryo-v2", "bench", "/b", "/v", "--on", "/p", "--repeat", "2", "--engine"
        ]));
        assert!(engine(&["ryo-v2", "bench", "--large", "/x", "--engine"]));
        // A flag, not an option with a value; and not a flag of the other commands.
        assert!(Cli::try_parse_from(["ryo-v2", "bench", "/b", "/v", "--engine=true"]).is_err());
        assert!(Cli::try_parse_from(["ryo-v2", "apply", "/ws", "b.json", "--engine"]).is_err());
        // No second command for the Engine: `--engine` is how a bench runs on it.
        assert!(Cli::try_parse_from(["ryo-v2", "bench-cow", "/b", "/v"]).is_err());
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
                engine,
                repeat,
                ..
            } => (base, variants, depth_sweep, depth_files, engine, repeat),
            _ => panic!("not bench"),
        };
        let (base, variants, depths, files, engine, repeat) = parse(&[
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
        assert_eq!((files, engine, repeat), (None, false, 3));
        let (_, _, depths, files, engine, _) = parse(&[
            "ryo-v2",
            "bench",
            "/b",
            "--depth-sweep",
            "2",
            "--depth-files",
            "3",
            "--engine",
        ]);
        assert_eq!((depths, files, engine), (vec![2], Some(3), true));
        // Without it a bench still needs its variants, and keeps parsing as before.
        let (_, variants, depths, files, _, _) = parse(&["ryo-v2", "bench", "/b", "/v"]);
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
