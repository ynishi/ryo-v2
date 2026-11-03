// ryo-v2 fork: tests of memos, tracked structs, interned values and claims under a layer (D11 step 2b). Not an upstream test.
#![cfg(all(feature = "fork", feature = "inventory"))]

//! Tracked functions, tracked structs, interned values, cycles and claims under layers.
//!
//! The layer revision counter is process-wide and the tests of this file run in parallel, so no
//! test assumes a revision number. Every test has its own tracked functions where it counts runs.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, mpsc};
use std::thread;
use std::time::Duration;

use salsa::fork::{self, Layer};
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
fn layered(db: &Db, layer: &Arc<Layer>) -> Db {
    let mut handle = db.clone();
    fork::set_layer(&mut handle, layer.clone());
    handle
}

/// Sets the text of `file` in the layer `db` carries.
fn write(db: &Db, file: File, text: &str) {
    fork::set_input_field::<File, _>(db, file, 0, |fields| fields.0 = text.to_owned());
}

/// A sealed child of `parent` that wrote `text` into `file`.
fn sealed_child(db: &Db, parent: &Arc<Layer>, file: File, text: &str) -> Arc<Layer> {
    let child = fork::child_layer(parent);
    write(&layered(db, &child), file, text);
    child.seal();
    child
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

/// Runs `f` on another thread and fails when it does not return within the timeout.
fn within_timeout<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    let (done, result) = mpsc::channel();
    thread::spawn(move || done.send(f()).unwrap());
    result
        .recv_timeout(Duration::from_secs(60))
        .expect("the layers blocked each other")
}

// ---------------------------------------------------------------------------------------------
// Stamps (E1, E2, E3)

#[salsa::tracked(returns(copy))]
fn stamp_len(db: &dyn Database, file: File) -> usize {
    STAMP_RUNS.fetch_add(1, Ordering::SeqCst);
    file.text(db).len()
}
static STAMP_RUNS: AtomicUsize = AtomicUsize::new(0);

#[test]
fn fork_stamps_do_not_leak_to_siblings() {
    let db = Db::default();
    let a = File::new(&db, "aaa".to_owned());
    let b = File::new(&db, "b".to_owned());
    assert_eq!(stamp_len(&db, a), 3);
    fork::freeze(&db);
    let runs = STAMP_RUNS.load(Ordering::SeqCst);

    // F writes an unrelated input: the base memo of `a` is deep-verified in F and stamped there.
    let f = fork::root_layer(&db);
    let f_db = layered(&db, &f);
    write(&f_db, b, "changed in f");
    assert_eq!(stamp_len(&f_db, a), 3);
    assert_eq!(STAMP_RUNS.load(Ordering::SeqCst), runs);
    assert_eq!(f.stats().deep_verifies, 1);
    assert_eq!(f.stats().stamps, 1);

    // G changed `a`: F's stamp is not on G's chain, so the base memo is not valid in G.
    let g = fork::root_layer(&db);
    let g_db = layered(&db, &g);
    write(&g_db, a, "a");
    assert_eq!(stamp_len(&g_db, a), 1);
    assert_eq!(STAMP_RUNS.load(Ordering::SeqCst), runs + 1);
    assert_eq!(g.stats().stamps, 0);
    // Neither touched the other or the base.
    assert_eq!(stamp_len(&f_db, a), 3);
    assert_eq!(STAMP_RUNS.load(Ordering::SeqCst), runs + 1);
    assert_eq!(fork::refused_writes(&db), 0);
}

#[salsa::tracked(returns(copy))]
fn read_child_len(db: &dyn Database, file: File) -> usize {
    file.text(db).len()
}

#[salsa::tracked(returns(copy))]
fn read_child_sum(db: &dyn Database, a: File, b: File) -> usize {
    read_child_len(db, a) + read_child_len(db, b)
}

#[test]
fn a_read_child_does_no_deep_verify() {
    let db = Db::default();
    let a = File::new(&db, "aaa".to_owned());
    let b = File::new(&db, "bb".to_owned());
    let c = File::new(&db, "c".to_owned());
    assert_eq!(read_child_sum(&db, a, b), 5);
    fork::freeze(&db);
    let before = fork::base_fingerprint(&db);

    // The parent writes `c` (which the query does not read): its diagnosis deep-verifies the
    // base memos and stamps them in the parent.
    let parent = fork::root_layer(&db);
    write(&layered(&db, &parent), c, "changed");
    assert_eq!(read_child_sum(&layered(&db, &parent), a, b), 5);
    assert!(parent.stats().deep_verifies > 0);
    parent.seal();

    // A write-free child inherits the parent's LOW revision: everything the parent verified is
    // shallow-valid in it, with no deep verify and no stamp.
    let child = fork::child_layer(&parent);
    assert_eq!(read_child_sum(&layered(&db, &child), a, b), 5);
    assert_eq!(child.stats().deep_verifies, 0);
    assert_eq!(child.stats().stamps, 0);
    assert_eq!(child.stats().memos, 0);
    assert_eq!(before.preserved_in(&fork::base_fingerprint(&db)), Ok(()));
}

// ---------------------------------------------------------------------------------------------
// Tracked structs

#[salsa::tracked(debug)]
struct Item<'db> {
    name: String,
    #[tracked]
    #[returns(clone)]
    value: String,
}

