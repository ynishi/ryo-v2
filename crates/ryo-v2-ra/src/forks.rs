//! `Forks` for the adapter (D11, stage 3b): N forks of a frozen base, each job a layer written
//! and diagnosed on a worker of the Engine's job pool.
//!
//! A job has two phases. On the session thread, `submit` resolves its edits against its parent
//! Layer's state ([`fork::resolve_pure`]) and completes them on the session
//! ([`fork::complete_on_session`]: the disk, the vfs ids of created files, their sibling roots,
//! the paths of revived ones); a refused edit is a `Done` with `Edit` at once, with no layer and no
//! worker. On a worker, the job writes the resolved actions into a child of the parent's layer,
//! diagnoses its scope there on `t` threads under the check's rules (the cut-off, D1, and the
//! detector-panic rule, D7), and the pool seals the layer. The worker never touches the session:
//! what it needs of it is copied once, at `open`, into [`Facts`].
//!
//! The session is poisoned only when the base was written: a write the frozen base refused
//! (its counter, read on every outcome), or, at `close`, a fingerprint of the base that changed.

use crate::check::{base_text, finish_diagnosis, map_error};
use crate::diagnostics::{Deadline, Detector, GapDetector, ThreadCount};
use crate::engine::{self, engine_error};
use crate::fork::{self, Action, Below, LayerState};
use crate::rev_deps::{RevDeps, scoped_files};
use crate::session::{DetectorPanic, ForkPort, Live, RaSession};
use crate::{DetectorFault, JobHook, RaAnalyzer};
use ra_ap_ide::AnalysisHost;
use ra_ap_ide_db::RootDatabase;
use ra_ap_vfs::FileId;
use ryo_v2_core::app::ports::{
    AnalyzerError, CheckError, CheckReach, Checked, Clock, Event, Forks, JobId, Next, Observer,
};
use ryo_v2_core::domain::{Edit, RelPath, Scope, ScopedFindings};
use ryo_v2_engine::{Budget, Hooks, JobCtx, JobKey, Jobs, OpenLayer, Outcome, Recv, SealedLayer};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fmt;
use std::num::NonZeroUsize;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// Ids of the jobs of every session of the process: an id of another session, or one already
/// delivered, is never that of a job in flight.
static NEXT_JOB_ID: AtomicU64 = AtomicU64::new(1);

/// A Layer of the adapter's `Forks`: the base's root, or the sealed layer of a job that ran to its
/// end, with the state its writes left (the files, the scope, and the chain below it).
#[derive(Clone)]
pub struct RaLayer {
    sealed: SealedLayer<RootDatabase>,
    /// `None` for the base's root.
    state: Option<Arc<LayerState>>,
}

impl fmt::Debug for RaLayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RaLayer")
            .field("layer", &self.sealed)
            .field("edited", &self.state.as_ref().map(|s| &s.edited))
            .finish()
    }
}

impl RaLayer {
    /// The files of the layer's state.
    fn live<'a>(&'a self, base: &'a [(FileId, RelPath)]) -> &'a [(FileId, RelPath)] {
        self.state.as_deref().map_or(base, |s| s.live.as_slice())
    }

    /// The text of `id` at this layer. `Files::file_text` panics on an unknown id, so the read
    /// is wrapped.
    fn text(&self, id: FileId) -> Result<String, AnalyzerError> {
        catch_unwind(AssertUnwindSafe(|| self.sealed.text(id).to_string()))
            .map_err(|p| AnalyzerError::Panicked(crate::diagnostics::panic_message(p)))
    }
}

/// What a job's worker needs of the analyzer and the session, copied at `open`: the worker never
/// borrows the session (it is not `Sync`), and the analyzer is not `'static`.
pub(crate) struct Facts {
    /// The files of the base.
    files: Vec<(FileId, RelPath)>,
    rev_deps: RevDeps,
    /// The detector and file of each detector panic of the baseline (D7).
    baseline_panics: BTreeSet<(GapDetector, RelPath)>,
    gap_detectors: bool,
    check_timeout_ms: Option<u64>,
    clock: Arc<dyn Clock>,
    observer: Arc<dyn Observer>,
    detector_fault: Option<DetectorFault>,
    diagnose_panic: bool,
    write_panic: bool,
    base_write: bool,
    thread_count: Option<Arc<ThreadCount>>,
    job_hook: Option<JobHook>,
}

