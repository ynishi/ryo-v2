//! `Expander` for `RaAnalyzer`, on one throwaway workspace of two crates (`geo` with three
//! files, `app` using it). The corpus is never touched.

mod common;

use common::{Fixture, rel};
use ryo_v2_core::app::ports::{Analyzer, Expander, Verifier};
use ryo_v2_core::domain::text::splice;
use ryo_v2_core::domain::{
    Blueprint, Edit, Freshness, IntentCall, IntentKind, MutationSpec, RelPath, Scope, Target,
    TextRange,
};
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_ra::{RaAnalyzer, RaConfig, RaSession};
use std::collections::BTreeMap;
use std::sync::Arc;

const SHAPES: &str = "\
use std::fmt;

pub struct Circle {
    pub radius: f64,
}

pub enum Shape {
    Round(Circle),
    Square(f64),
}

pub fn area(shape: &Shape) -> f64 {
    let pi = std::f64::consts::PI;
    match shape {
        Shape::Round(c) => pi * c.radius * c.radius,
        Shape::Square(side) => side * side,
    }
}

pub fn describe(shape: &Shape) -> String {
    let size = area(shape);
    let label = format!(\"area {size:.1}\");
    let upper = label.to_uppercase();
    upper.trim().to_string()
}

pub fn scale(shapes: &[Shape], factor: f64) -> Vec<f64> {
    let mut out = Vec::new();
    for s in shapes {
        let a = area(s);
        out.push(a * factor);
    }
    out
}

impl fmt::Display for Circle {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, \"circle {}\", self.radius)
    }
}
";

const REPORT: &str = "\
use crate::shapes::{Shape, describe};

