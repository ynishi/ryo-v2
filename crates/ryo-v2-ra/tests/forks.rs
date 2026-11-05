//! `Forks` on the adapter (D11, stage 3b): N jobs over a frozen base on a pool of workers, on
//! throwaway workspaces (the corpus is never touched). Every test runs on the Engine, which
//! `Forks` needs; `forks_need_the_engine_backend` covers the undo log.

mod common;

use common::{Fixture, rel};
use ra_ap_base_db::salsa::Cancelled;
use ryo_v2_core::app::ports::{
    Analyzer, CheckError, Checked, ExpandError, Expander, Forks, JobId, Limit, Next, RevertAudit,
    Stacked, Verifier,
};
use ryo_v2_core::domain::{Blueprint, CommandKind, Edit, Scope, ScopedFindings, StateFp, Target};
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_ra::{ForksConfig, GapDetector, RaAnalyzer, RaConfig, RaLayer, RaSession, ThreadCount};
use std::collections::{BTreeMap, HashMap};
use std::mem::Discriminant;
use std::num::NonZeroUsize;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// How long a test waits for one outcome before it calls the jobs blocked.
const WAIT: Duration = Duration::from_secs(120);

/// The config of the tests: no proc-macro server, gap detectors on, `threads` T, and forks of
/// width `width` (N) with `caller_works`.
fn config(width: usize, threads: usize, caller_works: bool, engine: bool) -> RaConfig {
    RaConfig {
        proc_macro: false,
        threads: Some(threads),
        gap_detectors: true,
        cfg_test: false,
        engine,
        forks: ForksConfig {
            width: NonZeroUsize::new(width),
            caller_works,
        },
        ..RaConfig::default()
    }
}

fn analyzer_of(config: RaConfig) -> RaAnalyzer {
    RaAnalyzer::new(
        config,
        Arc::new(MonotonicClock::new()),
        Arc::new(RecordingObserver::default()),
    )
}

/// An analyzer on the Engine with forks of width `width` over `threads` threads, the caller only
/// waiting.
fn analyzer(width: usize, threads: usize) -> RaAnalyzer {
    analyzer_of(config(width, threads, false, true))
}

/// Every outcome until `Idle`, in the order `next` gives them; a wait past [`WAIT`] fails.
fn drain(a: &RaAnalyzer, s: &mut RaSession) -> Vec<Next<RaLayer>> {
    let mut out = Vec::new();
    loop {
        match a.next(s, WAIT).expect("next") {
            Next::Idle => return out,
            Next::Pending => panic!("no outcome within {WAIT:?}: the jobs are blocked"),
            outcome => out.push(outcome),
        }
    }
}

/// The one outcome of `id` in `outcomes`.
fn outcome_of(outcomes: &[Next<RaLayer>], id: JobId) -> &Next<RaLayer> {
    let of: Vec<&Next<RaLayer>> = outcomes
        .iter()
        .filter(|o| match o {
            Next::Done(i, _) | Next::Expanded(i, _) | Next::Withdrawn(i) => *i == id,
            _ => false,
        })
        .collect();
    assert_eq!(of.len(), 1, "one outcome of {id:?}: {outcomes:?}");
    of[0]
}

/// Holds every job that reaches it until `release`.
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    cv: Condvar,
}

impl Gate {
    fn wait(&self) {
        let open = self.open.lock().unwrap();
        let (_open, waited) = self
            .cv
            .wait_timeout_while(open, WAIT, |open| !*open)
            .unwrap();
        assert!(!waited.timed_out(), "the gate was never opened");
    }

    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.cv.notify_all();
    }
}

/// Lets jobs through once `n` of them have reached it: `n` jobs are then at that point at once.
struct Rendezvous {
    n: usize,
    came: Mutex<usize>,
    cv: Condvar,
}

impl Rendezvous {
    fn new(n: usize) -> Rendezvous {
        Rendezvous {
            n,
            came: Mutex::new(0),
            cv: Condvar::new(),
        }
    }

