//! The job pool of the Engine (D11, stage 3b): W worker threads that each open a child of a
//! sealed layer, run a job on it and seal it, so N forks of one base share the cores.
//!
//! - A *write job* ([`Jobs::submit`]) opens a child of a sealed layer, runs its closure on the
//!   open layer (writes, then queries), and seals the layer when the closure returns `Ok`.
//! - A *read job* ([`Jobs::read`]) runs its closure on a throwaway, write-free child of a sealed
//!   layer, as [`SealedLayer::read`] does. It takes a worker like a write job, so it counts in the
//!   width.
//!
//! The pool is owned by one caller thread: submitting, receiving and cancelling take `&mut self`.
//! Each job has a state, changed only by compare-and-swap, which alone decides whether its
//! outcome is a withdrawal: `Queued → Running → Finished`, or `Queued | Running → Withdrawn`. An
//! error kind never decides it; a salsa cancellation the caller did not ask for is a panic.
//!
//! Locks: the queue is a `Mutex<VecDeque>` with a `Condvar`, held only to push, pop or remove a
//! task (W workers pull from one queue, and `std::sync::mpsc` has a single consumer). Outcomes go
//! back over one `mpsc` channel. No lock is held across a job.

use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io;
use std::num::NonZeroUsize;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::{Canceller, EngineDb, EngineError, OpenLayer, SealedLayer};

/// Accepted, not taken by a worker.
const QUEUED: u8 = 0;
/// Taken by a worker, its closure running or its layer being sealed.
const RUNNING: u8 = 1;
/// Ran to its end; its outcome is what it produced.
const FINISHED: u8 = 2;
/// Withdrawn before a worker started it: the pool delivered `Withdrawn` at the cancel, and no
/// worker sends anything for it.
const WITHDRAWN_QUEUED: u8 = 3;
/// Withdrawn while it ran: its worker delivers `Withdrawn`, whatever the closure produced.
const WITHDRAWN_RUNNING: u8 = 4;

/// How many jobs run at once and how many threads one job may use, from the forks asked for
/// (N), the threads of the process's budget (T) and whether the caller's own thread works
/// between its calls (c = 1) or only waits for outcomes (c = 0).
///
/// The workers' share is `T_w = max(1, T - c)`; the width is `W = min(N, T_w)` and a job's
/// threads `t = max(1, floor(T_w / W))`. So busy threads, the caller's included, are at most
/// `W·t + c ≤ T` whenever `T > c`.
///
/// # Contract
/// - C1: `width` is at most N and at most `T_w`, `job_threads` is at least 1, `width ×
///   job_threads` is at most `T_w`, and one more thread per job would pass `T_w`.
///   tested: tests/jobs.rs width_and_job_threads_fit_the_budget
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Budget {
    width: NonZeroUsize,
    job_threads: NonZeroUsize,
}

impl Budget {
    /// The budget of `forks` jobs at once over `threads` threads; `caller_works` counts the
    /// caller's thread out of the workers' share.
    pub fn new(forks: NonZeroUsize, threads: NonZeroUsize, caller_works: bool) -> Budget {
        let workers = threads
            .get()
            .saturating_sub(usize::from(caller_works))
            .max(1);
        let width = forks.get().min(workers);
        let job_threads = workers / width;
        Budget {
            width: NonZeroUsize::new(width).unwrap_or(NonZeroUsize::MIN),
            job_threads: NonZeroUsize::new(job_threads).unwrap_or(NonZeroUsize::MIN),
        }
    }

    /// W: the jobs, write and read, that run at once.
    pub fn width(&self) -> usize {
        self.width.get()
    }

    /// t: the threads one job may use.
    pub fn job_threads(&self) -> usize {
        self.job_threads.get()
    }
}

/// The key of a job of one pool; keys increase in submission order.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobKey(u64);

impl JobKey {
    /// The key's number: the count of jobs the pool accepted before this one.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// What a job's closure is given besides its layer.
#[derive(Copy, Clone, Debug)]
pub struct JobCtx {
    threads: NonZeroUsize,
    started: Instant,
}

impl JobCtx {
    /// t: the threads this job may use for its own work (the budget's `job_threads`).
    pub fn threads(&self) -> usize {
        self.threads.get()
    }

