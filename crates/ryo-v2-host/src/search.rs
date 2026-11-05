//! `ryo-v2 search`: the search of one decision as a dry run (nothing is written).

use crate::resolution_json::exit_code;
use crate::search_json::{JudgeSpec, Request, parse_replay, parse_request, report_to_json};
use anyhow::{Context, Result};
use ryo_v2_core::app::memory::MemoryBudget;
use ryo_v2_core::app::ports::{Analyzer, Judge};
use ryo_v2_core::app::search::{ASSERT_REVERT_EVERY, SEARCH_FORKS, SearchInput, search_decision};
use ryo_v2_core::domain::{DEFAULT_WEIGHTS, Scope};
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::judge::MetricJudge;
use ryo_v2_core::infra::memory::{ProcGauge, budget_at_start};
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_ra::{ForksConfig, RaAnalyzer, RaConfig, SyntaxAssessor};
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

/// Arguments of one `search` invocation.
pub(crate) struct SearchArgs {
    pub(crate) workspace: PathBuf,
    pub(crate) request: PathBuf,
    pub(crate) scope: Scope,
    pub(crate) proc_macro: bool,
}

/// Searches the workspace as the request says and prints the `SearchReport` as JSON. Writes
/// nothing to disk. Exit code 0 for `Decided`, 2 for `Failed`; an error (no candidate at all
/// included) is 1.
pub(crate) fn run_search(args: &SearchArgs) -> Result<ExitCode> {
    // JSON errors surface before the (slow) load.
    let text = std::fs::read_to_string(&args.request)
        .with_context(|| format!("read request {}", args.request.display()))?;
    let request = parse_request(&text)?;
    for (index, bp) in request.roots.iter().enumerate() {
        if request.generated.contains(&bp.id()) {
            eprintln!("generated id {} for root {index}", bp.id());
        }
    }
    match &request.judge {
        JudgeSpec::Metric => search_with(args, &request, &MetricJudge),
        JudgeSpec::Replay(file) => {
            let fixture = std::fs::read_to_string(file)
                .with_context(|| format!("read replay fixture {file}"))?;
            search_with(args, &request, &parse_replay(&fixture)?)
        }
    }
}

fn search_with<J: Judge>(args: &SearchArgs, request: &Request, judge: &J) -> Result<ExitCode> {
    let clock = Arc::new(MonotonicClock::new());
    let analyzer = RaAnalyzer::new(
        RaConfig {
            proc_macro: args.proc_macro,
            threads: None,
            assert_revert: false,
            gap_detectors: true,
            assert_revert_every: NonZeroU32::new(ASSERT_REVERT_EVERY),
            cfg_test: true,
            // The search's forks (D11, stage 4): `SEARCH_FORKS` jobs at once, and the caller
            // only waits while they run (c = 0).
            forks: ForksConfig {
                width: NonZeroUsize::new(SEARCH_FORKS),
                caller_works: false,
            },
            ..Default::default()
        },
        clock.clone(),
        Arc::new(RecordingObserver::default()),
    )
    .with_check_timeout(Some(request.budget.per_candidate_ms));
    let mut session = analyzer
        .load(&args.workspace)
        .with_context(|| format!("load {}", args.workspace.display()))?;
    let input = SearchInput {
        start: &[],
        roots: &request.roots,
        targets: &request.targets,
        probes: &request.probes,
        scope: args.scope,
        budget: request.budget,
        // The RSS may grow by what the machine has available now, less the reserve; where
        // that cannot be read the stop never fires, as a gauge that cannot read.
        memory: budget_at_start().unwrap_or_else(|e| {
            eprintln!("no memory budget for the search: {e}");
            MemoryBudget::unbounded()
        }),
        weights: DEFAULT_WEIGHTS,
        keep_trace_edits: false,
    };
    let report = search_decision(
        &analyzer,
        &SyntaxAssessor,
        judge,
        &*clock,
        &ProcGauge,
        &mut session,
        &input,
    )?;
    println!(
        "{}",
        serde_json::to_string_pretty(&report_to_json(&report))?
    );
    Ok(ExitCode::from(exit_code(&report.resolution)))
}
