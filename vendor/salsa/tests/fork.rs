// ryo-v2 fork: tests of the fork layer (src/fork/, D11 step 2a). Not an upstream test.
#![cfg(all(feature = "fork", feature = "inventory"))]

//! Layers over a frozen base: inputs per layer, the refusals, and thaw.
//!
//! The layer revision counter is process-wide and the tests of this file run in parallel, so no
//! test assumes a revision number, only how revisions compare.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use salsa::fork::{self, Layer};
use salsa::plumbing::ZalsaDatabase;
use salsa::{Database, Durability, Setter};

#[salsa::db]
#[derive(Clone, Default)]
struct Db {
    storage: salsa::Storage<Self>,
}

#[salsa::db]
impl salsa::Database for Db {}

#[salsa::input]
struct File {
    #[returns(clone)]
    text: String,
}

/// A handle of `db`'s base carrying `layer`.
fn layered(db: &Db, layer: &std::sync::Arc<Layer>) -> Db {
    let mut handle = db.clone();
    fork::set_layer(&mut handle, layer.clone());
    handle
}

/// Sets the text of `file` in the layer `db` carries.
fn write(db: &Db, file: File, text: &str) {
    fork::set_input_field::<File, _>(db, file, 0, |fields| fields.0 = text.to_owned());
}

/// The panic message of `f`, which must panic.
fn panic_message(f: impl FnOnce()) -> String {
    let payload = catch_unwind(AssertUnwindSafe(f)).expect_err("expected a panic");
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_default()
}

#[test]
fn fork_layer_reads_its_own_input() {
    let db = Db::default();
    let file = File::new(&db, "base".to_owned());
    fork::freeze(&db);

    let f = fork::root_layer(&db);
    let g = fork::root_layer(&db);
    let f_db = layered(&db, &f);
    let g_db = layered(&db, &g);
    write(&f_db, file, "in f");

    assert_eq!(file.text(&f_db), "in f");
    assert_eq!(file.text(&db), "base");
    assert_eq!(file.text(&g_db), "base");

    // A child of the sealed layer reads through the chain, and a write in it shadows its parent's.
    f.seal();
    let child = fork::child_layer(&f);
    let child_db = layered(&db, &child);
    assert_eq!(file.text(&child_db), "in f");
    write(&child_db, file, "in the child");
    assert_eq!(file.text(&child_db), "in the child");
    assert_eq!(file.text(&f_db), "in f");
    assert_eq!(file.text(&db), "base");

    // The handle's revision is its layer's, and the layer that wrote is what LOW last changed at.
    assert_eq!(f_db.zalsa().current_revision(), f.revision());
    assert_eq!(
        child_db.zalsa().last_changed_revision(Durability::LOW),
        child.revision()
    );
    assert!(child.revision() > f.revision());
    assert!(f.revision() > db.zalsa().current_revision());
    // A write-free layer keeps the base's LOW last-changed revision.
    assert_eq!(
        g_db.zalsa().last_changed_revision(Durability::LOW),
        db.zalsa().last_changed_revision(Durability::LOW)
    );
    // Clones keep the layer.
    assert_eq!(file.text(&f_db.clone()), "in f");
}

#[test]
fn a_write_to_a_frozen_base_panics() {
    let mut db = Db::default();
    let file = File::new(&db, "base".to_owned());
    fork::freeze(&db);
    let message = panic_message(|| {
        file.set_text(&mut db).to("changed".to_owned());
    });
    assert!(message.contains("write to a frozen base"), "{message}");
}

#[test]
fn a_setter_on_a_layered_handle_panics() {
    let db = Db::default();
    let file = File::new(&db, "base".to_owned());
    fork::freeze(&db);
    let layer = fork::root_layer(&db);
    let mut layer_db = layered(&db, &layer);

    // The base handle `db` stays alive: a setter that waited for it would never return.
    let (done, result) = mpsc::channel();
    thread::spawn(move || {
        let message = panic_message(|| {
            file.set_text(&mut layer_db).to("changed".to_owned());
        });
        done.send(message).unwrap();
    });
    let message = result
        .recv_timeout(Duration::from_secs(30))
        .expect("the setter waited instead of panicking");
    assert!(message.contains("a setter on a layered handle"), "{message}");
    assert_eq!(file.text(&db), "base");
}

#[test]
fn fork_write_above_low_is_refused() {
    let db = Db::default();
    let file = File::builder("base".to_owned())
        .durability(Durability::HIGH)
        .new(&db);
    fork::freeze(&db);
    let layer = fork::root_layer(&db);
    let layer_db = layered(&db, &layer);
    let message = panic_message(|| write(&layer_db, file, "changed"));
    assert!(message.contains("a layer writes only LOW fields"), "{message}");
    assert_eq!(file.text(&layer_db), "base");
    assert!(!layer.has_writes());
}