    /// When a worker started the job. A time limit of the job starts here, so the time it
    /// waited in the queue is not counted against it.
    pub fn started(&self) -> Instant {
        self.started
    }
}

/// The points of a worker where a probe of [`Hooks::probe`] runs; for tests of the pool's races.
#[doc(hidden)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Point {
    /// A worker took the job from the queue and has not started it.
    Popped,
    /// A write job's layer is sealed; its state is not yet `Finished`.
    Sealed,
    /// The job's state is `Finished`; its outcome is not yet sent.
    Finished,
    /// The outcome is about to be sent (outside the job's own panic catch).
    Delivering,
}

type AfterJob = Arc<dyn Fn() + Send + Sync>;
type Probe = Arc<dyn Fn(Point, JobKey) + Send + Sync>;

/// Code the workers run besides the jobs.
#[derive(Clone, Default)]
pub struct Hooks {
    after_job: Option<AfterJob>,
    probe: Option<Probe>,
}

impl fmt::Debug for Hooks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Hooks")
            .field("after_job", &self.after_job.is_some())
            .field("probe", &self.probe.is_some())
            .finish()
    }
}

impl Hooks {
    /// Runs `f` on the worker after every job that ran, once its outcome is sent, a job whose
    /// worker panicked outside it included: for state the job leaves in the worker thread's
    /// thread-locals (rust-analyzer's trait-solver cache). A panic of `f` is caught and ignored.
    pub fn after_job(mut self, f: impl Fn() + Send + Sync + 'static) -> Hooks {
        self.after_job = Some(Arc::new(f));
        self
    }

    /// Runs `f` at each [`Point`] of every job, on its worker. For tests: a probe that blocks
    /// holds the job at that point.
    #[doc(hidden)]
    pub fn probe(mut self, f: impl Fn(Point, JobKey) + Send + Sync + 'static) -> Hooks {
        self.probe = Some(Arc::new(f));
        self
    }

    fn at(&self, point: Point, key: JobKey) {
        if let Some(probe) = &self.probe {
            probe(point, key);
        }
    }
}

/// How a job ended.
pub enum Outcome<Db: EngineDb, R, E> {
    /// A write job whose closure returned `Ok`: its sealed layer and the value.
    Sealed(SealedLayer<Db>, R),
    /// A read job whose closure returned `Ok`: the value.
    Read(R),
    /// The closure returned `Err`; a write job's layer was dropped unsealed.
    Failed(E),
    /// The Engine refused the layer: a clone of its database outlived the closure
    /// ([`EngineError::InUse`]); the layer was dropped.
    Refused(EngineError),
    /// The caller withdrew the job ([`Jobs::cancel`] returned true). No layer and nothing the
    /// closure produced. (The jobs [`Jobs::close`] withdraws are not delivered.)
    Withdrawn,
    /// The closure panicked (a salsa cancellation the caller did not ask for included), or the
    /// worker panicked outside it; the layer was dropped.
    Panicked(String),
}

impl<Db: EngineDb, R: fmt::Debug, E: fmt::Debug> fmt::Debug for Outcome<Db, R, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::Sealed(layer, r) => f.debug_tuple("Sealed").field(layer).field(r).finish(),
            Outcome::Read(r) => f.debug_tuple("Read").field(r).finish(),
            Outcome::Failed(e) => f.debug_tuple("Failed").field(e).finish(),
            Outcome::Refused(e) => f.debug_tuple("Refused").field(e).finish(),
            Outcome::Withdrawn => f.write_str("Withdrawn"),
            Outcome::Panicked(m) => f.debug_tuple("Panicked").field(m).finish(),
        }
    }
}

/// What [`Jobs::recv`] gives.
pub enum Recv<Db: EngineDb, R, E> {
    /// A job's outcome.
    Done(JobKey, Outcome<Db, R, E>),
    /// The timeout passed with jobs in flight.
    Pending,
    /// No job is in flight or undelivered; given at once.
    Idle,
    /// Every worker is gone with jobs in flight: their outcomes can no longer come.
    Broken,
}

impl<Db: EngineDb, R: fmt::Debug, E: fmt::Debug> fmt::Debug for Recv<Db, R, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Recv::Done(key, outcome) => f.debug_tuple("Done").field(key).field(outcome).finish(),
            Recv::Pending => f.write_str("Pending"),
            Recv::Idle => f.write_str("Idle"),
            Recv::Broken => f.write_str("Broken"),
        }
    }
}

