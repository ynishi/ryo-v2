//! Loading a workspace into a session.

use crate::RaAnalyzer;
use crate::diagnostics::{collect_errors_par, panic_message};
use crate::rev_deps::{RevDeps, build_rev_deps};
use ra_ap_ide::AnalysisHost;
use ra_ap_load_cargo::{LoadCargoConfig, ProcMacroServerChoice, load_workspace_at};
use ra_ap_project_model::{CargoConfig, RustLibSource};
use ra_ap_vfs::{FileId, Vfs};
use ryo_v2_core::app::ports::{AnalyzerError, Event};
use ryo_v2_core::domain::{Finding, RelPath};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};

/// A loaded workspace: the rust-analyzer database, its VFS and the baseline diagnostics.
pub struct RaSession {
    pub(crate) host: AnalysisHost,
    /// Kept from load to assign FileIds to created files.
    pub(crate) vfs: Vfs,
    /// Local .rs files of the workspace: (FileId, relative path).
    pub(crate) files: Vec<(FileId, RelPath)>,
    pub(crate) root: PathBuf,
    pub(crate) rev_deps: RevDeps,
    /// Every Error at load.
    pub(crate) baseline: Vec<Finding>,
    /// Set when a panic inside the database may have left it half-updated; the session is
    /// then not reused (`fork` refuses). `None` at load.
    pub(crate) poisoned: Option<String>,
    pub(crate) threads: usize,
    pub(crate) assert_revert: bool,
    pub(crate) live_fork: bool,
}

/// Relative path of `path` under `root`, as a [`RelPath`].
fn rel_of(root: &Path, path: &Path) -> Option<RelPath> {
    let rel = path.strip_prefix(root).ok()?;
    let parts: Vec<&str> = rel
        .components()
        .map(|c| c.as_os_str().to_str())
        .collect::<Option<_>>()?;
    RelPath::parse(&parts.join("/")).ok()
}

pub(crate) fn load(a: &RaAnalyzer, root: &Path) -> Result<RaSession, AnalyzerError> {
    let t0 = a.clock.now_ms();
    let root = root
        .canonicalize()
        .map_err(|e| AnalyzerError::Load(format!("workspace root {}: {e}", root.display())))?;
    let proc_macro = a.config.proc_macro;

    let cargo_config = CargoConfig {
        sysroot: Some(RustLibSource::Discover),
        ..CargoConfig::default()
    };
    let load_config = LoadCargoConfig {
        load_out_dirs_from_check: proc_macro,
        with_proc_macro_server: if proc_macro {
            ProcMacroServerChoice::Sysroot
        } else {
            ProcMacroServerChoice::None
        },
        prefill_caches: false,
        num_worker_threads: 4,
        // 0 makes ra_ap_proc_macro_api's process pool empty, and it panics.
        proc_macro_processes: 2,
    };

    let (db, vfs, _proc_macro) = catch_unwind(AssertUnwindSafe(|| {
        load_workspace_at(&root, &cargo_config, &load_config, &|_| {})
    }))
    .map_err(|p| AnalyzerError::Panicked(panic_message(p)))?
    .map_err(|e| AnalyzerError::Load(format!("{e:#}")))?;
    let host = AnalysisHost::with_database(db);

    let mut files = Vec::new();
    for (file_id, vfs_path) in vfs.iter() {
        let Some(path) = vfs_path.as_path() else {
            continue;
        };
        let path: &Path = path.as_ref();
        if path.extension().is_some_and(|e| e == "rs")
            && !path.components().any(|c| c.as_os_str() == "target")
            && let Some(rel) = rel_of(&root, path)
        {
            files.push((file_id, rel));
        }
    }
    let load_ms = a.clock.now_ms().saturating_sub(t0);

    let rev_deps = build_rev_deps(&root).unwrap_or_default();
    let threads = a
        .config
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(8, |n| n.get()));

    let t1 = a.clock.now_ms();
    let baseline = collect_errors_par(&host, &files, threads)?;
    let baseline_ms = a.clock.now_ms().saturating_sub(t1);

    a.observer.observe(Event::Loaded {
        load_ms,
        baseline_ms,
        files: files.len(),
        baseline_errors: baseline.len(),
    });
    Ok(RaSession {
        host,
        vfs,
        files,
        root,
        rev_deps,
        baseline,
        poisoned: None,
        threads,
        assert_revert: a.config.assert_revert,
        live_fork: false,
    })
}