/// Creates the item of `file`: its value is the file's first line.
#[salsa::tracked(returns(copy))]
fn make_item(db: &dyn Database, file: File) -> Item<'_> {
    MAKE_RUNS.fetch_add(1, Ordering::SeqCst);
    let text = file.text(db);
    Item::new(
        db,
        "item".to_owned(),
        text.lines().next().unwrap_or("").to_owned(),
    )
}
static MAKE_RUNS: AtomicUsize = AtomicUsize::new(0);

#[salsa::tracked(returns(clone))]
fn read_item<'db>(db: &'db dyn Database, item: Item<'db>) -> String {
    READ_RUNS.fetch_add(1, Ordering::SeqCst);
    item.value(db)
}
static READ_RUNS: AtomicUsize = AtomicUsize::new(0);

#[salsa::tracked(returns(clone))]
fn shout(db: &dyn Database, file: File) -> String {
    read_item(db, make_item(db, file)).to_uppercase()
}

#[test]
fn fork_tracked_struct_keeps_its_id() {
    let db = Db::default();
    let file = File::new(&db, "first\nrest".to_owned());
    let base_item = make_item(&db, file);
    assert_eq!(shout(&db, file), "FIRST");
    fork::freeze(&db);
    let before = fork::base_fingerprint(&db);

    let layer = fork::root_layer(&db);
    let layer_db = layered(&db, &layer);
    write(&layer_db, file, "second\nrest");
    let layer_item = make_item(&layer_db, file);
    // The re-run kept the id and put the new field into the layer's entry.
    assert_eq!(layer_item, base_item);
    assert_eq!(layer.stats().tracked_entries, 1);
    assert_eq!(layer_item.value(&layer_db), "second");
    assert_eq!(shout(&layer_db, file), "SECOND");
    // The base slot keeps its fields, revisions and `updated_at`.
    assert_eq!(base_item.value(&db), "first");
    assert_eq!(before.preserved_in(&fork::base_fingerprint(&db)), Ok(()));
}

/// A tracked struct whose identity is a file's text: a layer that changes the text creates it
/// with a new identity, in a slot the layer owns.
#[salsa::tracked(debug)]
struct Named<'db> {
    #[returns(clone)]
    name: String,
}

#[salsa::tracked(returns(copy))]
fn make_named(db: &dyn Database, file: File) -> Named<'_> {
    Named::new(db, file.text(db))
}

/// The id of `file`'s named struct, and its name, on `db`.
fn named(db: &Db, file: File) -> (salsa::Id, String) {
    let named = make_named(db, file);
    (salsa::plumbing::AsId::as_id(&named), named.name(db))
}

#[test]
fn a_dropped_layers_tracked_slot_is_taken_again() {
    let db = Db::default();
    let file = File::new(&db, "base".to_owned());
    let (base_id, _) = named(&db, file);
    fork::freeze(&db);
    let before = fork::base_fingerprint(&db);

    let first = fork::root_layer(&db);
    let first_db = layered(&db, &first);
    write(&first_db, file, "one");
    let (one, name) = named(&first_db, file);
    assert_eq!(name, "one");
    assert_ne!(one.index(), base_id.index());
    assert_eq!(fork::layer_slots(&db).owned, 1);
    drop(first_db);
    drop(first);
    assert_eq!(fork::layer_slots(&db).owned, 0);
    assert_eq!(fork::shared_writes(&db).slot_frees, 1);

    let second = fork::root_layer(&db);
    let second_db = layered(&db, &second);
    write(&second_db, file, "two");
    let (two, name) = named(&second_db, file);
    // The same slot, a new identity: its generation is bumped and it holds the new fields.
    assert_eq!(name, "two");
    assert_eq!(two.index(), one.index());
    assert_eq!(two.generation(), one.generation() + 1);
    assert_eq!(fork::shared_writes(&db).slot_reuses, 1);
    // The base struct is unchanged.
    assert_eq!(named(&second_db, file).0, two);
    drop(second_db);
    drop(second);
    assert_eq!(named(&db, file), (base_id, "base".to_owned()));

    let slots = fork::layer_slots(&db);
    assert_eq!((slots.owned, slots.leaked), (0, 0));
    assert_eq!(fork::shared_writes(&db).slot_frees, 2);
    assert_eq!(fork::refused_writes(&db), 0);
    assert_eq!(before.preserved_in(&fork::base_fingerprint(&db)), Ok(()));
}

#[test]
fn after_a_thaw_a_given_back_tracked_slot_is_the_bases() {
    let mut db = Db::default();
    let file = File::new(&db, "base".to_owned());
    named(&db, file);
    fork::freeze(&db);

    let layer = fork::root_layer(&db);
    let layer_db = layered(&db, &layer);
    write(&layer_db, file, "in a layer");
    let (in_layer, _) = named(&layer_db, file);
    drop(layer_db);
    drop(layer);
    fork::try_thaw(&mut db).expect("no other handle is alive");

    // The base takes the slot back from its own free list, with a bumped generation.
    file.set_text(&mut db).to("after the thaw".to_owned());
    let (after, name) = named(&db, file);
    assert_eq!(name, "after the thaw");
    assert_eq!(after.index(), in_layer.index());
    assert_eq!(after.generation(), in_layer.generation() + 1);
}

