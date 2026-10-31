//! Error diagnostics out of rust-analyzer, as domain [`Finding`]s.

use crate::session::DetectorPanic;
use crate::unresolved::{self, Hit};
use crate::{copy_impl, duplicate};
use ra_ap_base_db::salsa::Cancelled;
use ra_ap_hir::PrefixKind;
use ra_ap_ide::{
    Analysis, AnalysisHost, AssistResolveStrategy, DiagnosticCode, DiagnosticsConfig,
    ExprFillDefaultMode, Severity,
};
use ra_ap_ide::{TextRange, TextSize};
use ra_ap_ide_db::imports::insert_use::{ImportGranularity, InsertUseConfig};
use ra_ap_ide_db::{RootDatabase, SnippetCap};
use ra_ap_vfs::FileId;
use ryo_v2_core::app::ports::{AnalyzerError, Clock};
use ryo_v2_core::domain::{ErrorCode, Finding, FindingCode, RelPath};
use std::collections::BTreeSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Maps a rust-analyzer code to an E-code where it is one.
pub(crate) fn classify(code: &DiagnosticCode) -> FindingCode {
    if let DiagnosticCode::RustcHardError(s) = code
        && let Ok(c) = ErrorCode::parse(s)
    {
        return FindingCode::Rustc(c);
    }
    FindingCode::Analyzer(code.as_str().to_owned())
}

/// Text of a caught panic payload.
pub(crate) fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic".to_owned()
    }
}

/// The diagnostics configuration, spelled out field by field. The values equal 0.0.358's
/// `DiagnosticsConfig::test_sample()` (which the measured oracle counts were taken with); a test
/// pins that. Spelling them out keeps a test helper out of production code.
pub(crate) fn diagnostics_config() -> DiagnosticsConfig {
    DiagnosticsConfig {
        // Diagnostics on.
        enabled: true,
        // No handler in 0.0.358 reads this; whether ra expands proc macros is decided at load
        // by `ProcMacroServerChoice`.
        proc_macros_enabled: false,
        // Same: no handler reads it; attribute proc macros are decided by load-cargo.
        proc_attr_macros_enabled: false,
        // Keep experimental diagnostics: the measured oracle counts were taken with them.
        disable_experimental: false,
        // No diagnostic is switched off by name.
        disabled: Default::default(),
        // Only shapes fixes, which are not resolved (`AssistResolveStrategy::None`).
        expr_fill_default: ExprFillDefaultMode::Todo,
        // Style lints are warnings; they are filtered out by severity anyway.
        style_lints: true,
        // Only shapes fixes.
        snippet_cap: SnippetCap::new(true),
        // Only shapes fixes (import insertion style).
        insert_use: InsertUseConfig {
            granularity: ImportGranularity::Item,
            enforce_granularity: false,
            prefix_kind: PrefixKind::Plain,
            group: false,
            skip_glob_imports: false,
        },
        // Only shapes fixes.
        prefer_no_std: false,
        // Only shapes fixes.
        prefer_prelude: true,
        // Only shapes fixes.
        prefer_absolute: false,
        // Only shapes fixes (term search budget).
        term_search_fuel: 400,
        // Only shapes fixes (rename conflicts).
        show_rename_conflicts: true,
    }
}

/// A point in time after which diagnostics stop. Cooperative: it is looked at before each file,
/// so one slow file can overrun it.
pub(crate) struct Deadline {
    clock: Arc<dyn Clock>,
    at_ms: u64,
    hit: AtomicBool,
}

impl Deadline {
    /// A deadline `limit_ms` after now on `clock`.
    pub(crate) fn after(clock: Arc<dyn Clock>, limit_ms: u64) -> Self {
        let at_ms = clock.now_ms().saturating_add(limit_ms);
        Self {
            clock,
            at_ms,
            hit: AtomicBool::new(false),
        }
    }

