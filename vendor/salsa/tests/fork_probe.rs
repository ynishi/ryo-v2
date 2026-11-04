// ryo-v2 fork: the walk counters of `fork-probe` on deep chains (D11, the chain views). Not an upstream test.
#![cfg(all(feature = "fork", feature = "fork-probe", feature = "inventory"))]

//! How many maps and layers a read under a layer touches, read from the per-layer counters of
//! `fork-probe` (`LayerStats`' doc-hidden fields). Each test pins the count of the code as it
//! is; a stage of the chain views that changes a read changes the test's assertion to its ceiling in the
//! same change.
//!
//! The chains are 41 layers: a root layer (depth 0) and 39 sealed children below an open top
//! layer at depth 40. No layer below the top ran a query, so a lookup that misses in the top
//! misses in every layer.
//!
//! Stage 1 of the chain views (the view of `fork/flat.rs`) bounds memo and tracked lookups;
//! stage 2 skips the stamp lookup for a memo above `LOW` (the durability shortcut) and keeps the
//! `LOW` stamp walk under a ceiling (`a_low_memo_stamp_lookup_is_bounded`); stage 4 walks the
//! ancestors' cancellation tokens only after a cancel (the cancel epoch).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use salsa::fork::{self, Layer};
use salsa::{Database, Durability};

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

#[salsa::tracked(debug)]
struct Item<'db> {
    name: String,
    #[tracked]
    #[returns(clone)]
    value: String,
}

#[salsa::tracked(returns(copy))]
fn make_item(db: &dyn Database, file: File) -> Item<'_> {
    Item::new(db, "item".to_owned(), file.text(db))
}

#[salsa::tracked(returns(copy))]
fn text_len(db: &dyn Database, file: File) -> usize {
    file.text(db).len()
}

/// The layers below the top.
const DEPTH: usize = 40;

/// A handle of `db`'s base carrying `layer`.
fn layered(db: &Db, layer: &Arc<Layer>) -> Db {
    let mut handle = db.clone();
    fork::set_layer(&mut handle, layer.clone());
    handle
}

/// The open top layer at depth `DEPTH` of a chain of write-free, query-free layers on the frozen
/// base of `db`, and the layers below it.
fn deep_chain(db: &Db) -> (Arc<Layer>, Vec<Arc<Layer>>) {
    let mut below = Vec::with_capacity(DEPTH);
    let mut layer = fork::root_layer(db);
    for _ in 0..DEPTH {
        layer.seal().expect("no handle carries the layer");
        below.push(layer.clone());
        layer = fork::child_layer(&layer);
    }
    (layer, below)
}

#[test]
fn a_memo_lookup_probes_at_most_four_maps_at_depth_40() {
    let db = Db::default();
    let file = File::new(&db, "base".to_owned());
    assert_eq!(text_len(&db, file), 4);
    fork::freeze(&db);
    let (top, below) = deep_chain(&db);
    let top_db = layered(&db, &top);

    // One lookup per fetch; the base memo is valid in the write-free chain and nothing is
    // stored in the top, so the repeat looks up again.
    let before = top.stats().chain_memo_probes;
    assert_eq!(text_len(&top_db, file), 4);
    let first = top.stats().chain_memo_probes - before;
    assert_eq!(text_len(&top_db, file), 4);
    let second = top.stats().chain_memo_probes - before - first;
    // The open top's memo and tracked maps, then its parent's chain view (memos, hidden ids):
    // four maps whatever the depth (a walk probed two maps of each of the 41 layers, 82).
    assert_eq!((first, second), (4, 4));
    assert_eq!(top.stats().memos, 0);

    // A handle of a sealed layer probes its own view only.
    let sealed = below.last().expect("a layer below the top");
    let sealed_db = layered(&db, sealed);
    let before = sealed.stats().chain_memo_probes;
    assert_eq!(text_len(&sealed_db, file), 4);
    assert_eq!(sealed.stats().chain_memo_probes - before, 2);
}

#[test]
fn a_tracked_lookup_probes_at_most_three_maps_at_depth_40() {
    let db = Db::default();
    let file = File::new(&db, "base".to_owned());
    let item = make_item(&db, file);
    fork::freeze(&db);
    let (top, _below) = deep_chain(&db);
    let top_db = layered(&db, &top);

    let before = top.stats().chain_tracked_probes;
    assert_eq!(item.value(&top_db), "base");
    let read = top.stats().chain_tracked_probes - before;
    // The open top's tracked map, then its parent's chain view (a walk probed the 41 layers'),
    // then the base slot.
    assert_eq!(read, 2);
    assert!(read <= 3);
}

