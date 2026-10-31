//! Whether the `Expander` of `RaAnalyzer` generates the restore of a planted state (the inverse
//! of one refactor, as `ryo-v2-oracle`'s planter writes it): one test per cause found by the
//! search-recall harness (SEARCH_LEDGER "Generation"). Each test expands the planted text and
//! applies the Blueprint that should restore the clean text. Throwaway workspaces; the corpus is
//! never touched.

mod common;

use common::{Fixture, rel};
use ryo_v2_core::app::ports::{Analyzer, Expander, Verifier};
use ryo_v2_core::domain::text::splice;
use ryo_v2_core::domain::{Blueprint, Edit, IntentCall, MutationSpec, RelPath, Target};
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_ra::{RaAnalyzer, RaConfig, RaSession};
use std::collections::BTreeMap;
use std::sync::Arc;

/// The clean library: every function and item below is the clean side of one plant.
const LIB: &str = "\
pub mod plan;

// The widest slot a plan holds.
pub const WIDE: u32 = 8;

const LIMIT: u32 = 3; // slots per plan

pub struct Slot {
    pub width: u32,
    pub count: u32,
}

pub fn needed(v: &[u32]) -> u32 {
    v.iter().sum()
}

pub fn fit(width: u32, limit: u32) -> u32 {
    width.min(limit).min(WIDE)
}

pub fn pack(v: &[u32], limit: u32) -> u32 {
    let width = needed(v);
    let fitted = fit(width, limit);
    fitted + LIMIT
}

pub fn slot(v: &[u32]) -> Slot {
    let width = needed(v);
    Slot { width, count: 1 }
}
";

const PLAN: &str = "pub fn plan_one() -> u32 {\n    1\n}\n";

fn fixture(test: &str) -> Fixture {
    Fixture::new(
        test,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"gen\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
            ),
            ("src/lib.rs", LIB),
            ("src/plan.rs", PLAN),
        ],
    )
}

fn load(f: &Fixture) -> (RaAnalyzer, RaSession) {
    let a = RaAnalyzer::new(
        RaConfig {
            proc_macro: false,
            threads: Some(1),
            assert_revert: false,
            gap_detectors: true,
            assert_revert_every: None,
            cfg_test: false,
        },
        Arc::new(MonotonicClock::new()),
        Arc::new(RecordingObserver::default()),
    );
    let s = a.load(&f.dir).expect("load fixture");
    (a, s)
}

fn lib() -> RelPath {
    rel("src/lib.rs")
}

fn plan() -> RelPath {
    rel("src/plan.rs")
}

/// Applies the specs of a Blueprint to `texts`, per file.
fn apply(bp: &Blueprint, texts: &BTreeMap<RelPath, String>) -> BTreeMap<RelPath, String> {
    let mut out = texts.clone();
    for (path, specs) in bp.edits_by_file() {
        let specs: Vec<&MutationSpec> = specs.into_iter().collect();
        let base = texts.get(path).expect("text of the file");
        out.insert(path.clone(), splice(base, &specs).expect("splice"));
    }
    out
}

/// Expands the planted state (`planted` replaces the clean text of each file it names) over the
/// whole of both files, and returns the Blueprints with the planted texts.
fn expand_planted(
    test: &str,
    planted: &[(RelPath, String)],
) -> (Vec<Blueprint>, BTreeMap<RelPath, String>) {
    let f = fixture(test);
    let (a, mut s) = load(&f);
    let mut texts: BTreeMap<RelPath, String> = [lib(), plan()]
        .into_iter()
        .map(|p| {
            let t = a.text(&s, &p).expect("indexed");
            (p, t)
        })
        .collect();
    let mut state = Vec::new();
    for (p, t) in planted {
        texts.insert(p.clone(), t.clone());
        state.push(Edit::write(p.clone(), t.clone()));
    }
    let targets = [Target::new(lib(), None), Target::new(plan(), None)];
    let bps = a
        .expand(&mut s, &state, &targets, 500)
        .expect("expand the planted state");
    assert!(
        s.expander_panics().is_empty(),
        "nothing panicked: {:?}",
        s.expander_panics()
    );
    (bps, texts)
}

/// The Blueprints among `bps` whose specs turn `texts` into the clean texts.
fn restores(bps: &[Blueprint], texts: &BTreeMap<RelPath, String>) -> Vec<String> {
    let clean: BTreeMap<RelPath, String> =
        [(lib(), LIB.to_owned()), (plan(), PLAN.to_owned())].into();
    bps.iter()
        .filter(|bp| apply(bp, texts) == clean)
        .map(|bp| bp.intent().to_string())
        .collect()
}

fn names(bps: &[Blueprint]) -> Vec<String> {
    bps.iter().map(|bp| bp.intent().to_string()).collect()
}

