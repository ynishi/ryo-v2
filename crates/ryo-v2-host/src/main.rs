//! The `ryo-v2` command: parses arguments, calls
//! `ryo-v2-core`, prints the result.
//!
//! The argument parser's definition (clap's derive types) is the only
//! description of the commands: help, the command list and validation come
//! from it, not from a second hand-kept table.

mod apply;
mod bench;
mod blueprint_json;
mod resolution_json;
mod rss;
mod rules_json;
mod search;
mod search_json;

use anyhow::Result;
use bench::BenchArgs;
use clap::{Parser, Subcommand};
use ryo_v2_core::domain::{Scope, TextRange};
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
        #[arg(required_unless_present = "large")]
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
    },
    /// Verify BLUEPRINTS against the workspace and print the Resolution as JSON (a dry run: nothing is written).
    Apply {
        /// The workspace to load.
        workspace: PathBuf,
        /// The blueprints JSON file.
        blueprints: PathBuf,
        /// Rules JSON file; without it every candidate needs review.
        #[arg(long, value_name = "FILE")]
        rules: Option<PathBuf>,
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
        /// Rules JSON file; without it every candidate needs review.
        #[arg(long, value_name = "FILE")]
        rules: Option<PathBuf>,
        /// How far each check reaches.
        #[arg(long, value_enum, default_value_t = ApplyScopeArg::Affected)]
        scope: ApplyScopeArg,
        /// Do not start the proc-macro server.
        #[arg(long)]
        no_proc_macro: bool,
    },
    /// Print the `reads` entry (path, range, fingerprint) of a byte range of a file, read from disk.
    Fingerprint {
        /// The workspace root.
        workspace: PathBuf,
        /// The file, relative to the workspace.
        path: String,
        /// Byte range `START..END`.
        #[arg(value_parser = parse_range)]
        range: TextRange,
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
            rules,
            scope,
            no_proc_macro,
        } => apply::run_apply(&apply::ApplyArgs {
            workspace,
            blueprints,
            rules,
            scope: match scope {
                ApplyScopeArg::Affected => Scope::Affected,
                ApplyScopeArg::Workspace => Scope::Workspace,
            },
            proc_macro: !no_proc_macro,
        }),
        Command::Search {
            workspace,
            request,
            rules,
            scope,
            no_proc_macro,
        } => search::run_search(&search::SearchArgs {
            workspace,
            request,
            rules,
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
        } => {
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
            let scopes = match scope {
                ScopeArg::Affected => vec![Scope::Affected],
                ScopeArg::Workspace => vec![Scope::Workspace],
                ScopeArg::Both => vec![Scope::Affected, Scope::Workspace],
            };
            bench::run(&BenchArgs {
                base,
                variants,
                repeat,
                scopes,
                proc_macro: !no_proc_macro,
                assert_revert: !no_assert_revert,
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
            "--rules",
            "r.json",
            "--scope",
            "workspace",
            "--no-proc-macro",
        ]);
        assert!(r.is_ok());
        assert!(
            Cli::try_parse_from(["ryo-v2", "apply", "/ws", "b.json", "--scope", "both"]).is_err()
        );
        assert!(Cli::try_parse_from(["ryo-v2", "apply", "/ws"]).is_err());
    }

    #[test]
    fn cli_parses_search() {
        let r = Cli::try_parse_from([
            "ryo-v2",
            "search",
            "/ws",
            "req.json",
            "--rules",
            "r.json",
            "--scope",
            "workspace",
            "--no-proc-macro",
        ]);
        assert!(r.is_ok());
        assert!(
            Cli::try_parse_from(["ryo-v2", "search", "/ws", "r.json", "--scope", "both"]).is_err()
        );
        assert!(Cli::try_parse_from(["ryo-v2", "search", "/ws"]).is_err());
    }

    #[test]
    fn cli_parses_fingerprint_range() {
        let ok = Cli::try_parse_from(["ryo-v2", "fingerprint", "/ws", "src/lib.rs", "3..9"])
            .map_err(|e| e.to_string())
            .expect("parses");
        match ok.command {
            Command::Fingerprint { range, .. } => {
                assert_eq!((range.start(), range.end()), (3, 9));
            }
            _ => panic!("not fingerprint"),
        }
        assert!(Cli::try_parse_from(["ryo-v2", "fingerprint", "/ws", "p", "3-9"]).is_err());
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
