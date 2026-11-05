//! Error diagnostics out of rust-analyzer, as domain [`Finding`]s.

use crate::session::DetectorPanic;
use crate::unresolved::{self, Hit};
use crate::{duplicate, field_bounds};
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
use ryo_v2_core::domain::{ErrorCode, Finding, FindingCode, RelPath, UnseenClass};
use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

/// One detector of an error rust-analyzer 0.0.358 does not report (a gap detector). All of them
/// run when [`crate::RaConfig::gap_detectors`] is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GapDetector {
    /// Unresolved type names and path prefixes (E0405, E0425, E0433), `unresolved.rs`.
    ///
    /// # Contract
    /// - C1: on a file where rust-analyzer reports a macro call it could not expand, this
    ///   detector reports nothing.
    ///   tested: tests/unresolved.rs build_script_include_types_are_not_reported_without_out_dir
    ///   tested: tests/unresolved.rs function_like_proc_macro_types_are_not_reported_without_the_server
    ///   tested: diagnostics::tests::unexpanded_macro_codes
    /// - C2: the skip is per file: other files keep their findings, and a file whose macro calls
    ///   all expand is not skipped.
    ///   tested: tests/unresolved.rs unresolved_names_outside_an_unexpanded_macro_file_are_still_reported
    ///   tested: tests/unresolved.rs a_file_whose_macros_expand_keeps_its_findings
    /// - C3: a diagnostics run that skips a file by C1 names `UnexpandedMacro` in the `unseen_at`
    ///   of its findings, with that file; a run that skips none, or that runs no gap detector,
    ///   does not.
    ///   tested: tests/unresolved.rs unexpanded_macro_file_names_the_class
    ///   tested: tests/stack.rs a_diagnosis_names_where_it_observed_each_unseen_class
    UnresolvedPath,
    /// A name an item and a `use` both define (E0255), `duplicate.rs`.
    DuplicateName,
    /// The trait a field owes to a derived impl or a written `Copy` impl (E0204, E0277, E0369), `field_bounds.rs`.
    FieldBounds,
}

impl GapDetector {
    /// Every gap detector, in the order a collect runs them.
    pub const ALL: [GapDetector; 3] = [
        GapDetector::UnresolvedPath,
        GapDetector::DuplicateName,
        GapDetector::FieldBounds,
    ];

    /// Whether this detector is skipped on a file where rust-analyzer reports a macro call it
    /// could not expand. Only the unresolved-path detector is: a name the missing expansion would
    /// define cannot be told from a missing one, so it would report a false error. The other two
    /// only lose findings, never gain false ones, from an item that is missing.
    ///
    /// # Contract
    /// - C1: on a file with a macro call rust-analyzer could not expand, the duplicate-name and
    ///   field-bounds detectors still report.
    ///   tested: tests/gap_detectors.rs unexpanded_macro_file_keeps_the_other_detectors
    pub fn skips_files_with_unexpanded_macros(self) -> bool {
        matches!(self, GapDetector::UnresolvedPath)
    }

    /// "unresolved-path", "duplicate-name", "field-bounds".
    pub fn name(self) -> &'static str {
        match self {
            GapDetector::UnresolvedPath => "unresolved-path",
            GapDetector::DuplicateName => "duplicate-name",
            GapDetector::FieldBounds => "field-bounds",
        }
    }

    /// The class of error this detector reports, named as unseen where the detector cannot run.
    ///
    /// # Contract
    /// - C1: `UnresolvedPath`, `DuplicateName` and `FieldBounds` give the `UnseenClass` of the
    ///   same name.
    ///   tested: diagnostics::tests::gap_detector_unseen_class_is_its_own
    pub fn unseen_class(self) -> UnseenClass {
        match self {
            GapDetector::UnresolvedPath => UnseenClass::UnresolvedPath,
            GapDetector::DuplicateName => UnseenClass::DuplicateName,
            GapDetector::FieldBounds => UnseenClass::FieldBounds,
        }
    }
}

