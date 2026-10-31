//! Builds the pairs of the history precision measurement from a git repo.
//!
//! - `history-cases export <REPO> <RANGE> --name <SET> --out <OUT> [--max N] [--force]` writes
//!   `<OUT>/<SET>/index.toml` and, per usable commit, `<id>/base` (the parent tree with the child's
//!   manifests) and `<id>/overlay` (the child's `.rs` files of the workspace members). Git only.
//! - `history-cases expect <OUT>/<SET> [--target-dir DIR]` runs `cargo check --all-targets` on each
//!   child and writes `<id>/expected.check.json`.
//!
//! `OUT` must have no `target` path component: ryo-v2-ra skips every such path, so a base under
//! one would have no files.
#![warn(missing_docs)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use ryo_v2_core::domain::RelPath;
use ryo_v2_oracle::cargo_check::{CargoCheckError, cargo_check};
use ryo_v2_oracle::case::render_expected;
use ryo_v2_oracle::history::{
    ChangeKind, NO_OVERLAY, PairEntry, PairPlan, load_set, plan_pair, write_index,
};
use ryo_v2_oracle::materials::Materials;

const USAGE: &str = "usage:\n  history-cases export <REPO> <RANGE> --name <SET> --out <OUT> [--max N] [--force]\n  history-cases expect <OUT>/<SET> [--target-dir DIR]";

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("export") => export(args.collect()),
        Some("expect") => expect(args.collect()),
        _ => bail!("{USAGE}"),
    }
}

fn export(args: Vec<String>) -> Result<()> {
    let mut pos = Vec::new();
    let (mut name, mut out, mut max, mut force) = (None, None, 20usize, false);
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--name" => name = Some(it.next().context("--name needs a value")?),
            "--out" => out = Some(PathBuf::from(it.next().context("--out needs a value")?)),
            "--max" => {
                max = it
                    .next()
                    .context("--max needs a value")?
                    .parse()
                    .context("--max is a number")?
            }
            "--force" => force = true,
            s if s.starts_with("--") => bail!("unknown argument {s:?}\n{USAGE}"),
            _ => pos.push(a),
        }
    }
    let [repo, range] = pos.as_slice() else {
        bail!("{USAGE}");
    };
    let name = name.context("--name is required")?;
    let out = out.context("--out is required")?;
    let repo = Path::new(repo);

    std::fs::create_dir_all(&out).with_context(|| format!("create {}", out.display()))?;
    let out = out
        .canonicalize()
        .with_context(|| format!("resolve {}", out.display()))?;
    if out.components().any(|c| c.as_os_str() == "target") {
        bail!("OUT has a `target` component; ryo-v2-ra skips those paths");
    }
    let set_dir = out.join(&name);
    if set_dir.exists() {
        if !force {
            bail!("{} exists; pass --force to replace it", set_dir.display());
        }
        std::fs::remove_dir_all(&set_dir)
            .with_context(|| format!("remove {}", set_dir.display()))?;
    }
    std::fs::create_dir_all(&set_dir)?;

    let listing = git(repo, &["rev-list", "--first-parent", "--reverse", range])?;
    let all: Vec<&str> = listing.lines().filter(|l| !l.is_empty()).collect();
    let first_kept = all.len().saturating_sub(max);
    let mut entries = Vec::new();
    for (i, child) in all.iter().enumerate().skip(first_kept) {
        let id = format!("{:02}-{}", i + 1, &child[..8.min(child.len())]);
        let entry = export_pair(repo, &set_dir, &id, child)?;
        match &entry.skip {
            Some(why) => println!("{} skip {why}", entry.id),
            None => println!(
                "{} use overlay={} manifest_from_child={} ignored_rs={}",
                entry.id, entry.overlay_files, entry.manifest_from_child, entry.ignored_rs
            ),
        }
        entries.push(entry);
    }
    write_index(&set_dir, &entries).context("write index.toml")?;
    println!(
        "{}: {} pair(s), {} usable",
        set_dir.display(),
        entries.len(),
        entries.iter().filter(|e| e.skip.is_none()).count()
    );
    Ok(())
}

