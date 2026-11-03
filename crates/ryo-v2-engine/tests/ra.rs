//! The Engine bound to rust-analyzer's database: the revision rust-analyzer reads through the
//! handle is the layer's in and out of queries (D11, E6), the diagnostic threads read the layer,
//! and a created file takes no write lock.

use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use ra_ap_base_db::{FileId, SourceDatabase};
use ra_ap_ide::AnalysisHost;
use ra_ap_ide_db::RootDatabase;
use ryo_v2_core::app::ports::{Analyzer, Verifier};
use ryo_v2_core::domain::{Edit, RelPath, Scope};
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_engine::{Engine, EngineDb};
use ryo_v2_ra::{RaAnalyzer, RaConfig, RaSession};

/// A temp workspace that removes itself on drop.
struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    /// One package `probe` whose `src/lib.rs` declares `modules` modules `m0`.. with the texts
    /// given.
    fn new(test: &str, modules: &[String]) -> Self {
        let dir = std::env::temp_dir().join(format!("ryo-v2-engine-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut files = vec![
            (
                "Cargo.toml".to_owned(),
                "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"
                    .to_owned(),
            ),
            (
                "src/lib.rs".to_owned(),
                (0..modules.len()).map(|i| format!("pub mod m{i};\n")).collect(),
            ),
        ];
        for (i, text) in modules.iter().enumerate() {
            files.push((format!("src/m{i}.rs"), text.clone()));
        }
        for (rel, text) in &files {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).unwrap();
            std::fs::write(path, text).unwrap();
        }
        Self { dir }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn rel(s: &str) -> RelPath {
    RelPath::parse(s).unwrap()
}

fn analyzer(threads: usize) -> RaAnalyzer {
    RaAnalyzer::new(
        RaConfig {
            proc_macro: false,
            threads: Some(threads),
            assert_revert: false,
            gap_detectors: true,
            assert_revert_every: None,
            cfg_test: false,
        },
        Arc::new(MonotonicClock::new()),
        Arc::new(RecordingObserver::default()),
    )
}

/// A module whose functions need the trait solver (a trait, an impl, method calls, `Into`).
fn solver_module(i: usize) -> String {
    format!(
        "pub trait Speak{i} {{\n    fn speak(&self) -> u32;\n}}\npub struct V{i}(pub u32);\nimpl Speak{i} for V{i} {{\n    fn speak(&self) -> u32 {{\n        self.0\n    }}\n}}\npub fn use{i}(v: V{i}) -> u64 {{\n    let n: u64 = v.speak().into();\n    n\n}}\n"
    )
}

fn modules(n: usize) -> Vec<String> {
    (0..n).map(solver_module).collect()
}

/// A session and its twin of `fixture`, the session's database frozen into an Engine.
fn frozen(
    a: &RaAnalyzer,
    fixture: &Fixture,
) -> (RaSession, RaSession, ryo_v2_engine::Frozen<RootDatabase>) {
    let mut s = Analyzer::load(a, &fixture.dir).expect("load");
    let twin = Analyzer::load(a, &fixture.dir).expect("load the twin");
    let frozen = Engine::freeze(s.lend_database());
    (s, twin, frozen)
}

/// The file of module `i`.
fn module(i: usize) -> String {
    format!("src/m{i}.rs")
}

#[test]
fn solver_cache_does_not_cross_layers() {
    // One thread diagnoses F, then its sibling G, on the same thread: the trait solver's
    // thread-local cache is keyed by the handle's (nonce, revision), which differ between
    // layers, so G's results are G's own.
    let a = analyzer(1);
    let fixture = Fixture::new("solver_cache", &modules(2));
    let (s, mut twin, frozen) = frozen(&a, &fixture);
    let root = frozen.root();
    let file = s.file_id(&rel(&module(0))).unwrap();
    // F: the impl returns a `u64`, so `.into()` on it into `u64` is fine but the impl no longer
    // matches the trait. G: the impl is unchanged, but `use0` now asks `u8: From<u32>`.
    let f_text = solver_module(0).replace(
        "fn speak(&self) -> u32 {\n        self.0",
        "fn speak(&self) -> u64 {\n        self.0 as u64",
    );
    let g_text = solver_module(0).replace("let n: u64", "let n: u8");
    let mut f = root.child();
    f.write(file, f_text.as_str()).expect("F is writable");
    let mut g = root.child();
    g.write(file, g_text.as_str()).expect("G is writable");

    let in_f = a.diagnose_database(&s, &f.db()).expect("F");
    let in_g = a.diagnose_database(&s, &g.db()).expect("G");
    let flat_f = a
        .check(
            &mut twin,
            &[Edit::write(rel(&module(0)), f_text)],
            Scope::Workspace,
        )
        .expect("flat F");
    let flat_g = a
        .check(
            &mut twin,
            &[Edit::write(rel(&module(0)), g_text)],
            Scope::Workspace,
        )
        .expect("flat G");
    assert_ne!(
        flat_f.findings, flat_g.findings,
        "the two edits differ in their findings"
    );
    assert_eq!(in_f, flat_f);
    assert_eq!(in_g, flat_g);
    // And back to F on the same thread.
    assert_eq!(a.diagnose_database(&s, &f.db()).expect("F again"), flat_f);
}

#[test]
fn revision_is_the_layers_through_attach() {
    let a = analyzer(1);
    let fixture = Fixture::new("through_attach", &modules(1));
    let (_s, _twin, frozen) = frozen(&a, &fixture);
    let layer = frozen.root().child();
    let db = layer.db();
    // No query runs: the revision is read outside a query, as rust-analyzer's `attach` reads it
    // to reset the solver cache.
    let through_hir = ra_ap_hir_ty::attach_db(&*db, || db.nonce_and_revision().1);
    let through_salsa = salsa::attach(&*db, || db.nonce_and_revision().1);
    assert_eq!(through_hir, layer.revision());
    assert_eq!(through_salsa, layer.revision());
    assert!(layer.revision() > frozen.base_revision());
    // A sibling's differs.
    let sibling = frozen.root().child();
    assert_ne!(sibling.db().nonce_and_revision().1, layer.revision());
    assert_eq!(
        sibling.db().nonce_and_revision().0,
        db.nonce_and_revision().0
    );
}

#[test]
fn revision_is_the_layers_through_new_with() {
    let a = analyzer(1);
    let fixture = Fixture::new("through_new_with", &modules(1));
    let (_s, _twin, frozen) = frozen(&a, &fixture);
    let layer = frozen.root().child();
    let db = layer.db();
    let krate = ra_ap_base_db::all_crates(&*db)
        .iter()
        .copied()
        .next()
        .expect("a crate");
    // `new_with` resets the solver cache from the handle's revision, with no query running.
    let interner = ra_ap_hir_ty::next_solver::DbInterner::new_with(&*db, krate);
    assert_eq!(interner.db().nonce_and_revision().1, layer.revision());
}

#[test]
fn diagnostic_threads_read_the_layer() {
    // Ten files, four threads: the diagnosis chunks the files over `host.analysis()` clones and
    // database clones on spawned threads, each of which keeps the layer.
    let a = analyzer(4);
    let fixture = Fixture::new("threads", &modules(10));
    let (s, mut twin, frozen) = frozen(&a, &fixture);
    let mut layer = frozen.root().child();
    let mut edits = Vec::new();
    for i in [1, 4, 7, 9] {
        let text = solver_module(i).replace(".into()", ".missing_on_a_layer()");
        layer
            .write(s.file_id(&rel(&module(i))).unwrap(), text.as_str())
            .expect("the layer is writable");
        edits.push(Edit::write(rel(&module(i)), text));
    }
    let flat = a.check(&mut twin, &edits, Scope::Workspace).expect("flat");
    assert_eq!(flat.findings.len(), 4);
    assert_eq!(a.diagnose_database(&s, &layer.db()).expect("layer"), flat);

    // The clones on threads read the layer's text.
    let file = s.file_id(&rel(&module(4))).unwrap();
    let host = AnalysisHost::with_database(RootDatabase::clone(&layer.db()));
    thread::scope(|scope| {
        for _ in 0..4 {
            let analysis = host.analysis();
            scope.spawn(move || {
                let text = analysis.file_text(file).expect("not cancelled");
                assert!(text.contains("missing_on_a_layer"));
            });
        }
    });
    drop(host);
    assert!(layer.seal().is_ok(), "every clone dropped with its thread");
}

#[test]
fn creating_a_file_in_a_layer_never_waits() {
    let a = analyzer(1);
    let fixture = Fixture::new("create", &modules(1));
    let (_s, _twin, frozen) = frozen(&a, &fixture);
    // Another handle of the base alive: a write lock on the base would wait for it.
    let other = frozen.root().child();
    let held = RootDatabase::clone(&other.db());
    // An id no file of the session has (the vfs counts up from 0).
    let new_file = FileId::from_raw(0x7fff_0000);
    let (done, result) = mpsc::channel();
    let frozen = Arc::new(frozen);
    let in_thread = Arc::clone(&frozen);
    thread::spawn(move || {
        let mut layer = in_thread.root().child();
        layer
            .create(new_file, "pub fn created() {}\n")
            .expect("the layer is writable");
        let text = EngineDb::file_text(&*layer.db(), new_file);
        done.send(text).unwrap();
    });
    let text = result
        .recv_timeout(Duration::from_secs(60))
        .expect("the creation waited for another handle");
    assert_eq!(&*text, "pub fn created() {}\n");
    drop(held);
}

#[test]
fn two_layers_create_the_same_file() {
    // rust-analyzer's file map is shared by every handle of the base: the first creation takes
    // its vacant entry; a sibling's, and a descendant's, must not take the occupied entry's
    // setter (which panics on a layered handle).
    let a = analyzer(1);
    let fixture = Fixture::new("create_twice", &modules(1));
    let (_s, _twin, frozen) = frozen(&a, &fixture);
    let new_file = FileId::from_raw(0x7fff_0001);
    let root = frozen.root();
    let mut first = root.child();
    first
        .create(new_file, "pub fn first() {}\n")
        .expect("first is writable");
    let mut second = root.child();
    second
        .create(new_file, "pub fn second() {}\n")
        .expect("second is writable");
    assert_eq!(
        &*EngineDb::file_text(&*first.db(), new_file),
        "pub fn first() {}\n"
    );
    assert_eq!(
        &*EngineDb::file_text(&*second.db(), new_file),
        "pub fn second() {}\n"
    );
    let first = first.seal().expect("no clone alive");
    let mut grandchild = first.child();
    grandchild
        .create(new_file, "pub fn third() {}\n")
        .expect("the grandchild is writable");
    assert_eq!(
        &*EngineDb::file_text(&*grandchild.db(), new_file),
        "pub fn third() {}\n"
    );
    assert_eq!(&*first.text(new_file), "pub fn first() {}\n");
}