/// A job's state and its layer's canceller, shared by the pool and the worker running it.
#[derive(Default)]
struct Shared {
    state: AtomicU8,
    /// Set by the worker before it moves the state to `Running`, so a cancel that sees `Running`
    /// finds it.
    canceller: OnceLock<Canceller>,
}

impl Shared {
    /// Moves a running (or, for a worker that died before starting it, a queued) job to
    /// `Finished`; whether its own outcome is to be delivered, `Withdrawn` instead, or nothing.
    fn settle(&self) -> Settled {
        let mut current = self.state.load(Ordering::Acquire);
        loop {
            match current {
                QUEUED | RUNNING => {
                    match self.state.compare_exchange(
                        current,
                        FINISHED,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => return Settled::Own,
                        Err(now) => current = now,
                    }
                }
                FINISHED => return Settled::Own,
                WITHDRAWN_RUNNING => return Settled::Withdrawn,
                _ => return Settled::Delivered,
            }
        }
    }

    /// Withdraws a queued or running job: whether this call withdrew it, and from which state.
    fn withdraw(&self) -> Option<u8> {
        let mut current = self.state.load(Ordering::Acquire);
        loop {
            let to = match current {
                QUEUED => WITHDRAWN_QUEUED,
                RUNNING => WITHDRAWN_RUNNING,
                _ => return None,
            };
            match self
                .state
                .compare_exchange(current, to, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    if let Some(canceller) = self.canceller.get() {
                        canceller.cancel();
                    }
                    return Some(current);
                }
                Err(now) => current = now,
            }
        }
    }
}

enum Settled {
    /// The job's own outcome goes out.
    Own,
    /// It was withdrawn while it ran: `Withdrawn` goes out.
    Withdrawn,
    /// It was withdrawn in the queue: the pool already delivered it.
    Delivered,
}

type WriteFn<Db, R, E> = Box<dyn FnOnce(&mut OpenLayer<Db>, JobCtx) -> Result<R, E> + Send>;
type ReadFn<Db, R, E> = Box<dyn FnOnce(&Db, JobCtx) -> Result<R, E> + Send>;

enum Work<Db: EngineDb, R, E> {
    Write(SealedLayer<Db>, WriteFn<Db, R, E>),
    Read(SealedLayer<Db>, ReadFn<Db, R, E>),
}

struct Task<Db: EngineDb, R, E> {
    key: JobKey,
    shared: Arc<Shared>,
    work: Work<Db, R, E>,
}

struct QueueState<T> {
    tasks: VecDeque<T>,
    closed: bool,
}

/// The tasks not yet taken by a worker.
struct Queue<T> {
    state: Mutex<QueueState<T>>,
    ready: Condvar,
}

impl<T> Queue<T> {
    fn lock(&self) -> MutexGuard<'_, QueueState<T>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn push(&self, task: T) {
        self.lock().tasks.push_back(task);
        self.ready.notify_one();
    }

    /// The next task, waiting for one; `None` once the queue is closed.
    fn pop(&self) -> Option<T> {
        let guard = self.lock();
        let mut guard = self
            .ready
            .wait_while(guard, |q| q.tasks.is_empty() && !q.closed)
            .unwrap_or_else(PoisonError::into_inner);
        if guard.closed {
            return None;
        }
        guard.tasks.pop_front()
    }

    /// Closes the queue and drops the tasks left in it (their parents with them).
    fn close(&self) {
        let left = {
            let mut guard = self.lock();
            guard.closed = true;
            std::mem::take(&mut guard.tasks)
        };
        self.ready.notify_all();
        drop(left);
    }
}

type Delivered<Db, R, E> = (JobKey, Outcome<Db, R, E>);