/// Cause 1 (SEARCH_LEDGER "Generation"): a call folded into the initializer of a `let`.
/// Rust-analyzer's edit for it is a diff that lines up on the shared `let ` of the new and the
/// old statement, so a name read from the inserted text is not the new variable's. The name is
/// read from the edited text instead, and the restore is generated.
#[test]
fn extract_variable_restores_a_call_folded_into_a_let_initializer() {
    let planted = LIB.replace(
        "    let width = needed(v);\n    let fitted = fit(width, limit);\n",
        "    let fitted = fit(needed(v), limit);\n",
    );
    assert_ne!(planted, LIB);
    let (bps, texts) = expand_planted("gen-let-init", &[(lib(), planted)]);
    let got = restores(&bps, &texts);
    assert_eq!(
        got,
        vec!["extract-variable width".to_owned()],
        "the restore is generated: {:?}",
        names(&bps)
    );
}

/// Cause 2 (SEARCH_LEDGER "Generation", once CHECK_LEDGER S-6): a call folded into the value of a struct-literal field
/// (`Slot { width }` became `Slot { width: needed(v) }`). Rust-analyzer names the variable after
/// the field and writes the field as the shorthand; the expander writes the same.
#[test]
fn extract_variable_restores_a_record_field_value_as_the_shorthand() {
    let planted = LIB.replace(
        "    let width = needed(v);\n    Slot { width, count: 1 }\n",
        "    Slot { width: needed(v), count: 1 }\n",
    );
    assert_ne!(planted, LIB);
    let (bps, texts) = expand_planted("gen-field", &[(lib(), planted)]);
    let got = restores(&bps, &texts);
    assert_eq!(
        got,
        vec!["extract-variable width".to_owned()],
        "the restore is generated: {:?}",
        names(&bps)
    );
}

/// Cause 3: an item with a plain comment line right above it, moved to another module file
/// (the planter moves the item's lines and leaves the comment above the re-export). Rust-analyzer
/// attaches that comment to the `use` item, so the `use` text no longer starts with `pub use`.
#[test]
fn move_item_collapses_a_reexport_under_a_comment() {
    let planted_lib = LIB.replace("pub const WIDE: u32 = 8;\n", "pub use crate::plan::WIDE;\n");
    let planted_plan = format!("{PLAN}\npub const WIDE: u32 = 8;\n");
    assert_ne!(planted_lib, LIB);
    let (bps, texts) = expand_planted(
        "gen-comment-above",
        &[(lib(), planted_lib), (plan(), planted_plan)],
    );
    let got = restores(&bps, &texts);
    assert_eq!(
        got,
        vec!["move-item WIDE -> src/lib.rs".to_owned()],
        "the restore is generated: {:?}",
        names(&bps)
    );
}

/// Cause 4: an item whose line ends with a comment, moved to another module file (the planter
/// moves the whole line). The item's node ends before the comment, so the item is not followed by
/// its line end.
#[test]
fn move_item_collapses_an_item_with_a_trailing_comment() {
    let planted_lib = LIB.replace(
        "const LIMIT: u32 = 3; // slots per plan\n",
        "use crate::plan::LIMIT;\n",
    );
    let planted_plan = format!("{PLAN}\npub(crate) const LIMIT: u32 = 3; // slots per plan\n");
    assert_ne!(planted_lib, LIB);
    let (bps, texts) = expand_planted(
        "gen-comment-after",
        &[(lib(), planted_lib), (plan(), planted_plan)],
    );
    let got = restores(&bps, &texts);
    assert_eq!(
        got,
        vec!["move-item LIMIT -> src/lib.rs".to_owned()],
        "the restore is generated: {:?}",
        names(&bps)
    );
}

/// Rust-analyzer names an extracted value without regard to the locals of the block: for the
/// value of the field `width` it proposes `width` even when the block binds `width` already and
/// reads it later (`count: width`). The new `let` would shadow that binding, so `count` would read
/// the new value; the expander refuses the name.
#[test]
fn extract_variable_refuses_a_name_the_block_still_reads() {
    let planted = LIB.replace(
        "    let width = needed(v);\n    Slot { width, count: 1 }\n",
        "    let width = needed(v);\n    Slot { width: needed(v), count: width }\n",
    );
    assert_ne!(planted, LIB);
    let (bps, texts) = expand_planted("gen-shadow", &[(lib(), planted)]);
    let rewritten: Vec<String> = bps
        .iter()
        .filter(|bp| matches!(bp.intent(), IntentCall::ExtractVariable { .. }))
        .filter(|bp| !apply(bp, &texts)[&lib()].contains("Slot { width: needed(v), count: width }"))
        .map(|bp| bp.intent().to_string())
        .collect();
    assert_eq!(
        rewritten,
        Vec::<String>::new(),
        "no extraction of the field value shadows `width`: {:?}",
        names(&bps)
    );
}
