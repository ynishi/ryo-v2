//! The expected side of the oracle: what `cargo check --workspace --all-targets` reports.
//!
//! The CI gate (`clippy --all-targets`, `cargo test`) sees test code, so the truth the in-memory
//! verifier is compared with is taken over all targets too.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use ryo_v2_core::domain::{Diagnostic, ErrorCode, RelPath};

/// The arguments of the check that defines the expected diagnostics. `--keep-going` makes the set
/// of reported errors deterministic: without it cargo stops scheduling units after the first
/// failed one, and which of the independent lib-test and integration-test units still report is a
/// race.
pub const CHECK_ARGS: [&str; 5] = [
    "check",
    "--workspace",
    "--all-targets",
    "--keep-going",
    "--message-format=json",
];

/// Why a check could not produce diagnostics.
#[derive(Debug, thiserror::Error)]
pub enum CargoCheckError {
    /// `cargo` could not be started.
    #[error("run cargo check in {dir}: {source}")]
    Spawn {
        /// The directory the check ran in.
        dir: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The check failed and printed no error with a rustc code (a resolver or manifest failure).
    #[error("cargo check failed without a rustc error code:\n{stderr_tail}")]
    NoCode {
        /// The last lines of stderr.
        stderr_tail: String,
    },
    /// The file of a diagnostic is not a relative path.
    #[error("path of a diagnostic: {0}")]
    Path(String),
}

/// The span in workspace code that a primary span comes from: an error inside a std macro
/// (`assert_eq!`) has its primary span in the toolchain's sources, and `expansion.span` leads to
/// the call site. A span that is relative or under `root` is returned as it is.
fn call_site<'a>(mut span: &'a serde_json::Value, root: &Path) -> &'a serde_json::Value {
    while let Some(file) = span["file_name"].as_str() {
        let outside = Path::new(file).is_absolute() && !Path::new(file).starts_with(root);
        match span["expansion"]["span"].is_object() {
            true if outside => span = &span["expansion"]["span"],
            _ => break,
        }
    }
    span
}

/// The errors with a rustc code in the `--message-format=json` lines of a check.
///
/// Only `compiler-message` lines of level `error` with an E-code and a primary span count; the
/// span's file is made relative to `root` when it is under it. The result is a set sorted by
/// `Ord`.
///
/// # Contract
/// - C1: the lib target and its test target report the same error once.
///   tested: cargo_check::tests::lib_and_lib_test_duplicates_collapse
/// - C2: warnings, lines that are not compiler messages and errors without a code are dropped.
///   tested: cargo_check::tests::non_error_and_codeless_lines_dropped
/// - C3: an error whose primary span lies in a toolchain macro is placed at the call site in the
///   workspace.
///   tested: cargo_check::tests::macro_span_resolves_to_call_site
pub fn parse_messages(stdout: &str, root: &Path) -> Result<Vec<Diagnostic>, CargoCheckError> {
    let mut found: BTreeSet<Diagnostic> = BTreeSet::new();
    for line in stdout.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v["reason"] != "compiler-message" || v["message"]["level"] != "error" {
            continue;
        }
        let Some(code) = v["message"]["code"]["code"]
            .as_str()
            .and_then(|c| ErrorCode::parse(c).ok())
        else {
            continue;
        };
        let Some(span) = v["message"]["spans"]
            .as_array()
            .and_then(|s| s.iter().find(|s| s["is_primary"] == true))
        else {
            continue;
        };
        let span = call_site(span, root);
        let Some(file) = span["file_name"].as_str() else {
            continue;
        };
        let rel = match Path::new(file).strip_prefix(root) {
            Ok(p) => p.to_string_lossy().replace('\\', "/"),
            Err(_) => file.to_string(),
        };
        let path = RelPath::parse(&rel).map_err(|_| CargoCheckError::Path(file.to_string()))?;
        let Some(line) = span["line_start"].as_u64() else {
            continue;
        };
        found.insert(Diagnostic {
            code,
            path,
            line: line as u32,
        });
    }
    Ok(found.into_iter().collect())
}

/// Runs [`CHECK_ARGS`] in `dir` with `CARGO_TARGET_DIR` = `target_dir` and parses the result.
///
/// A failed check that printed no coded error is [`CargoCheckError::NoCode`]; a clean or a
/// failed check with coded errors is `Ok`.
pub fn cargo_check(dir: &Path, target_dir: &Path) -> Result<Vec<Diagnostic>, CargoCheckError> {
    let out = Command::new("cargo")
        .args(CHECK_ARGS)
        .current_dir(dir)
        .env("CARGO_TARGET_DIR", target_dir)
        .output()
        .map_err(|source| CargoCheckError::Spawn {
            dir: dir.to_path_buf(),
            source,
        })?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let found = parse_messages(&stdout, dir)?;
    if found.is_empty() && !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(20).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        return Err(CargoCheckError::NoCode {
            stderr_tail: tail.join("\n"),
        });
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(level: &str, code: Option<&str>, file: &str, line: u32) -> String {
        let code = code.map_or("null".to_string(), |c| format!("{{\"code\":\"{c}\"}}"));
        format!(
            "{{\"reason\":\"compiler-message\",\"message\":{{\"level\":\"{level}\",\"code\":{code},\"spans\":[{{\"is_primary\":false,\"file_name\":\"other.rs\",\"line_start\":1}},{{\"is_primary\":true,\"file_name\":\"{file}\",\"line_start\":{line}}}]}}}}"
        )
    }

    #[test]
    fn lib_and_lib_test_duplicates_collapse() {
        let root = Path::new("/w/build");
        let one = msg("error", Some("E0425"), "/w/build/src/lib.rs", 7);
        let out = format!(
            "{one}\n{one}\n{}\n",
            msg("error", Some("E0308"), "/w/build/tests/it.rs", 3)
        );
        let got = parse_messages(&out, root).expect("parse");
        assert_eq!(got.len(), 2);
        assert!(
            got.iter()
                .any(|d| d.code.as_str() == "E0425" && d.path.as_str() == "src/lib.rs")
        );
        assert!(
            got.iter()
                .any(|d| d.path.as_str() == "tests/it.rs" && d.line == 3)
        );
    }

    #[test]
    fn macro_span_resolves_to_call_site() {
        let root = Path::new("/w/build");
        let line = "{\"reason\":\"compiler-message\",\"message\":{\"level\":\"error\",\"code\":{\"code\":\"E0277\"},\"spans\":[{\"is_primary\":true,\"file_name\":\"/rust/library/core/src/macros/mod.rs\",\"line_start\":46,\"expansion\":{\"span\":{\"file_name\":\"lib/src/lib.rs\",\"line_start\":9,\"expansion\":null}}}]}}";
        let got = parse_messages(line, root).expect("parse");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].path.as_str(), "lib/src/lib.rs");
        assert_eq!(got[0].line, 9);
    }

    #[test]
    fn non_error_and_codeless_lines_dropped() {
        let root = Path::new("/w/build");
        let out = [
            msg("warning", Some("E0425"), "/w/build/src/lib.rs", 1),
            msg("error", None, "/w/build/src/lib.rs", 2),
            "{\"reason\":\"compiler-artifact\"}".to_string(),
            "not json".to_string(),
            msg("error", Some("E0599"), "/w/build/src/lib.rs", 4),
        ]
        .join("\n");
        let got = parse_messages(&out, root).expect("parse");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].code.as_str(), "E0599");
        assert_eq!(got[0].line, 4);
    }
}
