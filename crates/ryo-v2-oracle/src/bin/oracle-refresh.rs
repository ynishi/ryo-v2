//! Rewrites `corpus/<ws>/expected/<id>.check.json` from `cargo check`.
//!
//! The only place the corpus is built. Each case is composed (base workspace
//! plus overlay) under `oracle-refresh/<id>/` in the repo's Cargo build directory, so that the
//! repo's `rust-toolchain.toml` applies, and the corpus itself (and its
//! `Cargo.lock`) is never touched by cargo.
//!
//! Usage: `oracle-refresh <ws> [--case <id>]...`
#![warn(missing_docs)]

use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use ryo_v2_core::domain::{Diagnostic, ErrorCode, RelPath};
use ryo_v2_oracle::case::{Case, render_expected};
use ryo_v2_oracle::materials::Materials;

/// Directories of the base workspace that are not part of the build.
const SKIP_DIRS: [&str; 6] = ["target", "cases", "expected", "breakage", ".git", ".ryo"];

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let ws = args
        .next()
        .context("usage: oracle-refresh <ws> [--case <id>]...")?;
    let mut only = Vec::new();
    while let Some(a) = args.next() {
        if a == "--case" {
            only.push(args.next().context("--case needs an id")?);
        } else {
            bail!("unknown argument {a:?}");
        }
    }

    let m = Materials::find().context("find materials")?;
    let cases: Vec<Case> = Case::load_all(&m.corpus)
        .context("load cases")?
        .into_iter()
        .filter(|c| c.workspace_rel == ws && (only.is_empty() || only.contains(&c.id)))
        .collect();
    if cases.is_empty() {
        bail!("no case for workspace {ws:?}");
    }
    let target_dir = m.repo_root.join("target/oracle-refresh/_target");
    for case in &cases {
        refresh(&m, case, &target_dir)?;
    }
    Ok(())
}

fn refresh(m: &Materials, case: &Case, target_dir: &Path) -> Result<()> {
    let build = m.repo_root.join("target/oracle-refresh").join(&case.id);
    if build.exists() {
        std::fs::remove_dir_all(&build).with_context(|| format!("remove {}", build.display()))?;
    }
    copy_tree(&case.workspace, &build)?;
    for edit in case
        .edits()
        .with_context(|| format!("edits of {}", case.id))?
    {
        let dest = build.join(edit.path.as_str());
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match edit.new_text {
            Some(text) => {
                std::fs::write(&dest, text).with_context(|| format!("write {}", dest.display()))?
            }
            None => {
                let _ = std::fs::remove_file(&dest);
            }
        }
    }

    let out = Command::new("cargo")
        .args(["check", "--workspace", "--message-format=json"])
        .current_dir(&build)
        .env("CARGO_TARGET_DIR", target_dir)
        .output()
        .context("run cargo check")?;
    let stdout = String::from_utf8_lossy(&out.stdout);
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
        let Some(file) = span["file_name"].as_str() else {
            continue;
        };
        let rel = match Path::new(file).strip_prefix(&build) {
            Ok(p) => p.to_string_lossy().replace('\\', "/"),
            Err(_) => file.to_string(),
        };
        let path = RelPath::parse(&rel).with_context(|| format!("path of a diagnostic: {file}"))?;
        let line = span["line_start"].as_u64().context("line_start")? as u32;
        found.insert(Diagnostic { code, path, line });
    }
    if found.is_empty() && !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(20).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        bail!(
            "{}: cargo check failed without a rustc error code:\n{}",
            case.id,
            tail.join("\n")
        );
    }

    let diags: Vec<Diagnostic> = found.into_iter().collect();
    if let Some(dir) = case.expected_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&case.expected_path, render_expected(&diags))
        .with_context(|| format!("write {}", case.expected_path.display()))?;
    let shown = case
        .expected_path
        .strip_prefix(&m.corpus)
        .unwrap_or(&case.expected_path);
    println!(
        "{}: {} error(s) -> {}",
        case.id,
        diags.len(),
        shown.display()
    );
    for d in &diags {
        println!("  {} {}:{}", d.code, d.path, d.line);
        if !case.group.codes().contains(&d.code.as_str()) {
            println!("  warning: {} is not in group {}", d.code, case.group.id());
        }
    }
    if case.group.id() == "valid" && !diags.is_empty() {
        println!("  warning: valid case has errors");
    }
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from).with_context(|| format!("read {}", from.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let src = entry.path();
        let dest = to.join(&name);
        if src.is_dir() {
            if SKIP_DIRS.iter().any(|s| name == *s) {
                continue;
            }
            copy_tree(&src, &dest)?;
        } else {
            std::fs::copy(&src, &dest).with_context(|| format!("copy {}", src.display()))?;
        }
    }
    Ok(())
}