/// Exports one pair; a pair that cannot be a case comes back with `skip` set and no files.
fn export_pair(repo: &Path, set_dir: &Path, id: &str, child: &str) -> Result<PairEntry> {
    let mut entry = PairEntry {
        id: id.to_string(),
        parent: String::new(),
        child: child.to_string(),
        manifest_from_child: false,
        overlay_files: 0,
        ignored_rs: 0,
        skip: None,
    };
    let parent = match git(repo, &["rev-parse", "--verify", &format!("{child}^1")]) {
        Ok(p) => p.trim().to_string(),
        Err(_) => {
            entry.skip = Some("no parent commit".into());
            return Ok(entry);
        }
    };
    entry.parent = parent.clone();

    let raw = git_bytes(repo, &["diff", "--name-status", "-M", "-z", &parent, child])?;
    let changes = match parse_name_status(&raw) {
        Ok(c) => c,
        Err(why) => {
            entry.skip = Some(why);
            return Ok(entry);
        }
    };
    // With no member dir known, only the pair-level reasons can come back.
    if let PairPlan::Skip(why) = plan_pair(&changes, &[])
        && why != NO_OVERLAY
    {
        entry.skip = Some(why);
        return Ok(entry);
    }

    let dir = set_dir.join(id);
    let base = dir.join("base");
    let result = (|| -> Result<PairPlan> {
        std::fs::create_dir_all(&base)?;
        archive(repo, &parent, &base)?;
        // Non-.rs changes do not depend on the member dirs; the manifests must be in place
        // before `cargo metadata` can name them.
        for (kind, path) in &changes {
            if path.as_str().ends_with(".rs") {
                continue;
            }
            if *kind == ChangeKind::Deleted {
                let _ = std::fs::remove_file(base.join(path.as_str()));
            } else {
                write_blob(repo, child, path, &base)?;
            }
        }
        let members = match member_dirs(&base) {
            Ok(m) => m,
            Err(e) => return Ok(PairPlan::Skip(format!("cargo metadata failed: {e}"))),
        };
        let plan = plan_pair(&changes, &members);
        if let PairPlan::Use { overlay, .. } = &plan {
            let overlay_dir = dir.join("overlay");
            for path in overlay {
                write_blob(repo, child, path, &overlay_dir)?;
            }
        }
        Ok(plan)
    })();
    match result {
        Ok(PairPlan::Use {
            overlay,
            manifest_from_child,
            ignored_rs,
            ..
        }) => {
            entry.manifest_from_child = manifest_from_child;
            entry.overlay_files = overlay.len();
            entry.ignored_rs = ignored_rs;
        }
        Ok(PairPlan::Skip(why)) => {
            entry.skip = Some(why);
            let _ = std::fs::remove_dir_all(&dir);
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e.context(format!("export {id}")));
        }
    }
    Ok(entry)
}

/// Member dirs relative to the workspace root, from `cargo metadata --no-deps`. A package at the
/// root contributes its `src`, `tests`, `examples` and `benches` dirs instead, since a root has
/// no `RelPath`.
fn member_dirs(base: &Path) -> Result<Vec<RelPath>> {
    let out = Command::new("cargo")
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--offline",
        ])
        .current_dir(base)
        .output()
        .context("run cargo metadata")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("{}", stderr.lines().next().unwrap_or("no message"));
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).context("metadata json")?;
    let root = PathBuf::from(
        v["workspace_root"]
            .as_str()
            .context("metadata has no workspace_root")?,
    );
    let mut dirs = Vec::new();
    for pkg in v["packages"]
        .as_array()
        .context("metadata has no packages")?
    {
        let manifest = PathBuf::from(pkg["manifest_path"].as_str().context("manifest_path")?);
        let Some(pkg_dir) = manifest.parent() else {
            continue;
        };
        let Ok(rel) = pkg_dir.strip_prefix(&root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel.is_empty() {
            for sub in ["src", "tests", "examples", "benches"] {
                if let Ok(p) = RelPath::parse(sub) {
                    dirs.push(p);
                }
            }
        } else if let Ok(p) = RelPath::parse(&rel) {
            dirs.push(p);
        }
    }
    dirs.sort();
    dirs.dedup();
    Ok(dirs)
}

/// `git diff --name-status -M -z` into (kind, path) rows. A rename of a `.rs` file stays one
/// `Renamed` row on the new path; any other rename is a delete and an add.
fn parse_name_status(raw: &[u8]) -> Result<Vec<(ChangeKind, RelPath)>, String> {
    let text = String::from_utf8_lossy(raw);
    let mut fields = text.split('\0').filter(|f| !f.is_empty());
    let mut rows = Vec::new();
    let rel = |p: &str| RelPath::parse(p).map_err(|_| format!("unsupported path {p:?}"));
    while let Some(status) = fields.next() {
        let letter = status.chars().next().unwrap_or('?');
        match letter {
            'A' | 'M' | 'D' | 'T' => {
                let path = rel(fields.next().ok_or("truncated git output")?)?;
                let kind = match letter {
                    'A' => ChangeKind::Added,
                    'D' => ChangeKind::Deleted,
                    _ => ChangeKind::Modified,
                };
                rows.push((kind, path));
            }
            'R' | 'C' => {
                let old = rel(fields.next().ok_or("truncated git output")?)?;
                let new = rel(fields.next().ok_or("truncated git output")?)?;
                if letter == 'C' {
                    rows.push((ChangeKind::Added, new));
                } else if old.as_str().ends_with(".rs") || new.as_str().ends_with(".rs") {
                    rows.push((ChangeKind::Renamed, new));
                } else {
                    rows.push((ChangeKind::Deleted, old));
                    rows.push((ChangeKind::Added, new));
                }
            }
            _ => return Err(format!("unsupported change status {status:?}")),
        }
    }
    Ok(rows)
}