/// A pool of W workers running jobs over the layers of one base.
///
/// Dropping the pool withdraws every job and closes the queue without waiting for the workers;
/// [`Jobs::close`] also joins them.
///
/// # Contract
/// - C1: until the pool is closed or dropped, every accepted job yields exactly one outcome
///   through `recv`, with its key: a write
///   job that ran `Sealed`, `Failed`, `Refused` or `Panicked`, a read job `Read`, `Failed`,
///   `Refused` or `Panicked`, and a withdrawn one `Withdrawn`; once all are delivered `recv` is
///   `Idle` at once.
///   tested: tests/jobs.rs one_outcome_per_submit
/// - C2: `cancel` returns true only when it withdrew a queued or running job, and then the
///   job's outcome is `Withdrawn`; whether it is `Withdrawn` is decided from the job's state alone,
///   never from what its closure returned or raised; a finished job's cancel is false and its
///   outcome what it produced.
///   tested: tests/jobs.rs cancel_true_is_always_withdrawn
///   tested: tests/jobs.rs a_cancelled_payload_without_withdrawal_is_panicked
/// - C3: at most `width` jobs, write and read, run at once.
///   tested: tests/jobs.rs a_read_job_counts_in_the_width
/// - C4: a panic of a worker outside a job's closure still delivers that job's outcome
///   (`Panicked`), and the pool keeps running jobs.
///   tested: tests/jobs.rs a_worker_death_is_delivered
/// - C5: a job's `JobCtx::started` is when a worker started it, not when it was submitted.
///   tested: tests/jobs.rs queued_jobs_do_not_start_their_deadline
/// - C6: the outcomes of a job set do not depend on the width or on the order the jobs finish
///   in, and two jobs on one sealed parent computing one query do not wait for each other.
///   tested: tests/jobs.rs results_do_not_depend_on_the_width
///   tested: tests/jobs.rs siblings_in_one_query_do_not_block
pub struct Jobs<Db: EngineDb + Send, R: Send + 'static, E: Send + 'static> {
    budget: Budget,
    queue: Arc<Queue<Task<Db, R, E>>>,
    results: Receiver<Delivered<Db, R, E>>,
    workers: Vec<JoinHandle<()>>,
    /// The jobs accepted and not delivered.
    jobs: HashMap<JobKey, Arc<Shared>>,
    /// Outcomes delivered by the pool itself (jobs withdrawn in the queue), before the channel's.
    ready: VecDeque<Delivered<Db, R, E>>,
    next: u64,
}

impl<Db: EngineDb + Send, R: Send + 'static, E: Send + 'static> fmt::Debug for Jobs<Db, R, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Jobs")
            .field("budget", &self.budget)
            .field("workers", &self.workers.len())
            .field("in_flight", &self.jobs.len())
            .finish_non_exhaustive()
    }
}

impl<Db: EngineDb + Send, R: Send + 'static, E: Send + 'static> Jobs<Db, R, E> {
    /// Starts `budget.width()` workers. A failure to spawn a thread is the error, with the
    /// workers already started told to stop.
    pub fn new(budget: Budget, hooks: Hooks) -> io::Result<Self> {
        let queue = Arc::new(Queue {
            state: Mutex::new(QueueState {
                tasks: VecDeque::new(),
                closed: false,
            }),
            ready: Condvar::new(),
        });
        let (send, results) = mpsc::channel();
        let mut jobs = Jobs {
            budget,
            queue,
            results,
            workers: Vec::with_capacity(budget.width()),
            jobs: HashMap::new(),
            ready: VecDeque::new(),
            next: 0,
        };
        for index in 0..budget.width() {
            let queue = Arc::clone(&jobs.queue);
            let send = send.clone();
            let hooks = hooks.clone();
            let threads = budget.job_threads;
            let worker = thread::Builder::new()
                .name(format!("ryo-v2-job-{index}"))
                .spawn(move || work(&queue, &send, &hooks, threads))?;
            jobs.workers.push(worker);
        }
        Ok(jobs)
    }

    /// W: the jobs that run at once.
    pub fn width(&self) -> usize {
        self.budget.width()
    }

    /// t: the threads one job may use.
    pub fn job_threads(&self) -> usize {
        self.budget.job_threads()
    }

    /// The jobs accepted and not yet delivered by `recv`.
    pub fn in_flight(&self) -> usize {
        self.jobs.len() + self.ready.len()
    }

    /// A write job: on a worker, `job` runs on a new child of `parent`; the layer is sealed when
    /// it returns `Ok` and dropped when it returns `Err` or panics.
    pub fn submit(
        &mut self,
        parent: &SealedLayer<Db>,
        job: impl FnOnce(&mut OpenLayer<Db>, JobCtx) -> Result<R, E> + Send + 'static,
    ) -> JobKey {
        self.push(Work::Write(parent.clone(), Box::new(job)))
    }

    /// A read job: on a worker, `f` runs on the database of a throwaway, write-free child of
    /// `layer` ([`SealedLayer::read`]). It counts in the width as a write job does.
    pub fn read(
        &mut self,
        layer: &SealedLayer<Db>,
        f: impl FnOnce(&Db, JobCtx) -> Result<R, E> + Send + 'static,
    ) -> JobKey {
        self.push(Work::Read(layer.clone(), Box::new(f)))
    }