#[test]
fn a_slot_given_back_after_a_refreeze_by_an_older_layer_is_leaked() {
    let mut db = Db::default();
    let file = File::new(&db, "base".to_owned());
    named(&db, file);
    fork::freeze(&db);
    // A layer that outlives the thaw (past the Engine, which refuses that).
    let old = fork::root_layer(&db);
    let old_db = layered(&db, &old);
    write(&old_db, file, "old freeze");
    named(&old_db, file);
    drop(old_db);
    fork::try_thaw(&mut db).expect("no other handle is alive");
    fork::freeze(&db);
    let before = fork::base_fingerprint(&db);

    // Given back under the new freeze, it is not taken again: the new freeze's fingerprint
    // covers that slot.
    drop(old);
    let layer = fork::root_layer(&db);
    let layer_db = layered(&db, &layer);
    write(&layer_db, file, "new freeze");
    assert_eq!(named(&layer_db, file).1, "new freeze");
    assert_eq!(fork::layer_slots(&db).leaked, 1);
    assert_eq!(fork::shared_writes(&db).slot_reuses, 0);
    drop(layer_db);
    assert_eq!(fork::refused_writes(&db), 0);
    assert_eq!(before.preserved_in(&fork::base_fingerprint(&db)), Ok(()));
}

#[salsa::tracked(returns(copy))]
fn eq_make(db: &dyn Database, file: File) -> Item<'_> {
    EQ_MAKES.fetch_add(1, Ordering::SeqCst);
    let text = file.text(db);
    Item::new(
        db,
        "item".to_owned(),
        text.lines().next().unwrap_or("").to_owned(),
    )
}
static EQ_MAKES: AtomicUsize = AtomicUsize::new(0);

#[salsa::tracked(returns(clone))]
fn eq_read<'db>(db: &'db dyn Database, item: Item<'db>) -> String {
    EQ_READS.fetch_add(1, Ordering::SeqCst);
    item.value(db)
}
static EQ_READS: AtomicUsize = AtomicUsize::new(0);

#[salsa::tracked(returns(clone))]
fn eq_shout(db: &dyn Database, file: File) -> String {
    eq_read(db, eq_make(db, file)).to_uppercase()
}

#[test]
fn fork_equal_tracked_struct_backdates() {
    let db = Db::default();
    let file = File::new(&db, "same\nfirst rest".to_owned());
    assert_eq!(eq_shout(&db, file), "SAME");
    fork::freeze(&db);
    let (makes, reads) = (
        EQ_MAKES.load(Ordering::SeqCst),
        EQ_READS.load(Ordering::SeqCst),
    );

    let layer = fork::root_layer(&db);
    let layer_db = layered(&db, &layer);
    // The first line, and so the item's value, stays equal.
    write(&layer_db, file, "same\nother rest");
    assert_eq!(eq_shout(&layer_db, file), "SAME");
    // The creator re-ran; the re-created struct kept its field revision, so its reader did not.
    assert_eq!(EQ_MAKES.load(Ordering::SeqCst), makes + 1);
    assert_eq!(EQ_READS.load(Ordering::SeqCst), reads);
    assert_eq!(layer.stats().tracked_entries, 1);
}

#[test]
fn fork_tracked_read_leaves_the_base_updated_at() {
    let db = Db::default();
    let file = File::new(&db, "x".to_owned());
    let other = File::new(&db, "o".to_owned());
    let item = make_item(&db, file);
    assert_eq!(read_item(&db, item), "x");
    fork::freeze(&db);
    let before = fork::base_fingerprint(&db);

    // A layer with a write (so its queries deep-verify) reads the base struct through a field
    // getter and through a memo keyed by it.
    let layer = fork::root_layer(&db);
    let layer_db = layered(&db, &layer);
    write(&layer_db, other, "changed");
    assert_eq!(item.value(&layer_db), "x");
    assert_eq!(read_item(&layer_db, item), "x");
    assert_eq!(make_item(&layer_db, file), item);
    assert_eq!(layer.stats().tracked_entries, 0);
    // The base slot's `updated_at` (in the fingerprint) is as it was at the freeze.
    assert_eq!(before.preserved_in(&fork::base_fingerprint(&db)), Ok(()));
}

#[salsa::tracked(returns(clone))]
fn rerun_read<'db>(db: &'db dyn Database, item: Item<'db>) -> String {
    RERUN_READS.fetch_add(1, Ordering::SeqCst);
    item.value(db)
}
static RERUN_READS: AtomicUsize = AtomicUsize::new(0);

#[test]
fn fork_creator_rerun_after_a_read_writes_the_overlay() {
    let db = Db::default();
    let file = File::new(&db, "old".to_owned());
    let item = make_item(&db, file);
    assert_eq!(rerun_read(&db, item), "old");
    fork::freeze(&db);
    let reads = RERUN_READS.load(Ordering::SeqCst);

    let layer = fork::root_layer(&db);
    let layer_db = layered(&db, &layer);
    write(&layer_db, file, "new");
    // A read of the base struct in the layer, before its creator re-runs.
    assert_eq!(item.value(&layer_db), "old");
    // The creator's re-run writes the layer's entry even so (upstream would keep the old fields
    // of a struct read in the revision).
    assert_eq!(make_item(&layer_db, file), item);
    assert_eq!(layer.stats().tracked_entries, 1);
    assert_eq!(item.value(&layer_db), "new");
    // The field's revision is the creator's `changed_at`, newer than the reader's memo.
    assert_eq!(rerun_read(&layer_db, item), "new");
    assert_eq!(RERUN_READS.load(Ordering::SeqCst), reads + 1);
    assert_eq!(item.value(&db), "old");
}

