//! The Engine's job pool over a minimal salsa database: one outcome per job, the job state that
//! alone decides a withdrawal, per-layer cancellation, a worker that panics outside its job, the
//! width and thread budget, and results that do not depend on the width or on timing.
//!
//! Every wait on the pool or on a rendezvous has a timeout, so a job that blocks another fails
//! the test instead of hanging it.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ra_ap_ide_db::RootDatabase;
use ryo_v2_engine::{
    Budget, Canceller, Engine, EngineDb, EngineError, Frozen, Hooks, JobKey, Jobs, OpenLayer,
    Outcome, Point, Recv, SealedLayer,
};
use salsa::Setter;

const TIMEOUT: Duration = Duration::from_secs(60);

#[salsa::db]
#[derive(Clone, Default)]
struct Db {
    storage: salsa::Storage<Self>,
}

#[salsa::db]
impl salsa::Database for Db {}

#[salsa::input(debug)]
struct FileText {
    #[returns(clone)]
    text: Arc<str>,
}

#[salsa::input(singleton)]
struct Singleton {
    n: usize,
}

#[salsa::input(debug)]
struct Files {
    #[returns(ref)]
    files: Vec<FileText>,
}

impl EngineDb for Db {
    type File = FileText;

    fn file_text(&self, file: FileText) -> Arc<str> {
        file.text(self)
    }

    fn set_file_text_in_layer(&self, file: FileText, text: Arc<str>) {
        salsa::fork::set_input_field::<FileText, _>(self, file, 0, |fields| fields.0 = text);
    }

    fn create_file_in_layer(&mut self, _file: FileText, _text: Arc<str>) {
        unreachable!("a file of this test database is an input created by the test")
    }
}

#[salsa::tracked(returns(copy))]
fn length(db: &dyn salsa::Database, file: FileText) -> usize {
    file.text(db).len()
}

#[salsa::tracked(returns(copy))]
fn total(db: &dyn salsa::Database, files: Files) -> usize {
    files.files(db).iter().map(|file| length(db, *file)).sum()
}

/// Waits, inside a query, until two threads are in it: a query that blocked the second caller
/// on the first one's claim would never meet. Only `siblings_in_one_query_do_not_block` calls it.
#[salsa::tracked(returns(copy))]
fn meet(db: &dyn salsa::Database, file: FileText) -> bool {
    let _ = file.text(db);
    static ARRIVED: Mutex<usize> = Mutex::new(0);
    static MET: Condvar = Condvar::new();
    let mut arrived = ARRIVED.lock().unwrap();
    *arrived += 1;
    MET.notify_all();
    let (arrived, _) = MET
        .wait_timeout_while(arrived, TIMEOUT, |arrived| *arrived < 2)
        .unwrap();
    *arrived >= 2
}

/// A base with `n` files whose texts are `f0`, `f1`, ... and the input listing them.
fn base(n: usize) -> (Db, Vec<FileText>, Files) {
    let db = Db::default();
    let files: Vec<FileText> = (0..n)
        .map(|i| FileText::new(&db, Arc::from(format!("f{i}"))))
        .collect();
    let list = Files::new(&db, files.clone());
    (db, files, list)
}

fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

/// A pool of `width` workers, one thread per job.
fn pool<R: Send + 'static, E: Send + 'static>(width: usize, hooks: Hooks) -> Jobs<Db, R, E> {
    Jobs::new(Budget::new(nz(width), nz(width), false), hooks).unwrap()
}

/// The next outcome; fails the test on `Pending`, `Idle` or `Broken`.
fn next<R: std::fmt::Debug + Send, E: std::fmt::Debug + Send>(
    jobs: &mut Jobs<Db, R, E>,
) -> (JobKey, Outcome<Db, R, E>) {
    match jobs.recv(TIMEOUT) {
        Recv::Done(key, outcome) => (key, outcome),
        other => panic!("expected an outcome, got {other:?}"),
    }
}

/// Every outcome until the pool is idle, by key.
fn drain<R: std::fmt::Debug + Send, E: std::fmt::Debug + Send>(
    jobs: &mut Jobs<Db, R, E>,
) -> BTreeMap<JobKey, Outcome<Db, R, E>> {
    let mut out = BTreeMap::new();
    loop {
        match jobs.recv(TIMEOUT) {
            Recv::Done(key, outcome) => {
                assert!(
                    out.insert(key, outcome).is_none(),
                    "{key:?} delivered twice"
                );
            }
            Recv::Idle => return out,
            other => panic!("expected an outcome or Idle, got {other:?}"),
        }
    }
}