#[test]
fn a_durable_memo_under_a_deep_chain_visits_no_stamp() {
    let db = Db::default();
    let file = File::builder("durable".to_owned())
        .durability(Durability::HIGH)
        .new(&db);
    let low = File::new(&db, "low".to_owned());
    assert_eq!(text_len(&db, file), 7);
    fork::freeze(&db);
    let (top, _below) = deep_chain(&db);
    let top_db = layered(&db, &top);
    // The top layer writes a LOW input: LOW changed in it, HIGH did not.
    fork::set_input_field::<File, _>(&top_db, low, 0, |fields| fields.0 = "changed".to_owned());

    let before = top.stats().stamp_visits;
    assert_eq!(text_len(&top_db, file), 7);
    let first = top.stats().stamp_visits - before;
    assert_eq!(text_len(&top_db, file), 7);
    let second = top.stats().stamp_visits - before - first;
    // The base's HIGH revision is read before any stamp (the durability shortcut, D11): no layer is visited (a
    // walk read the stamps first and visited the 41 layers on every check).
    assert_eq!((first, second), (0, 0));
    assert_eq!(top.stats().stamps, 0);
    assert_eq!(top.stats().memos, 0);
}

/// The layers of the diagnosed chain of [`a_low_memo_stamp_lookup_is_bounded`].
const DIAGNOSED: usize = 20;

/// The stamp-walk ceiling of the chain views (SEARCH_LEDGER D11): a LOW memo's stamp is still looked
/// up layer by layer, which is bounded on the paths the search takes. On a chain of 20 layers,
/// each of which wrote a LOW input and checked a LOW and a HIGH base memo:
/// - a warm repeat in the top layer finds the LOW memo's stamp in the top: 1 visit per check;
/// - a fresh child's first check finds it in its parent: at most 2 visits;
/// - the HIGH memo visits no layer (the durability shortcut).
///
/// If this ceiling fails, the stamp view (the last stage of the chain views) is due.
#[test]
fn a_low_memo_stamp_lookup_is_bounded() {
    let db = Db::default();
    let low = File::new(&db, "low".to_owned());
    let high = File::builder("high".to_owned())
        .durability(Durability::HIGH)
        .new(&db);
    let written: Vec<File> = (0..DIAGNOSED)
        .map(|i| File::new(&db, format!("written {i}")))
        .collect();
    assert_eq!(text_len(&db, low), 3);
    assert_eq!(text_len(&db, high), 4);
    fork::freeze(&db);

    // Each layer writes a LOW input, so LOW changed in it, and then checks both memos: the LOW
    // memo deep-verifies and is stamped in the layer, the HIGH memo passes on durability.
    let mut layer = fork::root_layer(&db);
    for (i, &file) in written.iter().enumerate() {
        let layer_db = layered(&db, &layer);
        fork::set_input_field::<File, _>(&layer_db, file, 0, |fields| {
            fields.0 = format!("layer {i}");
        });
        assert_eq!(text_len(&layer_db, low), 3);
        assert_eq!(text_len(&layer_db, high), 4);
        drop(layer_db);
        if i + 1 < DIAGNOSED {
            layer.seal().expect("no handle carries the layer");
            layer = fork::child_layer(&layer);
        }
    }
    let top = layer;
    assert_eq!(top.stats().memos, 0, "both base memos stay valid");
    assert_eq!(top.stats().stamps, 1, "the LOW memo is stamped in the top");

    // A warm repeat in the top layer: the LOW memo's stamp is the top's own.
    let top_db = layered(&db, &top);
    let before = top.stats().stamp_visits;
    assert_eq!(text_len(&top_db, low), 3);
    assert_eq!(text_len(&top_db, high), 4);
    assert_eq!(top.stats().stamp_visits - before, 1);
    drop(top_db);

    // A fresh child of the diagnosed chain: the LOW memo's newest stamp is its parent's.
    top.seal().expect("no handle carries the layer");
    let child = fork::child_layer(&top);
    let child_db = layered(&db, &child);
    assert_eq!(text_len(&child_db, low), 3);
    assert_eq!(text_len(&child_db, high), 4);
    let first = child.stats().stamp_visits;
    assert!(first <= 2, "a fresh child's first check visited {first} layers");
    assert_eq!(child.stats().memos, 0);
}

