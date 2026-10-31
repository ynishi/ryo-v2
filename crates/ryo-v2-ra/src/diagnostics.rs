//! Error diagnostics out of rust-analyzer, normalised to rustc E-codes.

use ra_ap_ide::{
    Analysis, AnalysisHost, AssistResolveStrategy, DiagnosticCode, DiagnosticsConfig, Severity,
};
use ra_ap_vfs::FileId;
use ryo_v2_core::app::ports::AnalyzerError;
use ryo_v2_core::domain::{Diagnostic, Diagnostics, ErrorCode, RelPath, UnmappedDiagnostic};
use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, catch_unwind};

/// `Ok` = a rustc E-code; `Err` = the rust-analyzer specific code name.
pub(crate) type Classified = Result<ErrorCode, String>;

/// One Error diagnostic, before the baseline difference.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RawError {
    pub(crate) code: Classified,
    pub(crate) message: String,
    pub(crate) rel: RelPath,
    pub(crate) line: u32,
}

/// Baseline-difference key: line is deliberately not part of it, so an edit that shifts a
/// pre-existing error to another line does not make it "new".
pub(crate) type Key = (String, String, RelPath);

impl RawError {
    pub(crate) fn key(&self) -> Key {
        let code = match &self.code {
            Ok(c) => c.as_str().to_owned(),
            Err(s) => s.clone(),
        };
        (code, self.message.clone(), self.rel.clone())
    }
}

/// Maps a rust-analyzer code to an E-code where it is one.
pub(crate) fn classify(code: &DiagnosticCode) -> Classified {
    if let DiagnosticCode::RustcHardError(s) = code
        && let Ok(c) = ErrorCode::parse(s)
    {
        return Ok(c);
    }
    Err(code.as_str().to_owned())
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

/// Error diagnostics of the files, sorted.
fn collect_errors(
    analysis: &Analysis,
    files: &[(FileId, RelPath)],
) -> Result<Vec<RawError>, AnalyzerError> {
    let diag_config = DiagnosticsConfig::test_sample();
    let mut errors = Vec::new();
    for (file_id, rel) in files {
        let got = catch_unwind(AssertUnwindSafe(|| {
            let diags = analysis
                .full_diagnostics(&diag_config, AssistResolveStrategy::None, *file_id)
                .map_err(|_| ())?;
            let index = analysis.file_line_index(*file_id).map_err(|_| ())?;
            Ok::<_, ()>((diags, index))
        }))
        .map_err(|p| AnalyzerError::Panicked(panic_message(p)))?;
        let (diags, index) = got.map_err(|()| AnalyzerError::Cancelled)?;
        for d in diags {
            if d.severity == Severity::Error {
                errors.push(RawError {
                    code: classify(&d.code),
                    message: d.message,
                    rel: rel.clone(),
                    line: index.line_col(d.range.range.start()).line + 1,
                });
            }
        }
    }
    errors.sort();
    Ok(errors)
}

/// Parallel [`collect_errors`]: chunks the files and diagnoses each chunk on its own snapshot.
pub(crate) fn collect_errors_par(
    host: &AnalysisHost,
    files: &[(FileId, RelPath)],
    threads: usize,
) -> Result<Vec<RawError>, AnalyzerError> {
    if threads <= 1 || files.len() < 8 {
        return collect_errors(&host.analysis(), files);
    }
    let chunk_size = files.len().div_ceil(threads);
    let chunks: Vec<&[(FileId, RelPath)]> = files.chunks(chunk_size).collect();
    let snapshots: Vec<Analysis> = (0..chunks.len()).map(|_| host.analysis()).collect();
    let mut errors = Vec::new();
    std::thread::scope(|s| -> Result<(), AnalyzerError> {
        let handles: Vec<_> = chunks
            .into_iter()
            .zip(snapshots)
            .map(|(chunk, analysis)| s.spawn(move || collect_errors(&analysis, chunk)))
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

/// The baseline entries that belong to files in `scope`.
pub(crate) fn filter_baseline(baseline: &[RawError], scope: &[(FileId, RelPath)]) -> Vec<RawError> {
    let rels: HashSet<&RelPath> = scope.iter().map(|(_, r)| r).collect();
    baseline
        .iter()
        .filter(|e| rels.contains(&e.rel))
        .cloned()
        .collect()
}

/// Sorted keys, for comparing two error sets ignoring lines.
pub(crate) fn sorted_keys(errors: &[RawError]) -> Vec<Key> {
    let mut keys: Vec<Key> = errors.iter().map(RawError::key).collect();
    keys.sort();
    keys
}

/// Converts to the domain's sorted [`Diagnostics`].
pub(crate) fn to_diagnostics<'a>(errors: impl IntoIterator<Item = &'a RawError>) -> Diagnostics {
    let mut out = Diagnostics::default();
    for e in errors {
        match &e.code {
            Ok(code) => out.errors.push(Diagnostic {
                code: code.clone(),
                path: e.rel.clone(),
                line: e.line,
            }),
            Err(ra_code) => out.unmapped.push(UnmappedDiagnostic {
                ra_code: ra_code.clone(),
                path: e.rel.clone(),
                line: e.line,
            }),
        }
    }
    out.errors.sort();
    out.unmapped.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_maps_rustc_hard_error() {
        let got = classify(&DiagnosticCode::RustcHardError("E0599"));
        assert_eq!(got, Ok(ErrorCode::parse("E0599").unwrap()));
    }

    #[test]
    fn classify_keeps_ra_codes_unmapped() {
        assert_eq!(
            classify(&DiagnosticCode::RustcHardError("typed-hole")),
            Err("typed-hole".to_owned())
        );
        assert!(classify(&DiagnosticCode::SyntaxError).is_err());
        assert_eq!(
            classify(&DiagnosticCode::Ra("bad-rtn", Severity::Error)),
            Err("bad-rtn".to_owned())
        );
    }
}