/// A small deterministic generator (no dependency): splitmix64.
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

#[test]
fn layers_are_send_and_sync() {
    fn send<T: Send>() {}
    fn sync<T: Sync>() {}
    send::<SealedLayer<RootDatabase>>();
    sync::<SealedLayer<RootDatabase>>();
    send::<OpenLayer<RootDatabase>>();
    send::<Canceller>();
    sync::<Canceller>();
    send::<Jobs<RootDatabase, (), ()>>();
    send::<Outcome<RootDatabase, (), ()>>();
    send::<SealedLayer<Db>>();
    sync::<SealedLayer<Db>>();
    send::<OpenLayer<Db>>();
}

#[test]
fn width_and_job_threads_fit_the_budget() {
    for forks in 1..=32 {
        for threads in 1..=32 {
            for caller_works in [false, true] {
                let budget = Budget::new(nz(forks), nz(threads), caller_works);
                let workers = (threads - usize::from(caller_works)).max(1);
                let (w, t) = (budget.width(), budget.job_threads());
                let at = format!("N={forks} T={threads} caller_works={caller_works}: W={w} t={t}");
                assert_eq!(w, forks.min(workers), "{at}");
                assert!(t >= 1, "{at}");
                assert!(w * t <= workers, "{at}");
                assert!(w * (t + 1) > workers, "a thread left unused: {at}");
                if threads > usize::from(caller_works) {
                    assert!(w * t + usize::from(caller_works) <= threads, "{at}");
                }
            }
        }
    }
    // The decided rows of the budget for T = 12.
    let row = |n, caller_works| {
        let b = Budget::new(nz(n), nz(12), caller_works);
        (b.width(), b.job_threads())
    };
    assert_eq!(row(4, false), (4, 3));
    assert_eq!(row(8, false), (8, 1));
    assert_eq!(row(12, false), (12, 1));
    assert_eq!(row(16, false), (12, 1));
    assert_eq!(row(4, true), (4, 2));
    assert_eq!(row(12, true), (11, 1));
    assert_eq!(row(16, true), (11, 1));
}

#[test]
fn one_outcome_per_submit() {
    let (db, files, _) = base(1);
    let file = files[0];
    let frozen = Engine::freeze(db);
    let root = frozen.root();
    let mut jobs: Jobs<Db, String, String> = pool(1, Hooks::default());

    // The first job holds the only worker until released, so the jobs after it wait queued.
    let (release_first, first_gate) = mpsc::channel::<()>();
    let first = jobs.submit(&root, move |layer, _| {
        first_gate.recv_timeout(TIMEOUT).unwrap();
        layer.write(file, "sealed").unwrap();
        Ok(length(&*layer.db(), file).to_string())
    });
    let failed = jobs.submit(&root, |_, _| Err("refused by the job".to_owned()));
    let panicked = jobs.submit(&root, |_, _| panic!("a job panics"));
    let read = jobs.read(&root, move |db, _| Ok(db.file_text(file).to_string()));
    let read_failed = jobs.read(&root, |_, _| Err("a read refused".to_owned()));
    let read_panicked = jobs.read(&root, |_, _| panic!("a read panics"));
    let escaped: Arc<Mutex<Option<Db>>> = Arc::default();
    let keep = Arc::clone(&escaped);
    let refused = jobs.submit(&root, move |layer, _| {
        *keep.lock().unwrap() = Some(Db::clone(&layer.db()));
        Ok(String::new())
    });
    let withdrawn_queued = jobs.submit(&root, |_, _| Ok("never runs".to_owned()));
    let (started, started_rx) = mpsc::channel::<()>();
    let (release_last, last_gate) = mpsc::channel::<()>();
    let withdrawn_running = jobs.submit(&root, move |_, _| {
        started.send(()).unwrap();
        last_gate.recv_timeout(TIMEOUT).unwrap();
        Ok("withdrawn while it ran".to_owned())
    });
    assert_eq!(jobs.in_flight(), 9);

    assert!(jobs.cancel(withdrawn_queued), "a queued job is withdrawn");
    release_first.send(()).unwrap();
    started_rx.recv_timeout(TIMEOUT).unwrap();
    assert!(jobs.cancel(withdrawn_running), "a running job is withdrawn");
    assert!(
        !jobs.cancel(withdrawn_running),
        "a second cancel withdraws nothing"
    );
    release_last.send(()).unwrap();

    let mut outcomes = drain(&mut jobs);
    assert_eq!(outcomes.len(), 9);
    match outcomes.remove(&first) {
        Some(Outcome::Sealed(layer, value)) => {
            assert_eq!(value, "6");
            assert_eq!(&*layer.text(file), "sealed");
        }
        other => panic!("first: {other:?}"),
    }
    assert!(
        matches!(outcomes.remove(&failed), Some(Outcome::Failed(e)) if e == "refused by the job")
    );
    assert!(
        matches!(outcomes.remove(&panicked), Some(Outcome::Panicked(m)) if m == "a job panics")
    );
    assert!(matches!(outcomes.remove(&read), Some(Outcome::Read(t)) if t == "f0"));
    assert!(
        matches!(outcomes.remove(&read_failed), Some(Outcome::Failed(e)) if e == "a read refused")
    );
    assert!(
        matches!(outcomes.remove(&read_panicked), Some(Outcome::Panicked(m)) if m == "a read panics")
    );
    assert!(matches!(
        outcomes.remove(&refused),
        Some(Outcome::Refused(EngineError::InUse))
    ));
    assert!(matches!(
        outcomes.remove(&withdrawn_queued),
        Some(Outcome::Withdrawn)
    ));
    assert!(matches!(
        outcomes.remove(&withdrawn_running),
        Some(Outcome::Withdrawn)
    ));
    assert!(outcomes.is_empty());

    // Keys increase in submission order; a delivered job is no longer known to the pool.
    assert!(first < failed && failed < withdrawn_running);
    assert!(!jobs.cancel(first));
    assert_eq!(jobs.in_flight(), 0);
    let at = Instant::now();
    assert!(matches!(jobs.recv(TIMEOUT), Recv::Idle));
    assert!(at.elapsed() < Duration::from_secs(1), "Idle comes at once");
    drop(escaped);
    assert_eq!(jobs.close(), 0);
}