#[test]
fn the_chain_unique_counts_are_the_union_over_the_chain() {
    let db = Db::default();
    let (a, b) = (File::new(&db, "a".to_owned()), File::new(&db, "bb".to_owned()));
    fork::freeze(&db);
    let root = fork::root_layer(&db);
    assert_eq!(text_len(&layered(&db, &root), a), 1);
    root.seal().expect("no handle carries the layer");
    let child = fork::child_layer(&root);
    let child_db = layered(&db, &child);
    // `a` is the root's memo, valid in the child; `b` is computed in the child.
    assert_eq!(text_len(&child_db, a), 1);
    assert_eq!(text_len(&child_db, b), 2);
    let item = make_item(&child_db, b);
    assert_eq!(item.value(&child_db), "bb");

    let stats = child.stats();
    assert_eq!((root.stats().memos, stats.memos), (1, 2));
    assert_eq!(stats.chain_unique_memo_keys, 3);
    assert_eq!(stats.chain_unique_tracked_ids, 0, "a new struct has a slot, no entry");
    assert_eq!(stats.chain_unique_tombstoned_ids, 0);
    assert_eq!(root.stats().chain_unique_memo_keys, 1);
}

/// The cancel epoch is process-wide: a test that cancels a token makes every layer of a test
/// running beside it walk once more, so the tests that cancel or count cancellation walks take
/// this lock.
static CANCEL_SERIAL: Mutex<()> = Mutex::new(());

fn cancel_serial() -> MutexGuard<'static, ()> {
    CANCEL_SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

#[test]
fn no_cancel_no_walk() {
    let _serial = cancel_serial();
    let db = Db::default();
    let files: Vec<File> = (0..8)
        .map(|i| File::new(&db, format!("file {i}")))
        .collect();
    fork::freeze(&db);
    let (top, _below) = deep_chain(&db);
    let top_db = layered(&db, &top);

    // A layer's cached answer starts at "no cancel" (epoch 0); a cancel of a token earlier in
    // the process makes the first check walk once, and no later check walks.
    assert_eq!(text_len(&top_db, files[0]), 6);
    let before = top.stats().cancel_walks;
    assert!(before <= 1, "the first fetch walked {before} times");
    for &file in &files {
        assert_eq!(text_len(&top_db, file), 6);
    }
    // No token was cancelled since: no fetch walks the ancestors' tokens (a walk on every fetch
    // walked 8 times).
    assert_eq!(top.stats().cancel_walks - before, 0);
}

#[test]
fn a_repeated_cancel_bumps_the_epoch_once() {
    let _serial = cancel_serial();
    let db = Db::default();
    let files: Vec<File> = (0..4)
        .map(|i| File::new(&db, format!("file {i}")))
        .collect();
    fork::freeze(&db);
    let (top, _below) = deep_chain(&db);
    let top_db = layered(&db, &top);
    // A sibling chain's token, cancelled again and again (a deadline loop).
    let sibling = fork::root_layer(&db);
    let token = sibling.cancellation_token();

    assert_eq!(text_len(&top_db, files[0]), 6);
    let before = top.stats().cancel_walks;
    for (i, &file) in files.iter().enumerate() {
        for _ in 0..16 {
            token.cancel();
        }
        assert_eq!(text_len(&top_db, file), 6, "fetch {i}");
    }
    // Only the first cancel raised the epoch: one walk, at the next fetch; the top's chain is
    // not cancelled (a walk on every fetch walked 4 times).
    assert_eq!(top.stats().cancel_walks - before, 1);
}

#[test]
fn a_cancel_after_a_cached_answer_is_seen() {
    let _serial = cancel_serial();
    let db = Db::default();
    let file = File::new(&db, "file".to_owned());
    fork::freeze(&db);
    let (top, below) = deep_chain(&db);
    let top_db = layered(&db, &top);
    assert_eq!(text_len(&top_db, file), 4);
    let walks = top.stats().cancel_walks;

    // The cached "not cancelled" is dropped by the epoch the cancel raises: the next check walks,
    // finds the cancelled ancestor and unwinds, and keeps that answer without walking again.
    below[DEPTH / 2].cancellation_token().cancel();
    for _ in 0..3 {
        let cancelled = catch_unwind(AssertUnwindSafe(|| top_db.unwind_if_revision_cancelled()));
        assert!(cancelled.is_err());
    }
    assert_eq!(top.stats().cancel_walks - walks, 1);
}