fn git_bytes(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("run git")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8_lossy(&git_bytes(repo, args)?).into_owned())
}

/// `git archive <rev> | tar -x -C <dest>`.
fn archive(repo: &Path, rev: &str, dest: &Path) -> Result<()> {
    let mut git = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["archive", rev])
        .stdout(Stdio::piped())
        .spawn()
        .context("run git archive")?;
    let pipe = git.stdout.take().context("git archive stdout")?;
    let tar = Command::new("tar")
        .arg("-x")
        .arg("-C")
        .arg(dest)
        .stdin(Stdio::from(pipe))
        .status()
        .context("run tar")?;
    let git_status = git.wait().context("wait git archive")?;
    if !git_status.success() || !tar.success() {
        bail!("git archive {rev} into {} failed", dest.display());
    }
    Ok(())
}

/// Writes `git show <rev>:<path>` to `<dest>/<path>`.
fn write_blob(repo: &Path, rev: &str, path: &RelPath, dest: &Path) -> Result<()> {
    let bytes = git_bytes(repo, &["show", &format!("{rev}:{}", path.as_str())])?;
    let file = dest.join(path.as_str());
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&file, bytes).with_context(|| format!("write {}", file.display()))
}

fn expect(args: Vec<String>) -> Result<()> {
    let mut set_dir = None;
    let mut target_dir = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--target-dir" => {
                target_dir = Some(PathBuf::from(
                    it.next().context("--target-dir needs a value")?,
                ))
            }
            s if s.starts_with("--") => bail!("unknown argument {s:?}\n{USAGE}"),
            _ => set_dir = Some(PathBuf::from(a)),
        }
    }
    let set_dir = set_dir.context(USAGE)?;
    let pairs = load_set(&set_dir).context("load the set")?;
    let m = Materials::find().context("find materials")?;
    let target_dir =
        target_dir.unwrap_or_else(|| m.repo_root.join("target/history-expect/_target"));
    let set_name = pairs.first().map(|p| p.set.clone()).unwrap_or_default();
    let mut entries: Vec<PairEntry> = pairs.iter().map(|p| p.entry.clone()).collect();
    for (pair, entry) in pairs.iter().zip(entries.iter_mut()) {
        if entry.skip.is_some() {
            continue;
        }
        let build = m
            .repo_root
            .join("target/history-expect")
            .join(&set_name)
            .join(&entry.id);
        if build.exists() {
            std::fs::remove_dir_all(&build)
                .with_context(|| format!("remove {}", build.display()))?;
        }
        copy_tree(&pair.base, &build)?;
        copy_tree(&pair.overlay, &build)?;
        match cargo_check(&build, &target_dir) {
            Ok(diags) => {
                std::fs::write(&pair.expected, render_expected(&diags))
                    .with_context(|| format!("write {}", pair.expected.display()))?;
                println!("{}: {} error(s)", entry.id, diags.len());
            }
            Err(CargoCheckError::NoCode { stderr_tail }) => {
                let first = stderr_tail.lines().next().unwrap_or("no message");
                let why = format!("expect failed: {first}");
                println!("{}: {why}", entry.id);
                entry.skip = Some(why);
                let _ = std::fs::remove_file(&pair.expected);
            }
            Err(e) => return Err(e).with_context(|| entry.id.clone()),
        }
    }
    write_index(&set_dir, &entries).context("write index.toml")?;
    Ok(())
}

/// Copies `from` over `to`, skipping build output and the git directory.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from).with_context(|| format!("read {}", from.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let src = entry.path();
        let dest = to.join(&name);
        let ty = entry.file_type()?;
        if ty.is_dir() {
            if name == "target" || name == ".git" {
                continue;
            }
            copy_tree(&src, &dest)?;
        } else if ty.is_symlink() {
            // Archives keep symlinks; copy what they point to when it is a file.
            if src.is_file() {
                std::fs::copy(&src, &dest).with_context(|| format!("copy {}", src.display()))?;
            }
        } else {
            std::fs::copy(&src, &dest).with_context(|| format!("copy {}", src.display()))?;
        }
    }
    Ok(())
}
