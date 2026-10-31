//! `ryo-v2 apply` (a dry run: nothing is written) and `ryo-v2 fingerprint`.

use crate::blueprint_json::parse_blueprints;
use crate::resolution_json::{exit_code, resolution_to_json};
use crate::rules_json::parse_rules;
use anyhow::{Context, Result};
use ryo_v2_core::app::apply::apply_blueprints;
use ryo_v2_core::app::ports::Analyzer;
use ryo_v2_core::domain::{BaseFp, ReadRange, RelPath, RuleCatalog, Scope, TextRange};
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_ra::{RaAnalyzer, RaConfig};
use serde::Serialize;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

/// Arguments of one `apply` invocation.
pub(crate) struct ApplyArgs {
    pub(crate) workspace: PathBuf,
    pub(crate) blueprints: PathBuf,
    pub(crate) rules: Option<PathBuf>,
    pub(crate) scope: Scope,
    pub(crate) proc_macro: bool,
}

/// Verifies the blueprints against the workspace and prints the `Resolution` as JSON. Writes
/// nothing to disk.
pub(crate) fn run_apply(args: &ApplyArgs) -> Result<ExitCode> {
    // JSON errors surface before the (slow) load.
    let rules = match &args.rules {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("read rules {}", path.display()))?;
            parse_rules(&text)?
        }
        None => RuleCatalog::default(),
    };
    let text = std::fs::read_to_string(&args.blueprints)
        .with_context(|| format!("read blueprints {}", args.blueprints.display()))?;
    let parsed = parse_blueprints(&text)?;
    for (index, bp) in parsed.set.iter().enumerate() {
        if parsed.generated.contains(&bp.id()) {
            eprintln!("generated id {} for blueprint {index}", bp.id());
        }
    }

    let analyzer = RaAnalyzer::new(
        RaConfig {
            proc_macro: args.proc_macro,
            threads: None,
            assert_revert: false,
            gap_detectors: true,
            assert_revert_every: None,
            cfg_test: true,
        },
        Arc::new(MonotonicClock::new()),
        Arc::new(RecordingObserver::default()),
    );
    let mut session = analyzer
        .load(&args.workspace)
        .with_context(|| format!("load {}", args.workspace.display()))?;
    let resolution = apply_blueprints(&analyzer, &mut session, &parsed.set, &rules, args.scope)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&resolution_to_json(&resolution))?
    );
    Ok(ExitCode::from(exit_code(&resolution)))
}

#[derive(Serialize)]
struct FingerprintLine {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    start: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end: Option<u32>,
    fingerprint: String,
}

/// Prints the `reads` entry for `range` of `<workspace>/<path>`, read from the disk; without a
/// range, the `bases` entry for the whole file.
///
/// # Contract
/// - C1: with a range it prints `{path, start, end, fingerprint}` of those bytes; without one it
///   prints `{path, fingerprint}` of the whole file, the value `Fingerprint::of` gives for the
///   file's text.
///   tested: tests/apply.rs fingerprint_prints_shared_vector
///   tested: tests/apply.rs fingerprint_without_range_is_whole_file
pub(crate) fn run_fingerprint(
    workspace: &std::path::Path,
    path: &str,
    range: Option<TextRange>,
) -> Result<ExitCode> {
    let rel = RelPath::parse(path).map_err(|e| anyhow::anyhow!("path: {e}"))?;
    let file = workspace.join(rel.as_str());
    let text =
        std::fs::read_to_string(&file).with_context(|| format!("read {}", file.display()))?;
    let line = match range {
        Some(range) => {
            let read =
                ReadRange::capture(rel, &text, range).map_err(|e| anyhow::anyhow!("range: {e}"))?;
            FingerprintLine {
                path: read.path().to_string(),
                start: Some(read.range().start()),
                end: Some(read.range().end()),
                fingerprint: read.fingerprint().to_string(),
            }
        }
        None => {
            let base = BaseFp::capture(rel, &text);
            FingerprintLine {
                path: base.path().to_string(),
                start: None,
                end: None,
                fingerprint: base.fingerprint().to_string(),
            }
        }
    };
    println!("{}", serde_json::to_string(&line)?);
    Ok(ExitCode::SUCCESS)
}