    fn push(&mut self, work: Work<Db, R, E>) -> JobKey {
        let key = JobKey(self.next);
        self.next += 1;
        let shared = Arc::new(Shared::default());
        self.jobs.insert(key, Arc::clone(&shared));
        self.queue.push(Task { key, shared, work });
        key
    }

    /// The next outcome, in the order the jobs finished; waits at most `timeout` for one.
    pub fn recv(&mut self, timeout: Duration) -> Recv<Db, R, E> {
        if let Some((key, outcome)) = self.ready.pop_front() {
            return Recv::Done(key, outcome);
        }
        if self.jobs.is_empty() {
            return Recv::Idle;
        }
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.results.recv_timeout(left) {
                Ok((key, outcome)) => {
                    if self.jobs.remove(&key).is_some() {
                        return Recv::Done(key, outcome);
                    }
                    // Not a job in flight: nothing sends one, but a stray outcome is no reason
                    // to stop waiting for the others.
                }
                Err(RecvTimeoutError::Timeout) => return Recv::Pending,
                Err(RecvTimeoutError::Disconnected) => return Recv::Broken,
            }
        }
    }

    /// Withdraws `key` if it is queued or running: true when this call withdrew it, and then its
    /// outcome is `Withdrawn`. A running job's layer is cancelled, which stops its queries at
    /// their next fetch and no other layer's. False for a finished, already withdrawn or unknown
    /// job.
    pub fn cancel(&mut self, key: JobKey) -> bool {
        let Some(shared) = self.jobs.get(&key) else {
            return false;
        };
        match shared.withdraw() {
            Some(QUEUED) => {
                self.jobs.remove(&key);
                // The task leaves the queue now, so its parent is not kept alive until a worker
                // reaches it; a worker that already took it sees the state and sends nothing.
                self.queue.lock().tasks.retain(|task| task.key != key);
                self.ready.push_back((key, Outcome::Withdrawn));
                true
            }
            Some(_) => true,
            None => false,
        }
    }

    /// Withdraws every job, closes the queue, and tells the workers to stop.
    fn shut_down(&mut self) {
        for shared in self.jobs.values() {
            shared.withdraw();
        }
        self.jobs.clear();
        self.ready.clear();
        self.queue.close();
    }

    /// Withdraws every job (nothing more is delivered), then joins the workers: when it returns,
    /// no job of the pool runs and the pool holds no layer. The number of workers that ended by
    /// a panic.
    pub fn close(mut self) -> usize {
        self.shut_down();
        std::mem::take(&mut self.workers)
            .into_iter()
            .map(JoinHandle::join)
            .filter(Result::is_err)
            .count()
    }
}

impl<Db: EngineDb + Send, R: Send + 'static, E: Send + 'static> Drop for Jobs<Db, R, E> {
    fn drop(&mut self) {
        if !self.workers.is_empty() {
            // No join: a drop does not wait. The workers stop after their current job.
            self.shut_down();
        }
    }
}

/// A worker: runs tasks until the queue closes. A panic outside a job's closure is caught here
/// once the job's [`Delivery`] guard has delivered its outcome, and the worker goes on.
fn work<Db: EngineDb + Send, R: Send + 'static, E: Send + 'static>(
    queue: &Queue<Task<Db, R, E>>,
    send: &Sender<Delivered<Db, R, E>>,
    hooks: &Hooks,
    threads: NonZeroUsize,
) {
    while let Some(task) = queue.pop() {
        let ran = catch_unwind(AssertUnwindSafe(|| run(task, send, hooks, threads)));
        // A panic outside the job may come after its closure ran (at the seal, at the send), so
        // the hook runs then too: only a job withdrawn in the queue surely left nothing behind.
        if !matches!(ran, Ok(false))
            && let Some(after) = &hooks.after_job
        {
            let _ = catch_unwind(AssertUnwindSafe(|| after()));
        }
    }
}

/// Delivers a job's outcome exactly once: by [`Delivery::send`], or, when the worker unwinds
/// before that, by its drop, as `Panicked` (or `Withdrawn`, by the state).
struct Delivery<'a, Db: EngineDb, R, E> {
    key: JobKey,
    shared: &'a Shared,
    send: &'a Sender<Delivered<Db, R, E>>,
    armed: bool,
}