/// Holds a job at one point until the test has cancelled it.
struct Rendezvous {
    at: Mutex<Sender<Option<JobKey>>>,
    go: Mutex<Receiver<()>>,
}

impl Rendezvous {
    fn new() -> (Arc<Rendezvous>, Receiver<Option<JobKey>>, Sender<()>) {
        let (at, at_rx) = mpsc::channel();
        let (go_tx, go) = mpsc::channel();
        let rendezvous = Arc::new(Rendezvous {
            at: Mutex::new(at),
            go: Mutex::new(go),
        });
        (rendezvous, at_rx, go_tx)
    }

    /// Holds the caller until the test says go; `key` when the caller knows it.
    fn hold(&self, key: Option<JobKey>) {
        self.at.lock().unwrap().send(key).unwrap();
        self.go.lock().unwrap().recv_timeout(TIMEOUT).unwrap();
    }
}

#[test]
fn cancel_true_is_always_withdrawn() {
    let (db, files, _) = base(1);
    let file = files[0];
    let frozen = Engine::freeze(db);
    let root = frozen.root();

    // A cancel at each point of a job: in the queue, while its closure runs, between its seal
    // and its state's `Finished`, and after that.
    #[derive(Copy, Clone, Debug, PartialEq)]
    enum At {
        Queued,
        Running,
        Sealed,
        Finished,
    }
    for at in [At::Queued, At::Running, At::Sealed, At::Finished] {
        let (rendezvous, at_rx, go) = Rendezvous::new();
        let probe = Arc::clone(&rendezvous);
        let hooks = Hooks::default().probe(move |point, key| {
            let hold = match point {
                Point::Popped => at == At::Queued,
                Point::Sealed => at == At::Sealed,
                Point::Finished => at == At::Finished,
                Point::Delivering => false,
            };
            if hold {
                probe.hold(Some(key));
            }
        });
        let mut jobs: Jobs<Db, usize, ()> = pool(1, hooks);
        for i in 0..250 {
            let during = Arc::clone(&rendezvous);
            let query_after = i % 2 == 0;
            let key = jobs.submit(&root, move |layer, _| {
                layer.write(file, "written").unwrap();
                if at == At::Running {
                    during.hold(None);
                }
                let mut n = 0;
                if query_after || at != At::Running {
                    // After a cancel the layer's next fetch raises `Cancelled`.
                    for _ in 0..3 {
                        n = length(&*layer.db(), file);
                    }
                }
                Ok(n)
            });
            let held = at_rx.recv_timeout(TIMEOUT).unwrap();
            if let Some(held) = held {
                assert_eq!(held, key);
            }
            let cancelled = jobs.cancel(key);
            go.send(()).unwrap();
            let (got, outcome) = next(&mut jobs);
            assert_eq!(got, key);
            assert_eq!(cancelled, at != At::Finished, "{at:?}");
            match (cancelled, outcome) {
                (true, Outcome::Withdrawn) => {}
                (false, Outcome::Sealed(layer, n)) => {
                    assert_eq!(n, 7);
                    assert_eq!(&*layer.text(file), "written");
                }
                (cancelled, outcome) => {
                    panic!("{at:?} #{i}: cancel {cancelled}, outcome {outcome:?}")
                }
            }
        }
        assert!(matches!(jobs.recv(TIMEOUT), Recv::Idle));
        assert_eq!(jobs.close(), 0);
    }

    // And with no hook: cancels racing the jobs at whatever point they reached.
    let mut jobs: Jobs<Db, usize, ()> = pool(4, Hooks::default());
    let mut cancelled = BTreeMap::new();
    for i in 0..1000u64 {
        let key = jobs.submit(&root, move |layer, _| {
            layer.write(file, "racing").unwrap();
            Ok(length(&*layer.db(), file))
        });
        for _ in 0..mix(i) % 4000 {
            std::hint::spin_loop();
        }
        cancelled.insert(key, jobs.cancel(key));
    }
    let outcomes = drain(&mut jobs);
    assert_eq!(outcomes.len(), 1000);
    let mut withdrawn = 0;
    for (key, outcome) in outcomes {
        match (cancelled[&key], outcome) {
            (true, Outcome::Withdrawn) => withdrawn += 1,
            (false, Outcome::Sealed(_, 6)) => {}
            (cancelled, outcome) => panic!("{key:?}: cancel {cancelled}, outcome {outcome:?}"),
        }
    }
    eprintln!("racing cancels: {withdrawn} of 1000 withdrawn");
    assert_eq!(jobs.close(), 0);
}

