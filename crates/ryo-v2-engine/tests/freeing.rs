//! What a layer owns is given back when it drops (D11): over many layers made and dropped one
//! after another, each with item-level edits diagnosed by rust-analyzer, the base's table stops
//! growing once the first layers have given their slots back, every slot is given back, and the
//! base takes no write but the listed shared ones (I2). An input a layer creates is kept: one
//! per distinct created file, however many layers create it.

use std::path::PathBuf;
use std::sync::Arc;

use ra_ap_base_db::FileId;
use ra_ap_ide_db::RootDatabase;
use ryo_v2_core::app::ports::Analyzer;
use ryo_v2_core::domain::RelPath;
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_engine::{Engine, Frozen};
use ryo_v2_ra::{RaAnalyzer, RaConfig, RaSession};

/// A temp workspace that removes itself on drop.
struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    /// One package `probe` with `src/lib.rs` and `src/m0.rs` holding `text`.
    fn new(test: &str, text: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("ryo-v2-engine-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let files = [
            (
                "Cargo.toml",
                "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
            ),
            ("src/lib.rs", "pub mod m0;\n"),
            ("src/m0.rs", text),
        ];
        for (rel, text) in files {
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

fn analyzer() -> RaAnalyzer {
    RaAnalyzer::new(
        RaConfig {
            proc_macro: false,
            threads: Some(2),
            gap_detectors: true,
            cfg_test: false,
            ..RaConfig::default()
        },
        Arc::new(MonotonicClock::new()),
        Arc::new(RecordingObserver::default()),
    )
}

const BASE: &str = "pub trait Speak {\n    fn speak(&self) -> u32;\n}\npub struct V(pub u32);\nimpl Speak for V {\n    fn speak(&self) -> u32 {\n        self.0\n    }\n}\npub fn use_it(v: V) -> u64 {\n    let n: u64 = v.speak().into();\n    n\n}\n";

/// An item-level edit, different for every `i`: a renamed function, a new struct and impl, and a
/// caller with a type error, so every layer re-creates item trees and interns new values.
fn item_edit(i: usize) -> String {
    format!(
        "{}pub struct W{i}(pub u8);\nimpl Speak for W{i} {{\n    fn speak(&self) -> u32 {{\n        self.0.into()\n    }}\n}}\npub fn call{i}(w: W{i}) -> u8 {{\n    w.speak()\n}}\n",
        BASE.replace("pub fn use_it", &format!("pub fn use_it{i}"))
    )
}

fn load(a: &RaAnalyzer, fixture: &Fixture) -> (RaSession, Frozen<RootDatabase>) {
    let mut s = Analyzer::load(a, &fixture.dir).expect("load");
    let frozen = Engine::freeze(s.lend_database());
    (s, frozen)
}

const LAYERS: usize = 50;

#[test]
fn dropped_layers_give_their_slots_back() {
    let a = analyzer();
    let fixture = Fixture::new("freeing", BASE);
    let (mut s, frozen) = load(&a, &fixture);
    let file = s.file_id(&RelPath::parse("src/m0.rs").unwrap()).unwrap();
    let at_freeze = frozen.isolation();
    let root = frozen.root();

    // Fresh table slots each layer took, and the layer-owned slots while it was live.
    let mut fresh = Vec::new();
    let mut owned_live = Vec::new();
    for i in 0..LAYERS {
        let before = frozen.isolation().shared.slot_allocations;
        let mut layer = root.child();
        layer.write(file, item_edit(i)).expect("writable");
        let found = a
            .diagnose_database(&s, &layer.db())
            .expect("the layer's diagnosis");
        assert!(!found.findings.is_empty(), "layer {i}: the edit has errors");
        owned_live.push(frozen.isolation().slots.owned);
        drop(layer);
        let after = frozen.isolation();
        assert_eq!(after.slots.owned, 0, "layer {i} gave every slot back");
        fresh.push(after.shared.slot_allocations - before);
    }
    let after = frozen.isolation();
    let first: usize = fresh[..10].iter().sum();
    let last: usize = fresh[LAYERS - 10..].iter().sum();
    let total: usize = fresh.iter().sum();
    let peak = owned_live.iter().copied().max().unwrap_or(0);
    eprintln!(
        "freeing: {LAYERS} layers; fresh slots per layer {fresh:?}; owned while live {owned_live:?}; \
         {:?}; {:?}; base slots {} unchanged",
        after.shared,
        after.slots,
        at_freeze.fingerprint.slots(),
    );
    // Every slot a layer took was given back, and later layers took them again. An interned slot
    // is taken again only for a value of its own key-map shard, so the table grows until the
    // free list of each shard a layer interns into holds a slot; that bound depends on the
    // shards, not on the number of layers, so the growth tapers off.
    assert_eq!(after.slots.owned, 0);
    assert_eq!(
        after.shared.slot_frees,
        after.shared.slot_allocations + after.shared.slot_reuses - after.slots.created_inputs,
        "every owned slot, allocated or taken again, was given back"
    );
    assert!(
        after.shared.slot_reuses > 0,
        "later layers took slots again"
    );
    assert!(
        total * 3 < peak * LAYERS * 2,
        "too few slots taken again: {total} fresh slots for {LAYERS} layers of {peak}"
    );
    assert!(
        last * 2 <= first,
        "the growth does not taper off: first 10 layers {first}, last 10 layers {last}"
    );
    // I2: no write reached the base but the listed shared ones.
    assert_eq!(after.refused, 0);
    assert_eq!(
        at_freeze.fingerprint.preserved_in(&after.fingerprint),
        Ok(())
    );

    drop(root);
    let db = match frozen.thaw() {
        Ok(db) => db,
        Err((_, e)) => panic!("thaw: {e:?}"),
    };
    s.return_database(db);
}

#[test]
fn a_created_input_is_kept_once_per_file() {
    let a = analyzer();
    let fixture = Fixture::new("freeing_created", BASE);
    let (mut s, frozen) = load(&a, &fixture);
    let root = frozen.root();
    // An id no file of the session has (the vfs counts up from 0).
    let new_file = FileId::from_raw(0x7fff_1000);
    let mut allocations = Vec::new();
    for i in 0..LAYERS {
        let before = frozen.isolation().shared.slot_allocations;
        let mut layer = root.child();
        layer
            .create(new_file, format!("pub fn created{i}() {{}}\n"))
            .expect("writable");
        drop(layer);
        allocations.push(frozen.isolation().shared.slot_allocations - before);
    }
    let after = frozen.isolation();
    eprintln!(
        "freeing (created): {LAYERS} layers create one file; slots allocated per layer {allocations:?}; {:?}",
        after.slots
    );
    // The first creation allocates the file's input; every later one sets its text in the layer.
    assert!(allocations[0] >= 1);
    assert!(allocations[1..].iter().all(|&n| n == 0), "{allocations:?}");
    assert_eq!(after.slots.created_inputs, allocations[0]);
    assert_eq!(after.slots.owned, 0);
    drop(root);
    let db = match frozen.thaw() {
        Ok(db) => db,
        Err((_, e)) => panic!("thaw: {e:?}"),
    };
    s.return_database(db);
}
