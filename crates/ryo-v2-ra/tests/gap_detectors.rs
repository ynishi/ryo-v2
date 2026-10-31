//! The detectors of duplicate names (E0255) and of `Copy` impls on non-`Copy` fields (E0204), on throwaway workspaces.

use ryo_v2_core::app::ports::Analyzer;
use ryo_v2_core::app::verify::verify;
use ryo_v2_core::domain::{Diagnostic, Edit, RelPath, Scope};
use ryo_v2_core::infra::clock::MonotonicClock;
use ryo_v2_core::infra::observer::RecordingObserver;
use ryo_v2_ra::{GapDetector, RaAnalyzer, RaConfig, RaSession};
use std::path::PathBuf;
use std::sync::Arc;

const BASE: &str = "pub struct S;\n";

/// A temp workspace (one crate) that removes itself on drop.
struct Probe {
    dir: PathBuf,
}

impl Probe {
    fn new(test: &str) -> Self {
        // Not CARGO_TARGET_TMPDIR: `session::load` skips every path with a `target` component.
        let dir = std::env::temp_dir().join(format!("ryo-v2-gap-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/lib.rs"), BASE).unwrap();
        Self { dir }
    }

    fn load(&self, gap_detectors: bool) -> (RaAnalyzer, RaSession) {
        let a = RaAnalyzer::new(
            RaConfig {
                proc_macro: false,
                threads: Some(1),
                assert_revert: true,
                gap_detectors,
                assert_revert_every: None,
                cfg_test: false,
            },
            Arc::new(MonotonicClock::new()),
            Arc::new(RecordingObserver::default()),
        );
        let s = a.load(&self.dir).expect("load probe");
        assert!(a.baseline(&s).is_empty(), "got {:?}", a.baseline(&s));
        (a, s)
    }