#[test]
fn a_cancelled_payload_without_withdrawal_is_cancelled() {
    let (db, files, _) = base(1);
    let file = files[0];
    let frozen = Engine::freeze(db);
    let root = frozen.root();
    let mut jobs: Jobs<Db, usize, ()> = pool(2, Hooks::default());

    // The job's own layer cancelled by the job, not by the pool: salsa raises `Cancelled`.
    let own = jobs.submit(&root, move |layer, _| {
        layer.canceller().cancel();
        Ok(length(&*layer.db(), file))
    });
    // A propagated-panic cancellation, as a thread of a job sees when another one panicked.
    let propagated = jobs.submit(&root, |_, _| {
        std::panic::resume_unwind(Box::new(salsa::Cancelled::PropagatedPanic))
    });
    let read = jobs.read(&root, |_, _| {
        std::panic::resume_unwind(Box::new(salsa::Cancelled::Local))
    });
    let outcomes = drain(&mut jobs);
    // Not withdrawn, so never `Withdrawn`: the job could not finish (`Cancelled`, the kind
    // rust-analyzer's adapter gives a cancellation inside a diagnosis), or another thread's
    // panic reached it (`Panicked`).
    for key in [own, read] {
        assert!(
            matches!(&outcomes[&key], Outcome::Cancelled),
            "{key:?}: {:?}",
            outcomes[&key]
        );
    }
    match &outcomes[&propagated] {
        Outcome::Panicked(message) => assert!(message.contains("cancellation"), "{message}"),
        other => panic!("{propagated:?}: {other:?}"),
    }
    assert_eq!(jobs.close(), 0);
}