impl<Db: EngineDb, R, E> Delivery<'_, Db, R, E> {
    /// Sends `outcome`, or `Withdrawn` when the job was withdrawn while it ran, or nothing when
    /// the pool delivered it at the cancel.
    fn send(mut self, outcome: Outcome<Db, R, E>, hooks: &Hooks) {
        let outcome = match self.shared.settle() {
            Settled::Own => {
                hooks.at(Point::Finished, self.key);
                outcome
            }
            Settled::Withdrawn => {
                drop(outcome);
                Outcome::Withdrawn
            }
            Settled::Delivered => {
                self.armed = false;
                return;
            }
        };
        hooks.at(Point::Delivering, self.key);
        // A closed pool dropped its receiver: there is no one left to tell.
        let _ = self.send.send((self.key, outcome));
        self.armed = false;
    }
}

impl<Db: EngineDb, R, E> Drop for Delivery<'_, Db, R, E> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let outcome = match self.shared.settle() {
            Settled::Own => Outcome::Panicked(format!(
                "a pool worker panicked outside job {} before delivering it",
                self.key.0
            )),
            Settled::Withdrawn => Outcome::Withdrawn,
            Settled::Delivered => return,
        };
        let _ = self.send.send((self.key, outcome));
    }
}

/// Runs one task on this worker; whether its job ran (it was not withdrawn in the queue).
fn run<Db: EngineDb + Send, R: Send + 'static, E: Send + 'static>(
    task: Task<Db, R, E>,
    send: &Sender<Delivered<Db, R, E>>,
    hooks: &Hooks,
    threads: NonZeroUsize,
) -> bool {
    let Task { key, shared, work } = task;
    let delivery = Delivery {
        key,
        shared: &shared,
        send,
        armed: true,
    };
    hooks.at(Point::Popped, key);
    let outcome = match work {
        Work::Write(parent, job) => {
            let mut layer = parent.child();
            drop(parent);
            let Some(ctx) = start(&shared, layer.canceller(), threads) else {
                let mut delivery = delivery;
                delivery.armed = false;
                return false;
            };
            match catch_unwind(AssertUnwindSafe(|| job(&mut layer, ctx))) {
                Ok(Ok(value)) => match layer.seal() {
                    Ok(sealed) => {
                        hooks.at(Point::Sealed, key);
                        Outcome::Sealed(sealed, value)
                    }
                    Err((open, refused)) => {
                        drop(open);
                        Outcome::Refused(refused)
                    }
                },
                Ok(Err(error)) => {
                    drop(layer);
                    Outcome::Failed(error)
                }
                Err(payload) => {
                    drop(layer);
                    Outcome::Panicked(message(&*payload))
                }
            }
        }
        Work::Read(layer, f) => {
            let child = layer.child();
            drop(layer);
            let Some(ctx) = start(&shared, child.canceller(), threads) else {
                let mut delivery = delivery;
                delivery.armed = false;
                return false;
            };
            match catch_unwind(AssertUnwindSafe(|| child.read_throwaway(|db| f(db, ctx)))) {
                Ok(Ok(Ok(value))) => Outcome::Read(value),
                Ok(Ok(Err(error))) => Outcome::Failed(error),
                Ok(Err(refused)) => Outcome::Refused(refused),
                Err(payload) => Outcome::Panicked(message(&*payload)),
            }
        }
    };
    delivery.send(outcome, hooks);
    true
}

/// Registers the job's canceller and moves it from `Queued` to `Running`: its context, or
/// `None` when it was withdrawn in the queue.
fn start(shared: &Shared, canceller: Canceller, threads: NonZeroUsize) -> Option<JobCtx> {
    // Set before the state says `Running`, so a cancel that sees `Running` finds it.
    let _ = shared.canceller.set(canceller);
    shared
        .state
        .compare_exchange(QUEUED, RUNNING, Ordering::AcqRel, Ordering::Acquire)
        .ok()?;
    Some(JobCtx {
        threads,
        started: Instant::now(),
    })
}

/// The text of a panic payload. A salsa cancellation here is one the caller did not ask for (a
/// withdrawal is decided from the job's state before this is read), so it is named as such.
fn message(payload: &(dyn Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else if let Some(cancelled) = payload.downcast_ref::<salsa::Cancelled>() {
        format!("a salsa cancellation the caller did not ask for: {cancelled}")
    } else {
        "a panic with a payload that is not a string".to_owned()
    }
}