impl std::fmt::Display for GapDetector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// The detectors a collect runs when it has a database, in this order: unresolved type names and
/// path prefixes (E0405, E0425, E0433), names an item and a `use` both define (E0255), and the
/// trait a field owes to a derived impl or a written `Copy` impl (E0204, E0277, E0369).
const DETECTORS: [(GapDetector, Detect); 3] = [
    (GapDetector::UnresolvedPath, unresolved::find),
    (GapDetector::DuplicateName, duplicate::find),
    (GapDetector::FieldBounds, field_bounds::find),
];

/// The error of a payload caught around a diagnosis: a salsa cancellation is `Cancelled` (the
/// check could not finish: its database was cancelled), never a panic; a cancellation salsa
/// raises because a query another thread was computing panicked (`PropagatedPanic`) is that
/// panic; any other payload is `Panicked` with its message.
fn caught(payload: Box<dyn std::any::Any + Send>) -> AnalyzerError {
    match payload.downcast::<Cancelled>() {
        Ok(cancelled) => match *cancelled {
            Cancelled::PropagatedPanic => AnalyzerError::Panicked(cancelled.to_string()),
            _ => AnalyzerError::Cancelled,
        },
        Err(payload) => AnalyzerError::Panicked(panic_message(payload)),
    }
}

/// What a collect needs to run, and to report on, the detectors of [`DETECTORS`].
#[derive(Clone, Copy)]
pub(crate) struct Detector<'a> {
    /// Detector and file on which that detector is made to panic, or to raise a salsa
    /// cancellation (test hook).
    pub(crate) fault: Option<&'a crate::DetectorFault>,
    /// Where a contained detector panic is recorded.
    pub(crate) panics: &'a Mutex<BTreeSet<DetectorPanic>>,
    /// Counts the threads diagnosing (test hook `RaAnalyzer::with_thread_count`).
    pub(crate) probe: Option<&'a ThreadCount>,
}

/// The threads that diagnose files at once, over every collect of an analyzer, and the most
/// threads one collect spread over: what a test of the thread budget of `Forks` reads
/// (`RaAnalyzer::with_thread_count`). Only [`collect_errors_par`] spawns threads in a diagnosis,
/// so it is counted there.
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct ThreadCount {
    now: AtomicUsize,
    most: AtomicUsize,
    most_in_one: AtomicUsize,
}

impl ThreadCount {
    /// The most threads that diagnosed files at once.
    pub fn most(&self) -> usize {
        self.most.load(Ordering::SeqCst)
    }

    /// The most threads one diagnosis spread its files over.
    pub fn most_in_one(&self) -> usize {
        self.most_in_one.load(Ordering::SeqCst)
    }

    /// One thread starts diagnosing; it stops when the guard drops.
    fn enter(&self) -> Busy<'_> {
        let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
        self.most.fetch_max(now, Ordering::SeqCst);
        Busy(self)
    }

    fn spread(&self, threads: usize) {
        self.most_in_one.fetch_max(threads, Ordering::SeqCst);
    }
}

/// A thread of [`ThreadCount`] that is diagnosing.
struct Busy<'a>(&'a ThreadCount);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.now.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Whether `code` is one rust-analyzer 0.0.358 gives a macro call it could not expand: an
/// expansion error (missing proc-macro expander, unset `OUT_DIR`, failed `include!`, a
/// macro-by-example error), a disabled proc macro, or an unresolved macro call. Attribute macros
/// are left out: the unresolved-path detector already skips their input item by item.
fn is_unexpanded_macro(code: &DiagnosticCode) -> bool {
    matches!(
        code,
        DiagnosticCode::Ra("macro-error" | "proc-macro-disabled", _)
            | DiagnosticCode::RustcHardError("unresolved-macro-call")
    )
}