#[salsa::input]
struct Source {
    #[returns(clone)]
    text: String,
}

#[salsa::tracked(returns(copy))]
fn durable_item(db: &dyn Database, source: Source) -> Item<'_> {
    Item::new(db, "durable".to_owned(), source.text(db))
}

#[salsa::tracked(returns(clone))]
fn durable_read<'db>(db: &'db dyn Database, item: Item<'db>) -> String {
    DURABLE_READS.fetch_add(1, Ordering::SeqCst);
    item.value(db)
}
static DURABLE_READS: AtomicUsize = AtomicUsize::new(0);

#[test]
fn fork_tracked_durability_is_lowered_in_the_entry() {
    let mut db = Db::default();
    let source = Source::builder("text".to_owned())
        .durability(Durability::HIGH)
        .new(&db);
    // The struct is created from a HIGH read: its durability is HIGH.
    let item = durable_item(&db, source);
    assert_eq!(durable_read(&db, item), "text");
    let item = salsa::plumbing::AsId::as_id(&item);
    // Before the freeze the input is set again, equal but with LOW durability; nothing re-runs.
    source
        .set_text(&mut db)
        .with_durability(Durability::LOW)
        .to("text".to_owned());
    fork::freeze(&db);
    let before = fork::base_fingerprint(&db);
    let reads = DURABLE_READS.load(Ordering::SeqCst);

    // In the layer the creator re-runs from a LOW read: the entry's durability is lowered and
    // every tracked field gets the creator's `changed_at`, so the reader re-runs although the
    // value is equal.
    let layer = fork::root_layer(&db);
    let layer_db = layered(&db, &layer);
    let layer_item = durable_item(&layer_db, source);
    assert_eq!(salsa::plumbing::AsId::as_id(&layer_item), item);
    assert_eq!(layer.stats().tracked_entries, 1);
    assert_eq!(durable_read(&layer_db, layer_item), "text");
    assert_eq!(DURABLE_READS.load(Ordering::SeqCst), reads + 1);
    // The base slot keeps its own durability and revisions.
    assert_eq!(before.preserved_in(&fork::base_fingerprint(&db)), Ok(()));
}

#[salsa::input]
struct Flag {
    #[returns(copy)]
    on: bool,
}

#[salsa::tracked(returns(clone))]
fn maybe_item(db: &dyn Database, flag: Flag) -> Option<Item<'_>> {
    flag.on(db)
        .then(|| Item::new(db, "maybe".to_owned(), "here".to_owned()))
}

#[salsa::interned]
struct Holder<'db> {
    item: Item<'db>,
}

#[salsa::tracked(returns(clone))]
fn read_held<'db>(db: &'db dyn Database, holder: Holder<'db>) -> String {
    holder.item(db).value(db)
}

#[salsa::tracked(returns(clone))]
fn read_stale<'db>(db: &'db dyn Database, item: Item<'db>) -> String {
    STALE_RUNS.fetch_add(1, Ordering::SeqCst);
    item.value(db)
}
static STALE_RUNS: AtomicUsize = AtomicUsize::new(0);

#[test]
fn fork_stale_output_keeps_the_base_slot() {
    let db = Db::default();
    let flag = Flag::new(&db, true);
    let item = maybe_item(&db, flag).expect("created");
    let holder = Holder::new(&db, item);
    assert_eq!(read_held(&db, holder), "here");
    assert_eq!(read_stale(&db, item), "here");
    fork::freeze(&db);
    let before = fork::base_fingerprint(&db);
    let runs = STALE_RUNS.load(Ordering::SeqCst);

    let layer = fork::root_layer(&db);
    let layer_db = layered(&db, &layer);
    fork::set_input_field::<Flag, _>(&layer_db, flag, 0, |fields| fields.0 = false);
    // The creator no longer creates the struct: it is tombstoned in the layer.
    assert_eq!(maybe_item(&layer_db, flag), None);
    assert_eq!(layer.stats().tombstones, 1);

    // In the layer a field getter panics, as upstream's read of a deleted struct does.
    let message = panic_message(|| {
        item.value(&layer_db);
    });
    assert!(message.contains("write lock taken"), "{message}");
    // The base memo keyed by the struct is not found: the function runs again (and panics).
    let message = panic_message(|| {
        read_stale(&layer_db, item);
    });
    assert!(message.contains("write lock taken"), "{message}");
    assert_eq!(STALE_RUNS.load(Ordering::SeqCst), runs + 1);
    // A base memo that read the field sees it as changed and re-runs (and panics), instead of
    // returning its old value.
    let message = panic_message(|| {
        read_held(&layer_db, holder);
    });
    assert!(message.contains("write lock taken"), "{message}");

    // The base slot stays, for the base and for every other chain.
    assert_eq!(item.value(&db), "here");
    let sibling = fork::root_layer(&db);
    assert_eq!(read_stale(&layered(&db, &sibling), item), "here");
    drop(layer_db);
    assert_eq!(before.preserved_in(&fork::base_fingerprint(&db)), Ok(()));
}

// ---------------------------------------------------------------------------------------------
// Interned values (E7)

#[salsa::interned(revisions = 1, debug)]
struct Word<'db> {
    #[returns(ref)]
    text: String,
}

#[salsa::tracked(returns(copy))]
fn intern_text<'db>(db: &'db dyn Database, file: File) -> Word<'db> {
    Word::new(db, file.text(db))
}

