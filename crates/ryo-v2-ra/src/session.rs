//! Loading a workspace into a session.

use crate::RaAnalyzer;
use crate::diagnostics::{Deadline, Detector, GapDetector, collect_errors_par, panic_message};
use crate::rev_deps::{PathFacts, RevDeps, build_rev_deps};
use ra_ap_ide::AnalysisHost;
use ra_ap_load_cargo::{LoadCargoConfig, ProcMacroServerChoice, load_workspace_at};
use ra_ap_project_model::{CargoConfig, RustLibSource};
use ra_ap_vfs::{FileId, Vfs};
use ryo_v2_core::app::ports::{AnalyzerError, Event};
use ryo_v2_core::domain::{Finding, Fingerprint, IntentKind, RelPath};
use std::collections::BTreeSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

/// A file on which a gap detector panicked: in the collect that hit it the file kept its
/// rust-analyzer findings and the other detectors' findings, and contributed none of that
/// detector's.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DetectorPanic {
    /// The detector that panicked.
    pub detector: GapDetector,
    /// The file.
    pub path: RelPath,
    /// The raw panic message.
    pub message: String,
}

/// A call of the expander that panicked: it contributed no Blueprint and the expansion went on.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ExpanderPanic {
    /// The edit kind whose call panicked; `None` when the panic was outside every edit kind.
    pub kind: Option<IntentKind>,
    /// The file of the target.
    pub path: Option<RelPath>,
    /// The panic message.
    pub message: String,
}

/// What the dependencies of a workspace were at load, besides its `.rs` files; the provenance
/// reports it.
///
/// Read from disk once, at load; a later change on disk does not move it.
#[derive(Debug, Clone, Default)]
pub(crate) struct LoadInputs {
    /// Fingerprint of the root `Cargo.toml`, of each member's `Cargo.toml` and of the root
    /// `Cargo.lock` (when it exists), by path relative to the root. A file that could not be
    /// read is left out.
    pub(crate) manifests: Vec<(RelPath, Fingerprint)>,
    /// First line of `rustc --version` run in the root (so a `rust-toolchain` file is honoured);
    /// `unknown` when it could not be run. It stands for the sysroot of that toolchain; the
    /// sysroot path itself is an execution-environment value and is not recorded.
    pub(crate) toolchain: String,
}

/// Captures the manifests, the lock file and the toolchain version of the workspace at `root`.
fn capture_load_inputs(root: &Path, rev_deps: &RevDeps) -> LoadInputs {
    let mut dirs: BTreeSet<PathBuf> = rev_deps.keys().cloned().collect();
    dirs.insert(PathBuf::new());
    let mut candidates: Vec<PathBuf> = dirs.iter().map(|d| d.join("Cargo.toml")).collect();
    candidates.push(PathBuf::from("Cargo.lock"));
    let manifests = candidates
        .iter()
        .filter_map(|rel| {
            let rel_path = rel_of(root, &root.join(rel))?;
            let bytes = std::fs::read(root.join(rel)).ok()?;
            Some((rel_path, Fingerprint::of(&bytes)))
        })
        .collect();
    let toolchain = Command::new("rustc")
        .arg("--version")
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            let text = String::from_utf8(o.stdout).ok()?;
            let line = text.lines().next()?.trim().to_owned();
            (!line.is_empty()).then_some(line)
        })
        .unwrap_or_else(|| "unknown".to_owned());
    LoadInputs {
        manifests,
        toolchain,
    }
}

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
    /// Whether the gap detectors run; baseline and fork diagnostics must agree on it.
    pub(crate) gap_detectors: bool,
    pub(crate) live_fork: bool,
    /// Build-script sources and proc-macro member dirs, from `cargo metadata`.
    pub(crate) path_facts: PathFacts,
    /// Manifests, lock file and toolchain as of load.
    pub(crate) load_inputs: LoadInputs,
    /// Number of `Verifier::check` calls so far.
    pub(crate) check_count: u32,
    /// Cut-off of the diagnostics of the `check` that is running; `None` outside the diagnostics
    /// step of a `check`.
    pub(crate) deadline: Option<Deadline>,
    /// Paths edited by every `check` since the last revert audit.
    pub(crate) edited_since_audit: BTreeSet<RelPath>,
    /// Detector panics contained so far; collects run on several threads and take `&RaSession`.
    pub(crate) detector_panics: Mutex<BTreeSet<DetectorPanic>>,
    /// Expander panics contained so far.
    pub(crate) expander_panics: Mutex<BTreeSet<ExpanderPanic>>,
    /// When set, every call of that edit kind in the expander panics; for the tests of the
    /// containment.
    pub(crate) expander_fault: Option<IntentKind>,
}

impl RaSession {
    /// Marks the session unusable, as a panic inside the database would. For the tests of
    /// callers that must refuse a poisoned session; nothing in the adapter calls it.
    #[doc(hidden)]
    pub fn poison(&mut self, reason: &str) {
        self.poisoned = Some(reason.to_owned());
    }

    /// Every distinct detector panic since load, the baseline's included, sorted.
    ///
    /// # Contract
    /// - C1: a panic of any gap detector on one file leaves load and the session usable (not
    ///   poisoned); the file keeps its rust-analyzer findings and the other detectors' findings,
    ///   has none of that detector's findings in that collect, and is listed here with the
    ///   detector and the message.
    ///   tested: tests/unresolved.rs detector_panic_is_contained_per_file
    ///   tested: tests/gap_detectors.rs each_detector_panic_is_contained_on_its_own
    pub fn detector_panics(&self) -> Vec<DetectorPanic> {
        self.detector_panics
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// Every distinct expander panic since load, sorted.
    ///
    /// # Contract
    /// - C1: a panic of one edit kind's call leaves the expansion Ok with the other kinds'
    ///   Blueprints, the session usable (not poisoned), and the panic listed here with its kind
    ///   and path.
    ///   tested: tests/expand_panics.rs an_expander_panic_is_contained_and_recorded
    pub fn expander_panics(&self) -> Vec<ExpanderPanic> {
        self.expander_panics
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// Makes every call of `kind` in the expander that reaches rust-analyzer (realizing a seed,
    /// or the candidates of a target) panic; for the tests of the containment, like `poison`.
    /// Nothing in the adapter calls it.
    #[doc(hidden)]
    pub fn inject_expander_fault(&mut self, kind: Option<IntentKind>) {
        self.expander_fault = kind;
    }
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
        set_test: a.config.cfg_test,
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

    let (rev_deps, path_facts) = build_rev_deps(&root).unwrap_or_default();
    let load_inputs = capture_load_inputs(&root, &rev_deps);
    let threads = a
        .config
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(8, |n| n.get()));

    let t1 = a.clock.now_ms();
    // The baseline is never cut off.
    let detector_panics = Mutex::new(BTreeSet::new());
    let baseline = collect_errors_par(
        &host,
        &files,
        &files,
        threads,
        a.config.gap_detectors,
        None,
        Detector {
            fault: a.detector_fault.as_ref(),
            panics: &detector_panics,
        },
    )?;
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
        gap_detectors: a.config.gap_detectors,
        live_fork: false,
        path_facts,
        load_inputs,
        check_count: 0,
        deadline: None,
        edited_since_audit: BTreeSet::new(),
        detector_panics,
        expander_panics: Mutex::new(BTreeSet::new()),
        expander_fault: None,
    })
}
