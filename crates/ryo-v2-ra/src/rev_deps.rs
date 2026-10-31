//! Reverse-dependency closure of workspace members, from `cargo metadata`.
//!
//! Own implementation, so nothing depends on rust-analyzer's crate graph API
//! (whose shape moves between 0.0.x releases).

use anyhow::{Context, Result};
use ra_ap_vfs::FileId;
use ryo_v2_core::domain::RelPath;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Member crate dir (relative to the root) -> dirs of the members that depend on it, transitively, itself included.
pub(crate) type RevDeps = HashMap<PathBuf, Vec<PathBuf>>;

/// Builds the closure; the caller treats failure as "empty" (every file in scope).
pub(crate) fn build_rev_deps(root: &Path) -> Result<RevDeps> {
    let out = std::process::Command::new("cargo")
        .args(["metadata", "--format-version", "1"])
        .current_dir(root)
        .output()
        .context("spawn cargo metadata")?;
    if !out.status.success() {
        anyhow::bail!("cargo metadata failed");
    }
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let members: Vec<&str> = meta["workspace_members"]
        .as_array()
        .context("no workspace_members")?
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let packages = meta["packages"].as_array().context("no packages")?;
    let mut dir_of = HashMap::new();
    let mut name_of = HashMap::new();
    for pkg in packages {
        let id = pkg["id"].as_str().unwrap_or_default();
        if !members.contains(&id) {
            continue;
        }
        let name = pkg["name"].as_str().unwrap_or_default().to_string();
        let manifest = Path::new(pkg["manifest_path"].as_str().unwrap_or_default());
        let dir = manifest
            .parent()
            .and_then(|d| d.strip_prefix(root).ok())
            .map(|d| d.to_path_buf())
            .unwrap_or_default();
        dir_of.insert(name.clone(), dir);
        name_of.insert(id.to_string(), name);
    }
    let mut direct_dependents: HashMap<String, Vec<String>> = HashMap::new();
    for pkg in packages {
        let id = pkg["id"].as_str().unwrap_or_default();
        let Some(pkg_name) = name_of.get(id) else {
            continue;
        };
        for dep in pkg["dependencies"].as_array().into_iter().flatten() {
            let dep_name = dep["name"].as_str().unwrap_or_default();
            if dir_of.contains_key(dep_name) {
                direct_dependents
                    .entry(dep_name.to_string())
                    .or_default()
                    .push(pkg_name.clone());
            }
        }
    }
    let mut rev_deps = HashMap::new();
    for (name, dir) in &dir_of {
        let mut seen = vec![name.clone()];
        let mut stack = vec![name.clone()];
        while let Some(n) = stack.pop() {
            for dep in direct_dependents.get(&n).into_iter().flatten() {
                if !seen.contains(dep) {
                    seen.push(dep.clone());
                    stack.push(dep.clone());
                }
            }
        }
        rev_deps.insert(
            dir.clone(),
            seen.iter().filter_map(|n| dir_of.get(n).cloned()).collect(),
        );
    }
    Ok(rev_deps)
}

/// Files to diagnose for a set of edited files: the edited crates and their transitive dependents.
/// Empty `rev_deps` (single package / metadata failure) or no owner found: every file.
pub(crate) fn scoped_files(
    files: &[(FileId, RelPath)],
    rev_deps: &RevDeps,
    edited_rels: &[&RelPath],
) -> Vec<(FileId, RelPath)> {
    if rev_deps.is_empty() {
        return files.to_vec();
    }
    let mut scope_dirs: Vec<&PathBuf> = Vec::new();
    for rel in edited_rels {
        let rel = Path::new(rel.as_str());
        let owner = rev_deps
            .keys()
            .filter(|dir| rel.starts_with(dir))
            .max_by_key(|dir| dir.as_os_str().len());
        if let Some(owner) = owner {
            for dep_dir in &rev_deps[owner] {
                if !scope_dirs.contains(&dep_dir) {
                    scope_dirs.push(dep_dir);
                }
            }
        }
    }
    if scope_dirs.is_empty() {
        return files.to_vec();
    }
    files
        .iter()
        .filter(|(_, rel)| {
            let rel = Path::new(rel.as_str());
            scope_dirs.iter().any(|d| rel.starts_with(d))
        })
        .cloned()
        .collect()
}
