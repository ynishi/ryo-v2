//! The Engine's layers over a minimal salsa database: writes per layer, the refusals that never
//! wait, and the revisions across a thaw and a second freeze.
//!
//! The layer revision counter is process-wide and these tests run in parallel, so no test
//! assumes a revision number, only how revisions compare.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use ryo_v2_engine::{Engine, EngineDb, EngineError};
use salsa::plumbing::ZalsaDatabase;
use salsa::{Durability, Setter};

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

impl EngineDb for Db {
    type File = FileText;

    fn file_text(&self, file: FileText) -> Arc<str> {
        file.text(self)
    }

    fn set_file_text_in_layer(&self, file: FileText, text: Arc<str>) {
        salsa::fork::set_input_field::<FileText, _>(self, file, 0, |fields| fields.0 = text);
    }
}

#[salsa::tracked(returns(copy))]
fn length(db: &dyn salsa::Database, file: FileText) -> usize {
    file.text(db).len()
}

/// A base with one file, `a`.
fn base() -> (Db, FileText) {
    let db = Db::default();
    let file = FileText::new(&db, Arc::from("a"));
    (db, file)
}

/// Runs `f` on another thread and fails when it has not returned within the timeout: a call
/// that waited for a handle would never return.
fn within_timeout<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    let (done, result) = mpsc::channel();
    thread::spawn(move || done.send(f()).unwrap());
    result
        .recv_timeout(Duration::from_secs(30))
        .expect("the call waited instead of returning")
}

/// The refusal of a `thaw` that must refuse.
fn refused<T, E>(result: Result<T, E>) -> E {
    match result {
        Ok(_) => panic!("expected a refusal"),
        Err(e) => e,
    }
}

#[test]
fn a_write_is_seen_by_its_chain_only() {
    let (db, file) = base();
    let frozen = Engine::freeze(db);
    let root = frozen.root();

    let mut f = root.child();
    f.write(file, "f1");
    f.write(file, "f2");
    assert_eq!(&*f.db().file_text(file), "f2");
    let sibling = root.child();
    assert_eq!(&*sibling.db().file_text(file), "a");
    let f = f.seal().expect("no clone of the layer's database lives");

    let child = f.child();
    assert_eq!(&*child.db().file_text(file), "f2");
    assert_eq!(f.read(|db| db.file_text(file)), Ok(Arc::from("f2")));
    assert_eq!(&*f.text(file), "f2");
    assert_eq!(&*root.text(file), "a");
    assert_eq!(f.files(), vec![file]);
    assert!(root.files().is_empty());
    assert!(f.revision() > root.revision());
    assert!(child.revision() > f.revision());
}

#[test]
fn seal_refuses_while_a_db_clone_lives() {
    let (db, file) = base();
    let frozen = Engine::freeze(db);
    let mut open = frozen.root().child();
    open.write(file, "b");
    let clone = Db::clone(&open.db());

    let (open, error) = within_timeout(move || refused(open.seal()));
    assert_eq!(error, EngineError::InUse);
    assert_eq!(&*open.db().file_text(file), "b");

    drop(clone);
    let sealed = open.seal().expect("the clone is gone");
    assert_eq!(&*sealed.text(file), "b");
}

#[test]
fn a_clone_escaping_read_is_refused() {
    let (db, file) = base();
    let frozen = Engine::freeze(db);
    let mut open = frozen.root().child();
    open.write(file, "b");
    let sealed = open.seal().unwrap();

    let mut escaped = None;
    let refused = sealed.read(|db| {
        escaped = Some(db.clone());
        db.file_text(file)
    });
    assert_eq!(refused, Err(EngineError::InUse));
    // The escaped clone keeps only the throwaway child alive; it still reads the chain.
    assert_eq!(&*escaped.as_ref().unwrap().file_text(file), "b");

    drop(escaped);
    assert_eq!(sealed.read(|db| db.file_text(file)), Ok(Arc::from("b")));
}

#[test]
fn thaw_refuses_instead_of_waiting() {
    let (db, file) = base();
    let frozen = Engine::freeze(db);

    // A layer of the base alive.
    let sealed = frozen.root().child().seal().unwrap();
    let (frozen, error) = within_timeout(move || refused(frozen.thaw()));
    assert_eq!(error, EngineError::InUse);

    // No layer object left, but a clone of a layer's database alive: another handle of the base.
    let open = sealed.child();
    let clone = Db::clone(&open.db());
    drop((open, sealed));
    let (frozen, error) = within_timeout(move || refused(frozen.thaw()));
    assert_eq!(error, EngineError::InUse);

    drop(clone);
    let mut db = frozen.thaw().expect("no handle of the base is left");
    assert_eq!(&*file.text(&db), "a");
    file.set_text(&mut db).to(Arc::from("after thaw"));
    assert_eq!(&*file.text(&db), "after thaw");
}

#[test]
fn refreeze_after_base_writes_keeps_layers_above_the_base() {
    let (db, file) = base();
    assert_eq!(length(&db, file), 1);
    let frozen = Engine::freeze(db);
    let mut first = frozen.root().child();
    first.write(file, "in a layer");
    let first_revision = first.revision();
    drop(first);
    let mut db = frozen.thaw().unwrap();

    for text in ["b", "cc", "ddd"] {
        file.set_text(&mut db).to(Arc::from(text));
        assert_eq!(length(&db, file), text.len());
    }
    let base_revision = db.zalsa().current_revision();
    assert!(base_revision > first_revision);

    let frozen = Engine::freeze(db);
    assert_eq!(frozen.base_revision(), base_revision);
    let mut layer = frozen.root().child();
    assert!(layer.revision() > base_revision);
    layer.write(file, "eeee");
    // Every memo of the base was verified at or before the base's revision; the layer's write
    // is newer than that, so it invalidates them in the layer.
    let db = layer.db();
    assert!(db.zalsa().current_revision() > base_revision);
    assert!(db.zalsa().last_changed_revision(Durability::LOW) > base_revision);
    assert_eq!(&*db.file_text(file), "eeee");
}

#[test]
fn a_write_reaching_a_sealed_layer_panics() {
    let (db, file) = base();
    salsa::fork::freeze(&db);
    let layer = salsa::fork::root_layer(&db);
    let mut handle = db.clone();
    salsa::fork::set_layer(&mut handle, layer.clone());
    layer.seal();

    let panic = catch_unwind(AssertUnwindSafe(|| {
        handle.set_file_text_in_layer(file, Arc::from("b"));
    }))
    .expect_err("a write to a sealed layer panics");
    let message = panic.downcast_ref::<String>().cloned().unwrap_or_default();
    assert!(message.contains("write to a sealed layer"), "{message}");
    assert_eq!(&*handle.file_text(file), "a");
}