#[test]
fn fork_interned_slot_is_not_reused_while_frozen() {
    let mut db = Db::default();
    let file = File::new(&db, "old".to_owned());
    let old = salsa::plumbing::AsId::as_id(&intern_text(&db, file));
    // New revisions in which "old" is not interned make it stale and its slot reusable.
    for i in 0..4 {
        file.set_text(&mut db).to(format!("filler {i}"));
        intern_text(&db, file);
    }
    fork::freeze(&db);
    let inserts = fork::shared_writes(&db).interned_inserts;

    let layer = fork::root_layer(&db);
    let layer_db = layered(&db, &layer);
    write(&layer_db, file, "new under a layer");
    let new = intern_text(&layer_db, file);
    // A new slot: the stale one keeps its id and content.
    assert_ne!(salsa::plumbing::AsId::as_id(&new).index(), old.index());
    let old: Word<'_> = salsa::plumbing::FromId::from_id(old);
    assert_eq!(old.text(&layer_db), "old");
    assert_eq!(new.text(&layer_db), "new under a layer");
    assert_eq!(fork::shared_writes(&db).interned_inserts, inserts + 1);
    // Chain-scoped: a sibling interning the same text gets an id of its own, also a new slot.
    let sibling = fork::root_layer(&db);
    let sibling_db = layered(&db, &sibling);
    write(&sibling_db, file, "new under a layer");
    let in_sibling = intern_text(&sibling_db, file);
    assert_ne!(in_sibling, new);
    assert_ne!(
        salsa::plumbing::AsId::as_id(&in_sibling).index(),
        salsa::plumbing::AsId::as_id(&new).index()
    );
    assert_eq!(in_sibling.text(&sibling_db), "new under a layer");
    assert_eq!(fork::shared_writes(&db).interned_inserts, inserts + 2);
}

/// A word interned from `file`'s text in a tracked function of its own.
#[salsa::tracked(returns(copy))]
fn chain_word<'db>(db: &'db dyn Database, file: File) -> Word<'db> {
    Word::new(db, file.text(db))
}

/// The id of `text` interned as a word outside any query.
fn word_of(db: &Db, text: &str) -> salsa::Id {
    salsa::plumbing::AsId::as_id(&Word::new(db, text.to_owned()))
}

/// The text of the word `id`.
fn word_text(db: &Db, id: salsa::Id) -> String {
    let word: Word<'_> = salsa::plumbing::FromId::from_id(id);
    word.text(db).clone()
}

#[test]
fn a_layer_interned_value_is_seen_by_its_chain_only() {
    let db = Db::default();
    let file = File::new(&db, "base".to_owned());
    fork::freeze(&db);
    let root = fork::root_layer(&db);
    root.seal();

    let parent = sealed_child(&db, &root, file, "word");
    let parent_word = {
        let read = fork::child_layer(&parent);
        let read_db = layered(&db, &read);
        salsa::plumbing::AsId::as_id(&chain_word(&read_db, file))
    };
    // The read child that interned it is gone: its value was given back.
    assert_eq!(fork::layer_slots(&db).owned, 0);

    let parent = sealed_child(&db, &root, file, "word");
    let open = fork::child_layer(&parent);
    let open_db = layered(&db, &open);
    let in_parent = word_of(&open_db, "word");
    open.seal();
    drop(open_db);
    // A descendant finds its ancestor's value; a sibling of the ancestor does not.
    let child = fork::child_layer(&open);
    let child_db = layered(&db, &child);
    assert_eq!(word_of(&child_db, "word"), in_parent);
    let sibling = fork::child_layer(&parent);
    let sibling_db = layered(&db, &sibling);
    let in_sibling = word_of(&sibling_db, "word");
    assert_ne!(in_sibling, in_parent);
    assert_eq!(word_text(&sibling_db, in_sibling), "word");
    // The first value's slot was taken again by one of them, with a bumped generation.
    let ids = [in_parent, in_sibling];
    let again = ids
        .iter()
        .find(|id| id.index() == parent_word.index())
        .expect("a later layer takes the given-back slot");
    assert!(again.generation() > parent_word.generation());
    assert!(fork::shared_writes(&db).slot_reuses >= 1);
}

#[test]
fn after_a_thaw_the_base_never_finds_a_layers_value() {
    let mut db = Db::default();
    // A value the base interned before the freeze: every layer finds it.
    let shared = word_of(&db, "shared");
    fork::freeze(&db);
    let layer = fork::root_layer(&db);
    let layer_db = layered(&db, &layer);
    assert_eq!(word_of(&layer_db, "shared"), shared);
    let in_layer = word_of(&layer_db, "layer word");
    drop(layer_db);
    drop(layer);
    fork::try_thaw(&mut db).expect("no other handle is alive");

    // The base's lookup has no owner check: the thaw took the layer's value out of the key maps,
    // so the base interns the content in a slot of its own.
    let in_base = word_of(&db, "layer word");
    assert_ne!(in_base.index(), in_layer.index());
    assert_eq!(word_of(&db, "layer word"), in_base);
    assert_eq!(word_text(&db, in_base), "layer word");
    assert_eq!(word_of(&db, "shared"), shared);

    // A thaw after a freeze whose layers interned nothing keeps the base's values.
    fork::freeze(&db);
    let reader = fork::root_layer(&db);
    let reader_db = layered(&db, &reader);
    assert_eq!(word_of(&reader_db, "layer word"), in_base);
    drop(reader_db);
    drop(reader);
    fork::try_thaw(&mut db).expect("no other handle is alive");
    assert_eq!(word_of(&db, "layer word"), in_base);
    assert_eq!(word_of(&db, "shared"), shared);
}