    fn meet(&self) {
        let mut came = self.came.lock().unwrap();
        *came += 1;
        self.cv.notify_all();
        let (_came, waited) = self
            .cv
            .wait_timeout_while(came, WAIT, |came| *came < self.n)
            .unwrap();
        assert!(
            !waited.timed_out(),
            "the jobs never met: they ran one by one"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Fixture W: three members. `a` is a lib with a module `x`, `b` depends on `a`, `c` stands alone.
// Each crate has an E0599 and a name only the unresolved-path detector reports. A lib's `mod`
// lines follow the modules present, so a create or a delete writes the lib too.
// ---------------------------------------------------------------------------------------------

const A_REST: &str =
    "pub struct S;\npub fn ga(s: S) {\n    s.missing();\n}\npub fn fa(_x: MissingA) {}\n";
const A_X: &str = "pub fn xf() -> u32 {\n    1\n}\n";
const B_LIB: &str = "pub mod y;\npub struct T;\npub fn gb(t: T) {\n    t.missing();\n}\npub fn fb(_x: MissingB) {}\n";
const B_Y: &str = "pub fn yf() -> u32 {\n    2\n}\n";
const C_REST: &str =
    "pub struct U;\npub fn gc(u: U) {\n    u.missing();\n}\npub fn fc(_x: MissingC) {}\n";
const C_Z: &str = "pub fn zf() -> u32 {\n    3\n}\n";

/// A state of W: each path's text, `None` where the state has no file.
type State = BTreeMap<String, Option<String>>;

fn w_base() -> State {
    let mut state = State::new();
    for (path, text) in [
        ("a/src/x.rs", A_X.to_owned()),
        ("b/src/lib.rs", B_LIB.to_owned()),
        ("b/src/y.rs", B_Y.to_owned()),
        ("c/src/z.rs", C_Z.to_owned()),
    ] {
        state.insert(path.to_owned(), Some(text));
    }
    let a_lib = a_lib(&state);
    let c_lib = c_lib(&state);
    state.insert("a/src/lib.rs".to_owned(), Some(a_lib));
    state.insert("c/src/lib.rs".to_owned(), Some(c_lib));
    state
}

fn has(state: &State, path: &str) -> bool {
    state.get(path).is_some_and(Option::is_some)
}

fn a_lib(state: &State) -> String {
    let x = if has(state, "a/src/x.rs") {
        "pub mod x;\n"
    } else {
        ""
    };
    let m = if has(state, "a/src/m.rs") {
        "pub mod m;\n"
    } else {
        ""
    };
    format!("{x}{m}{A_REST}")
}

fn c_lib(state: &State) -> String {
    let n = if has(state, "c/src/n.rs") {
        "pub mod n;\n"
    } else {
        ""
    };
    format!("pub mod z;\n{n}{C_REST}")
}

fn w_fixture(test: &str) -> Fixture {
    let manifest = |name: &str, deps: &str| {
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{deps}")
    };
    let a_cargo = manifest("wa", "");
    let b_cargo = manifest("wb", "\n[dependencies]\nwa = { path = \"../a\" }\n");
    let c_cargo = manifest("wc", "");
    let base = w_base();
    let mut files: Vec<(&str, &str)> = vec![
        (
            "Cargo.toml",
            "[workspace]\nmembers = [\"a\", \"b\", \"c\"]\nresolver = \"2\"\n",
        ),
        ("a/Cargo.toml", &a_cargo),
        ("b/Cargo.toml", &b_cargo),
        ("c/Cargo.toml", &c_cargo),
    ];
    for (path, text) in &base {
        files.push((path, text.as_deref().unwrap_or_default()));
    }
    Fixture::new(test, &files)
}

/// A function with an error of its own.
fn err_fn(tag: &str) -> String {
    format!("pub fn e_{tag}() {{\n    1u32.nope_{tag}();\n}}\n")
}

/// The edits of the job `c` (0..4) of a node of the job tree whose parent has `state`, tagged
/// `tag`:
/// - 0: an item-level edit (an erroneous function appended to `c/src/z.rs`), and `c/src/n.rs`
///   created when the state has none;
/// - 1: a body-level edit of `b/src/y.rs` (its body with or without an error), and `a/src/x.rs`
///   deleted, or brought back (revived) when the state deleted it;
/// - 2: `a/src/m.rs` created, or deleted when the state has it;
/// - 3: `c/src/n.rs` created (with an error), or deleted when the state has it; with job 0, two
///   siblings that create one path.
fn node_edits(state: &State, tag: &str, c: usize) -> Vec<Edit> {
    let mut next = state.clone();
    let mut edits = Vec::new();
    let mut set = |next: &mut State, path: &str, text: Option<String>| {
        next.insert(path.to_owned(), text.clone());
        edits.push(match text {
            Some(text) => Edit::write(rel(path), text),
            None => Edit::delete(rel(path)),
        });
    };
    match c {
        0 => {
            let z = state["c/src/z.rs"].clone().unwrap_or_default();
            set(&mut next, "c/src/z.rs", Some(format!("{z}{}", err_fn(tag))));
            if !has(state, "c/src/n.rs") {
                set(
                    &mut next,
                    "c/src/n.rs",
                    Some(format!("pub fn n_{tag}() -> u32 {{\n    0\n}}\n")),
                );
                let lib = c_lib(&next);
                set(&mut next, "c/src/lib.rs", Some(lib));
            }
        }
        1 => {
            let body = if tag.len().is_multiple_of(2) {
                "2".to_owned()
            } else {
                format!("1u32.nope_{tag}()")
            };
            set(
                &mut next,
                "b/src/y.rs",
                Some(format!("pub fn yf() -> u32 {{\n    {body}\n}}\n")),
            );
            let x = (!has(state, "a/src/x.rs")).then(|| format!("{A_X}{}", err_fn(tag)));
            set(&mut next, "a/src/x.rs", x);
            let lib = a_lib(&next);
            set(&mut next, "a/src/lib.rs", Some(lib));
        }
        2 => {
            let m = (!has(state, "a/src/m.rs"))
                .then(|| format!("pub fn mf_{tag}() -> u32 {{\n    1\n}}\n{}", err_fn(tag)));
            set(&mut next, "a/src/m.rs", m);
            let lib = a_lib(&next);
            set(&mut next, "a/src/lib.rs", Some(lib));
        }
        _ => {
            let n = (!has(state, "c/src/n.rs")).then(|| err_fn(&format!("n{tag}")));
            set(&mut next, "c/src/n.rs", n);
            let lib = c_lib(&next);
            set(&mut next, "c/src/lib.rs", Some(lib));
        }
    }
    edits
}

/// `state` after `edits`.
fn applied(state: &State, edits: &[Edit]) -> State {
    let mut next = state.clone();
    for edit in edits {
        next.insert(edit.path.as_str().to_owned(), edit.new_text.clone());
    }
    next
}

/// The edits that take the base of W to `state`.
fn composed(state: &State) -> Vec<Edit> {
    let base = w_base();
    state
        .iter()
        .filter(|(path, text)| base.get(*path).cloned().flatten() != **text)
        .filter_map(|(path, text)| match text {
            Some(text) => Some(Edit::write(rel(path), text.clone())),
            None => has(&base, path).then(|| Edit::delete(rel(path))),
        })
        .collect()
}

/// A node of the job tree: its place (child indices from the root) and its scope.
type Place = Vec<usize>;

/// What a run of the job tree gave each node: the diagnosis, or the kind of the failure (its
/// message and the order of several failures depend on the chunking, F10).
type Ran = BTreeMap<Place, Result<ScopedFindings, Discriminant<CheckError>>>;

/// The job tree, depth 3 and fan-out 4 (84 jobs), plus one refused job on the root: each job is
/// submitted as soon as its parent is done. Returns what each node gave, compared by the node,
/// and the state each node's job wrote.
fn run_tree(a: &RaAnalyzer, s: &mut RaSession) -> (Ran, BTreeMap<Place, State>) {
    let root = a.open(s).expect("open");
    let mut pending: HashMap<JobId, (Place, State, Scope)> = HashMap::new();
    let mut states: BTreeMap<Place, State> = BTreeMap::new();
    let submit = |a: &RaAnalyzer,
                  s: &mut RaSession,
                  pending: &mut HashMap<JobId, (Place, State, Scope)>,
                  parent: &RaLayer,
                  place: Place,
                  state: State,
                  edits: &[Edit]| {
        // The root's children diagnose the workspace, the others their own scope.
        let scope = if place.len() == 1 {
            Scope::Workspace
        } else {
            Scope::Affected
        };
        let id = a
            .submit(s, parent, edits, scope, Limit::Check)
            .expect("submit");
        assert!(
            pending.insert(id, (place, state, scope)).is_none(),
            "{id:?}"
        );
        id
    };
    let base = w_base();
    for c in 0..4 {
        let edits = node_edits(&base, &format!("r{c}"), c);
        submit(
            a,
            s,
            &mut pending,
            &root,
            vec![c],
            applied(&base, &edits),
            &edits,
        );
    }
    // The refused job: a delete of a path that is not a file of the base.
    let refused = [Edit::delete(rel("a/src/nothing.rs"))];
    submit(a, s, &mut pending, &root, vec![4], base.clone(), &refused);
    drop(root);

    let mut ran = Ran::new();
    loop {
        match a.next(s, WAIT).expect("next") {
            Next::Done(id, result) => {
                // Once: a second outcome of the id finds it gone.
                let (place, state, scope) = pending.remove(&id).expect("a job of this run");
                let summary = match result {
                    Ok(Checked { layer, findings }) => {
                        if place.len() < 3 {
                            for c in 0..4 {
                                let tag: String =
                                    place.iter().chain([&c]).map(|i| i.to_string()).collect();
                                let edits = node_edits(&state, &format!("t{tag}"), c);
                                let mut child = place.clone();
                                child.push(c);
                                submit(
                                    a,
                                    s,
                                    &mut pending,
                                    &layer,
                                    child,
                                    applied(&state, &edits),
                                    &edits,
                                );
                            }
                        }
                        if scope == Scope::Workspace {
                            let files = a.layer_files(s, &layer, scope).expect("layer_files");
                            assert_eq!(files, findings.files, "{place:?}: the files of the state");
                        }
                        Ok(findings)
                    }
                    Err(e) => Err(std::mem::discriminant(&e)),
                };
                states.insert(place.clone(), state);
                assert!(ran.insert(place, summary).is_none());
            }
            Next::Expanded(id, _) => panic!("{id:?}: no expansion was asked for"),
            Next::Withdrawn(id) => panic!("{id:?} withdrawn though nobody cancelled it"),
            Next::Pending => panic!("no outcome within {WAIT:?}: the jobs are blocked"),
            Next::Idle => break,
        }
    }
    assert!(pending.is_empty(), "every job has its outcome: {pending:?}");
    a.close(s).expect("close");
    (ran, states)
}

/// A delay of 0 to 3 ms, a function of the seed and the job.
fn delay(seed: u64, id: JobId) -> Duration {
    let mut x = seed
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(id.get());
    x ^= x >> 33;
    x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    x ^= x >> 33;
    Duration::from_micros(x % 3000)
}

#[test]
fn parallel_jobs_equal_serial_and_the_reference() {
    let f = w_fixture("forks_parallel");
    // T = 8 and the caller only waits: W = 1, 4, 8 take 8, 2 and 1 thread(s) a job. No cut-off
    // (D1 off), so no outcome depends on the clock.
    let mut s = Analyzer::load(&analyzer(1, 8), &f.dir).expect("load W");

    // W = 1: the serial run every other run is compared with. Its order does not depend on the
    // delays, so it runs once.
    let (serial, states) = run_tree(&analyzer(1, 8), &mut s);
    assert_eq!(serial.len(), 85);
    let refused = serial.iter().filter(|(_, r)| r.is_err()).count();
    assert_eq!(refused, 1, "only the refused job fails: {serial:?}");
    let edit_kind = std::mem::discriminant(&CheckError::Edit {
        path: rel("a"),
        reason: String::new(),
    });
    assert_eq!(
        serial[&[4][..]],
        Err(edit_kind),
        "the refused job is an Edit"
    );

    // The reference: the flat check, on a twin session, of the edits that take the base to each
    // job's state, kept to the job's files.
    let reference = analyzer(1, 8);
    let mut twin = Analyzer::load(&reference, &f.dir).expect("load the twin");
    let mut compared = 0;
    for (place, found) in &serial {
        let Ok(found) = found else { continue };
        let flat = reference
            .reference_check(&mut twin, &composed(&states[place]), Scope::Workspace)
            .unwrap_or_else(|e| panic!("{place:?}: the reference check: {e:?}"));
        assert!(
            found.files.iter().all(|p| flat.files.contains(p)),
            "{place:?}: the job's files are files of its state"
        );
        assert_eq!(
            *found,
            flat.within(&found.files),
            "{place:?}: against the reference"
        );
        compared += 1;
    }
    assert_eq!(compared, 84);

    for width in [4, 8] {
        for seed in 0..20u64 {
            let a = analyzer(width, 8).with_job_hook(Some(Arc::new(move |id| {
                std::thread::sleep(delay(seed, id))
            })));
            assert_eq!(a.width(&s), width);
            let (ran, _) = run_tree(&a, &mut s);
            assert_eq!(ran, serial, "W = {width}, seed {seed}: against W = 1");
        }
    }
    eprintln!(
        "W4-3b: 85 jobs at W = 4 and 8 over 20 seeds each equal W = 1; {compared} against the reference"
    );
}

#[test]
fn a_refused_edit_is_a_done_without_a_layer() {
    let f = w_fixture("forks_refused_edit");
    let mut s = Analyzer::load(&analyzer(1, 2), &f.dir).expect("load W");
    let gate = Arc::new(Gate::default());
    let held = Arc::clone(&gate);
    let a = analyzer(1, 2).with_job_hook(Some(Arc::new(move |_| held.wait())));
    let root = a.open(&mut s).expect("open");

    // The one worker is held by a job in the pool.
    let edit = Edit::write(rel("a/src/x.rs"), format!("{A_X}{}", err_fn("held")));
    let held_job = a
        .submit(&mut s, &root, &[edit], Scope::Affected, Limit::Check)
        .expect("submit");
    // Refused by the resolution: a delete of a path that is not a file of the root's state.
    let refused = a
        .submit(
            &mut s,
            &root,
            &[Edit::delete(rel("a/src/nothing.rs"))],
            Scope::Affected,
            Limit::Check,
        )
        .expect("a refused edit is no error of submit");
    assert!(refused > held_job, "ids increase in submission order");

    // Its `Done` comes at once, while the worker is still held: it took no worker.
    match a.next(&mut s, Duration::ZERO).expect("next") {
        Next::Done(id, Err(CheckError::Edit { path, .. })) => {
            assert_eq!(id, refused);
            assert_eq!(path, rel("a/src/nothing.rs"));
        }
        other => panic!("the refused job's Done: {other:?}"),
    }
    assert!(matches!(
        a.next(&mut s, Duration::from_millis(50)).expect("next"),
        Next::Pending
    ));
    // Delivered: its id is no longer a job.
    assert_eq!(a.cancel(&mut s, refused), Err(CheckError::ForkInUse));

    gate.release();
    let outcomes = drain(&a, &mut s);
    assert_eq!(outcomes.len(), 1);
    assert!(
        matches!(outcome_of(&outcomes, held_job), Next::Done(_, Ok(_))),
        "{outcomes:?}"
    );
    drop(outcomes);
    drop(root);
    a.close(&mut s).expect("close");
    assert_eq!(a.text(&s, &rel("a/src/x.rs")).as_deref(), Some(A_X));
}

/// A workspace of one crate with ten files, each with an error: a diagnosis of the workspace
/// spreads over its threads (a scope under eight files runs on one).
fn wide_fixture(test: &str) -> Fixture {
    let mut lib = String::new();
    let mut files: Vec<(String, String)> = Vec::new();
    for i in 0..9 {
        lib.push_str(&format!("pub mod f{i};\n"));
        files.push((format!("src/f{i}.rs"), err_fn(&format!("f{i}"))));
    }
    files.push(("src/lib.rs".to_owned(), lib));
    files.push((
        "Cargo.toml".to_owned(),
        "[package]\nname = \"wide\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"
            .to_owned(),
    ));
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, t)| (p.as_str(), t.as_str()))
        .collect();
    Fixture::new(test, &refs)
}

#[test]
fn job_threads_times_jobs_fit_the_budget() {
    let f = wide_fixture("forks_budget");
    // T = 6.
    let mut s = Analyzer::load(&analyzer(1, 6), &f.dir).expect("load");
    let edit = |i: usize| {
        Edit::write(
            rel("src/f0.rs"),
            format!("{}{}", err_fn("f0"), err_fn(&format!("b{i}"))),
        )
    };

    // (N, caller works) → (W, t, the chunks of a diagnosis of the ten files): the caller's
    // thread counted out, T_w = 5: N = 2 is W = 2, t = 2; N = 3 is W = 3, t = 1; the caller only
    // waiting, T_w = 6: N = 8 is capped at W = 6, t = 1; N = 1 is W = 1, t = 6, whose ten files
    // make five chunks of two.
    for (n, caller_works, width, threads, chunks) in [
        (2, true, 2, 2, 2),
        (8, false, 6, 1, 1),
        (1, false, 1, 6, 5),
        (3, true, 3, 1, 1),
    ] {
        let count = Arc::new(ThreadCount::default());
        let met = Arc::new(Rendezvous::new(width));
        let a = analyzer_of(config(n, 6, caller_works, true))
            .with_thread_count(Some(Arc::clone(&count)))
            .with_job_hook(Some(Arc::new(move |_| met.meet())));
        assert_eq!(a.width(&s), width, "N = {n}, caller works {caller_works}");
        let root = a.open(&mut s).expect("open");
        assert_eq!(a.width(&s), width);
        for i in 0..2 * width {
            a.submit(&mut s, &root, &[edit(i)], Scope::Workspace, Limit::Check)
                .expect("submit");
        }
        drop(root);
        let outcomes = drain(&a, &mut s);
        assert_eq!(outcomes.len(), 2 * width);
        assert!(
            outcomes.iter().all(|o| matches!(o, Next::Done(_, Ok(_)))),
            "{outcomes:?}"
        );
        drop(outcomes);
        a.close(&mut s).expect("close");
        let workers = 6 - usize::from(caller_works);
        assert_eq!(
            count.most_in_one(),
            chunks,
            "N = {n}: a job's diagnosis spreads over at most t threads"
        );
        assert!(chunks <= threads);
        assert!(
            count.most() <= width * threads && width * threads <= workers,
            "N = {n}: {} threads diagnosed at once, W·t = {}, T_w = {workers}",
            count.most(),
            width * threads
        );
    }
}

#[test]
fn a_layer_of_another_session_is_refused() {
    let f = w_fixture("forks_brand");
    let a = analyzer(2, 2);
    let mut s = Analyzer::load(&a, &f.dir).expect("load");
    let mut other = Analyzer::load(&a, &f.dir).expect("load the other session");
    let root = a.open(&mut s).expect("open");
    let foreign = a.open(&mut other).expect("open the other");
    let x = rel("a/src/x.rs");
    let edit = [Edit::write(x.clone(), "pub fn xf() -> u32 {\n    7\n}\n")];

    assert_eq!(
        a.submit(&mut s, &foreign, &edit, Scope::Affected, Limit::Check),
        Err(CheckError::ForkInUse)
    );
    assert_eq!(a.layer_text(&s, &foreign, &x), Err(CheckError::ForkInUse));
    assert_eq!(
        a.layer_files(&s, &foreign, Scope::Workspace),
        Err(CheckError::ForkInUse)
    );
    // The brand comes before the poison.
    s.poison("test");
    assert_eq!(
        a.submit(&mut s, &foreign, &edit, Scope::Affected, Limit::Check),
        Err(CheckError::ForkInUse)
    );
    assert!(matches!(
        a.submit(&mut s, &root, &edit, Scope::Affected, Limit::Check),
        Err(CheckError::Poisoned(_))
    ));

    // The refusals changed nothing: the other session's forks run its own job.
    let id = a
        .submit(&mut other, &foreign, &edit, Scope::Affected, Limit::Check)
        .expect("submit on its own session");
    let outcomes = drain(&a, &mut other);
    match outcome_of(&outcomes, id) {
        Next::Done(_, Ok(checked)) => {
            assert_eq!(
                a.layer_text(&other, &checked.layer, &x)
                    .expect("layer_text")
                    .as_deref(),
                Some("pub fn xf() -> u32 {\n    7\n}\n")
            );
            assert_eq!(
                a.layer_text(&s, &checked.layer, &x),
                Err(CheckError::ForkInUse)
            );
        }
        other => panic!("{other:?}"),
    }
    drop(outcomes);
    drop(foreign);
    a.close(&mut other).expect("close the other");
    drop(root);
    assert!(matches!(a.close(&mut s), Err(CheckError::Poisoned(_))));
}

#[test]
fn session_operations_refuse_while_forks_are_open() {
    let f = w_fixture("forks_refuse");
    let a = analyzer(2, 2);
    let mut s = Analyzer::load(&a, &f.dir).expect("load");
    let x = rel("a/src/x.rs");
    let edit = Edit::write(x.clone(), format!("{A_X}{}", err_fn("open")));
    let root = a.open(&mut s).expect("open");

    let refusals = |a: &RaAnalyzer, s: &mut RaSession| {
        assert_eq!(
            a.check(s, std::slice::from_ref(&edit), Scope::Affected),
            Err(CheckError::ForkInUse)
        );
        assert!(matches!(Stacked::fork(a, s), Err(CheckError::ForkInUse)));
        assert_eq!(a.audit_revert(s), Err(CheckError::ForkInUse));
        assert!(matches!(a.open(s), Err(CheckError::ForkInUse)));
        assert!(Analyzer::fork(a, s).is_err());
        assert_eq!(
            a.reference_check(s, std::slice::from_ref(&edit), Scope::Affected),
            Err(CheckError::ForkInUse)
        );
        // The reads describe the base.
        assert_eq!(a.text(s, &x).as_deref(), Some(A_X));
    };
    refusals(&a, &mut s);
    let id = a
        .submit(
            &mut s,
            &root,
            std::slice::from_ref(&edit),
            Scope::Affected,
            Limit::Check,
        )
        .expect("submit");
    let outcomes = drain(&a, &mut s);
    assert!(matches!(outcome_of(&outcomes, id), Next::Done(_, Ok(_))));
    drop(outcomes);
    refusals(&a, &mut s);

    drop(root);
    a.close(&mut s).expect("close");
    a.check(&mut s, &[edit], Scope::Affected)
        .expect("a check once the forks are closed");
    assert_eq!(a.text(&s, &x).as_deref(), Some(A_X));
}

#[test]
fn expand_refuses_while_forks_are_open() {
    let f = w_fixture("forks_expand");
    let a = analyzer(1, 2);
    let mut s = Analyzer::load(&a, &f.dir).expect("load");
    let targets = [Target::new(rel("a/src/lib.rs"), None)];
    let before = Expander::expand(&a, &mut s, &[], &targets, 10).expect("expand at the base");

    let root = a.open(&mut s).expect("open");
    let refused = Expander::expand(&a, &mut s, &[], &targets, 10);
    assert!(
        matches!(refused, Err(ExpandError::Session(CheckError::ForkInUse))),
        "{refused:?}"
    );
    // The forks are as they were.
    let id = a
        .submit(
            &mut s,
            &root,
            &[Edit::write(rel("c/src/z.rs"), C_Z)],
            Scope::Affected,
            Limit::Check,
        )
        .expect("submit");
    let outcomes = drain(&a, &mut s);
    assert!(matches!(outcome_of(&outcomes, id), Next::Done(_, Ok(_))));
    drop(outcomes);
    drop(root);
    a.close(&mut s).expect("close");
    assert_eq!(
        Expander::expand(&a, &mut s, &[], &targets, 10).expect("expand"),
        before
    );
}

#[test]
fn a_refused_base_write_poisons_and_withdraws() {
    let f = w_fixture("forks_base_write");
    let mut s = Analyzer::load(&analyzer(1, 2), &f.dir).expect("load");
    let a = analyzer(1, 2).with_job_base_write(true);
    let root = a.open(&mut s).expect("open");
    let ids: Vec<JobId> = ["c/src/z.rs", "b/src/y.rs", "a/src/x.rs"]
        .iter()
        .map(|path| {
            let edit = Edit::write(rel(path), "pub fn w() {}\n");
            a.submit(&mut s, &root, &[edit], Scope::Affected, Limit::Check)
                .expect("submit")
        })
        .collect();
    drop(root);

    // Every job not yet delivered is withdrawn, the one that wrote the base included, then the
    // session is poisoned.
    let mut withdrawn = Vec::new();
    let poisoned = loop {
        match a.next(&mut s, WAIT) {
            Ok(Next::Withdrawn(id)) => withdrawn.push(id),
            Ok(other) => panic!("only withdrawals: {other:?}"),
            Err(e) => break e,
        }
    };
    assert!(
        matches!(&poisoned, CheckError::Poisoned(m) if m.contains("I2")),
        "{poisoned:?}"
    );
    withdrawn.sort();
    assert_eq!(withdrawn, ids);
    assert!(matches!(a.next(&mut s, WAIT), Err(CheckError::Poisoned(_))));
    // `close` still thaws the base, and says the poison.
    assert!(matches!(a.close(&mut s), Err(CheckError::Poisoned(_))));
    assert!(s.fork_counters().is_some_and(|c| !c.frozen), "thawed");
    assert!(matches!(
        a.check(&mut s, &[], Scope::Affected),
        Err(CheckError::Poisoned(_))
    ));
}

#[test]
fn a_refused_close_can_be_retried() {
    let f = w_fixture("forks_close");
    let a = analyzer(2, 2);
    let mut s = Analyzer::load(&a, &f.dir).expect("load");
    let x = rel("a/src/x.rs");
    let text = format!("{A_X}{}", err_fn("kept"));
    let root = a.open(&mut s).expect("open");
    let id = a
        .submit(
            &mut s,
            &root,
            &[Edit::write(x.clone(), text.clone())],
            Scope::Affected,
            Limit::Check,
        )
        .expect("submit");
    let queued = a
        .submit(
            &mut s,
            &root,
            &[Edit::write(rel("c/src/z.rs"), C_Z)],
            Scope::Affected,
            Limit::Check,
        )
        .expect("submit");
    let kept = match a.next(&mut s, WAIT).expect("next") {
        Next::Done(done, Ok(checked)) if done == id || done == queued => checked.layer,
        other => panic!("{other:?}"),
    };

    // A Layer is held: the pool is joined (the other job is not delivered), the base stays frozen.
    assert_eq!(a.close(&mut s), Err(CheckError::ForkInUse));
    assert!(s.fork_counters().is_none(), "the base is still frozen");
    // Closing: only the reads and `close`.
    assert_eq!(
        a.submit(&mut s, &kept, &[], Scope::Affected, Limit::Check),
        Err(CheckError::ForkInUse)
    );
    assert!(matches!(
        a.next(&mut s, Duration::ZERO),
        Err(CheckError::ForkInUse)
    ));
    assert_eq!(a.cancel(&mut s, queued), Err(CheckError::ForkInUse));
    assert_eq!(
        a.check(&mut s, &[], Scope::Affected),
        Err(CheckError::ForkInUse)
    );
    let at_layer = a.layer_text(&s, &kept, &x).expect("layer_text");
    assert!(
        at_layer.as_deref() == Some(text.as_str()) || at_layer.as_deref() == Some(A_X),
        "{at_layer:?}"
    );
    a.layer_files(&s, &kept, Scope::Affected)
        .expect("layer_files while closing");
    assert_eq!(a.text(&s, &x).as_deref(), Some(A_X));
    // Still held (the root too): refused again.
    drop(kept);
    assert_eq!(a.close(&mut s), Err(CheckError::ForkInUse));

    drop(root);
    a.close(&mut s)
        .expect("the close after the last Layer is dropped thaws");
    assert!(s.fork_counters().is_some_and(|c| !c.frozen));
    a.check(&mut s, &[], Scope::Affected)
        .expect("the session is usable again");
    // And the forks open again.
    let root = a.open(&mut s).expect("open again");
    drop(root);
    a.close(&mut s).expect("close");
}

#[test]
fn forks_need_the_engine_backend() {
    let f = w_fixture("forks_undo_log");
    let a = analyzer_of(config(2, 2, true, false));
    let mut s = Analyzer::load(&a, &f.dir).expect("load");
    assert!(matches!(a.open(&mut s), Err(CheckError::ForkInUse)));
    // Nothing opened: the session works as before.
    assert_eq!(a.close(&mut s), Err(CheckError::ForkInUse));
    assert!(matches!(
        a.next(&mut s, Duration::ZERO),
        Err(CheckError::ForkInUse)
    ));
    a.check(
        &mut s,
        &[Edit::write(rel("c/src/z.rs"), C_Z)],
        Scope::Affected,
    )
    .expect("a check on the undo log");
}

#[test]
fn sibling_jobs_in_one_query_do_not_block() {
    let f = w_fixture("forks_siblings");
    let mut s = Analyzer::load(&analyzer(2, 2), &f.dir).expect("load");
    // Armed once the parent is done: then each sibling waits, after its writes, for the other,
    // so both diagnose the same files of one parent's state at once.
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let met = Arc::new(Rendezvous::new(2));
    let (arm, meet) = (Arc::clone(&armed), Arc::clone(&met));
    let a = analyzer(2, 2).with_job_hook(Some(Arc::new(move |_| {
        if arm.load(std::sync::atomic::Ordering::SeqCst) {
            meet.meet();
        }
    })));
    let root = a.open(&mut s).expect("open");
    // The parent: a sealed layer of its own, not the base.
    let parent_edit = [Edit::write(
        rel("a/src/x.rs"),
        format!("{A_X}{}", err_fn("p")),
    )];
    let parent_id = a
        .submit(&mut s, &root, &parent_edit, Scope::Affected, Limit::Check)
        .expect("submit the parent");
    let parent = match a.next(&mut s, WAIT).expect("next") {
        Next::Done(id, Ok(checked)) if id == parent_id => checked.layer,
        other => panic!("{other:?}"),
    };
    drop(root);

    armed.store(true, std::sync::atomic::Ordering::SeqCst);
    // The same edit of `b/src/y.rs` (whose crate depends on `a`): one scope, the same queries.
    let sibling = [Edit::write(
        rel("b/src/y.rs"),
        format!("{B_Y}{}", err_fn("s")),
    )];
    let ids: Vec<JobId> = (0..2)
        .map(|_| {
            a.submit(&mut s, &parent, &sibling, Scope::Workspace, Limit::Check)
                .expect("submit a sibling")
        })
        .collect();
    let outcomes = drain(&a, &mut s);
    let found: Vec<&ScopedFindings> = ids
        .iter()
        .map(|id| match outcome_of(&outcomes, *id) {
            Next::Done(_, Ok(checked)) => &checked.findings,
            other => panic!("a sibling: {other:?}"),
        })
        .collect();
    assert_eq!(found[0], found[1], "siblings of one state find the same");
    drop(found);
    drop(outcomes);
    drop(parent);
    a.close(&mut s).expect("close");
}

#[test]
fn a_cancellation_is_cancelled_wherever_a_job_raises_it() {
    let f = Fixture::single(
        "forks_cancelled",
        "pub struct S;\npub fn f(_x: Missing) {}\n",
    );
    let mut s = Analyzer::load(&analyzer(1, 2), &f.dir).expect("load");
    let lib = rel("src/lib.rs");
    let edit = [Edit::write(
        lib.clone(),
        "pub struct S;\npub fn f(_x: Gone) {}\n",
    )];

    // Inside the diagnosis: a gap detector that raises salsa's cancellation.
    let in_detector =
        analyzer(1, 2).with_detector_cancel(Some((GapDetector::UnresolvedPath, lib.clone())));
    // In the job's own code, outside the diagnosis; and a propagated panic, which is a panic.
    let in_job = analyzer(1, 2).with_job_hook(Some(Arc::new(|_| {
        std::panic::resume_unwind(Box::new(Cancelled::Local))
    })));
    let propagated = analyzer(1, 2).with_job_hook(Some(Arc::new(|_| {
        std::panic::resume_unwind(Box::new(Cancelled::PropagatedPanic))
    })));
    for (a, want) in [
        (&in_detector, Some(CheckError::Cancelled)),
        (&in_job, Some(CheckError::Cancelled)),
        (&propagated, None),
    ] {
        let root = a.open(&mut s).expect("open");
        let id = a
            .submit(&mut s, &root, &edit, Scope::Affected, Limit::Check)
            .expect("submit");
        drop(root);
        let outcomes = drain(a, &mut s);
        match (outcome_of(&outcomes, id), &want) {
            (Next::Done(_, Err(e)), Some(want)) => assert_eq!(e, want),
            (Next::Done(_, Err(CheckError::Panicked(m))), None) => {
                assert!(m.contains("cancellation"), "{m}");
            }
            (other, _) => panic!("{want:?}: {other:?}"),
        }
        drop(outcomes);
        a.close(&mut s).expect("close");
    }
    assert!(s.detector_panics().is_empty(), "{:?}", s.detector_panics());
}

#[test]
fn a_cancel_withdraws_only_its_job() {
    let f = w_fixture("forks_cancel");
    let mut s = Analyzer::load(&analyzer(2, 2), &f.dir).expect("load");
    let gate = Arc::new(Gate::default());
    let held = Arc::clone(&gate);
    let a = analyzer(2, 2).with_job_hook(Some(Arc::new(move |_| held.wait())));
    let root = a.open(&mut s).expect("open");
    let z = rel("c/src/z.rs");
    let text = format!("{C_Z}{}", err_fn("kept"));
    // Two running siblings (held after their writes) and a queued one.
    let cancelled = a
        .submit(
            &mut s,
            &root,
            &[Edit::write(z.clone(), err_fn("gone"))],
            Scope::Affected,
            Limit::Check,
        )
        .expect("submit");
    let kept = a
        .submit(
            &mut s,
            &root,
            &[Edit::write(z.clone(), text.clone())],
            Scope::Affected,
            Limit::Check,
        )
        .expect("submit");
    let queued = a
        .submit(
            &mut s,
            &root,
            &[Edit::write(z.clone(), err_fn("q"))],
            Scope::Affected,
            Limit::Check,
        )
        .expect("submit");
    assert_eq!(a.cancel(&mut s, queued), Ok(true));
    assert_eq!(a.cancel(&mut s, cancelled), Ok(true));
    assert_eq!(a.cancel(&mut s, cancelled), Ok(false), "already withdrawn");
    gate.release();

    let outcomes = drain(&a, &mut s);
    assert_eq!(outcomes.len(), 3);
    assert!(matches!(
        outcome_of(&outcomes, cancelled),
        Next::Withdrawn(_)
    ));
    assert!(matches!(outcome_of(&outcomes, queued), Next::Withdrawn(_)));
    match outcome_of(&outcomes, kept) {
        Next::Done(_, Ok(checked)) => {
            assert_eq!(
                a.layer_text(&s, &checked.layer, &z).expect("layer_text"),
                Some(text)
            );
            // The parent is as it was.
            assert_eq!(
                a.layer_text(&s, &root, &z).expect("layer_text").as_deref(),
                Some(C_Z)
            );
        }
        other => panic!("{other:?}"),
    }
    drop(outcomes);
    for id in [cancelled, kept, queued] {
        assert_eq!(
            a.cancel(&mut s, id),
            Err(CheckError::ForkInUse),
            "delivered"
        );
    }
    drop(root);
    a.close(&mut s).expect("close");
}

/// A module whose functions need the trait solver (a trait, an impl, method calls, `Into`).
fn solver_module() -> String {
    "pub trait Speak {\n    fn speak(&self) -> u32;\n}\npub struct V(pub u32);\nimpl Speak for V {\n    fn speak(&self) -> u32 {\n        self.0\n    }\n}\npub fn use_v(v: V) -> u64 {\n    let n: u64 = v.speak().into();\n    n\n}\n".to_owned()
}

#[test]
fn a_worker_reused_across_layers_keeps_e6() {
    let f = Fixture::single("forks_e6", &solver_module());
    // W = 1 and T = 1: one worker, which diagnoses on its own thread (t = 1), runs every job.
    let a = analyzer(1, 1);
    let mut s = Analyzer::load(&a, &f.dir).expect("load");
    let lib = rel("src/lib.rs");
    // F: the impl returns a `u64`, so it no longer matches the trait. G: the impl is unchanged,
    // but `use_v` now asks `u8: From<u32>`. Each needs the trait solver, on the same items.
    let f_text = solver_module().replace(
        "fn speak(&self) -> u32 {\n        self.0",
        "fn speak(&self) -> u64 {\n        self.0 as u64",
    );
    let g_text = solver_module().replace("let n: u64", "let n: u8");
    let f_edit = [Edit::write(lib.clone(), f_text)];
    let g_edit = [Edit::write(lib.clone(), g_text)];

    let reference = analyzer(1, 1);
    let mut twin = Analyzer::load(&reference, &f.dir).expect("load the twin");
    let flat_f = reference
        .reference_check(&mut twin, &f_edit, Scope::Workspace)
        .expect("flat F");
    let flat_g = reference
        .reference_check(&mut twin, &g_edit, Scope::Workspace)
        .expect("flat G");
    assert_ne!(flat_f, flat_g, "the two edits differ in their findings");

    // Siblings on the root, one after the other on the one worker: F, G, then F again.
    let root = a.open(&mut s).expect("open");
    assert_eq!(a.width(&s), 1);
    let ids: Vec<JobId> = [&f_edit, &g_edit, &f_edit]
        .iter()
        .map(|edit| {
            a.submit(&mut s, &root, &edit[..], Scope::Workspace, Limit::Check)
                .expect("submit")
        })
        .collect();
    drop(root);
    let outcomes = drain(&a, &mut s);
    for (id, flat) in ids.iter().zip([&flat_f, &flat_g, &flat_f]) {
        match outcome_of(&outcomes, *id) {
            Next::Done(_, Ok(checked)) => {
                assert_eq!(&checked.findings, flat, "{id:?}: the layer's own result");
            }
            other => panic!("{id:?}: {other:?}"),
        }
    }
    drop(outcomes);
    a.close(&mut s).expect("close");
}

#[test]
fn isolation_holds_after_parallel_jobs() {
    let f = w_fixture("forks_isolation");
    let mut s = Analyzer::load(&analyzer(1, 4), &f.dir).expect("load W");
    let before = analyzer(1, 4)
        .check(&mut s, &[], Scope::Workspace)
        .expect("the base's check before open");
    let refused_before = s.fork_counters().expect("no fork is live").refused;

    // Every write sampled: `open` takes the base's fingerprint, and `close` compares the base
    // with it once the workers are joined.
    let a = analyzer_of(RaConfig {
        assert_revert_every: std::num::NonZeroU32::new(1),
        ..config(4, 4, false, true)
    })
    .with_job_hook(Some(Arc::new(|id| std::thread::sleep(delay(7, id)))));
    let root = a.open(&mut s).expect("open");
    let base = w_base();
    // Four children of the root, each with four children of its own: creates, deletes, revivals
    // and edits of every crate, side by side.
    let mut pending: HashMap<JobId, (State, usize)> = HashMap::new();
    for c in 0..4 {
        let edits = node_edits(&base, &format!("i{c}"), c);
        let id = a
            .submit(&mut s, &root, &edits, Scope::Affected, Limit::Check)
            .expect("submit");
        pending.insert(id, (applied(&base, &edits), 1));
    }
    drop(root);
    let mut done = 0;
    loop {
        match a.next(&mut s, WAIT).expect("next") {
            Next::Done(id, Ok(Checked { layer, .. })) => {
                let (state, depth) = pending.remove(&id).expect("a job of this run");
                done += 1;
                if depth == 2 {
                    continue;
                }
                for c in 0..4 {
                    let edits = node_edits(&state, &format!("i{done}{c}"), c);
                    let child = a
                        .submit(&mut s, &layer, &edits, Scope::Affected, Limit::Check)
                        .expect("submit");
                    pending.insert(child, (applied(&state, &edits), 2));
                }
            }
            Next::Done(id, Err(e)) => panic!("{id:?}: {e:?}"),
            Next::Expanded(id, _) => panic!("{id:?}: no expansion was asked for"),
            Next::Withdrawn(id) => panic!("{id:?} withdrawn though nobody cancelled it"),
            Next::Pending => panic!("no outcome within {WAIT:?}: the jobs are blocked"),
            Next::Idle => break,
        }
    }
    assert!(pending.is_empty());
    assert_eq!(done, 20);

    // The base is as `open` found it: the sampled fingerprint is preserved and no write was
    // refused (either would poison the session and fail the close).
    a.close(&mut s)
        .expect("close: the base's fingerprint is the one before open");
    let counters = s.fork_counters().expect("thawed");
    assert!(!counters.frozen);
    assert_eq!(counters.refused, refused_before, "I2: no write refused");
    for (path, text) in &base {
        assert_eq!(a.text(&s, &rel(path)), text.clone(), "{path}");
    }
    for created in ["a/src/m.rs", "c/src/n.rs"] {
        assert!(
            !s.vfs_has(&rel(created)),
            "{created} left the vfs at the thaw"
        );
    }
    assert_eq!(
        analyzer(1, 4)
            .check(&mut s, &[], Scope::Workspace)
            .expect("the base's check after close"),
        before,
        "the base diagnoses as before open"
    );
}

// ---------------------------------------------------------------------------------------------
// Expansions of a Layer (C10) and the limit of a job (C11).
// ---------------------------------------------------------------------------------------------

/// What an expansion gives, or how it failed.
type Expanded = Result<Vec<Blueprint>, ExpandError>;

/// The targets of the expansions below: every source file W's states can have.
fn w_targets() -> Vec<Target> {
    [
        "a/src/lib.rs",
        "a/src/x.rs",
        "a/src/m.rs",
        "b/src/lib.rs",
        "b/src/y.rs",
        "c/src/lib.rs",
        "c/src/z.rs",
        "c/src/n.rs",
    ]
    .iter()
    .map(|p| Target::new(rel(p), None))
    .collect()
}

/// The most children an expansion below proposes.
const MAX: usize = 40;

/// The layer of the one job `edits` on `parent`, once it ran.
fn layer_of(a: &RaAnalyzer, s: &mut RaSession, parent: &RaLayer, edits: &[Edit]) -> RaLayer {
    let id = a
        .submit(s, parent, edits, Scope::Affected, Limit::Check)
        .expect("submit");
    match a.next(s, WAIT).expect("next") {
        Next::Done(got, Ok(checked)) if got == id => checked.layer,
        other => panic!("{other:?}"),
    }
}

/// The `Expanded` of the one expansion of `layer`'s state `state`.
fn expansion_of(a: &RaAnalyzer, s: &mut RaSession, layer: &RaLayer, state: &State) -> Expanded {
    let fp = StateFp::of(&composed(state));
    let id = Forks::expand(a, s, layer, fp, &w_targets(), MAX).expect("expand");
    match a.next(s, WAIT).expect("next") {
        Next::Expanded(got, children) if got == id => children,
        other => panic!("{other:?}"),
    }
}

/// `Expander::expand` of `state` with no fork open: the reference of C10.
fn reference_expansion(a: &RaAnalyzer, s: &mut RaSession, state: &State) -> Expanded {
    Expander::expand(a, s, &composed(state), &w_targets(), MAX)
}

/// Whether some Blueprint of `children` splices `path`.
fn splices(children: &Expanded, path: &str) -> bool {
    children.as_ref().is_ok_and(|bps| {
        bps.iter()
            .any(|b| b.splices().iter().any(|x| x.path() == &rel(path)))
    })
}

#[test]
fn an_expansion_of_a_layer_is_that_of_its_state() {
    let f = w_fixture("forks_expansion");
    let a = analyzer(2, 2);
    let mut s = Analyzer::load(&a, &f.dir).expect("load W");
    // The root, a child (a body edit, a delete) and a grandchild (a created module).
    let base = w_base();
    let child_edits = node_edits(&base, "p", 1);
    let child_state = applied(&base, &child_edits);
    let grand_edits = node_edits(&child_state, "q", 2);
    let grand_state = applied(&child_state, &grand_edits);

    let root = a.open(&mut s).expect("open");
    let child = layer_of(&a, &mut s, &root, &child_edits);
    let grand = layer_of(&a, &mut s, &child, &grand_edits);
    let on_layers = [
        expansion_of(&a, &mut s, &root, &base),
        expansion_of(&a, &mut s, &child, &child_state),
        expansion_of(&a, &mut s, &grand, &grand_state),
    ];
    drop((root, child, grand));
    a.close(&mut s).expect("close");

    for (on_layer, state) in on_layers.iter().zip([&base, &child_state, &grand_state]) {
        let children = on_layer.as_ref().expect("an expansion of a layer");
        assert!(!children.is_empty(), "the state has children");
        assert_eq!(
            on_layer,
            &reference_expansion(&a, &mut s, state),
            "the same Blueprints, ids included"
        );
    }
}

#[test]
fn an_expansion_sees_a_file_the_state_created() {
    let f = w_fixture("forks_expansion_created");
    let a = analyzer(1, 2);
    let mut s = Analyzer::load(&a, &f.dir).expect("load W");
    // `a/src/m.rs` created, with a function to extract from and a module to move items into.
    let base = w_base();
    let edits = node_edits(&base, "c", 2);
    let state = applied(&base, &edits);
    assert!(has(&state, "a/src/m.rs") && !has(&base, "a/src/m.rs"));

    let root = a.open(&mut s).expect("open");
    let layer = layer_of(&a, &mut s, &root, &edits);
    let on_layer = expansion_of(&a, &mut s, &layer, &state);
    drop((root, layer));
    a.close(&mut s).expect("close");

    assert!(
        splices(&on_layer, "a/src/m.rs"),
        "no Blueprint of the created file among {:?}",
        on_layer.as_ref().map(Vec::len)
    );
    // With no fork open the expansion reads the state's files too.
    assert_eq!(on_layer, reference_expansion(&a, &mut s, &state));
}

/// The stacked top reads the files of its state too (stage 4, commit 3): before, it listed the
/// base's files, so a level that created a module gave no child of it. On both backends.
#[test]
fn a_stacked_top_expansion_sees_a_file_the_state_created() {
    common::both(a_stacked_top_expansion_sees_a_file_the_state_created_on);
}

fn a_stacked_top_expansion_sees_a_file_the_state_created_on() {
    let f = w_fixture("forks_stacked_top_created");
    let a = analyzer_of(config(1, 2, true, common::engine()));
    let mut s = Analyzer::load(&a, &f.dir).expect("load W");
    let base = w_base();
    let edits = node_edits(&base, "c", 2);
    let state = applied(&base, &edits);
    assert!(has(&state, "a/src/m.rs") && !has(&base, "a/src/m.rs"));

    let mut fork = Stacked::fork(&a, &mut s).expect("fork");
    Stacked::write(&a, &mut s, &mut fork, &edits).expect("write the level");
    let on_top = Expander::expand(&a, &mut s, &composed(&state), &w_targets(), MAX);
    Stacked::release(&a, &mut s, fork).expect("release");

    assert!(
        splices(&on_top, "a/src/m.rs"),
        "no Blueprint of the created file among {:?}",
        on_top.as_ref().map(Vec::len)
    );
    // The same as with no fork live (`Expander` C1).
    assert_eq!(on_top, reference_expansion(&a, &mut s, &state));
}

#[test]
fn expansions_run_beside_jobs_and_change_no_job() {
    let f = w_fixture("forks_expansion_beside");
    let mut s = Analyzer::load(&analyzer(1, 4), &f.dir).expect("load W");
    let base = w_base();
    let parent_edits = node_edits(&base, "p", 0);
    let parent_state = applied(&base, &parent_edits);
    let jobs: Vec<Vec<Edit>> = (0..4)
        .map(|c| node_edits(&parent_state, &format!("k{c}"), c))
        .collect();

    // Alone, at W = 1: each job, then each expansion.
    type Checks = Vec<Result<ScopedFindings, Discriminant<CheckError>>>;
    let alone = analyzer(1, 4);
    let root = alone.open(&mut s).expect("open");
    let parent = layer_of(&alone, &mut s, &root, &parent_edits);
    let mut checks_alone: Checks = Vec::new();
    for edits in &jobs {
        let id = alone
            .submit(&mut s, &parent, edits, Scope::Affected, Limit::Check)
            .expect("submit");
        let outcomes = drain(&alone, &mut s);
        checks_alone.push(match outcome_of(&outcomes, id) {
            Next::Done(_, Ok(c)) => Ok(c.findings.clone()),
            Next::Done(_, Err(e)) => Err(std::mem::discriminant(e)),
            other => panic!("{other:?}"),
        });
    }
    let expanded_alone = [
        expansion_of(&alone, &mut s, &root, &base),
        expansion_of(&alone, &mut s, &parent, &parent_state),
    ];
    drop((root, parent));
    alone.close(&mut s).expect("close");

    // Side by side, at W = 4, in seeded orders: the expansions between the jobs.
    for seed in 0..5u64 {
        let a = analyzer(4, 4).with_job_hook(Some(Arc::new(move |id| {
            std::thread::sleep(delay(seed, id))
        })));
        let root = a.open(&mut s).expect("open");
        let parent = layer_of(&a, &mut s, &root, &parent_edits);
        let targets = w_targets();
        let mut checks = Vec::new();
        let mut expansions = Vec::new();
        for (i, edits) in jobs.iter().enumerate() {
            checks.push(
                a.submit(&mut s, &parent, edits, Scope::Affected, Limit::Check)
                    .expect("submit"),
            );
            if i == 0 {
                let fp = StateFp::of(&composed(&base));
                expansions
                    .push(Forks::expand(&a, &mut s, &root, fp, &targets, MAX).expect("expand"));
            }
            if i == 2 {
                let fp = StateFp::of(&composed(&parent_state));
                expansions
                    .push(Forks::expand(&a, &mut s, &parent, fp, &targets, MAX).expect("expand"));
            }
        }
        let outcomes = drain(&a, &mut s);
        assert_eq!(outcomes.len(), 6, "one outcome each");
        let side_checks: Checks = checks
            .iter()
            .map(|id| match outcome_of(&outcomes, *id) {
                Next::Done(_, Ok(c)) => Ok(c.findings.clone()),
                Next::Done(_, Err(e)) => Err(std::mem::discriminant(e)),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(side_checks, checks_alone, "seed {seed}: the jobs as alone");
        for (id, want) in expansions.iter().zip(&expanded_alone) {
            match outcome_of(&outcomes, *id) {
                Next::Expanded(_, got) => assert_eq!(got, want, "seed {seed}: the expansion"),
                other => panic!("{other:?}"),
            }
        }
        drop(outcomes);
        drop((root, parent));
        a.close(&mut s).expect("close: an expansion wrote nothing");
    }
}

#[test]
fn an_expansion_counts_in_the_width() {
    let f = w_fixture("forks_expansion_width");
    let mut s = Analyzer::load(&analyzer(1, 2), &f.dir).expect("load W");
    let base = w_base();
    let edit = [Edit::write(
        rel("c/src/z.rs"),
        format!("{C_Z}{}", err_fn("w")),
    )];
    let fp = StateFp::of(&composed(&base));

    // Holds the one check job, then expands beside it: the expansion's outcome, and how long it
    // took to come with the job still held (`None` when it did not come within `wait`).
    let held_run = |width: usize, s: &mut RaSession, wait: Duration| {
        let gate = Arc::new(Gate::default());
        let held = Arc::clone(&gate);
        let a = analyzer(width, 2).with_job_hook(Some(Arc::new(move |_| held.wait())));
        let root = a.open(s).expect("open");
        let job = a
            .submit(s, &root, &edit, Scope::Affected, Limit::Check)
            .expect("submit");
        let started = std::time::Instant::now();
        let expansion = Forks::expand(&a, s, &root, fp, &w_targets(), MAX).expect("expand");
        let came = match a.next(s, wait).expect("next") {
            Next::Expanded(id, _) if id == expansion => Some(started.elapsed()),
            Next::Pending => None,
            other => panic!("{other:?}"),
        };
        gate.release();
        let outcomes = drain(&a, s);
        assert!(matches!(outcome_of(&outcomes, job), Next::Done(_, Ok(_))));
        if came.is_none() {
            assert!(matches!(
                outcome_of(&outcomes, expansion),
                Next::Expanded(_, Ok(_))
            ));
        }
        drop(outcomes);
        drop(root);
        a.close(s).expect("close");
        came
    };

    // At W = 2 the expansion runs beside the held job.
    let beside = held_run(2, &mut s, WAIT).expect("at W = 2 the expansion runs beside the job");
    // At W = 1 the held job has the one worker: the expansion waits for it, here three times as
    // long as it took beside it.
    let wait = (beside * 3).max(Duration::from_secs(1));
    assert_eq!(
        held_run(1, &mut s, wait),
        None,
        "at W = 1 an expansion is not run beside a running job"
    );
}

#[test]
fn an_expansions_panics_join_the_session_at_next() {
    let f = w_fixture("forks_expansion_panics");
    let a = analyzer(1, 2);
    let mut s = Analyzer::load(&a, &f.dir).expect("load W");
    let base = w_base();
    // Every add-derive call panics; the expansion contains it and goes on.
    s.inject_expander_fault(Some(CommandKind::AddDerive));
    let root = a.open(&mut s).expect("open");
    let id = Forks::expand(
        &a,
        &mut s,
        &root,
        StateFp::of(&composed(&base)),
        &w_targets(),
        MAX,
    )
    .expect("expand");
    // Nothing joins the session before `next` delivers the expansion, however far it ran.
    std::thread::sleep(Duration::from_millis(200));
    assert!(s.expander_panics().is_empty(), "joined before delivery");
    let children = match a.next(&mut s, WAIT).expect("next") {
        Next::Expanded(got, children) if got == id => children,
        other => panic!("{other:?}"),
    };
    assert!(
        children.is_ok(),
        "a contained panic fails no expansion: {children:?}"
    );
    let on_layer = s.expander_panics();
    assert!(
        !on_layer.is_empty()
            && on_layer
                .iter()
                .all(|p| p.kind == Some(CommandKind::AddDerive)),
        "{on_layer:?}"
    );
    drop(root);
    a.close(&mut s).expect("close");

    // The same panics as the expansion with no fork open records.
    let mut fresh = Analyzer::load(&a, &f.dir).expect("load W again");
    fresh.inject_expander_fault(Some(CommandKind::AddDerive));
    let reference = reference_expansion(&a, &mut fresh, &base);
    assert_eq!(children, reference);
    assert_eq!(on_layer, fresh.expander_panics());
}

#[test]
fn a_start_job_is_not_cut_off() {
    let f = w_fixture("forks_no_limit");
    let mut s = Analyzer::load(&analyzer(1, 2), &f.dir).expect("load W");
    let edits = node_edits(&w_base(), "s", 0);
    // Every job outlasts the cut-off before its diagnosis starts.
    let slow = |a: RaAnalyzer| {
        a.with_job_hook(Some(Arc::new(|_| {
            std::thread::sleep(Duration::from_millis(300))
        })))
    };
    let run = |a: &RaAnalyzer, s: &mut RaSession, limit: Limit| {
        let root = a.open(s).expect("open");
        let id = a
            .submit(s, &root, &edits, Scope::Affected, limit)
            .expect("submit");
        drop(root);
        let outcomes = drain(a, s);
        let got = match outcome_of(&outcomes, id) {
            Next::Done(_, Ok(c)) => Ok(c.findings.clone()),
            Next::Done(_, Err(e)) => Err(e.clone()),
            other => panic!("{other:?}"),
        };
        drop(outcomes);
        a.close(s).expect("close");
        got
    };
    let cut = slow(analyzer(1, 2).with_check_timeout(Some(50)));
    assert_eq!(
        run(&cut, &mut s, Limit::Check),
        Err(CheckError::TimedOut { limit_ms: 50 }),
        "a check job is cut off"
    );
    let unlimited = run(&cut, &mut s, Limit::None).expect("a job with no limit runs to its end");
    let reference = run(&slow(analyzer(1, 2)), &mut s, Limit::Check)
        .expect("the same job with no cut-off configured");
    assert_eq!(unlimited, reference);
}
