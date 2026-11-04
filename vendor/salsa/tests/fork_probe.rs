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
//! stamp lookups (stage 2) and the cancellation walk (stage 4) still walk the chain.

use std::sync::Arc;

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
    // The stamps are read before the durability test: every layer is visited and misses, on
    // every check (the durability shortcut of D11 reads the base's HIGH revision first).
    assert_eq!((first, second), (DEPTH + 1, DEPTH + 1));
    assert_eq!(top.stats().stamps, 0);
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

#[test]
fn no_cancel_no_walk() {
    let db = Db::default();
    let files: Vec<File> = (0..8)
        .map(|i| File::new(&db, format!("file {i}")))
        .collect();
    fork::freeze(&db);
    let (top, _below) = deep_chain(&db);
    let top_db = layered(&db, &top);

    let before = top.stats().cancel_walks;
    for &file in &files {
        assert_eq!(text_len(&top_db, file), 6);
    }
    let walks = top.stats().cancel_walks - before;
    // No token was cancelled, yet every fetch walks the ancestors' tokens once.
    assert_eq!(walks, files.len());
}