    /// Whether the deadline is reached; remembers that it was.
    fn expired(&self) -> bool {
        let over = self.clock.now_ms() >= self.at_ms;
        if over {
            self.hit.store(true, Ordering::Relaxed);
        }
        over
    }

    /// Whether some diagnostics stopped because of this deadline.
    pub(crate) fn was_hit(&self) -> bool {
        self.hit.load(Ordering::Relaxed)
    }
}

/// One detector of an error rust-analyzer 0.0.358 does not report: the rustc code, message and
/// line of each error it finds in a file, given the cfg-disabled ranges and the line of an offset.
type Detect = fn(&RootDatabase, FileId, &[TextRange], &dyn Fn(TextSize) -> u32) -> Vec<Hit>;

/// Name of the unresolved-path detector, the one the test fault hook targets.
const UNRESOLVED: &str = "unresolved-path";

/// The detectors a collect runs when it has a database, in this order: unresolved type names and
/// path prefixes (E0405, E0425, E0433), names an item and a `use` both define (E0255), and `Copy`
/// impls on types with a field that is not `Copy` (E0204).
const DETECTORS: [(&str, Detect); 3] = [
    (UNRESOLVED, unresolved::find),
    ("duplicate-name", duplicate::find),
    ("copy-impl", copy_impl::find),
];

/// What a collect needs to run, and to report on, the detectors of [`DETECTORS`].
#[derive(Clone, Copy)]
pub(crate) struct Detector<'a> {
    /// File on which the unresolved-path detector is made to panic (test hook).
    pub(crate) fault: Option<&'a RelPath>,
    /// Where a contained detector panic is recorded (a detector other than the unresolved-path
    /// one prefixes its name to the message).
    pub(crate) panics: &'a Mutex<BTreeSet<DetectorPanic>>,
}

/// Error diagnostics of the files, sorted. With `db`, the errors ra does not diagnose are added
/// by the [`DETECTORS`] ([`crate::unresolved`], [`crate::duplicate`], [`crate::copy_impl`]). With `deadline`, stops with `Cancelled`
/// before the first file it finds the deadline reached.
fn collect_errors(
    analysis: &Analysis,
    db: Option<&RootDatabase>,
    files: &[(FileId, RelPath)],
    deadline: Option<&Deadline>,
    detector: Detector<'_>,
) -> Result<Vec<Finding>, AnalyzerError> {
    let diag_config = diagnostics_config();
    let mut errors = Vec::new();
    for (file_id, rel) in files {
        if deadline.is_some_and(Deadline::expired) {
            return Err(AnalyzerError::Cancelled);
        }
        let got = catch_unwind(AssertUnwindSafe(|| {
            let diags = analysis
                .full_diagnostics(&diag_config, AssistResolveStrategy::None, *file_id)
                .map_err(|_| ())?;
            let index = analysis.file_line_index(*file_id).map_err(|_| ())?;
            let mut found = Vec::new();
            let mut ra_keys: Vec<(FindingCode, u32)> = Vec::new();
            let mut inactive: Vec<TextRange> = Vec::new();
            for d in diags {
                if matches!(d.code, DiagnosticCode::RaLint("inactive_code", _))
                    && d.range.file_id == *file_id
                {
                    inactive.push(d.range.range);
                }
                if d.severity == Severity::Error {
                    let code = classify(&d.code);
                    let line = index.line_col(d.range.range.start()).line + 1;
                    ra_keys.push((code.clone(), line));
                    found.push(Finding {
                        code,
                        message: d.message,
                        path: rel.clone(),
                        line,
                    });
                }
            }
            if let Some(db) = db {
                let line_of = |at: TextSize| index.line_col(at).line + 1;
                for (name, run) in DETECTORS {
                    // Each detector is contained on its own: a panic in one costs this file
                    // that detector's findings, not the others' and not the collect.
                    let hits = catch_unwind(AssertUnwindSafe(|| {
                        if name == UNRESOLVED && detector.fault == Some(rel) {
                            panic!("unresolved-path detector fault injected on {rel}");
                        }
                        run(db, *file_id, &inactive, &line_of)
                    }))
                    .unwrap_or_else(|payload| {
                        // salsa 0.28 raises its cancellation with `resume_unwind`; it is not a
                        // detector fault, so it goes on to the outer catch as before.
                        if payload.downcast_ref::<Cancelled>().is_some() {
                            std::panic::resume_unwind(payload);
                        }
                        let message = panic_message(payload);
                        detector
                            .panics
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(DetectorPanic {
                                path: rel.clone(),
                                message: if name == UNRESOLVED {
                                    message
                                } else {
                                    format!("{name} detector: {message}")
                                },
                            });
                        Vec::new()
                    });
                    for (code, message, line) in hits {
                        // The same code on the same line: ra already says it.
                        if !ra_keys.contains(&(code.clone(), line)) {
                            found.push(Finding {
                                code,
                                message,
                                path: rel.clone(),
                                line,
                            });
                        }
                    }
                }
            }
            Ok::<_, ()>(found)
        }))
        .map_err(|p| AnalyzerError::Panicked(panic_message(p)))?;
        errors.extend(got.map_err(|()| AnalyzerError::Cancelled)?);
    }
    errors.sort();
    Ok(errors)
}

