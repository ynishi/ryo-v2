//! The differential gate of stage 2 (D11, V2): rust-analyzer diagnostics of a layer at level k
//! equal those of the flat reference check of the composed edits on a twin session, over a
//! body-level and an item-level edit family, at two depths; and the base takes no write but the
//! listed shared ones (I2).
//!
//! The layers sit over the session's own database, lent to the Engine and given back at the end;
//! the twin is a second session of the same workspace that runs `RaAnalyzer::reference_check`
//! (write, diagnose, write back on the session's own database) with no layer code.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ryo_v2_core::app::ports::Analyzer;
use ryo_v2_core::domain::{Edit, RelPath, Scope, ScopedFindings};
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_engine::{Engine, OpenLayer, SealedLayer};
use ryo_v2_ra::{RaAnalyzer, RaConfig, RaSession};

/// A temp workspace that removes itself on drop (never under a `target` directory, which the
/// session does not index).
struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(test: &str, files: &[(&str, String)]) -> Self {
        let dir = std::env::temp_dir().join(format!("ryo-v2-engine-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
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

fn rel(s: &str) -> RelPath {
    RelPath::parse(s).unwrap()
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

// W: three crates, `wb` depending on `wa` (as in ryo-v2-ra's tests/stack.rs).
const A_LIB: &str = "pub mod x;\npub struct S;\npub fn ga(s: S) {\n    s.missing();\n}\npub fn fa(_x: MissingA) {}\n";
const A_X: &str = "pub fn xf() -> u32 {\n    1\n}\n";
const B_LIB: &str = "pub mod y;\npub struct T;\npub fn gb(t: T) {\n    t.missing();\n}\npub fn fb(_x: MissingB) {}\n";
const B_Y: &str = "pub fn yf() -> u32 {\n    2\n}\n";
const C_LIB: &str = "pub mod z;\npub struct U;\npub fn gc(u: U) {\n    u.missing();\n}\npub fn fc(_x: MissingC) {}\n";
const C_Z: &str = "pub fn zf() -> u32 {\n    3\n}\n";

fn w_fixture(test: &str) -> Fixture {
    let manifest = |name: &str, deps: &str| {
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n{deps}")
    };
    Fixture::new(
        test,
        &[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"a\", \"b\", \"c\"]\nresolver = \"2\"\n".to_owned(),
            ),
            ("a/Cargo.toml", manifest("wa", "")),
            (
                "b/Cargo.toml",
                manifest("wb", "\n[dependencies]\nwa = { path = \"../a\" }\n"),
            ),
            ("c/Cargo.toml", manifest("wc", "")),
            ("a/src/lib.rs", A_LIB.to_owned()),
            ("a/src/x.rs", A_X.to_owned()),
            ("b/src/lib.rs", B_LIB.to_owned()),
            ("b/src/y.rs", B_Y.to_owned()),
            ("c/src/lib.rs", C_LIB.to_owned()),
            ("c/src/z.rs", C_Z.to_owned()),
        ],
    )
}

/// A level: whole-file writes of existing files (modify-only edits).
type Level = Vec<(&'static str, String)>;

/// The W sequences: two levels each, of the body-level family, of the item-level family, and
/// of both.
fn w_sequences() -> Vec<(&'static str, Vec<Level>)> {
    // Body-level: only expressions inside fn bodies change; the item trees stay equal.
    let body_a = A_LIB.replace(
        "    s.missing();\n",
        "    s.missing_body();\n    let _n: u32 = \"no\";\n",
    );
    let body_x = A_X.replace("    1\n", "    1 + true\n");
    let body_y = B_Y.replace("    2\n", "    \"two\"\n");
    let body_a2 = A_LIB.replace("    s.missing();\n", "    let _ok = 1u32;\n");
    // Item-level: a renamed item, a new item and a `use`, a changed signature, used across the
    // crate boundary.
    let item_x =
        "pub fn xg() -> u32 {\n    1\n}\npub fn helper(n: u32) -> u32 {\n    n + xg()\n}\n"
            .to_owned();
    let item_a = format!("{A_LIB}pub fn calls() -> u32 {{\n    x::xf() + x::helper(2)\n}}\n");
    let item_b = format!(
        "{B_LIB}use wa::x::xg;\npub fn cross() -> u32 {{\n    xg() + wa::x::xf() + wa::x::helper()\n}}\npub fn wrong(s: wa::S) {{\n    wa::ga(s, 1);\n}}\n"
    );
    let item_c = C_LIB.replace("pub fn gc(u: U)", "pub fn gc(u: U, v: V)");
    vec![
        (
            "body",
            vec![
                vec![
                    ("a/src/lib.rs", body_a.clone()),
                    ("a/src/x.rs", body_x.clone()),
                ],
                vec![
                    ("b/src/y.rs", body_y.clone()),
                    ("a/src/lib.rs", body_a2.clone()),
                ],
            ],
        ),
        (
            "item",
            vec![
                vec![
                    ("a/src/x.rs", item_x.clone()),
                    ("a/src/lib.rs", item_a.clone()),
                ],
                vec![
                    ("b/src/lib.rs", item_b.clone()),
                    ("c/src/lib.rs", item_c.clone()),
                ],
            ],
        ),
        (
            "item then body",
            vec![
                vec![("a/src/x.rs", item_x)],
                vec![("a/src/x.rs", body_x), ("b/src/y.rs", body_y)],
            ],
        ),
        (
            "body then item",
            vec![
                vec![("a/src/lib.rs", body_a)],
                vec![("a/src/lib.rs", item_a), ("b/src/lib.rs", item_b)],
            ],
        ),
    ]
}

/// The extract-trait-multicrate base (four crates, cross-crate callers).
fn multicrate() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../corpus/base/extract-trait-multicrate")
}

fn multicrate_text(path: &str) -> String {
    std::fs::read_to_string(multicrate().join(path)).unwrap()
}

fn replace_once(text: &str, from: &str, to: &str) -> String {
    assert!(text.contains(from), "the base text holds {from:?}");
    text.replacen(from, to, 1)
}

/// The multicrate sequences: a body-level and an item-level family, two levels each.
fn multicrate_sequences() -> Vec<(&'static str, Vec<Level>)> {
    let lib = multicrate_text("lib/src/lib.rs");
    let user = multicrate_text("user/src/lib.rs");
    let body_lib = replace_once(&lib, "self.base + 1\n", "self.base + \"1\"\n");
    let body_lib2 = replace_once(&body_lib, "self.base + 2\n", "self.base.missing()\n");
    let item_lib = replace_once(
        &lib,
        "pub fn two(&self) -> u32 {",
        "pub fn two_renamed(&self) -> u32 {",
    );
    let item_lib2 = replace_once(
        &item_lib,
        "pub struct Case01 {\n    pub base: u32,\n}",
        "pub struct Case01 {\n    pub base: u32,\n    pub added: NotAType,\n}",
    );
    let body_user =
        format!("{user}\npub fn extra_user() -> u32 {{\n    let x: u32 = 1u8;\n    x\n}}\n");
    vec![
        (
            "multicrate body",
            vec![
                vec![("lib/src/lib.rs", body_lib)],
                vec![("lib/src/lib.rs", body_lib2)],
            ],
        ),
        (
            "multicrate item",
            vec![
                vec![("lib/src/lib.rs", item_lib)],
                vec![
                    ("lib/src/lib.rs", item_lib2),
                    ("user/src/lib.rs", body_user),
                ],
            ],
        ),
    ]
}

/// The composition of `levels`: the last text of each path.
fn compose(levels: &[Level]) -> Vec<Edit> {
    let mut net: BTreeMap<&str, &String> = BTreeMap::new();
    for (path, text) in levels.iter().flatten() {
        net.insert(path, text);
    }
    net.into_iter()
        .map(|(path, text)| Edit::write(rel(path), text.clone()))
        .collect()
}

fn write_level(s: &RaSession, layer: &mut OpenLayer<ra_ap_ide_db::RootDatabase>, level: &Level) {
    for (path, text) in level {
        let file = s.file_id(&rel(path)).expect("a load-time file");
        layer
            .write(file, text.as_str())
            .expect("a level is written before its diagnosis");
    }
}

/// Runs every sequence of `sequences` on `dir`, comparing each level with the flat check.
/// Returns the number of comparisons.
fn run_gate(base: &str, dir: &Path, sequences: &[(&'static str, Vec<Level>)]) -> usize {
    let a = analyzer();
    let mut s = Analyzer::load(&a, dir).expect("load the layered session");
    let mut twin = Analyzer::load(&a, dir).expect("load the flat twin");
    let frozen = Engine::freeze(s.lend_database());
    let at_freeze = frozen.isolation();
    let root = frozen.root();
    let mut compared = 0;

    for (name, levels) in sequences {
        let mut parent: SealedLayer<_> = root.clone();
        let mut sealed: Vec<(SealedLayer<_>, ScopedFindings)> = Vec::new();
        for (k, level) in levels.iter().enumerate() {
            let at = format!("{name}, level {}", k + 1);
            let flat = a
                .reference_check(&mut twin, &compose(&levels[..=k]), Scope::Workspace)
                .unwrap_or_else(|e| panic!("{at}: the flat check: {e:?}"));
            let mut layer = parent.child();
            write_level(&s, &mut layer, level);
            let layered = a
                .diagnose_database(&s, &layer.db())
                .unwrap_or_else(|e| panic!("{at}: the layer's diagnosis: {e:?}"));
            assert_eq!(layered.findings, flat.findings, "{at}: findings");
            assert_eq!(layered.files, flat.files, "{at}: files");
            assert_eq!(layered.unseen_at, flat.unseen_at, "{at}: unseen_at");
            assert!(
                !flat.findings.is_empty(),
                "{at}: the edits introduce errors"
            );
            compared += 1;
            let stats = layer.stats();
            assert!(stats.memos > 0, "{at}: the layer computed nothing");
            eprintln!(
                "V2 {base}, {at}: {} findings equal; {stats:?}",
                flat.findings.len()
            );
            let layer = layer
                .seal()
                .expect("no clone of the layer's database lives");
            parent = layer.clone();
            sealed.push((layer, flat));
        }
        // Each sealed level, read through a throwaway child after its children were diagnosed,
        // still gives its own flat check; the read takes nothing into the sealed layer, and
        // re-verifies nothing its own diagnosis verified.
        for (k, (layer, flat)) in sealed.iter().enumerate() {
            let at = format!("{name}, sealed level {}", k + 1);
            let before = layer.stats();
            let (read, deep_verifies) = layer
                .read(|db| {
                    let found = a.diagnose_database(&s, db);
                    let child = salsa::fork::layer_of(db).expect("a layered handle").stats();
                    (found, child.deep_verifies)
                })
                .expect("no clone escapes the read");
            let read = read.unwrap_or_else(|e| panic!("{at}: {e:?}"));
            assert_eq!(&read, flat, "{at}: read");
            // What a layer takes: memos, stamps, deep verifies, tracked entries, tombstones. The
            // read's child may build the layer's chain view (`flat_*`), a cache of the chain, on
            // its first lookup.
            let took = |s: &salsa::fork::LayerStats| {
                (
                    s.memos,
                    s.stamps,
                    s.deep_verifies,
                    s.tracked_entries,
                    s.tombstones,
                )
            };
            assert_eq!(
                took(&layer.stats()),
                took(&before),
                "{at}: the sealed layer took nothing"
            );
            assert_eq!(deep_verifies, 0, "{at}: the read child deep-verified");
        }
    }

    // I2: no write reached the base but the listed shared ones.
    let after = frozen.isolation();
    assert_eq!(after.refused, 0, "refused base writes");
    assert_eq!(
        at_freeze.fingerprint.preserved_in(&after.fingerprint),
        Ok(()),
        "the base's memos, revisions and updated_at are those at the freeze"
    );
    // Every layer but the root has dropped, and gave back what it owned.
    assert_eq!(
        after.slots.owned, 0,
        "layer-owned slots after every layer dropped"
    );
    eprintln!(
        "V2 {base}: {compared} levels compared, 0 differences; I2: refused {}, {:?}, {:?}, {:?}, {} base slots and {} memos unchanged",
        after.refused,
        after.shared,
        after.skipped,
        after.slots,
        at_freeze.fingerprint.slots(),
        at_freeze.fingerprint.memos,
    );

    drop(root);
    let db = match frozen.thaw() {
        Ok(db) => db,
        Err((_, e)) => panic!("thaw: {e:?}"),
    };
    s.return_database(db);
    // The returned database diagnoses as before it was lent.
    let back = a
        .reference_check(&mut s, &[], Scope::Workspace)
        .expect("a check on the returned database");
    let twin_base = a
        .reference_check(&mut twin, &[], Scope::Workspace)
        .expect("twin");
    assert_eq!(back, twin_base, "the returned database");
    compared
}

/// V2 and I2 over W and the multicrate base: body-level and item-level families, two depths.
///
/// One test, so the two loads per base run one after the other.
#[test]
fn v2_layers_diagnose_as_their_flat_check() {
    let w = w_fixture("v2");
    let compared = run_gate("W", &w.dir, &w_sequences());
    assert_eq!(compared, 8);
    let compared = run_gate("multicrate", &multicrate(), &multicrate_sequences());
    assert_eq!(compared, 4);
}