/// Error diagnostics of the files, sorted. With `db`, the errors ra does not diagnose are added
/// by the [`DETECTORS`] ([`crate::unresolved`], [`crate::duplicate`], [`crate::field_bounds`]). With `deadline`, stops with `Cancelled`
/// before the first file it finds the deadline reached.
///
/// `known` lists every file a diagnostic range may be attributed to (the files of the session,
/// plus the files a fork created, minus the ones it deleted).
///
/// # Contract
/// - C1: an error ra reports with a range in another known file than the scanned one (for
///   example code a file pulls in with `include!`) is a finding of that file, at the line of that
///   file. The included file is also scanned on its own and reports the same error there; the
///   finding is then kept once.
///   tested: tests/attribution.rs included_file_error_is_reported_against_the_included_file
/// - C2: a gap-detector hit is dropped only when ra reports the same code on the same line of
///   the same file.
///   tested: tests/attribution.rs detector_hit_is_kept_when_ra_reports_the_same_code_and_line_in_another_file
/// - C3 [free]: the path and line of an error whose range is in a file outside `known` (today
///   the scanned file, at the line that range's offset has there, which may be meaningless).
/// - C4: the file a finding is attributed to by C1 is in the crate of the scanned file: an
///   `include!` of another crate's file is not loaded (a `macro-error`), so a scope of whole
///   crates holds both, and the baseline difference and the revert asserts compare the
///   attributed file against its own baseline.
///   tested: tests/attribution.rs include_of_another_crates_file_is_not_loaded
/// - C5: a salsa cancellation raised while a file is diagnosed, inside a gap detector included,
///   is `Cancelled`, never `Panicked`: an error kind says whether the check could not finish or
///   panicked. The cancellation salsa raises on a thread whose query another thread was
///   computing when that thread panicked (a propagated panic) is that panic, `Panicked`.
///   tested: tests/timeout.rs a_cancel_inside_a_detector_is_cancelled
///   tested: diagnostics::tests::a_caught_cancellation_is_cancelled_and_a_propagated_panic_is_panicked
fn collect_errors(
    analysis: &Analysis,
    db: Option<&RootDatabase>,
    files: &[(FileId, RelPath)],
    known: &[(FileId, RelPath)],
    deadline: Option<&Deadline>,
    detector: Detector<'_>,
) -> Result<Collected, AnalyzerError> {
    let _busy = detector.probe.map(ThreadCount::enter);
    let diag_config = diagnostics_config();
    let known: HashMap<FileId, &RelPath> = known.iter().map(|(id, rel)| (*id, rel)).collect();
    let mut errors = Collected::default();
    for (file_id, rel) in files {
        if deadline.is_some_and(Deadline::expired) {
            return Err(AnalyzerError::Cancelled);
        }
        let got = catch_unwind(AssertUnwindSafe(|| {
            let diags = analysis
                .full_diagnostics(&diag_config, AssistResolveStrategy::None, *file_id)
                .map_err(|_| ())?;
            let index = analysis.file_line_index(*file_id).map_err(|_| ())?;
            let mut found = Collected::default();
            let mut ra_keys: Vec<(FindingCode, RelPath, u32)> = Vec::new();
            let mut other_indexes = HashMap::new();
            let mut inactive: Vec<TextRange> = Vec::new();
            let mut unexpanded_macro = false;
            for d in diags {
                unexpanded_macro |= is_unexpanded_macro(&d.code);
                if matches!(d.code, DiagnosticCode::RaLint("inactive_code", _))
                    && d.range.file_id == *file_id
                {
                    inactive.push(d.range.range);
                }
                if d.severity == Severity::Error {
                    let code = classify(&d.code);
                    let mut is_foreign = false;
                    let (path, line) = match known.get(&d.range.file_id) {
                        Some(other) if d.range.file_id != *file_id => {
                            let other_index = match other_indexes.entry(d.range.file_id) {
                                Entry::Occupied(e) => e.into_mut(),
                                Entry::Vacant(e) => e.insert(
                                    analysis.file_line_index(d.range.file_id).map_err(|_| ())?,
                                ),
                            };
                            let line = other_index.line_col(d.range.range.start()).line + 1;
                            is_foreign = true;
                            ((*other).clone(), line)
                        }
                        _ => (rel.clone(), index.line_col(d.range.range.start()).line + 1),
                    };
                    ra_keys.push((code.clone(), path.clone(), line));
                    let finding = Finding {
                        code,
                        message: d.message,
                        path,
                        line,
                    };
                    if is_foreign {
                        found.foreign.push(finding);
                    } else {
                        found.native.push(finding);
                    }
                }
            }
            if let Some(db) = db {
                let line_of = |at: TextSize| index.line_col(at).line + 1;
                for (which, run) in DETECTORS {
                    if unexpanded_macro && which.skips_files_with_unexpanded_macros() {
                        found.unexpanded_macro.push(rel.clone());
                        continue;
                    }
                    // Each detector is contained on its own: a panic in one costs this file
                    // that detector's findings, not the others' and not the collect.
                    let hits = catch_unwind(AssertUnwindSafe(|| {
                        if let Some(fault) = detector.fault
                            && fault.detector == which
                            && &fault.path == rel
                        {
                            if fault.cancel {
                                std::panic::resume_unwind(Box::new(Cancelled::Local));
                            }
                            panic!("{} detector fault injected on {rel}", which);
                        }
                        run(db, *file_id, &inactive, &line_of)
                    }))
                    .unwrap_or_else(|payload| {
                        // salsa 0.28 raises its cancellation with `resume_unwind`; it is not a
                        // detector fault, so it goes on to the outer catch, which makes it
                        // `Cancelled`.
                        if payload.downcast_ref::<Cancelled>().is_some() {
                            std::panic::resume_unwind(payload);
                        }
                        let message = panic_message(payload);
                        detector
                            .panics
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(DetectorPanic {
                                detector: which,
                                path: rel.clone(),
                                message,
                            });
                        found.panics.push((which, rel.clone()));
                        Vec::new()
                    });
                    for (code, message, line) in hits {
                        // The same code on the same line of the same file: ra already says it.
                        if !ra_keys.contains(&(code.clone(), rel.clone(), line)) {
                            found.native.push(Finding {
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
        .map_err(caught)?;
        errors.extend(got.map_err(|()| AnalyzerError::Cancelled)?);
    }
    Ok(errors)
}

/// The findings of a collect: those in the scanned file itself, and those ra reported with a
/// range in another known file. A file pulled in by `include!` is also scanned on its own and
/// gets the same finding there, so the second kind is kept only where the first lacks it.
#[derive(Default)]
struct Collected {
    native: Vec<Finding>,
    foreign: Vec<Finding>,
    panics: Vec<(GapDetector, RelPath)>,
    /// The files on which the unresolved-path detector was skipped.
    unexpanded_macro: Vec<RelPath>,
}

impl Collected {
    fn extend(&mut self, other: Collected) {
        self.native.extend(other.native);
        self.foreign.extend(other.foreign);
        self.panics.extend(other.panics);
        self.unexpanded_macro.extend(other.unexpanded_macro);
    }

    /// Native findings and the foreign ones no native finding equals (once each), sorted.
    fn into_collect(self) -> Collect {
        let seen: HashSet<Finding> = self.native.iter().cloned().collect();
        let mut foreign: Vec<Finding> = self
            .foreign
            .into_iter()
            .filter(|f| !seen.contains(f))
            .collect();
        foreign.sort();
        foreign.dedup();
        let mut all = self.native;
        all.extend(foreign);
        all.sort();
        let mut panics = self.panics;
        panics.sort();
        panics.dedup();
        let mut unexpanded_macro = self.unexpanded_macro;
        unexpanded_macro.sort();
        unexpanded_macro.dedup();
        Collect {
            findings: all,
            panics,
            unexpanded_macro,
        }
    }
}

/// What one collect found, and what it observed of its own run.
pub(crate) struct Collect {
    /// The errors, sorted.
    pub(crate) findings: Vec<Finding>,
    /// The detector and file of each detector panic contained in this collect, sorted, unique.
    /// The session's record keeps a panic once however many collects hit it; this list is of this
    /// collect only.
    pub(crate) panics: Vec<(GapDetector, RelPath)>,
    /// The files of this collect on which the unresolved-path detector stayed silent because a
    /// macro call on them was not expanded; sorted, unique.
    pub(crate) unexpanded_macro: Vec<RelPath>,
}

/// Parallel [`collect_errors`]: chunks the files and diagnoses each chunk on its own snapshot.
/// `gap_detectors` switches the [`DETECTORS`] on; each chunk then also takes
/// its own database clone, dropped before this function returns. `known` is as in
/// [`collect_errors`].
pub(crate) fn collect_errors_par(
    host: &AnalysisHost,
    files: &[(FileId, RelPath)],
    known: &[(FileId, RelPath)],
    threads: usize,
    gap_detectors: bool,
    deadline: Option<&Deadline>,
    detector: Detector<'_>,
) -> Result<Collect, AnalyzerError> {
    if threads <= 1 || files.len() < 8 {
        if let Some(probe) = detector.probe {
            probe.spread(1);
        }
        let db = gap_detectors.then(|| host.raw_database().clone());
        return collect_errors(
            &host.analysis(),
            db.as_ref(),
            files,
            known,
            deadline,
            detector,
        )
        .map(Collected::into_collect);
    }
    let chunk_size = files.len().div_ceil(threads);
    let chunks: Vec<&[(FileId, RelPath)]> = files.chunks(chunk_size).collect();
    if let Some(probe) = detector.probe {
        probe.spread(chunks.len());
    }
    let snapshots: Vec<(Analysis, Option<RootDatabase>)> = (0..chunks.len())
        .map(|_| {
            (
                host.analysis(),
                gap_detectors.then(|| host.raw_database().clone()),
            )
        })
        .collect();
    let mut errors = Collected::default();
    std::thread::scope(|s| -> Result<(), AnalyzerError> {
        let handles: Vec<_> = chunks
            .into_iter()
            .zip(snapshots)
            .map(|(chunk, (analysis, db))| {
                s.spawn(move || {
                    collect_errors(&analysis, db.as_ref(), chunk, known, deadline, detector)
                })
            })
            .collect();
        for h in handles {
            let got = h.join().map_err(caught)?;
            errors.extend(got?);
        }
        Ok(())
    })?;
    Ok(errors.into_collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_caught_cancellation_is_cancelled_and_a_propagated_panic_is_panicked() {
        assert!(matches!(
            caught(Box::new(Cancelled::Local)),
            AnalyzerError::Cancelled
        ));
        assert!(matches!(
            caught(Box::new(Cancelled::PendingWrite)),
            AnalyzerError::Cancelled
        ));
        assert!(
            matches!(
                caught(Box::new(Cancelled::PropagatedPanic)),
                AnalyzerError::Panicked(_)
            ),
            "a propagated panic is a panic"
        );
        assert!(matches!(
            caught(Box::new("boom")),
            AnalyzerError::Panicked(m) if m == "boom"
        ));
    }

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
    fn gap_detector_unseen_class_is_its_own() {
        assert_eq!(
            GapDetector::UnresolvedPath.unseen_class(),
            UnseenClass::UnresolvedPath
        );
        assert_eq!(
            GapDetector::DuplicateName.unseen_class(),
            UnseenClass::DuplicateName
        );
        assert_eq!(
            GapDetector::FieldBounds.unseen_class(),
            UnseenClass::FieldBounds
        );
    }

    #[test]
    fn unexpanded_macro_codes() {
        for yes in [
            DiagnosticCode::Ra("macro-error", Severity::Error),
            DiagnosticCode::Ra("macro-error", Severity::WeakWarning),
            DiagnosticCode::Ra("proc-macro-disabled", Severity::WeakWarning),
            DiagnosticCode::RustcHardError("unresolved-macro-call"),
        ] {
            assert!(is_unexpanded_macro(&yes), "{yes:?}");
        }
        for no in [
            DiagnosticCode::RaLint("inactive_code", Severity::WeakWarning),
            DiagnosticCode::RustcHardError("E0599"),
            DiagnosticCode::Ra("attribute-expansion-disabled", Severity::WeakWarning),
        ] {
            assert!(!is_unexpanded_macro(&no), "{no:?}");
        }
    }

    #[test]
    fn diagnostics_config_equals_test_sample() {
        assert_eq!(
            format!("{:?}", diagnostics_config()),
            format!("{:?}", DiagnosticsConfig::test_sample())
        );
    }
}