#[test]
fn a_write_to_a_sealed_layer_panics() {
    let db = Db::default();
    let file = File::new(&db, "base".to_owned());
    fork::freeze(&db);
    let layer = fork::root_layer(&db);
    let layer_db = layered(&db, &layer);
    layer.seal();
    let message = panic_message(|| write(&layer_db, file, "changed"));
    assert!(message.contains("write to a sealed layer"), "{message}");
    assert_eq!(file.text(&layer_db), "base");
}

#[test]
fn only_a_sealed_layer_gets_a_child() {
    let db = Db::default();
    fork::freeze(&db);
    let layer = fork::root_layer(&db);
    let message = panic_message(|| {
        fork::child_layer(&layer);
    });
    assert!(message.contains("only a sealed layer gets a child"), "{message}");
}

#[test]
fn a_layer_of_another_base_is_refused() {
    let db = Db::default();
    let other = Db::default();
    fork::freeze(&db);
    fork::freeze(&other);
    let layer = fork::root_layer(&other);
    let mut handle = db.clone();
    let message = panic_message(|| fork::set_layer(&mut handle, layer));
    assert!(message.contains("another base"), "{message}");
}

#[test]
fn an_input_created_under_a_layer_is_a_counted_shared_write() {
    let db = Db::default();
    fork::freeze(&db);
    let layer = fork::root_layer(&db);
    let layer_db = layered(&db, &layer);
    assert_eq!(fork::shared_writes(&db).slot_allocations, 0);
    let file = File::new(&layer_db, "new".to_owned());
    assert_eq!(file.text(&layer_db), "new");
    assert_eq!(fork::shared_writes(&db).slot_allocations, 1);
}

#[salsa::tracked(returns(copy))]
fn length(db: &dyn Database, file: File) -> usize {
    LENGTH_RUNS.fetch_add(1, Ordering::SeqCst);
    file.text(db).len()
}
static LENGTH_RUNS: AtomicUsize = AtomicUsize::new(0);

#[test]
fn a_tracked_function_under_a_layer_is_not_yet_supported() {
    let db = Db::default();
    let file = File::new(&db, "base".to_owned());
    fork::freeze(&db);
    let layer = fork::root_layer(&db);
    let layer_db = layered(&db, &layer);
    let message = panic_message(|| {
        length(&layer_db, file);
    });
    assert!(message.contains("not yet supported under a layer"), "{message}");
}

#[salsa::tracked(returns(copy))]
fn low_length(db: &dyn Database, file: File) -> usize {
    LOW_RUNS.fetch_add(1, Ordering::SeqCst);
    file.text(db).len()
}
static LOW_RUNS: AtomicUsize = AtomicUsize::new(0);

#[salsa::tracked(returns(copy))]
fn high_length(db: &dyn Database, file: File) -> usize {
    HIGH_RUNS.fetch_add(1, Ordering::SeqCst);
    file.text(db).len()
}
static HIGH_RUNS: AtomicUsize = AtomicUsize::new(0);

#[test]
fn thaw_skips_past_every_layer_revision() {
    let mut db = Db::default();
    let low = File::new(&db, "low".to_owned());
    let high = File::builder("high".to_owned())
        .durability(Durability::HIGH)
        .new(&db);
    assert_eq!(low_length(&db, low), 3);
    assert_eq!(high_length(&db, high), 4);
    let high_changed = db.zalsa().last_changed_revision(Durability::HIGH);

    fork::freeze(&db);
    let root = fork::root_layer(&db);
    root.seal();
    let child = fork::child_layer(&root);
    let child_db = layered(&db, &child);
    write(&child_db, low, "changed in a layer");
    let revisions = [root.revision(), child.revision()];

    // While a layered handle lives, thaw refuses at once.
    assert_eq!(
        fork::try_thaw(&mut db),
        Err(fork::ThawRefused { handles: 2 })
    );
    drop(child_db);
    drop((root, child));

    let thawed = fork::try_thaw(&mut db).expect("the base is the only handle");
    assert!(!fork::is_frozen(&db));
    assert_eq!(thawed, db.zalsa().current_revision());
    for revision in revisions {
        assert!(thawed > revision, "{thawed:?} <= layer revision {revision:?}");
    }
    // No base input changed: the HIGH last-changed revision is the one before the freeze, so the
    // HIGH memo stays shallow-valid, and neither memo re-executes.
    assert_eq!(
        db.zalsa().last_changed_revision(Durability::HIGH),
        high_changed
    );
    assert_eq!(high_length(&db, high), 4);
    assert_eq!(low_length(&db, low), 3);
    assert_eq!(HIGH_RUNS.load(Ordering::SeqCst), 1);
    assert_eq!(LOW_RUNS.load(Ordering::SeqCst), 1);
    // The layer's write never reached the base, and the thawed base takes writes again.
    assert_eq!(low.text(&db), "low");
    low.set_text(&mut db).to("after thaw".to_owned());
    assert_eq!(low_length(&db, low), 10);

    // A layer made after the thaw, of any base, has a revision above the thawed base's.
    let other = Db::default();
    fork::freeze(&other);
    assert!(fork::root_layer(&other).revision() > thawed);
}