#[test]
fn cancel_leaves_parent_and_siblings() {
    let (db, files, list) = base(2);
    let (a, b) = (files[0], files[1]);
    let frozen = Engine::freeze(db);
    let mut parent = frozen.root().child();
    parent.write(a, "parent").unwrap();
    let parent = parent.seal().unwrap();
    let mut jobs: Jobs<Db, usize, ()> = pool(2, Hooks::default());

    let (started, started_rx) = mpsc::channel::<()>();
    let unwound = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&unwound);
    let cancelled = jobs.submit(&parent, move |layer, _| {
        struct OnUnwind(Arc<AtomicBool>);
        impl Drop for OnUnwind {
            fn drop(&mut self) {
                if thread::panicking() {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
        }
        let _guard = OnUnwind(seen);
        layer.write(b, "cancelled child").unwrap();
        started.send(()).unwrap();
        let until = Instant::now() + TIMEOUT;
        while Instant::now() < until {
            let _ = total(&*layer.db(), list);
            thread::sleep(Duration::from_millis(1));
        }
        Ok(0)
    });
    started_rx.recv_timeout(TIMEOUT).unwrap();
    let sibling = jobs.submit(&parent, move |layer, _| {
        layer.write(b, "sibling").unwrap();
        Ok(total(&*layer.db(), list))
    });
    let (key, outcome) = next(&mut jobs);
    assert_eq!(key, sibling, "the sibling finishes while the other runs");
    let sibling_layer = match outcome {
        Outcome::Sealed(layer, total) => {
            assert_eq!(total, "parent".len() + "sibling".len());
            layer
        }
        other => panic!("sibling: {other:?}"),
    };

    assert!(jobs.cancel(cancelled));
    let (key, outcome) = next(&mut jobs);
    assert_eq!(key, cancelled);
    assert!(matches!(outcome, Outcome::Withdrawn), "{outcome:?}");
    assert!(
        unwound.load(Ordering::SeqCst),
        "the cancel stopped the job's query"
    );

    // The parent, the finished sibling and a new child of the parent still run their queries.
    assert_eq!(parent.read(|db| total(db, list)), Ok("parent".len() + 2));
    assert_eq!(
        sibling_layer.read(|db| total(db, list)),
        Ok("parent".len() + "sibling".len())
    );
    let after = jobs.submit(&parent, move |layer, _| Ok(total(&*layer.db(), list)));
    let (key, outcome) = next(&mut jobs);
    assert_eq!(key, after);
    assert!(matches!(outcome, Outcome::Sealed(_, n) if n == "parent".len() + 2));
    assert_eq!(jobs.close(), 0);
}

#[test]
fn a_worker_death_is_delivered() {
    let (db, files, _) = base(1);
    let file = files[0];
    let frozen = Engine::freeze(db);
    let root = frozen.root();
    let doomed = Arc::new(Mutex::new(None::<JobKey>));
    let target = Arc::clone(&doomed);
    let after_job = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&after_job);
    let hooks = Hooks::default()
        .probe(move |point, key| {
            if point == Point::Delivering && *target.lock().unwrap() == Some(key) {
                panic!("the worker panics outside the job");
            }
        })
        .after_job(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        });
    let mut jobs: Jobs<Db, usize, ()> = pool(1, hooks);

    let (release, gate) = mpsc::channel::<()>();
    let first = jobs.submit(&root, move |layer, _| {
        gate.recv_timeout(TIMEOUT).unwrap();
        layer.write(file, "lost").unwrap();
        Ok(length(&*layer.db(), file))
    });
    *doomed.lock().unwrap() = Some(first);
    release.send(()).unwrap();
    let second = jobs.submit(&root, move |layer, _| Ok(length(&*layer.db(), file)));

    let (key, outcome) = next(&mut jobs);
    assert_eq!(key, first);
    assert!(
        matches!(&outcome, Outcome::Panicked(m) if m.contains("outside job")),
        "{outcome:?}"
    );
    let (key, outcome) = next(&mut jobs);
    assert_eq!(key, second, "the worker goes on with the next job");
    assert!(matches!(outcome, Outcome::Sealed(_, 2)), "{outcome:?}");
    assert!(matches!(jobs.recv(TIMEOUT), Recv::Idle));
    assert_eq!(jobs.close(), 0, "no worker thread ended by a panic");
    assert_eq!(
        after_job.load(Ordering::SeqCst),
        2,
        "both jobs ran, the one whose worker panicked after it included"
    );
}