    /// New errors of writing `text` to src/lib.rs.
    fn errors_of(&self, gap_detectors: bool, text: &str) -> Vec<Diagnostic> {
        let (a, mut s) = self.load(gap_detectors);
        let edit = Edit::write(RelPath::parse("src/lib.rs").unwrap(), text.to_owned());
        verify(&a, &mut s, &[edit], Scope::Affected)
            .expect("verify")
            .new_errors
            .errors
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `(code, line)` of the diagnostics whose code is `code`, sorted.
fn codes_of(errors: &[Diagnostic], code: &str) -> Vec<(String, u32)> {
    let mut got: Vec<(String, u32)> = errors
        .iter()
        .filter(|d| d.code.as_str() == code)
        .map(|d| (d.code.as_str().to_owned(), d.line))
        .collect();
    got.sort();
    got
}

/// The 1-based line of the first line of `text` that contains `needle`.
fn line_of(text: &str, needle: &str) -> u32 {
    let at = text
        .lines()
        .position(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no line with {needle:?}"));
    u32::try_from(at + 1).unwrap()
}

fn at(code: &str, line: u32) -> Vec<(String, u32)> {
    vec![(code.to_owned(), line)]
}

/// An item and a `use` that define one name in one namespace are reported at the later of the two:
/// the item head when the `use` comes first, the `use` leaf when the item comes first, and in an
/// inline module as in the root; the switch turns the detector off.
#[test]
fn duplicate_name_reports_item_and_use() {
    let p = Probe::new("dup-positive");

    // Type namespace, through a cyclic re-export; the `use` comes first.
    let a = "pub struct S;\npub mod sizes {\n    pub use crate::Size;\n}\nuse sizes::Size;\npub struct Size {\n    pub side: u32,\n}\n";
    let got = p.errors_of(true, a);
    assert_eq!(
        codes_of(&got, "E0255"),
        at("E0255", line_of(a, "pub struct Size")),
        "got {got:?}"
    );
    let off = p.errors_of(false, a);
    assert!(codes_of(&off, "E0255").is_empty(), "got {off:?}");

    // Value namespace; the item comes first.
    let b = "pub struct S;\npub fn f() {}\npub mod m {\n    pub fn f() {}\n}\nuse m::f;\n";
    let got = p.errors_of(true, b);
    assert_eq!(
        codes_of(&got, "E0255"),
        at("E0255", line_of(b, "use m::f;")),
        "got {got:?}"
    );
    let off = p.errors_of(false, b);
    assert!(codes_of(&off, "E0255").is_empty(), "got {off:?}");

    // Inside an inline module; the `use` comes first.
    let c =
        "pub struct S;\npub struct X;\npub mod inner {\n    use super::X;\n    pub struct X;\n}\n";
    let got = p.errors_of(true, c);
    assert_eq!(
        codes_of(&got, "E0255"),
        at("E0255", line_of(c, "    pub struct X;")),
        "got {got:?}"
    );
    let off = p.errors_of(false, c);
    assert!(codes_of(&off, "E0255").is_empty(), "got {off:?}");
}

/// What rustc accepts is not reported: other namespaces, globs, `as _`, renames, the same name in
/// two modules, a `use` in a function body and a cfg-disabled item.
#[test]
fn duplicate_name_skips_what_rustc_accepts() {
    let text = r#"pub struct S;

pub mod m {
    pub fn scale() -> u32 {
        1
    }
}
use m::scale;
pub mod scale {}

pub mod m2 {
    pub struct Same;
}
use m2::*;
pub struct Same;

pub mod m3 {
    pub fn g() {}
}
use m3::g as _;
pub fn g() {}

pub mod m4 {
    pub fn h() {}
}
use m4::h as h2;
pub fn h() {}

pub struct Y;
pub mod other {
    pub struct Y;
}

pub mod m6 {
    pub struct W;
}
pub struct W;
pub fn body() {
    use m6::W;
    let _ = W;
}

pub mod m5 {
    pub fn z() {}
}
#[cfg(any())]
pub fn z() {}
use m5::z;
"#;
    let p = Probe::new("dup-negative");
    let got = p.errors_of(true, text);
    assert!(codes_of(&got, "E0255").is_empty(), "got {got:?}");
}

/// `Copy` on a type with a field that is not `Copy` is reported at the type head for a derive
/// (not at the attribute), at the `Copy` of a manual impl, and for an enum variant field.
#[test]
fn copy_impl_reports_non_copy_fields() {
    let text = r#"pub struct S;

#[derive(Clone, Copy)]
pub struct Note {
    pub text: String,
}

#[derive(Clone)]
pub struct Tags {
    pub list: Vec<u32>,
}
impl Copy for Tags {}

#[derive(Clone, Copy)]
pub enum Token {
    Number(u32),
    Word(String),
}
"#;
    let p = Probe::new("copy-positive");
    let got = p.errors_of(true, text);
    let mut want = vec![
        ("E0204".to_owned(), line_of(text, "pub struct Note")),
        ("E0204".to_owned(), line_of(text, "impl Copy for Tags")),
        ("E0204".to_owned(), line_of(text, "pub enum Token")),
    ];
    want.sort();
    assert_eq!(codes_of(&got, "E0204"), want, "got {got:?}");
    let off = p.errors_of(false, text);
    assert!(codes_of(&off, "E0204").is_empty(), "got {off:?}");
}

/// Types whose fields are all `Copy`, a generic type whose field is `Copy` under the derive's
/// `T: Copy`, lifetime-only types, fieldless enums, a `Clone` derive alone and a field of unknown
/// type are not reported.
#[test]
fn copy_impl_skips_copy_and_generic_types() {
    let text = r#"pub struct S;

#[derive(Clone, Copy)]
pub struct Inner {
    pub a: u8,
}

#[derive(Clone, Copy)]
pub struct AllCopy {
    pub n: u32,
    pub s: &'static str,
    pub arr: [u32; 2],
    pub inner: Inner,
}

#[derive(Clone, Copy)]
pub struct Pair2(pub (u8, char));

#[derive(Clone, Copy)]
pub struct Pair<T> {
    pub a: T,
}

#[derive(Clone, Copy)]
pub struct R<'a> {
    pub s: &'a str,
}

#[derive(Clone, Copy)]
pub enum Plain {
    A,
    B,
}

#[derive(Clone)]
pub struct Owned {
    pub text: String,
}

#[derive(Clone, Copy)]
pub struct Unknown {
    pub m: Missing,
}
"#;
    let p = Probe::new("copy-negative");
    let got = p.errors_of(true, text);
    assert!(codes_of(&got, "E0204").is_empty(), "got {got:?}");
}

/// A derive of Debug, Clone, Default or Eq over a field without the trait is reported as E0277 at
/// the field line, and a derive of PartialEq as E0369 at the field line.
#[test]
fn derive_field_reports_missing_field_traits() {
    let text = r#"pub struct Raw;

#[derive(PartialEq)]
pub struct PRaw;

#[derive(Debug)]
pub struct A {
    pub id: u32,
    pub raw_a: Raw,
}

#[derive(Clone)]
pub struct B {
    pub id: u32,
    pub raw_b: Raw,
}

#[derive(Default)]
pub struct C {
    pub id: u32,
    pub raw_c: Raw,
}

#[derive(PartialEq)]
pub struct D {
    pub id: u32,
    pub raw_d: Raw,
}

#[derive(PartialEq, Eq)]
pub struct E {
    pub raw_e: PRaw,
}
"#;
    let p = Probe::new("field-positive");
    let got = p.errors_of(true, text);
    let mut want77 = vec![
        ("E0277".to_owned(), line_of(text, "pub raw_a: Raw")),
        ("E0277".to_owned(), line_of(text, "pub raw_b: Raw")),
        ("E0277".to_owned(), line_of(text, "pub raw_c: Raw")),
        ("E0277".to_owned(), line_of(text, "pub raw_e: PRaw")),
    ];
    want77.sort();
    assert_eq!(codes_of(&got, "E0277"), want77, "got {got:?}");
    assert_eq!(
        codes_of(&got, "E0369"),
        at("E0369", line_of(text, "pub raw_d: Raw")),
        "got {got:?}"
    );
    let off = p.errors_of(false, text);
    assert!(
        codes_of(&off, "E0277").is_empty() && codes_of(&off, "E0369").is_empty(),
        "got {off:?}"
    );
}

/// Nothing is reported for a hand-written impl, a generic type, fields that have the traits, Clone
/// next to Copy (over a field without `Clone` only the E0204 is reported), a `#[default]` enum
/// variant with another variant that holds a field without the trait, a cfg-disabled type and a
/// field of unknown type; for `PartialEq, Eq` over a bare field only the E0369 is reported.
#[test]
fn derive_field_skips_what_needs_nothing() {
    let text = r#"pub struct Raw;

pub struct Opaque {
    pub raw_o: Raw,
}
impl std::fmt::Debug for Opaque {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Opaque")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct G<T> {
    pub t: T,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plain {
    pub n: u32,
    pub s: String,
    pub v: Vec<u8>,
}

#[derive(Clone, Copy)]
pub struct Num {
    pub n: u32,
}

#[derive(Default)]
pub enum Pick {
    #[default]
    A,
    B(Raw),
}

#[derive(PartialEq, Eq)]
pub struct Both {
    pub raw_both: Raw,
}

#[derive(Clone, Copy)]
pub struct CopyRaw {
    pub raw_copy: Raw,
}

#[cfg(any())]
#[derive(Debug, Clone, Copy)]
pub struct Gone {
    pub raw_gone: Raw,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Unknown {
    pub m: Missing,
}
"#;
    let p = Probe::new("field-negative");
    let got = p.errors_of(true, text);
    assert!(codes_of(&got, "E0277").is_empty(), "got {got:?}");
    assert_eq!(
        codes_of(&got, "E0204"),
        at("E0204", line_of(text, "pub struct CopyRaw")),
        "got {got:?}"
    );
    assert_eq!(
        codes_of(&got, "E0369"),
        at("E0369", line_of(text, "pub raw_both: Raw")),
        "got {got:?}"
    );
}

#[test]
fn each_detector_panic_is_contained_on_its_own() {
    use ryo_v2_core::app::ports::Verifier;
    use ryo_v2_core::domain::FindingCode;
    let lib = "pub struct S;\n\
pub fn g(s: S) {\n    s.missing();\n}\n\
pub fn f(_x: Missing) {}\n\
pub fn h() {}\n\
pub mod m {\n    pub fn h() {}\n}\n\
use m::h;\n\
#[derive(Clone, Copy)]\n\
pub struct Note {\n    pub text: String,\n}\n";
    let p = Probe::new("each-contained");
    std::fs::write(p.dir.join("src/lib.rs"), lib).unwrap();
    let lib_rel = RelPath::parse("src/lib.rs").unwrap();
    let analyzer = || {
        RaAnalyzer::new(
            RaConfig {
                proc_macro: false,
                threads: Some(1),
                assert_revert: true,
                gap_detectors: true,
                assert_revert_every: None,
                cfg_test: false,
            },
            Arc::new(MonotonicClock::new()),
            Arc::new(RecordingObserver::default()),
        )
    };
    let code_of = |d: GapDetector| match d {
        GapDetector::UnresolvedPath => "E0425",
        GapDetector::DuplicateName => "E0255",
        GapDetector::FieldBounds => "E0204",
    };
    let has = |base: &[ryo_v2_core::domain::Finding], code: &str| {
        base.iter()
            .any(|f| matches!(&f.code, FindingCode::Rustc(c) if c.as_str() == code))
    };

    let plain = analyzer();
    let s0 = plain.load(&p.dir).expect("load without fault");
    let base0 = Verifier::baseline(&plain, &s0);
    for code in ["E0425", "E0255", "E0204", "E0599"] {
        assert!(has(&base0, code), "{code} missing, got {base0:?}");
    }
    assert!(s0.detector_panics().is_empty());

    for d in GapDetector::ALL {
        let a = analyzer().with_detector_fault(Some((d, lib_rel.clone())));
        let mut s = a.load(&p.dir).expect("load survives a detector panic");
        let base = Verifier::baseline(&a, &s);
        for other in GapDetector::ALL {
            assert_eq!(
                has(&base, code_of(other)),
                other != d,
                "{d} fault, {other} code, got {base:?}"
            );
        }
        assert!(
            has(&base, "E0599"),
            "ra finding kept under {d}, got {base:?}"
        );
        let panics = s.detector_panics();
        assert_eq!(panics.len(), 1, "{d}: got {panics:?}");
        assert_eq!(panics[0].detector, d);
        assert_eq!(panics[0].path, lib_rel);
        assert!(
            panics[0].message.contains("fault injected"),
            "got {panics:?}"
        );
        let edit = Edit::write(lib_rel.clone(), format!("{lib}// edited\n"));
        a.check(&mut s, &[edit], Scope::Affected)
            .expect("check is not poisoned");
    }
}

/// A file with a macro call rust-analyzer could not expand (here an `include!` of `OUT_DIR`, which
/// the probe never sets) is skipped by the unresolved-path detector only: the duplicate-name and
/// field-bounds detectors still report on it.
#[test]
fn unexpanded_macro_file_keeps_the_other_detectors() {
    let p = Probe::new("unexpanded-others");
    std::fs::write(
        p.dir.join("build.rs"),
        "fn main() {\n    let out = std::env::var(\"OUT_DIR\").unwrap();\n    std::fs::write(std::path::Path::new(&out).join(\"generated.rs\"), \"pub struct Generated;\\n\").unwrap();\n}\n",
    )
    .unwrap();
    let text = "include!(concat!(env!(\"OUT_DIR\"), \"/generated.rs\"));\n\npub fn make(_g: Generated) {}\n\npub fn f() {}\npub mod m {\n    pub fn f() {}\n}\nuse m::f;\n\n#[derive(Clone, Copy)]\npub struct Note {\n    pub text: String,\n}\n";
    let got = p.errors_of(true, text);
    assert!(codes_of(&got, "E0425").is_empty(), "got {got:?}");
    assert_eq!(
        codes_of(&got, "E0255"),
        at("E0255", line_of(text, "use m::f;")),
        "got {got:?}"
    );
    assert_eq!(
        codes_of(&got, "E0204"),
        at("E0204", line_of(text, "pub struct Note")),
        "got {got:?}"
    );
}

/// A derive on a generic type is checked under the bounds the derived impl puts on the type's
/// parameters (D1): a field that lacks the trait whatever `T` is gets the same finding as on a
/// plain type, at the same position (E0277, E0369 for `PartialEq`, E0204 for `Copy`), for a struct
/// field, an enum variant field and a declared bound (D2).
#[test]
fn derive_field_generic_reports_missing_field_traits() {
    let text = r#"pub struct Raw;

#[derive(PartialEq)]
pub struct PRaw;

#[derive(Debug)]
pub struct GA<T> {
    pub t: T,
    pub raw_a: Raw,
}

#[derive(Clone)]
pub struct GB<T> {
    pub t: Vec<T>,
    pub raw_b: Raw,
}

#[derive(Default)]
pub struct GC<T> {
    pub t: Option<T>,
    pub raw_c: Raw,
}

#[derive(PartialEq)]
pub struct GD<T> {
    pub t: T,
    pub raw_d: Raw,
}

#[derive(PartialEq, Eq)]
pub struct GE<T> {
    pub t: T,
    pub raw_e: PRaw,
}

#[derive(Clone)]
pub enum GF<T> {
    Ok(T),
    Bad(Raw),
}

#[derive(Clone)]
pub struct GG<T: Clone> {
    pub t: T,
    pub raw_g: Raw,
}

#[derive(Clone)]
pub struct GH<T>
where
    T: Clone,
{
    pub t: T,
    pub raw_h: Raw,
}

#[derive(Clone, Copy)]
pub struct GI<T> {
    pub t: T,
    pub raw_i: Raw,
}

#[derive(Clone, Copy)]
pub struct GJ<const N: usize> {
    pub cells: [Raw; N],
}
"#;
    let p = Probe::new("field-generic-positive");
    let got = p.errors_of(true, text);
    let mut want77: Vec<(String, u32)> = [
        "pub raw_a: Raw",
        "pub raw_b: Raw",
        "pub raw_c: Raw",
        "pub raw_e: PRaw",
        "Bad(Raw)",
        "pub raw_g: Raw",
        "pub raw_h: Raw",
        "pub raw_i: Raw",
        "pub cells: [Raw; N]",
    ]
    .iter()
    .map(|n| ("E0277".to_owned(), line_of(text, n)))
    .collect();
    want77.sort();
    assert_eq!(codes_of(&got, "E0277"), want77, "got {got:?}");
    assert_eq!(
        codes_of(&got, "E0369"),
        at("E0369", line_of(text, "pub raw_d: Raw")),
        "got {got:?}"
    );
    assert_eq!(
        codes_of(&got, "E0204"),
        vec![
            ("E0204".to_owned(), line_of(text, "pub struct GI")),
            ("E0204".to_owned(), line_of(text, "pub struct GJ")),
        ],
        "got {got:?}"
    );
    let off = p.errors_of(false, text);
    assert!(
        ["E0277", "E0369", "E0204"]
            .iter()
            .all(|c| codes_of(&off, c).is_empty()),
        "got {off:?}"
    );
}

/// Fields that need only what the derived impl assumes of the parameters, or what the type
/// declares of them, give no finding on a generic type (D1, D2).
#[test]
fn derive_field_generic_accepts_what_the_derive_bounds_cover() {
    let text = r#"use std::marker::PhantomData;

pub trait Tr {}

pub struct Needs<T>(pub T);

impl<T: Tr + Clone> Clone for Needs<T> {
    fn clone(&self) -> Self {
        Needs(self.0.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cover<T> {
    pub a: T,
    pub v: Vec<T>,
    pub o: Option<T>,
    pub p: PhantomData<T>,
    pub b: Box<T>,
    pub n: u32,
}

#[derive(Clone, Copy)]
pub struct CoverCopy<T> {
    pub a: T,
    pub o: Option<T>,
    pub p: PhantomData<T>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CoverEnum<T> {
    One(T),
    Many(Vec<T>),
}

#[derive(Clone)]
pub struct Declared<T: Tr> {
    pub n: Needs<T>,
}

#[derive(Clone)]
pub struct DeclaredWhere<T>
where
    T: Tr,
{
    pub n: Needs<T>,
}

#[derive(Clone, Debug)]
pub struct Items<I: Iterator> {
    pub first: Option<I::Item>,
}
"#;
    let p = Probe::new("field-generic-negative");
    let got = p.errors_of(true, text);
    for c in ["E0277", "E0369", "E0204"] {
        assert!(codes_of(&got, c).is_empty(), "{c}: got {got:?}");
    }
}

/// Shapes the generic check cannot judge stay unreported (D3): a field that mentions a const
/// parameter of a type that also has type parameters, and a field of unknown type.
#[test]
fn derive_field_generic_skips_what_it_cannot_judge() {
    let text = r#"pub struct Raw;

#[derive(Clone)]
pub struct Mixed<T, const N: usize> {
    pub t: T,
    pub arr: [Raw; N],
}

#[derive(Clone)]
pub struct Unknown<T> {
    pub t: T,
    pub m: Missing,
}

#[derive(Clone)]
pub struct CloneOnly;

#[derive(Clone)]
pub struct CArr<const N: usize> {
    pub a: [CloneOnly; N],
}
impl<const N: usize> Copy for CArr<N> {}
"#;
    let p = Probe::new("field-generic-skip");
    let got = p.errors_of(true, text);
    assert!(codes_of(&got, "E0277").is_empty(), "got {got:?}");
    // A written `Copy` impl with a const parameter is not judged.
    assert!(codes_of(&got, "E0204").is_empty(), "got {got:?}");
}

/// A written `Copy` impl of a generic type is checked in the impl's own env (D5): a field that
/// lacks `Copy` whatever `T` is, is reported at the `Copy` of the impl; fields covered by the
/// impl's bounds, and a concrete instance of the type, are not.
#[test]
fn copy_impl_written_generic_uses_the_impl_env() {
    let text = r#"pub struct Raw;

#[derive(Clone)]
pub struct Bad<T> {
    pub t: T,
    pub raw: Raw,
}
impl<T: Copy> Copy for Bad<T> {}

#[derive(Clone)]
pub struct Good<T> {
    pub t: T,
    pub o: Option<T>,
}
impl<T: Copy> Copy for Good<T> {}

#[derive(Clone)]
pub struct Conc<T> {
    pub t: T,
}
impl Copy for Conc<u8> {}
impl Clone for Conc<u16> {
    fn clone(&self) -> Self {
        Conc { t: self.t }
    }
}
"#;
    let p = Probe::new("copy-written-generic");
    let got = p.errors_of(true, text);
    assert_eq!(
        codes_of(&got, "E0204"),
        at("E0204", line_of(text, "impl<T: Copy> Copy for Bad<T>")),
        "got {got:?}"
    );
}

/// A type with a lifetime and a type parameter is judged in the derived impl's env as well: the
/// lifetime argument does not make a field unknown, a borrowed `T` passes under `T: Trait`, and
/// a field that lacks the trait is reported.
#[test]
fn derive_field_generic_with_a_lifetime() {
    let text = r#"pub struct Raw;

#[derive(Debug, Clone, PartialEq)]
pub struct Fine<'a, T> {
    pub r: &'a T,
    pub v: Vec<T>,
}

#[derive(Debug)]
pub struct Lacks<'a, T> {
    pub r: &'a T,
    pub raw: Raw,
}
"#;
    let p = Probe::new("field-generic-lifetime");
    let got = p.errors_of(true, text);
    assert_eq!(
        codes_of(&got, "E0277"),
        at("E0277", line_of(text, "pub raw: Raw")),
        "got {got:?}"
    );
    assert!(codes_of(&got, "E0369").is_empty(), "got {got:?}");
}

/// `Eq` on a field of a generic type is skipped only when the field also fails `PartialEq` in the
/// derived `PartialEq` impl's env: `Wrap<T>` is `PartialEq` under `T: PartialEq` and never `Eq`,
/// so the derived `Eq` (under `T: Eq`) is reported at the field and the derived `PartialEq` is not.
#[test]
fn derive_field_generic_eq_beside_partialeq() {
    let text = r#"pub struct Wrap<T>(pub T);

impl<T: PartialEq> PartialEq for Wrap<T> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

#[derive(PartialEq, Eq)]
pub struct HasWrap<T> {
    pub w: Wrap<T>,
}
"#;
    let p = Probe::new("field-generic-eq");
    let got = p.errors_of(true, text);
    assert_eq!(
        codes_of(&got, "E0277"),
        at("E0277", line_of(text, "pub w: Wrap<T>")),
        "got {got:?}"
    );
    assert!(codes_of(&got, "E0369").is_empty(), "got {got:?}");
}
