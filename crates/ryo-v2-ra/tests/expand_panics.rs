//! Panics inside the `Expander` of `RaAnalyzer`: a shape that makes rust-analyzer panic is
//! refused before the call, and a panic that still happens is contained per call and recorded
//! on the session. Throwaway workspaces; the corpus is never touched.

mod common;

use common::{Fixture, rel};
use ryo_v2_core::app::ports::{Analyzer, Expander, Verifier};
use ryo_v2_core::domain::{Blueprint, Command, CommandKind, Scope, Target};
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_ra::{RaAnalyzer, RaConfig, RaSession};
use std::collections::BTreeSet;
use std::sync::Arc;

/// A struct literal whose first field is gated by a feature that is on by default, as in the
/// sample where the panic was seen; the same function has a call outside the literal to
/// extract, and a second module gives move-item a destination.
const GATED: &str = "\
pub mod other;

pub struct Holder {
    #[cfg(feature = \"workspace\")]
    pub dep: std::sync::OnceLock<u32>,
    pub n: usize,
}

pub fn make(v: &[u32]) -> usize {
    let h = Holder {
        #[cfg(feature = \"workspace\")]
        dep: std::sync::OnceLock::new(),
        n: 0,
    };
    let m = h.n + 1;
    m.max(v.iter().count())
}
";

const OTHER: &str = "pub fn one() -> u32 {\n    1\n}\n";

fn gated_fixture(test: &str) -> Fixture {
    Fixture::new(
        test,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"gated\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[features]\ndefault = [\"workspace\"]\nworkspace = []\n\n[workspace]\n",
            ),
            ("src/lib.rs", GATED),
            ("src/other.rs", OTHER),
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
            ..Default::default()
        },
        Arc::new(MonotonicClock::new()),
        Arc::new(RecordingObserver::default()),
    );
    let s = a.load(&f.dir).expect("load fixture");
    (a, s)
}

/// Rust-analyzer's extract-variable names the variable after the whole text of the record
/// field whose value it extracts; with an attribute in front that text is no identifier and its
/// syntax factory panics. The expander refuses that value and still offers the other
/// expressions of the function. (The value of a field without an attribute is extracted, as the
/// shorthand: `tests/expand_gen.rs`.)
#[test]
fn extract_variable_skips_the_value_of_an_attributed_record_field() {
    let f = gated_fixture("expand-gated-field");
    let (a, mut s) = load(&f);
    let target = Target::new(rel("src/lib.rs"), None);
    let bps = a
        .expand(&mut s, &[], &[target], 200)
        .expect("expansion of a struct literal with a cfg-gated field");
    let extracted: Vec<String> = bps
        .iter()
        .filter_map(|bp| match bp.command() {
            Command::ExtractVariable { name } => Some(name.as_str().to_owned()),
            _ => None,
        })
        .collect();
    assert!(
        // `make` is the only function of the target file, so these come from the same function
        // as the struct literal.
        !extracted.is_empty(),
        "the expressions after the struct literal, in the same function, are still extracted: {extracted:?} (panics {:?})",
        s.expander_panics()
    );
    assert!(
        !extracted.iter().any(|n| n == "dep"),
        "the value of the attributed field is not offered: {extracted:?}"
    );
    assert!(
        s.expander_panics().is_empty(),
        "nothing panicked: {:?}",
        s.expander_panics()
    );
}

/// The edit kinds among `bps`.
fn kinds(bps: &[Blueprint]) -> BTreeSet<CommandKind> {
    bps.iter().map(|bp| bp.command().kind()).collect()
}

/// A panic in a call of one edit kind that reaches rust-analyzer (realizing a seed, or the
/// candidates of a target) is contained, for each of the five kinds: the other kinds' Blueprints
/// come back, the panic is recorded with its kind and file, and the session stays usable.
#[test]
fn an_expander_panic_is_contained_and_recorded() {
    let f = gated_fixture("expand-fault");
    let (a, mut s) = load(&f);
    let target = Target::new(rel("src/lib.rs"), None);
    let all = kinds(
        &a.expand(&mut s, &[], std::slice::from_ref(&target), 500)
            .expect("expansion without a fault"),
    );
    assert_eq!(
        all,
        CommandKind::ALL.iter().copied().collect(),
        "the fixture gives every kind, so each fault below removes something"
    );
    for kind in CommandKind::ALL {
        s.inject_expander_fault(Some(kind));
        let got = kinds(
            &a.expand(&mut s, &[], std::slice::from_ref(&target), 500)
                .unwrap_or_else(|e| {
                    panic!("a contained panic of {kind:?} leaves the expansion Ok: {e}")
                }),
        );
        let mut rest = all.clone();
        rest.remove(&kind);
        assert_eq!(
            got, rest,
            "{kind:?} gives nothing, every other kind still comes"
        );
        let panics = s.expander_panics();
        assert!(
            panics.iter().any(|p| p.kind == Some(kind)
                && p.path == Some(rel("src/lib.rs"))
                && p.message.contains("fault injected")),
            "the panic of {kind:?} is recorded with its kind and file: {panics:?}"
        );
    }
    assert!(
        s.expander_panics().iter().all(|p| p.kind.is_some()),
        "no panic reached the backstop of the whole generation"
    );

    a.check(&mut s, &[], Scope::Affected)
        .expect("a check works after the panics");
    s.inject_expander_fault(None);
    let back = kinds(
        &a.expand(&mut s, &[], &[target], 500)
            .expect("expansion without the fault"),
    );
    assert_eq!(back, all, "every kind comes back once the fault is gone");
}