#[test]
fn a_read_job_counts_in_the_width() {
    let (db, files, list) = base(2);
    let frozen = Engine::freeze(db);
    let root = frozen.root();
    for width in [1, 2] {
        let running = Arc::new(AtomicUsize::new(0));
        let most = Arc::new(AtomicUsize::new(0));
        let mut jobs: Jobs<Db, usize, ()> = pool(width, Hooks::default());
        let enter = |running: &AtomicUsize, most: &AtomicUsize| {
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            most.fetch_max(now, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(20));
            running.fetch_sub(1, Ordering::SeqCst);
        };
        for i in 0..6 {
            let (running, most) = (Arc::clone(&running), Arc::clone(&most));
            if i % 2 == 0 {
                let file = files[0];
                jobs.submit(&root, move |layer, _| {
                    layer.write(file, "w").unwrap();
                    enter(&running, &most);
                    Ok(total(&*layer.db(), list))
                });
            } else {
                jobs.read(&root, move |db, _| {
                    enter(&running, &most);
                    Ok(total(db, list))
                });
            }
        }
        let outcomes = drain(&mut jobs);
        assert_eq!(outcomes.len(), 6);
        // The bound is what is kept; that W = 2 does overlap is timing, so only the bound fails.
        let most = most.load(Ordering::SeqCst);
        assert!((1..=width).contains(&most), "W = {width}: {most} at once");
        assert_eq!(jobs.close(), 0);
    }
}

#[test]
fn queued_jobs_do_not_start_their_deadline() {
    let (db, _, _) = base(0);
    let frozen = Engine::freeze(db);
    let root = frozen.root();
    let mut jobs: Jobs<Db, Duration, ()> = pool(1, Hooks::default());
    let wait = Duration::from_millis(150);
    // Taken before the first submit, so the first job starts after it whatever the scheduler
    // does, and the second one starts at least `wait` after it.
    let submitted = Instant::now();
    jobs.submit(&root, move |_, _| {
        thread::sleep(wait);
        Ok(Duration::ZERO)
    });
    let queued = jobs.submit(&root, move |_, ctx| {
        assert_eq!(ctx.threads(), 1);
        Ok(ctx.started().duration_since(submitted))
    });
    let outcomes = drain(&mut jobs);
    match &outcomes[&queued] {
        Outcome::Sealed(_, waited) => assert!(
            *waited >= wait - Duration::from_millis(20),
            "started {waited:?} after its submit"
        ),
        other => panic!("{other:?}"),
    }
    assert_eq!(jobs.close(), 0);
}

#[test]
fn is_of_tells_the_base() {
    let (db, files, _) = base(1);
    let (other, _, _) = base(1);
    let frozen = Engine::freeze(db);
    let other: Frozen<Db> = Engine::freeze(other);
    let root = frozen.root();
    let mut open = root.child();
    open.write(files[0], "x").unwrap();
    let sealed = open.seal().unwrap();
    let grandchild = sealed.child().seal().unwrap();
    for layer in [&root, &sealed, &grandchild] {
        assert!(layer.is_of(&frozen));
        assert!(!layer.is_of(&other));
    }
    assert!(other.root().is_of(&other));
    assert!(!other.root().is_of(&frozen));
}

/// The node `path` of a job tree: the file it writes and its text.
fn edit_of(path: &[usize], files: usize) -> (usize, String) {
    let seed = path.iter().fold(7u64, |h, &i| mix(h ^ i as u64));
    (seed as usize % files, format!("{path:?}"))
}

/// What a job of the tree returns: every file's text and length on its layer, and their total.
type Seen = (Vec<String>, Vec<usize>, usize);

fn seen(db: &Db, files: &[FileText], list: Files) -> Seen {
    (
        files.iter().map(|f| db.file_text(*f).to_string()).collect(),
        files.iter().map(|f| length(db, *f)).collect(),
        total(db, list),
    )
}