/// An immortal interned struct, as rust-analyzer's `SyntaxContextImpl` is.
#[salsa::interned(revisions = usize::MAX)]
struct Forever<'db> {
    #[returns(ref)]
    text: String,
}

#[test]
fn an_immortal_value_is_taken_again_under_its_own_id() {
    let db = Db::default();
    fork::freeze(&db);
    let before = fork::base_fingerprint(&db);
    let forever = |db: &Db, text: &str| salsa::plumbing::AsId::as_id(&Forever::new(db, text.to_owned()));

    let first = fork::root_layer(&db);
    let first_db = layered(&db, &first);
    let old = forever(&first_db, "kept id");
    drop(first_db);
    drop(first);

    let second = fork::root_layer(&db);
    let second_db = layered(&db, &second);
    let again = forever(&second_db, "kept id");
    // The same slot under the same id, generation 0: upstream never moves an immortal value's
    // generation, so code that rebuilds the id from its index alone (rust-analyzer's
    // `SyntaxContext`) gets this id.
    assert_eq!(again, old);
    assert_eq!(again.generation(), 0);
    assert_eq!(fork::shared_writes(&db).slot_reuses, 1);
    let word: Forever<'_> = salsa::plumbing::FromId::from_id(again);
    assert_eq!(word.text(&second_db), "kept id");
    drop(second_db);
    drop(second);
    assert_eq!(fork::layer_slots(&db).owned, 0);
    assert_eq!(before.preserved_in(&fork::base_fingerprint(&db)), Ok(()));
}

#[test]
fn a_given_back_interned_slot_does_not_find_its_old_content() {
    let db = Db::default();
    let file = File::new(&db, "base".to_owned());
    fork::freeze(&db);
    let before = fork::base_fingerprint(&db);

    let first = fork::root_layer(&db);
    let first_db = layered(&db, &first);
    let old = word_of(&first_db, "gone");
    drop(first_db);
    drop(first);
    assert_eq!(fork::shared_writes(&db).slot_frees, 1);

    // A later layer interning the same content does not find the dead value (its owner is on no
    // live chain): it takes the slot again (the free list is per shard, and the same content
    // hashes to the same shard), under a new id.
    let second = fork::root_layer(&db);
    let second_db = layered(&db, &second);
    let again = word_of(&second_db, "gone");
    assert_ne!(again, old);
    assert_eq!(again.index(), old.index());
    assert_eq!(again.generation(), old.generation() + 1);
    assert_eq!(word_of(&second_db, "gone"), again);
    assert_eq!(word_text(&second_db, again), "gone");
    assert_eq!(fork::shared_writes(&db).slot_reuses, 1);
    drop(second_db);
    drop(second);
    let slots = fork::layer_slots(&db);
    assert_eq!(slots.owned, 0);
    assert_eq!(slots.leaked, 0);
    assert_eq!(fork::refused_writes(&db), 0);
    assert_eq!(before.preserved_in(&fork::base_fingerprint(&db)), Ok(()));
    let _ = file;
}

// ---------------------------------------------------------------------------------------------
// Cycles

#[salsa::input]
struct Target {
    #[returns(copy)]
    value: u32,
}

#[salsa::tracked(returns(copy), cycle_initial = |_, _, _| 0)]
fn climb_a(db: &dyn Database, target: Target) -> u32 {
    let b = climb_b(db, target);
    (b + 1).min(target.value(db))
}

#[salsa::tracked(returns(copy), cycle_initial = |_, _, _| 0)]
fn climb_b(db: &dyn Database, target: Target) -> u32 {
    climb_a(db, target)
}

/// The flat result: the same composed state on a fresh database.
fn flat_climb(value: u32) -> u32 {
    let db = Db::default();
    climb_a(&db, Target::new(&db, value))
}

#[test]
fn fork_cycle_iterates_inside_the_layer() {
    let db = Db::default();
    let target = Target::new(&db, 3);
    assert_eq!(climb_a(&db, target), flat_climb(3));
    fork::freeze(&db);
    let before = fork::base_fingerprint(&db);

    let layer = fork::root_layer(&db);
    let layer_db = layered(&db, &layer);
    fork::set_input_field::<Target, _>(&layer_db, target, 0, |fields| fields.0 = 7);
    assert_eq!(climb_a(&layer_db, target), flat_climb(7));
    assert_eq!(climb_b(&layer_db, target), flat_climb(7));
    // The iteration's provisional memos are the layer's; the base has its own.
    assert!(layer.stats().memos >= 2);
    assert_eq!(climb_a(&db, target), flat_climb(3));
    assert_eq!(before.preserved_in(&fork::base_fingerprint(&db)), Ok(()));

    // A child of the sealed layer reads the final memos.
    layer.seal();
    let child = fork::child_layer(&layer);
    assert_eq!(climb_a(&layered(&db, &child), target), flat_climb(7));
    assert_eq!(child.stats().memos, 0);
}

// ---------------------------------------------------------------------------------------------
// Claims, the dependency graph and transfers (E5)

static RENDEZVOUS: std::sync::OnceLock<Barrier> = std::sync::OnceLock::new();