#[test]
fn freeze_needs_the_only_handle_of_the_base() {
    let db = Db::default();
    let clone = db.clone();
    let message = panic_message(|| {
        fork::freeze(&db);
    });
    assert!(message.contains("the only handle of the base"), "{message}");
    assert!(!fork::is_frozen(&db));
    drop(clone);
    fork::freeze(&db);
    assert!(fork::is_frozen(&db));
}

#[test]
fn a_frozen_base_takes_no_input_from_a_base_handle() {
    let db = Db::default();
    fork::freeze(&db);
    let message = panic_message(|| {
        File::new(&db, "new".to_owned());
    });
    assert!(message.contains("write to a frozen base"), "{message}");
    assert_eq!(fork::shared_writes(&db).slot_allocations, 0);
}

#[salsa::input(singleton)]
struct Config {
    value: u32,
}

#[test]
fn a_singleton_input_under_a_layer_is_not_yet_supported() {
    let db = Db::default();
    fork::freeze(&db);
    let layer = fork::root_layer(&db);
    let layer_db = layered(&db, &layer);
    let message = panic_message(|| {
        Config::new(&layer_db, 1);
    });
    assert!(message.contains("not yet supported under a layer"), "{message}");
    assert!(Config::try_get(&db).is_none());
}

#[salsa::input]
struct Pair {
    #[returns(clone)]
    a: u32,
    #[returns(clone)]
    b: u32,
}

#[test]
fn writes_to_two_fields_from_two_handles_of_a_layer_are_both_kept() {
    let db = Db::default();
    let pair = Pair::new(&db, 0, 0);
    fork::freeze(&db);
    let layer = fork::root_layer(&db);
    let handles: Vec<Db> = (0..2).map(|_| layered(&db, &layer)).collect();
    thread::scope(|scope| {
        for (field, handle) in handles.into_iter().enumerate() {
            scope.spawn(move || {
                for value in 1..=200 {
                    fork::set_input_field::<Pair, _>(&handle, pair, field, |fields| {
                        if field == 0 {
                            fields.0 = value;
                        } else {
                            fields.1 = value;
                        }
                    });
                }
            });
        }
    });
    let layer_db = layered(&db, &layer);
    assert_eq!((pair.a(&layer_db), pair.b(&layer_db)), (200, 200));
    assert_eq!((pair.a(&db), pair.b(&db)), (0, 0));
}

#[test]
fn a_layer_cancellation_survives_another_handles_attach() {
    let db = Db::default();
    fork::freeze(&db);
    let layer = fork::root_layer(&db);
    let first = layered(&db, &layer);
    let second = layered(&db, &layer);

    layer.cancellation_token().cancel();
    // The end of an `attach` scope resets a base handle's token; it must not reset the layer's.
    first.attach(|_| ());
    assert!(layer.cancellation_token().is_cancelled());
    let cancelled = catch_unwind(AssertUnwindSafe(|| second.unwind_if_revision_cancelled()));
    assert!(cancelled.is_err());
}

#[test]
fn cancelling_a_layer_stops_its_descendants() {
    let db = Db::default();
    fork::freeze(&db);
    let root = fork::root_layer(&db);
    root.seal();
    let child = fork::child_layer(&root);
    let child_db = layered(&db, &child);
    let sibling = fork::root_layer(&db);
    let sibling_db = layered(&db, &sibling);

    root.cancellation_token().cancel();
    let cancelled = catch_unwind(AssertUnwindSafe(|| child_db.unwind_if_revision_cancelled()));
    assert!(cancelled.is_err());
    sibling_db.unwind_if_revision_cancelled();
    // The child's own token is untouched; the handle uses it.
    assert!(!child_db.cancellation_token().is_cancelled());
    child.cancellation_token().cancel();
    assert!(child_db.cancellation_token().is_cancelled());
}