#[test]
fn results_do_not_depend_on_the_width() {
    // A job tree of depth 3 and fan-out 4: each job writes one file of its parent's layer and
    // reads every file. Jobs are delayed by seeded amounts, so they finish in different orders.
    const FILES: usize = 4;
    let (db, files, list) = base(FILES);
    let frozen = Engine::freeze(db);

    // The reference: the same tree, one layer at a time, with no pool.
    let mut reference: BTreeMap<Vec<usize>, Seen> = BTreeMap::new();
    let mut level: Vec<(Vec<usize>, SealedLayer<Db>)> = vec![(vec![], frozen.root())];
    for _ in 0..3 {
        let mut below = Vec::new();
        for (path, layer) in &level {
            for i in 0..4 {
                let mut child_path = path.clone();
                child_path.push(i);
                let (file, text) = edit_of(&child_path, FILES);
                let mut open = layer.child();
                open.write(files[file], text).unwrap();
                reference.insert(child_path.clone(), seen(&open.db(), &files, list));
                below.push((child_path, open.seal().unwrap()));
            }
        }
        level = below;
    }
    drop(level);

    for seed in 0..8u64 {
        for width in [1, 4, 8] {
            let mut jobs: Jobs<Db, Seen, ()> = pool(width, Hooks::default());
            let mut got: BTreeMap<Vec<usize>, Seen> = BTreeMap::new();
            let mut level: Vec<(Vec<usize>, SealedLayer<Db>)> = vec![(vec![], frozen.root())];
            for _ in 0..3 {
                let mut submitted = BTreeMap::new();
                for (path, layer) in &level {
                    for i in 0..4 {
                        let mut child_path = path.clone();
                        child_path.push(i);
                        let (file, text) = edit_of(&child_path, FILES);
                        let delay = mix(seed ^ mix(child_path.len() as u64 * 31 + i as u64)) % 3;
                        let (file, files) = (files[file], files.clone());
                        let key = jobs.submit(layer, move |layer, _| {
                            thread::sleep(Duration::from_millis(delay));
                            layer.write(file, text).unwrap();
                            Ok(seen(&layer.db(), &files, list))
                        });
                        submitted.insert(key, child_path);
                    }
                }
                let mut below = Vec::new();
                for (key, outcome) in drain(&mut jobs) {
                    let path = submitted.remove(&key).expect("a key of this level");
                    match outcome {
                        Outcome::Sealed(layer, value) => {
                            got.insert(path.clone(), value);
                            below.push((path, layer));
                        }
                        other => panic!("seed {seed} W={width} {path:?}: {other:?}"),
                    }
                }
                assert!(submitted.is_empty());
                level = below;
            }
            assert_eq!(got.len(), 84);
            assert_eq!(got, reference, "seed {seed} W={width}");
            drop(level);
            assert_eq!(jobs.close(), 0);
        }
    }
    let isolation = frozen.isolation();
    assert_eq!(isolation.refused, 0);
}

#[test]
fn siblings_in_one_query_do_not_block() {
    let (db, files, _) = base(1);
    let file = files[0];
    let frozen = Engine::freeze(db);
    let parent = frozen.root().child().seal().unwrap();
    let mut jobs: Jobs<Db, bool, ()> = pool(2, Hooks::default());
    let a = jobs.submit(&parent, move |layer, _| Ok(meet(&*layer.db(), file)));
    let b = jobs.submit(&parent, move |layer, _| Ok(meet(&*layer.db(), file)));
    let outcomes = drain(&mut jobs);
    for key in [a, b] {
        assert!(
            matches!(outcomes[&key], Outcome::Sealed(_, true)),
            "{key:?}: {:?}",
            outcomes[&key]
        );
    }
    assert_eq!(jobs.close(), 0);
}

#[test]
fn a_refused_base_write_is_counted() {
    // A job that writes the frozen base (a setter, past the layer) panics, and the refusal is
    // counted, so a caller reading `refused_writes` after each outcome sees it (I2) whatever the
    // job's outcome says. The other jobs go on.
    let (db, files, _) = base(1);
    let file = files[0];
    let frozen = Engine::freeze(db);
    let root = frozen.root();
    assert_eq!(frozen.refused_writes(), 0);
    let mut jobs: Jobs<Db, usize, ()> = pool(2, Hooks::default());
    let bad = jobs.submit(&root, move |layer, _| {
        // A singleton's index is shared by the base and every layer: its creation under a layer
        // is a refused write of the base.
        let _ = Singleton::new(&*layer.db(), 1);
        Ok(0)
    });
    let setter = jobs.submit(&root, move |layer, _| {
        // A setter on a layered handle panics before it could wait (not a counted refusal: it
        // changes nothing).
        let mut db = Db::clone(&layer.db());
        file.set_text(&mut db).to(Arc::from("past the layer"));
        Ok(0)
    });
    let good = jobs.submit(&root, move |layer, _| {
        layer.write(file, "in the layer").unwrap();
        Ok(length(&*layer.db(), file))
    });
    let outcomes = drain(&mut jobs);
    for key in [bad, setter] {
        assert!(
            matches!(outcomes[&key], Outcome::Panicked(_)),
            "{:?}",
            outcomes[&key]
        );
    }
    assert!(matches!(outcomes[&good], Outcome::Sealed(_, 12)));
    assert_eq!(frozen.refused_writes(), 1);
    assert_eq!(&*root.text(file), "f0");
    assert_eq!(jobs.close(), 0);
}