#[salsa::tracked(returns(copy))]
fn meet(db: &dyn Database, file: File) -> usize {
    // Both threads must be inside this query of the same key at once.
    RENDEZVOUS.get().unwrap().wait();
    file.text(db).len()
}

#[test]
fn two_layers_do_not_block() {
    let db = Db::default();
    let file = File::new(&db, "base".to_owned());
    RENDEZVOUS.get_or_init(|| Barrier::new(2));
    fork::freeze(&db);
    let f = fork::root_layer(&db);
    let g = fork::root_layer(&db);
    let f_db = layered(&db, &f);
    let g_db = layered(&db, &g);
    write(&f_db, file, "in f");
    write(&g_db, file, "in g, longer");

    let results = within_timeout(move || {
        let f_thread = thread::spawn(move || meet(&f_db, file));
        let g_thread = thread::spawn(move || meet(&g_db, file));
        (f_thread.join().unwrap(), g_thread.join().unwrap())
    });
    assert_eq!(results, ("in f".len(), "in g, longer".len()));
    assert!(fork::shared_writes(&db).sync_claims >= 2);
}

static CYCLE_RENDEZVOUS: std::sync::OnceLock<Barrier> = std::sync::OnceLock::new();
thread_local! {
    static MET: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[salsa::tracked(returns(copy), cycle_initial = |_, _, _| 0)]
fn head(db: &dyn Database, target: Target) -> u32 {
    let tail = tail(db, target);
    (tail + 1).min(target.value(db))
}

#[salsa::tracked(returns(copy), cycle_initial = |_, _, _| 0)]
fn tail(db: &dyn Database, target: Target) -> u32 {
    let value = head(db, target);
    // On each thread, once, while the cycle head is iterating: both siblings iterate at once.
    if !MET.get() {
        MET.set(true);
        CYCLE_RENDEZVOUS.get().unwrap().wait();
    }
    value
}

#[test]
fn one_cycle_head_on_two_siblings() {
    let db = Db::default();
    let target = Target::new(&db, 1);
    CYCLE_RENDEZVOUS.get_or_init(|| Barrier::new(2));
    let flat = |value| {
        let db = Db::default();
        MET.set(true);
        head(&db, Target::new(&db, value))
    };
    let (flat_f, flat_g) = (flat(4), flat(9));
    fork::freeze(&db);
    let f = fork::root_layer(&db);
    let g = fork::root_layer(&db);
    let f_db = layered(&db, &f);
    let g_db = layered(&db, &g);
    fork::set_input_field::<Target, _>(&f_db, target, 0, |fields| fields.0 = 4);
    fork::set_input_field::<Target, _>(&g_db, target, 0, |fields| fields.0 = 9);

    let results = within_timeout(move || {
        let f_thread = thread::spawn(move || head(&f_db, target));
        let g_thread = thread::spawn(move || head(&g_db, target));
        (f_thread.join().unwrap(), g_thread.join().unwrap())
    });
    assert_eq!(results, (flat_f, flat_g));
}

// ---------------------------------------------------------------------------------------------
// The shared writes (E2) and sealed layers (E9)

#[salsa::tracked(lru = 4, returns(copy))]
fn lru_len<'db>(db: &'db dyn Database, word: Word<'db>) -> usize {
    word.text(db).len()
}

#[salsa::tracked(returns(copy))]
fn word_len(db: &dyn Database, file: File) -> usize {
    lru_len(db, intern_text(db, file))
}

#[test]
fn shared_writes_under_a_layer_are_the_listed_ones() {
    let db = Db::default();
    let file = File::new(&db, "base".to_owned());
    let other = File::new(&db, "other".to_owned());
    assert_eq!(word_len(&db, file), 4);
    assert_eq!(word_len(&db, other), 5);
    fork::freeze(&db);
    let before = fork::base_fingerprint(&db);

    let layer = fork::root_layer(&db);
    let layer_db = layered(&db, &layer);
    write(&layer_db, file, "in the layer");
    // A new interned value, a new tracked function result, an LRU-limited read, a claim, and a
    // re-validated existing interned value (`other`'s word, through a deep verify).
    assert_eq!(word_len(&layer_db, file), 12);
    assert_eq!(word_len(&layer_db, other), 5);
    let _ = File::new(&layer_db, "created".to_owned());

    // The kinds of shared write are exactly the fields of `fork::SharedWrites` (this destructuring is exhaustive,
    // so adding a kind fails to compile here).
    let fork::SharedWrites {
        slot_allocations,
        interned_inserts,
        last_interned_at_stores,
        sync_claims,
        slot_reuses,
        slot_frees,
    } = fork::shared_writes(&db);
    assert!(slot_allocations >= 2, "an interned value and an input");
    assert_eq!(interned_inserts, 1);
    assert!(last_interned_at_stores >= 1);
    assert!(sync_claims >= 1);
    // No layer dropped yet: nothing given back, nothing taken again.
    assert_eq!((slot_reuses, slot_frees), (0, 0));
    let slots = fork::layer_slots(&db);
    assert_eq!(slots.owned, 1, "the interned value");
    assert_eq!(slots.created_inputs, 1);
    // The base writes on the "not done" list were reached and not made.
    let skipped = fork::skipped_writes(&db);
    assert!(skipped.lru_uses >= 1, "{skipped:?}");
    assert!(skipped.revision_queue_records >= 1, "{skipped:?}");
    // No other write reached the base: nothing was refused, and the base's memos, revisions and
    // `updated_at`s are as at the freeze.
    assert_eq!(fork::refused_writes(&db), 0);
    drop(layer_db);
    drop(layer);
    // The layer gave its interned value back; the created input is kept.
    assert_eq!(fork::shared_writes(&db).slot_frees, 1);
    assert_eq!(fork::layer_slots(&db).owned, 0);
    assert_eq!(before.preserved_in(&fork::base_fingerprint(&db)), Ok(()));
}