/// What a job that ran to its end gives besides its layer.
pub(crate) struct JobDone {
    state: Arc<LayerState>,
    findings: ScopedFindings,
    panics: BTreeSet<DetectorPanic>,
}

/// Why a job's check could not finish, and the detector panics it contained before.
pub(crate) struct JobFailed {
    error: CheckError,
    panics: BTreeSet<DetectorPanic>,
}

type Pool = Jobs<RootDatabase, JobDone, JobFailed>;

/// The forks of a session from `Forks::open` to the `Forks::close` that thaws its base.
pub(crate) struct ForksState {
    /// The pool; `None` once a `close` joined it (the session is closing: only reads and `close`).
    pool: Option<Pool>,
    facts: Arc<Facts>,
    /// The jobs whose edits were refused at `submit`, not yet delivered.
    ready: VecDeque<(JobId, CheckError)>,
    /// The id of each job in the pool, not yet delivered.
    ids: HashMap<JobKey, JobId>,
    keys: HashMap<JobId, JobKey>,
    /// Set when the session was found poisoned: every job not yet delivered is `Withdrawn`, then
    /// `next` is `Poisoned` (C7).
    withdrawing: bool,
}

impl ForksState {
    /// Withdraws every job not yet delivered: from now on each outcome is `Withdrawn`.
    fn withdraw_all(&mut self) {
        self.withdrawing = true;
        if let Some(pool) = self.pool.as_mut() {
            for key in self.keys.values() {
                pool.cancel(*key);
            }
        }
    }
}

/// The budget of the forks of `a` on a session of `threads` threads (C4).
fn budget(a: &RaAnalyzer, threads: usize) -> Budget {
    let threads = NonZeroUsize::new(threads).unwrap_or(NonZeroUsize::MIN);
    let forks = a.config.forks;
    let workers = threads
        .get()
        .saturating_sub(usize::from(forks.caller_works))
        .max(1);
    let width = forks
        .width
        .unwrap_or_else(|| NonZeroUsize::new(workers).unwrap_or(NonZeroUsize::MIN));
    Budget::new(width, threads, forks.caller_works)
}

/// The forks of `s` if `s` has them open; `ForkInUse` otherwise.
fn opened(s: &RaSession) -> Result<&ForksState, CheckError> {
    match (&s.forks, s.live) {
        (
            Some(forks),
            Some(Live {
                port: ForkPort::Forks,
                ..
            }),
        ) => Ok(forks),
        _ => Err(CheckError::ForkInUse),
    }
}

/// The brand of `layer`: a Layer of this session's open forks (of this freeze of its base), or
/// `ForkInUse` (C5).
fn brand(s: &RaSession, layer: &RaLayer) -> Result<(), CheckError> {
    opened(s)?;
    match &s.frozen {
        Some(base) if layer.sealed.is_of(&base.frozen) => Ok(()),
        _ => Err(CheckError::ForkInUse),
    }
}

fn refuse_poisoned(s: &RaSession) -> Result<(), CheckError> {
    match &s.poisoned {
        Some(msg) => Err(CheckError::Poisoned(msg.clone())),
        None => Ok(()),
    }
}

/// The edits of a job, resolved and completed on the session, that its worker writes.
struct Plan {
    id: JobId,
    edited: Vec<RelPath>,
    actions: Vec<Action>,
    parent: Option<Arc<LayerState>>,
    scope: Scope,
}