pub fn summary(shapes: &[Shape]) -> String {
    let mut lines = Vec::new();
    for s in shapes {
        lines.push(describe(s));
    }
    lines.join(\"\\n\")
}
";

const LIB: &str = "pub mod report;\npub mod shapes;\n\npub use shapes::{Circle, Shape};\n";

const APP: &str = "\
use geo::shapes::{Circle, Shape};

pub fn demo() -> String {
    let shapes = vec![Shape::Round(Circle { radius: 2.0 }), Shape::Square(3.0)];
    geo::report::summary(&shapes)
}
";

const EXTRA: &str = "\npub fn extra_total(v: &[f64]) -> f64 {\n    let total = v.iter().sum::<f64>();\n    total.sqrt()\n}\n";

fn fixture(test: &str) -> Fixture {
    Fixture::new(
        test,
        &[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"geo\", \"app\"]\nresolver = \"2\"\n",
            ),
            (
                "geo/Cargo.toml",
                "[package]\nname = \"geo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("geo/src/lib.rs", LIB),
            ("geo/src/shapes.rs", SHAPES),
            ("geo/src/report.rs", REPORT),
            (
                "app/Cargo.toml",
                "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ngeo = { path = \"../geo\" }\n",
            ),
            ("app/src/lib.rs", APP),
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

fn whole(path: &str) -> Target {
    Target::new(rel(path), None)
}

fn shapes_path() -> RelPath {
    rel("geo/src/shapes.rs")
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

fn texts_of(a: &RaAnalyzer, s: &RaSession) -> BTreeMap<RelPath, String> {
    ["geo/src/lib.rs", "geo/src/shapes.rs", "geo/src/report.rs"]
        .iter()
        .map(|p| (rel(p), a.text(s, &rel(p)).expect("indexed")))
        .collect()
}

fn kinds(bps: &[Blueprint]) -> BTreeMap<IntentKind, usize> {
    let mut m = BTreeMap::new();
    for b in bps {
        *m.entry(b.intent().kind()).or_insert(0) += 1;
    }
    m
}

#[test]
fn every_expansion_conforms() {
    let f = fixture("expand_conforms");
    let (a, mut s) = load(&f);
    let t0 = std::time::Instant::now();
    let bps = a
        .expand(
            &mut s,
            &[],
            &[whole("geo/src/shapes.rs"), whole("geo/src/report.rs")],
            500,
        )
        .expect("expand");
    println!("expand: {} blueprints in {:?}", bps.len(), t0.elapsed());
    let counts = kinds(&bps);
    println!("candidates per kind: {counts:?}");
    for k in IntentKind::ALL {
        assert!(
            counts.get(&k).copied().unwrap_or(0) > 0,
            "no {k}: {counts:?}"
        );
    }
    let texts = texts_of(&a, &s);
    for b in &bps {
        let base: BTreeMap<RelPath, String> = b
            .specs()
            .iter()
            .map(|sp| (sp.path().clone(), texts[sp.path()].clone()))
            .collect();
        b.intent()
            .conforms(b.specs(), &base)
            .unwrap_or_else(|e| panic!("{} does not conform: {e}", b.intent()));
        for r in b.reads() {
            assert_eq!(r.check(&texts[r.path()]), Freshness::Fresh);
        }
        assert_eq!(b.record().proposer(), "ryo-v2-ra/expand");
    }
}

#[test]
fn rename_local_expansion_covers_all_uses() {
    let f = fixture("expand_rename");
    let (a, mut s) = load(&f);
    let bps = a
        .expand(&mut s, &[], &[whole("geo/src/shapes.rs")], 500)
        .expect("expand");
    let renames: Vec<&Blueprint> = bps
        .iter()
        .filter(|b| matches!(b.intent(), IntentCall::RenameLocal { .. }))
        .collect();
    assert!(!renames.is_empty());
    // `size` is bound once and used once, inside a format string.
    let size = renames
        .iter()
        .find(|b| matches!(b.intent(), IntentCall::RenameLocal { from, .. } if from.as_str() == "size"))
        .expect("a rename of `size`");
    assert_eq!(
        size.specs().len(),
        2,
        "definition and the use in the format string"
    );
    // Applying it leaves no `size` binding or use in `describe`.
    let texts = texts_of(&a, &s);
    let after = apply(size, &texts);
    let IntentCall::RenameLocal { to, .. } = size.intent() else {
        unreachable!()
    };
    let new = &after[&shapes_path()];
    assert!(new.contains(&format!("let {to} = area(shape);")), "{new}");
    assert!(new.contains(&format!("{{{to}:.1}}")), "{new}");
    assert!(!new.contains("size"), "{new}");
}

#[test]
fn expansion_leaves_session_at_base() {
    let f = fixture("expand_base");
    let (a, mut s) = load(&f);
    let texts = texts_of(&a, &s);
    let baseline = Verifier::baseline(&a, &s);
    let state = [Edit::write(
        shapes_path(),
        format!("{}{EXTRA}", texts[&shapes_path()]),
    )];
    let bps = a
        .expand(&mut s, &state, &[whole("geo/src/shapes.rs")], 50)
        .expect("expand over a state");
    assert!(!bps.is_empty());
    assert_eq!(texts_of(&a, &s), texts);
    assert_eq!(Verifier::baseline(&a, &s), baseline);
    // A check still works afterwards.
    a.check(&mut s, &state, Scope::Affected)
        .expect("check after expand");
    assert_eq!(texts_of(&a, &s), texts);
    // And so does an expansion of the base state.
    a.expand(&mut s, &[], &[whole("geo/src/report.rs")], 5)
        .expect("expand again");
    assert_eq!(texts_of(&a, &s), texts);
}

#[test]
fn expansion_reads_the_given_state() {
    let f = fixture("expand_state");
    let (a, mut s) = load(&f);
    let texts = texts_of(&a, &s);
    let state_text = format!("{}{EXTRA}", texts[&shapes_path()]);
    let state = [Edit::write(shapes_path(), state_text.clone())];
    let bps = a
        .expand(&mut s, &state, &[whole("geo/src/shapes.rs")], 500)
        .expect("expand over a state");
    let mut on_new_code = 0;
    for b in &bps {
        for r in b.reads().iter().filter(|r| r.path() == &shapes_path()) {
            assert_eq!(r.check(&state_text), Freshness::Fresh, "{}", b.intent());
        }
        let in_extra = b.specs().iter().any(|sp| {
            sp.path() == &shapes_path() && sp.range().start() as usize > texts[&shapes_path()].len()
        });
        if in_extra {
            on_new_code += 1;
        }
        let base: BTreeMap<RelPath, String> = b
            .specs()
            .iter()
            .map(|sp| {
                let t = if sp.path() == &shapes_path() {
                    state_text.clone()
                } else {
                    texts[sp.path()].clone()
                };
                (sp.path().clone(), t)
            })
            .collect();
        assert!(b.intent().conforms(b.specs(), &base).is_ok());
    }
    assert!(
        on_new_code > 0,
        "no Blueprint touches the code only the state has"
    );
}

#[test]
fn expansion_respects_max_and_targets() {
    let f = fixture("expand_max");
    let (a, mut s) = load(&f);
    let three = a
        .expand(&mut s, &[], &[whole("geo/src/shapes.rs")], 3)
        .expect("expand");
    assert_eq!(three.len(), 3);
    assert!(
        a.expand(&mut s, &[], &[whole("geo/src/shapes.rs")], 0)
            .expect("expand")
            .is_empty()
    );
    assert!(a.expand(&mut s, &[], &[], 10).expect("expand").is_empty());

    // Only report.rs: every Blueprint touches it.
    let only = a
        .expand(&mut s, &[], &[whole("geo/src/report.rs")], 500)
        .expect("expand");
    assert!(!only.is_empty());
    for b in &only {
        assert!(
            b.specs()
                .iter()
                .any(|sp| sp.path() == &rel("geo/src/report.rs")),
            "{}",
            b.intent()
        );
    }

    // A range: only `scale`.
    let text = &texts_of(&a, &s)[&shapes_path()];
    let lo = text.find("pub fn scale").expect("scale") as u32;
    let hi = text[lo as usize..].find("\n}\n").expect("end") as u32 + lo + 3;
    let ranged = a
        .expand(
            &mut s,
            &[],
            &[Target::new(
                shapes_path(),
                Some(TextRange::new(lo, hi).unwrap()),
            )],
            500,
        )
        .expect("expand");
    assert!(!ranged.is_empty());
    for b in &ranged {
        if matches!(b.intent(), IntentCall::MoveItem { .. }) {
            continue;
        }
        let first = &b.specs()[0];
        assert!(
            first.range().start() >= lo && first.range().start() <= hi,
            "{} at {}",
            b.intent(),
            first.range()
        );
    }
}

#[test]
fn move_item_forward_then_collapse_restores_the_text() {
    let f = fixture("expand_move");
    let (a, mut s) = load(&f);
    let texts = texts_of(&a, &s);
    let bps = a
        .expand(&mut s, &[], &[whole("geo/src/shapes.rs")], 500)
        .expect("expand");
    let moves: Vec<&Blueprint> = bps
        .iter()
        .filter(|b| matches!(b.intent(), IntentCall::MoveItem { .. }))
        .collect();
    assert!(!moves.is_empty(), "no move-item: {:?}", kinds(&bps));
    // Forward: `scale` moves to report.rs and leaves a use.
    let fwd = moves
        .iter()
        .find(|b| {
            matches!(b.intent(), IntentCall::MoveItem { item, to }
                if item.as_str() == "scale" && to == &rel("geo/src/report.rs"))
        })
        .expect("scale -> report.rs");
    let moved = apply(fwd, &texts);
    assert!(
        moved[&shapes_path()].contains("use crate::report::scale;"),
        "{}",
        moved[&shapes_path()]
    );
    assert!(moved[&rel("geo/src/report.rs")].contains("pub fn scale"));

    // From the moved state, the reverse is offered and restores the text.
    let state: Vec<Edit> = [shapes_path(), rel("geo/src/report.rs")]
        .iter()
        .map(|p| Edit::write(p.clone(), moved[p].clone()))
        .collect();
    let back = a
        .expand(&mut s, &state, &[whole("geo/src/shapes.rs")], 500)
        .expect("expand over the moved state");
    let collapse = back
        .iter()
        .find(|b| {
            matches!(b.intent(), IntentCall::MoveItem { item, to }
                if item.as_str() == "scale" && to == &shapes_path())
        })
        .expect("the collapse");
    let restored = apply(collapse, &moved);
    assert_eq!(restored[&shapes_path()], texts[&shapes_path()]);
    assert_eq!(
        restored[&rel("geo/src/report.rs")],
        texts[&rel("geo/src/report.rs")]
    );
}

#[test]
fn extract_function_names_are_not_fun_name() {
    let f = fixture("expand_names");
    let (a, mut s) = load(&f);
    let bps = a
        .expand(
            &mut s,
            &[],
            &[whole("geo/src/shapes.rs"), whole("geo/src/report.rs")],
            500,
        )
        .expect("expand");
    let fns: Vec<&Blueprint> = bps
        .iter()
        .filter(|b| matches!(b.intent(), IntentCall::ExtractFunction { .. }))
        .collect();
    assert!(!fns.is_empty(), "no extract-function: {:?}", kinds(&bps));
    for b in fns {
        let IntentCall::ExtractFunction { name } = b.intent() else {
            unreachable!()
        };
        assert_ne!(name.as_str(), "fun_name");
        assert!(
            b.specs().iter().all(|sp| !sp.text().contains("fun_name")),
            "{}",
            b.intent()
        );
        assert!(
            b.specs()
                .iter()
                .any(|sp| sp.text().contains(&format!("fn {name}"))),
            "{}",
            b.intent()
        );
    }
}

#[test]
fn blueprint_ids_are_v7_and_deterministic() {
    let f = fixture("expand_ids");
    let (a, mut s) = load(&f);
    let targets = [whole("geo/src/shapes.rs")];
    let first = a.expand(&mut s, &[], &targets, 100).expect("expand");
    let second = a.expand(&mut s, &[], &targets, 100).expect("expand");
    let ids = |v: &[Blueprint]| v.iter().map(|b| b.id()).collect::<Vec<_>>();
    assert!(!first.is_empty());
    assert_eq!(ids(&first), ids(&second));
    let unique: std::collections::BTreeSet<_> = ids(&first).into_iter().collect();
    assert_eq!(unique.len(), first.len());
    for b in &first {
        assert_eq!(b.id().as_uuid().get_version_num(), 7);
    }
    let texts = texts_of(&a, &s);
    let state = [Edit::write(
        shapes_path(),
        format!("{}{EXTRA}", texts[&shapes_path()]),
    )];
    let other = a.expand(&mut s, &state, &targets, 100).expect("expand");
    let first_ids: std::collections::BTreeSet<_> = ids(&first).into_iter().collect();
    assert!(
        ids(&other).iter().all(|i| !first_ids.contains(i)),
        "ids of another state repeat"
    );
}
