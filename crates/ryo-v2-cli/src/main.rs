//! The `ryo-v2` command: parses arguments, calls
//! `ryo-v2-core`, prints the result.
//!
//! The argument parser's definition (clap's derive types) is the only
//! description of the commands: help, the command list and validation come
//! from it, not from a second hand-kept table.
#![warn(missing_docs)]

mod bench;
mod rss;

use anyhow::Result;
use bench::BenchArgs;
use clap::{Parser, Subcommand};
use ryo_v2_core::domain::Scope;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "ryo-v2", version, about)]
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum ScopeArg {
    Affected,
    Workspace,
    Both,
}

fn main() -> Result<()> {
    match Cli::parse().command {
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn cli_requires_base_without_large() {
        assert!(Cli::try_parse_from(["ryo-v2", "bench"]).is_err());
    }
}
