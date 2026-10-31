//! The `ryo-v2` command: parses arguments, calls
//! `ryo-v2-core`, prints the result.
//!
//! The argument parser's definition (clap's derive types) is the only
//! description of the commands: help, the command list and validation come
//! from it, not from a second hand-kept table.
#![warn(missing_docs)]

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use ryo_v2_core::app::ports::{Analyzer, AnalyzerError, Event};
use ryo_v2_core::app::verify::verify;
use ryo_v2_core::domain::Scope;
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_core::infra::overlay::overlay_edits;
use ryo_v2_ra::{RaAnalyzer, RaConfig};
use std::path::PathBuf;
use std::sync::Arc;

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
        base: PathBuf,
        /// Overlay dirs; each file that differs from (or is missing in) BASE is an edit.
        #[arg(required = true)]
        variants: Vec<PathBuf>,
        /// Do not start the proc-macro server (derives stay unexpanded: many false positives).
        #[arg(long)]
        no_proc_macro: bool,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Bench {
            base,
            variants,
            no_proc_macro,
        } => bench(&base, &variants, !no_proc_macro),
    }
}

fn bench(base: &std::path::Path, variants: &[PathBuf], proc_macro: bool) -> Result<()> {
    let observer = Arc::new(RecordingObserver::default());
    let analyzer = RaAnalyzer::new(
        RaConfig {
            proc_macro,
            threads: None,
            assert_revert: true,
        },
        Arc::new(MonotonicClock::new()),
        observer.clone(),
    );
    let mut session = analyzer.load(base)?;
    for event in observer.take() {
        if let Event::Loaded {
            load_ms,
            baseline_ms,
            files,
            baseline_errors,
        } = event
        {
            eprintln!("[load] {load_ms} ms, {files} local files");
            eprintln!(
                "[baseline] {baseline_errors} pre-existing errors in {baseline_ms} ms (cold)"
            );
        }
    }

    let mut fail_infra = 0usize;
    for variant in variants {
        let edits = overlay_edits(base, variant)
            .with_context(|| format!("overlay {}", variant.display()))?;
        let name = variant.display();
        if edits.is_empty() {
            println!("{name}: SKIP (no edits vs base)");
            continue;
        }
        let n_edits = edits.len();
        observer.take();
        let result = verify(&analyzer, &mut session, &edits, Scope::Affected);
        let (mut scope, mut total, mut check_ms, mut revert_ms, mut revert_ok) = (0, 0, 0, 0, None);
        for event in observer.take() {
            match event {
                Event::Checked {
                    scope_files,
                    total_files,
                    check_ms: c,
                } => (scope, total, check_ms) = (scope_files, total_files, c),
                Event::Reverted {
                    revert_ms: r,
                    revert_ok: ok,
                } => (revert_ms, revert_ok) = (r, ok),
                Event::Loaded { .. } => {}
            }
        }
        let verdict = match result {
            Ok(v) => Some(v),
            Err(AnalyzerError::RevertMismatch) => {
                fail_infra += 1;
                None
            }
            Err(e) => return Err(e.into()),
        };
        let passed = verdict.as_ref().map(|v| v.passed);
        println!(
            "{name}: {} ({n_edits} edited, scope {scope}/{total} files, check {check_ms} ms, revert {revert_ms} ms, revert_ok={revert_ok:?})",
            match passed {
                Some(true) => "PASS",
                Some(false) => "FAIL",
                None => "FAIL",
            },
        );
        if let Some(v) = verdict {
            for d in &v.new_errors.errors {
                println!("  + [{}] {}:{}", d.code, d.path, d.line);
            }
            for d in &v.new_errors.unmapped {
                println!("  + [ra:{}] {}:{}", d.ra_code, d.path, d.line);
            }
        }
    }
    if fail_infra > 0 {
        anyhow::bail!("{fail_infra} variant(s) failed revert assertion");
    }
    Ok(())
}