/// A job on its worker: writes the plan into `layer`, then diagnoses its scope there under the
/// rules of a check. The database clones of the diagnosis are dropped before it returns, so the
/// pool can seal the layer. A panic (a write the database cannot carry out, a query that panics)
/// is the pool's `Panicked`; a salsa cancellation the caller did not ask for, its `Cancelled`.
fn run_job(
    facts: &Facts,
    plan: Plan,
    layer: &mut OpenLayer<RootDatabase>,
    ctx: JobCtx,
) -> Result<JobDone, JobFailed> {
    let Plan {
        id,
        edited,
        actions,
        parent,
        scope,
    } = plan;
    let started_ms = facts.clock.now_ms();
    // D1's cut-off runs from the job's start on its worker (its time in the queue is not
    // counted), its writes included.
    let deadline = facts
        .check_timeout_ms
        .map(|ms| Deadline::after(facts.clock.clone(), ms));
    let failed = |error: CheckError| JobFailed {
        error,
        panics: BTreeSet::new(),
    };
    let mut state = LayerState::new(edited, parent);
    engine::write_actions(&mut state, layer, actions, facts.write_panic).map_err(|e| {
        // The layer is the job's own and lent to nobody: a refusal is a fault of the adapter.
        failed(CheckError::Panicked(format!(
            "fork Engine: a job's layer refused its write: {e}"
        )))
    })?;
    let below = state
        .parent
        .clone()
        .map_or_else(|| facts.files.clone(), |p| p.live.clone());
    fork::finish_level(&below, &facts.rev_deps, &mut state);
    if facts.base_write {
        // The test hook: a singleton input created under a layer is a write the frozen base
        // refuses and counts (and panics).
        let _ = ra_ap_base_db::LocalRoots::new(&*layer.db(), Default::default());
    }
    if let Some(hook) = &facts.job_hook {
        hook(id);
    }

    let diag_scope = match scope {
        Scope::Affected => &state.scope,
        Scope::Workspace => &state.live,
    };
    let panics = Mutex::new(BTreeSet::new());
    let diagnosed = {
        let host = AnalysisHost::with_database(layer.db().clone());
        fork::diagnose_on(
            &host,
            diag_scope,
            &state.live,
            ctx.threads(),
            facts.gap_detectors,
            deadline.as_ref(),
            Detector {
                fault: facts.detector_fault.as_ref(),
                panics: &panics,
                probe: facts.thread_count.as_deref(),
            },
            facts.diagnose_panic,
        )
    };
    let found = finish_diagnosis(
        facts.check_timeout_ms,
        &facts.baseline_panics,
        deadline.as_ref(),
        diagnosed,
    );
    let panics = panics.into_inner().unwrap_or_else(PoisonError::into_inner);
    match found {
        Ok(findings) => {
            facts.observer.observe(Event::Checked {
                scope_files: diag_scope.len(),
                total_files: facts.files.len(),
                check_ms: facts.clock.now_ms().saturating_sub(started_ms),
            });
            Ok(JobDone {
                state: Arc::new(state),
                findings,
                panics,
            })
        }
        Err(error) => Err(JobFailed { error, panics }),
    }
}

/// The adapter's `Forks`, on the fork Engine only (`RaConfig.engine`).
///
/// `open` freezes the base and starts the job pool of `RaConfig.forks` (C4). A job's edits are
/// resolved on the session thread at `submit`; its worker writes them into a child of its parent
/// Layer, diagnoses its scope there as `Verifier::check` would (the cut-off from the job's start,
/// D1; a detector panic judged against the baseline, D7), and the layer is sealed. Its detector
/// panics join the session's at `next`. The worker clears rust-analyzer's thread-local trait-solver
/// cache after every job (E6), so no worker keeps a cache of a layer that is gone.
///
/// The isolation of the base (I2) is guarded on every outcome by the count of writes the frozen
/// base refused, and at `close` by that count and, when the writes are sampled
/// (`assert_revert`, `assert_revert_every`), by the fingerprint of the base taken at `open`. A
/// job's error, its panic and a job its own code or its diagnosis cancelled are of reach
/// Blueprint (a salsa cancellation the caller did not ask for is `CheckError::Cancelled` wherever
/// the job raised it); only a base that was written poisons the session.
///
/// # Contract
/// - C1: a job whose edits are refused is a `Next::Done` with `Edit` at once, before any job
///   still in the pool; it opens no layer and takes no worker; `submit` itself fails only with an
///   error of reach Session.
///   tested: tests/forks.rs a_refused_edit_is_a_done_without_a_layer
/// - C2: whatever the width and the order the jobs finish in, every job's outcome is that of the
///   same job run alone (W = 1): the same files, findings and unseen classes, or the same kind of
///   failure; and the findings of a `Next::Done` on its files are those of the flat reference
///   check of the edits that take the base to its layer.
///   tested: tests/forks.rs parallel_jobs_equal_serial_and_the_reference
/// - C3: a salsa cancellation a job raises, in its own code or inside its diagnosis, without a
///   withdrawal, is `Next::Done` with `CheckError::Cancelled`, and a cancelled job is
///   `Next::Withdrawn`.
///   tested: tests/forks.rs a_cancellation_is_cancelled_wherever_a_job_raises_it
/// - C4: at most `width` jobs diagnose at once, each on at most `max(1, T_w / width)` threads, so
///   the threads diagnosing at once are at most `T_w`.
///   tested: tests/forks.rs job_threads_times_jobs_fit_the_budget
/// - C5: two sibling jobs (on one sealed parent) diagnosing at once on rust-analyzer's database do
///   not wait for each other.
///   tested: tests/forks.rs sibling_jobs_in_one_query_do_not_block
/// - C6: a worker that runs jobs of sibling layers one after another, its trait solver's
///   thread-local cache included, gives each layer its own result (E6).
///   tested: tests/forks.rs a_worker_reused_across_layers_keeps_e6
impl Forks for RaAnalyzer {
    type Layer = RaLayer;