#[salsa::tracked(returns(copy))]
fn sealed_len(db: &dyn Database, file: File) -> usize {
    file.text(db).len()
}

#[test]
fn an_insert_on_a_sealed_layer_panics() {
    let db = Db::default();
    let file = File::new(&db, "base".to_owned());
    fork::freeze(&db);
    let layer = fork::root_layer(&db);
    let layer_db = layered(&db, &layer);
    write(&layer_db, file, "in the layer");
    // A handle past the types: the layer is sealed while a handle of it lives.
    layer.seal();
    let message = panic_message(|| {
        sealed_len(&layer_db, file);
    });
    assert!(message.contains("write to a sealed layer"), "{message}");
    assert_eq!(layer.stats().memos, 0);
}

#[salsa::tracked(returns(copy))]
fn parent_child_len(db: &dyn Database, a: File, b: File) -> usize {
    a.text(db).len() * 10 + b.text(db).len()
}

#[test]
fn reading_the_parent_does_not_change_a_childs_result() {
    let db = Db::default();
    let a = File::new(&db, "a".to_owned());
    let b = File::new(&db, "b".to_owned());
    assert_eq!(parent_child_len(&db, a, b), 11);
    fork::freeze(&db);
    let root = fork::root_layer(&db);
    root.seal();
    let parent = sealed_child(&db, &root, a, "aaa");
    let child = fork::child_layer(&parent);
    let child_db = layered(&db, &child);
    write(&child_db, b, "bb");
    assert_eq!(parent_child_len(&child_db, a, b), 32);
    let parent_stats = parent.stats();

    // Reads of the sealed parent run in throwaway children; the parent takes nothing.
    for _ in 0..2 {
        let read = fork::child_layer(&parent);
        assert_eq!(parent_child_len(&layered(&db, &read), a, b), 31);
    }
    assert_eq!(parent.stats(), parent_stats);
    assert_eq!(parent_child_len(&child_db, a, b), 32);
    // A write after a query in the same layer is refused (one revision per layer).
    let message = panic_message(|| write(&child_db, a, "late"));
    assert!(message.contains("after a query ran in it"), "{message}");
}

// ---------------------------------------------------------------------------------------------
// Cancellation

static GATE: std::sync::OnceLock<Barrier> = std::sync::OnceLock::new();

#[salsa::tracked(returns(copy))]
fn gated(db: &dyn Database, file: File) -> usize {
    GATE.get().unwrap().wait();
    db.unwind_if_revision_cancelled();
    file.text(db).len()
}

#[test]
fn a_waiter_stops_when_an_ancestor_is_cancelled() {
    let db = Db::default();
    let file = File::new(&db, "base".to_owned());
    GATE.get_or_init(|| Barrier::new(2));
    fork::freeze(&db);
    let root = fork::root_layer(&db);
    root.seal();
    let child = sealed_child(&db, &root, file, "in the child");
    let read = fork::child_layer(&child);
    let (runner, waiter) = (layered(&db, &read), layered(&db, &read));

    let outcome = within_timeout(move || {
        // The runner holds the claim, inside the query; the waiter blocks on it.
        let running = thread::spawn(move || catch_unwind(AssertUnwindSafe(|| gated(&runner, file))));
        thread::sleep(Duration::from_millis(200));
        let waiting = thread::spawn(move || catch_unwind(AssertUnwindSafe(|| gated(&waiter, file))));
        thread::sleep(Duration::from_millis(300));
        // An ancestor of their layer is cancelled; the runner leaves the query.
        root.cancellation_token().cancel();
        GATE.get().unwrap().wait();
        // The waiter, woken, stops too instead of running the query again (it would wait at the
        // gate for ever).
        (
            running.join().unwrap().is_err(),
            waiting.join().unwrap().is_err(),
        )
    });
    assert_eq!(outcome, (true, true));
}

#[salsa::tracked(returns(copy), cycle_initial = |_, _, _| 0)]
fn spin(db: &dyn Database, target: Target) -> u32 {
    SPINNING.store(true, Ordering::SeqCst);
    let previous = spin(db, target);
    db.unwind_if_revision_cancelled();
    (previous + 1).min(target.value(db))
}
static SPINNING: AtomicBool = AtomicBool::new(false);

#[test]
fn the_disabled_bit_is_per_handle() {
    // A fixpoint iteration disables cancellation on its own handle only: another handle of the
    // same layer still stops when the layer is cancelled.
    let db = Db::default();
    let target = Target::new(&db, 3);
    fork::freeze(&db);
    let layer = fork::root_layer(&db);
    let first = layered(&db, &layer);
    let second = layered(&db, &layer);
    assert_eq!(spin(&first, target), 3);
    assert!(SPINNING.load(Ordering::SeqCst));
    layer.cancellation_token().cancel();
    let cancelled = catch_unwind(AssertUnwindSafe(|| second.unwind_if_revision_cancelled()));
    assert!(cancelled.is_err());
}