/// Parallel [`collect_errors`]: chunks the files and diagnoses each chunk on its own snapshot.
/// `unresolved` switches the [`DETECTORS`] on; each chunk then also takes
/// its own database clone, dropped before this function returns.
pub(crate) fn collect_errors_par(
    host: &AnalysisHost,
    files: &[(FileId, RelPath)],
    threads: usize,
    unresolved: bool,
    deadline: Option<&Deadline>,
    detector: Detector<'_>,
) -> Result<Vec<Finding>, AnalyzerError> {
    if threads <= 1 || files.len() < 8 {
        let db = unresolved.then(|| host.raw_database().clone());
        return collect_errors(&host.analysis(), db.as_ref(), files, deadline, detector);
    }
    let chunk_size = files.len().div_ceil(threads);
    let chunks: Vec<&[(FileId, RelPath)]> = files.chunks(chunk_size).collect();
    let snapshots: Vec<(Analysis, Option<RootDatabase>)> = (0..chunks.len())
        .map(|_| {
            (
                host.analysis(),
                unresolved.then(|| host.raw_database().clone()),
            )
        })
        .collect();
    let mut errors = Vec::new();
    std::thread::scope(|s| -> Result<(), AnalyzerError> {
        let handles: Vec<_> = chunks
            .into_iter()
            .zip(snapshots)
            .map(|(chunk, (analysis, db))| {
                s.spawn(move || collect_errors(&analysis, db.as_ref(), chunk, deadline, detector))
            })
            .collect();
        for h in handles {
            let got = h
                .join()
                .map_err(|p| AnalyzerError::Panicked(panic_message(p)))?;
            errors.extend(got?);
        }
        Ok(())
    })?;
    errors.sort();
    Ok(errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_maps_rustc_hard_error() {
        let got = classify(&DiagnosticCode::RustcHardError("E0599"));
        assert_eq!(got, FindingCode::Rustc(ErrorCode::parse("E0599").unwrap()));
    }

    #[test]
    fn classify_keeps_ra_codes_unmapped() {
        assert_eq!(
            classify(&DiagnosticCode::RustcHardError("typed-hole")),
            FindingCode::Analyzer("typed-hole".to_owned())
        );
        assert!(matches!(
            classify(&DiagnosticCode::SyntaxError),
            FindingCode::Analyzer(_)
        ));
        assert_eq!(
            classify(&DiagnosticCode::Ra("bad-rtn", Severity::Error)),
            FindingCode::Analyzer("bad-rtn".to_owned())
        );
    }

    #[test]
    fn diagnostics_config_equals_test_sample() {
        assert_eq!(
            format!("{:?}", diagnostics_config()),
            format!("{:?}", DiagnosticsConfig::test_sample())
        );
    }
}