    fn open(&self, s: &mut RaSession) -> Result<RaLayer, CheckError> {
        if !s.engine {
            // C9: on the undo log a fork writes the session's one database.
            return Err(CheckError::ForkInUse);
        }
        crate::stack::require_free(s)?;
        engine::freeze(s).map_err(|e| map_error(e, &[]))?;
        let sampled = s.assert_revert || self.config.assert_revert_every.is_some();
        let Some(base) = s.frozen.as_mut() else {
            return Err(CheckError::Panicked(
                "fork Engine: a freeze left no frozen base".to_owned(),
            ));
        };
        if sampled {
            base.sampled_from = Some(base.frozen.isolation());
        }
        let root = base.frozen.root();
        let hooks = Hooks::default().after_job(ra_ap_hir::clear_tls_solver_cache);
        let pool = match Pool::new(budget(self, s.threads), hooks) {
            Ok(pool) => pool,
            Err(e) => {
                drop(root);
                engine::try_thaw(s);
                return Err(CheckError::Load(format!(
                    "the job pool of the forks could not start: {e}"
                )));
            }
        };
        let facts = Facts {
            files: s.files.clone(),
            rev_deps: s.rev_deps.clone(),
            baseline_panics: s.baseline_panics.clone(),
            gap_detectors: s.gap_detectors,
            check_timeout_ms: self.check_timeout_ms,
            clock: self.clock.clone(),
            observer: self.observer.clone(),
            detector_fault: self.detector_fault.clone(),
            diagnose_panic: self.diagnose_panic,
            write_panic: self.db_write_panic == Some(crate::DbWritePanic::Write),
            base_write: self.job_base_write,
            thread_count: self.thread_count.clone(),
            job_hook: self.job_hook.clone(),
        };
        s.forks = Some(ForksState {
            pool: Some(pool),
            facts: Arc::new(facts),
            ready: VecDeque::new(),
            ids: HashMap::new(),
            keys: HashMap::new(),
            withdrawing: false,
        });
        let id =
            crate::stack::open_fork(s, ForkPort::Forks, false).map_err(|e| map_error(e, &[]))?;
        debug_assert!(s.live.is_some_and(|l| l.id == id));
        Ok(RaLayer {
            sealed: root,
            state: None,
        })
    }

    fn width(&self, s: &RaSession) -> usize {
        match s.forks.as_ref().and_then(|f| f.pool.as_ref()) {
            Some(pool) => pool.width(),
            None => budget(self, s.threads).width(),
        }
    }

    fn submit(
        &self,
        s: &mut RaSession,
        parent: &RaLayer,
        edits: &[Edit],
        scope: Scope,
    ) -> Result<JobId, CheckError> {
        brand(s, parent)?;
        if opened(s)?.pool.is_none() {
            return Err(CheckError::ForkInUse);
        }
        refuse_poisoned(s)?;
        let edited = fork::edited_paths(edits);
        // A job is one write, for the cadence of the sampling and for the revert audit.
        s.write_count = s.write_count.wrapping_add(1);
        s.edited_since_audit.extend(edited.iter().cloned());

        let text = |id: FileId| parent.text(id);
        let below = Below::new(parent.state.as_deref(), &s.files, &text);
        let pure = fork::resolve_pure(&below, edits);
        let completed = fork::complete_on_session(s, pure);
        let id = JobId::new(NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed));
        let forks = match s.forks.as_mut() {
            Some(forks) => forks,
            None => return Err(CheckError::ForkInUse),
        };
        let actions = match completed {
            Ok(actions) => actions,
            Err(e) => {
                let error = map_error(e, &edited);
                if error.reach() == CheckReach::Session {
                    return Err(error);
                }
                forks.ready.push_back((id, error));
                return Ok(id);
            }
        };
        let plan = Plan {
            id,
            edited,
            actions,
            parent: parent.state.clone(),
            scope,
        };
        let facts = Arc::clone(&forks.facts);
        let Some(pool) = forks.pool.as_mut() else {
            return Err(CheckError::ForkInUse);
        };
        let key = pool.submit(&parent.sealed, move |layer, ctx| {
            run_job(&facts, plan, layer, ctx)
        });
        forks.ids.insert(key, id);
        forks.keys.insert(id, key);
        Ok(id)
    }

    fn next(&self, s: &mut RaSession, timeout: Duration) -> Result<Next<RaLayer>, CheckError> {
        if opened(s)?.pool.is_none() {
            return Err(CheckError::ForkInUse);
        }
        let poisoned = s.poisoned.clone();
        let Some(forks) = s.forks.as_mut() else {
            return Err(CheckError::ForkInUse);
        };
        if poisoned.is_some() && !forks.withdrawing {
            forks.withdraw_all();
        }
        if let Some((id, error)) = forks.ready.pop_front() {
            return Ok(if forks.withdrawing {
                Next::Withdrawn(id)
            } else {
                Next::Done(id, Err(error))
            });
        }
        let Some(pool) = forks.pool.as_mut() else {
            return Err(CheckError::ForkInUse);
        };
        let (key, outcome) = match pool.recv(timeout) {
            Recv::Done(key, outcome) => (key, outcome),
            Recv::Pending => return Ok(Next::Pending),
            Recv::Idle => {
                return match poisoned {
                    Some(msg) => Err(CheckError::Poisoned(msg)),
                    None => Ok(Next::Idle),
                };
            }
            Recv::Broken => {
                let msg = "a worker of the forks' job pool died".to_owned();
                s.poisoned.get_or_insert(msg.clone());
                if let Some(forks) = s.forks.as_mut() {
                    forks.withdraw_all();
                }
                return Err(CheckError::Poisoned(msg));
            }
        };
        let Some(id) = forks.ids.remove(&key) else {
            return Err(CheckError::Panicked(
                "fork Engine: an outcome of a job the forks do not know".to_owned(),
            ));
        };
        forks.keys.remove(&id);
        // I2's live guard: a job that wrote the base poisons the session, whatever it gave.
        let refused = engine::refused_since_freeze(s);
        let Some(forks) = s.forks.as_mut() else {
            return Err(CheckError::ForkInUse);
        };
        if refused > 0 && !forks.withdrawing {
            forks.withdraw_all();
            s.poisoned.get_or_insert_with(|| {
                format!("a job wrote the base: {refused} write(s) refused (I2)")
            });
        }
        let Some(forks) = s.forks.as_mut() else {
            return Err(CheckError::ForkInUse);
        };
        if forks.withdrawing {
            drop(outcome);
            return Ok(Next::Withdrawn(id));
        }
        let merge = |panics: BTreeSet<DetectorPanic>| {
            s.detector_panics
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend(panics);
        };
        let done = match outcome {
            Outcome::Sealed(sealed, done) => {
                merge(done.panics);
                Ok(Checked {
                    layer: RaLayer {
                        sealed,
                        state: Some(done.state),
                    },
                    findings: done.findings,
                })
            }
            Outcome::Failed(failed) => {
                merge(failed.panics);
                Err(failed.error)
            }
            Outcome::Cancelled => Err(CheckError::Cancelled),
            Outcome::Panicked(msg) => Err(CheckError::Panicked(msg)),
            Outcome::Refused(e) => Err(CheckError::Panicked(format!(
                "fork Engine: a job's layer could not be sealed: {}",
                engine_error(e)
            ))),
            Outcome::Withdrawn => return Ok(Next::Withdrawn(id)),
            Outcome::Read(_) => Err(CheckError::Panicked(
                "fork Engine: a write job gave a read's outcome".to_owned(),
            )),
        };
        Ok(Next::Done(id, done))
    }

    fn cancel(&self, s: &mut RaSession, job: JobId) -> Result<bool, CheckError> {
        let forks = opened(s)?;
        if forks.pool.is_none() {
            return Err(CheckError::ForkInUse);
        }
        let refused_at_submit = forks.ready.iter().any(|(id, _)| *id == job);
        let key = forks.keys.get(&job).copied();
        if !refused_at_submit && key.is_none() {
            return Err(CheckError::ForkInUse);
        }
        refuse_poisoned(s)?;
        let Some(key) = key else {
            // Its outcome is decided already: the `Done` with `Edit` of `submit`.
            return Ok(false);
        };
        Ok(s.forks
            .as_mut()
            .and_then(|f| f.pool.as_mut())
            .is_some_and(|pool| pool.cancel(key)))
    }

    fn layer_files(
        &self,
        s: &RaSession,
        layer: &RaLayer,
        scope: Scope,
    ) -> Result<Vec<RelPath>, CheckError> {
        brand(s, layer)?;
        refuse_poisoned(s)?;
        let live = layer.live(&s.files);
        let covered: Vec<(FileId, RelPath)> = match scope {
            Scope::Workspace => live.to_vec(),
            Scope::Affected => {
                let read =
                    |e: AnalyzerError| CheckError::Load(format!("database read failed: {e}"));
                let edited: BTreeSet<&RelPath> =
                    std::iter::successors(layer.state.as_deref(), |state| state.parent.as_deref())
                        .flat_map(|state| state.edited.iter())
                        .collect();
                let mut changed: Vec<&RelPath> = Vec::new();
                for path in edited {
                    let at_layer = match live.iter().find(|(_, r)| r == path) {
                        Some((id, _)) => Some(layer.text(*id).map_err(read)?),
                        None => None,
                    };
                    if at_layer != base_text(self, s, path).map_err(read)? {
                        changed.push(path);
                    }
                }
                if changed.is_empty() {
                    Vec::new()
                } else {
                    scoped_files(live, &s.rev_deps, &changed)
                }
            }
        };
        let mut files: Vec<RelPath> = covered.into_iter().map(|(_, r)| r).collect();
        files.sort();
        files.dedup();
        Ok(files)
    }

    fn layer_text(
        &self,
        s: &RaSession,
        layer: &RaLayer,
        path: &RelPath,
    ) -> Result<Option<String>, CheckError> {
        brand(s, layer)?;
        refuse_poisoned(s)?;
        match layer.live(&s.files).iter().find(|(_, r)| r == path) {
            Some((id, _)) => layer.text(*id).map(Some).map_err(|e| map_error(e, &[])),
            None => Ok(None),
        }
    }

    fn close(&self, s: &mut RaSession) -> Result<(), CheckError> {
        opened(s)?;
        if let Some(pool) = s.forks.as_mut().and_then(|f| f.pool.take()) {
            // Withdraws every job and joins the workers: no job runs from here on.
            let died = pool.close();
            if let Some(forks) = s.forks.as_mut() {
                forks.ready.clear();
                forks.ids.clear();
                forks.keys.clear();
            }
            if died > 0 {
                s.poisoned
                    .get_or_insert_with(|| format!("{died} worker(s) of the forks' pool died"));
            }
        }
        // I2, with no job running: no write of the base refused, and the base as the open
        // found it when the writes are sampled.
        let refused = engine::refused_since_freeze(s);
        if refused > 0 {
            s.poisoned.get_or_insert_with(|| {
                format!("a job wrote the base: {refused} write(s) refused (I2)")
            });
        }
        if let Some(base) = &s.frozen
            && let Some(from) = &base.sampled_from
            && from
                .fingerprint
                .preserved_in(&base.frozen.isolation().fingerprint)
                .is_err()
        {
            s.poisoned
                .get_or_insert_with(|| "a job changed the base (I2)".to_owned());
        }
        if !engine::try_thaw(s) {
            // A Layer is still held: the session stays closing, the base frozen (C8).
            return Err(CheckError::ForkInUse);
        }
        s.forks = None;
        s.live = None;
        s.base_texts.clear();
        refuse_poisoned(s)
    }
}
