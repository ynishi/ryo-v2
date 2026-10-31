//! The catalog of refactoring intents, and the check that a set of mutations carries one out.
//!
//! A Blueprint names an intent (`IntentCall`) and gives the mutations. Nothing here trusts the
//! pairing: [`IntentCall::conforms`] reads the mutations against the base text and refuses a
//! Blueprint whose edits are not what its intent says.

use super::{DomainError, NonEmpty, RelPath, SpecKind};
use std::collections::{BTreeMap, BTreeSet};

/// Strict and reserved keywords of edition 2024. They cannot be a bare identifier.
const KEYWORDS: [&str; 52] = [
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "gen", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut",
    "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while", "abstract", "become", "box", "do", "final", "macro",
    "override", "priv", "try", "typeof", "unsized", "virtual", "yield",
];

/// Words that stay unusable after `r#`.
const NOT_RAW: [&str; 5] = ["self", "Self", "super", "crate", "_"];

fn is_ident_shape(s: &str) -> bool {
    let mut cs = s.chars();
    matches!(cs.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_keyword(s: &str) -> bool {
    KEYWORDS.contains(&s)
}

/// A Rust identifier a refactoring may introduce.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ident(String);

impl Ident {
    /// Parses an identifier.
    ///
    /// # Contract
    /// - C1: accepts `[A-Za-z_][A-Za-z0-9_]*` except `_` alone and every strict or reserved
    ///   keyword of edition 2024; accepts `r#name` except `r#self r#Self r#super r#crate r#_`.
    ///   tested: domain::intent::tests::ident_rejects_keywords
    pub fn parse(s: &str) -> Result<Self, DomainError> {
        let bad = || DomainError::Ident(s.to_owned());
        if let Some(name) = s.strip_prefix("r#") {
            if !is_ident_shape(name) || NOT_RAW.contains(&name) {
                return Err(bad());
            }
        } else if !is_ident_shape(s) || s == "_" || is_keyword(s) {
            return Err(bad());
        }
        Ok(Self(s.to_owned()))
    }

    /// The identifier as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Ident {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A derive macro name: identifiers joined by `::`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeriveName(String);

impl DeriveName {
    /// Parses a derive name such as `Clone` or `serde::Serialize`.
    ///
    /// # Contract
    /// - C1: accepts one or more [`Ident`]s separated by `::`, refuses anything else.
    ///   tested: domain::intent::tests::add_derive_checks_list
    pub fn parse(s: &str) -> Result<Self, DomainError> {
        if !s.is_empty() && s.split("::").all(|p| Ident::parse(p).is_ok()) {
            Ok(Self(s.to_owned()))
        } else {
            Err(DomainError::DeriveName(s.to_owned()))
        }
    }

    /// The name as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for DeriveName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The closed set of intents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IntentKind {
    /// Rename a local binding and its uses.
    RenameLocal,
    /// Bind an expression to a new `let` and use the name.
    ExtractVariable,
    /// Add names to a `#[derive(..)]`.
    AddDerive,
}

/// What can go wrong with an intent that the verifier alone does not show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RiskMark {
    /// The edit can break borrow checking, which the analyzer does not run.
    Borrow,
    /// The file is a build script: only a real build shows its effect.
    BuildScript,
    /// The file is in a proc-macro crate: only a real build shows its effect.
    ProcMacroCrate,
    /// The edit can change what the program does.
    Behaviour,
}

impl std::fmt::Display for RiskMark {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RiskMark::Borrow => "borrow",
            RiskMark::BuildScript => "build-script",
            RiskMark::ProcMacroCrate => "proc-macro-crate",
            RiskMark::Behaviour => "behaviour",
        })
    }
}

/// An entry of the catalog: an intent and the risks it always carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntentSpec {
    /// The intent.
    pub kind: IntentKind,
    /// The risks every use of it carries.
    pub risks: BTreeSet<RiskMark>,
}

impl IntentKind {
    /// Every intent.
    pub const ALL: [IntentKind; 3] = [
        IntentKind::RenameLocal,
        IntentKind::ExtractVariable,
        IntentKind::AddDerive,
    ];

    /// The catalog entry.
    ///
    /// # Contract
    /// - C1: `RenameLocal` and `AddDerive` carry no risk; `ExtractVariable` carries `Borrow`
    ///   and `Behaviour`, so it can never be registered as a Rule.
    ///   tested: domain::intent::tests::extract_variable_carries_borrow_and_behaviour
    pub fn spec(self) -> IntentSpec {
        let risks = match self {
            IntentKind::RenameLocal | IntentKind::AddDerive => BTreeSet::new(),
            IntentKind::ExtractVariable => BTreeSet::from([RiskMark::Borrow, RiskMark::Behaviour]),
        };
        IntentSpec { kind: self, risks }
    }

    /// The stable name: `rename-local`, `extract-variable` or `add-derive`.
    pub fn id(self) -> &'static str {
        match self {
            IntentKind::RenameLocal => "rename-local",
            IntentKind::ExtractVariable => "extract-variable",
            IntentKind::AddDerive => "add-derive",
        }
    }

    /// The intent whose [`id`](IntentKind::id) is `s`.
    ///
    /// # Contract
    /// - C1: `parse(k.id()) == Some(k)` for every kind, and `None` for any other text.
    ///   tested: domain::intent::tests::intent_ids_round_trip
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.id() == s)
    }
}

impl std::fmt::Display for IntentKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.id())
    }
}

/// An intent with its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntentCall {
    /// Rename `from` to `to`.
    RenameLocal {
        /// The old name.
        from: Ident,
        /// The new name.
        to: Ident,
    },
    /// Bind to a new variable `name`.
    ExtractVariable {
        /// The new variable.
        name: Ident,
    },
    /// Add derives.
    AddDerive {
        /// The names to add, in the order they appear in the edits.
        derives: NonEmpty<DeriveName>,
    },
}

/// Why mutations do not carry out their intent.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Nonconformance {
    /// The specs touch more than one file (or none).
    #[error("the specs do not touch exactly one file")]
    ManyFiles,
    /// No base text was given for the file.
    #[error("no base text for {path}")]
    MissingBase {
        /// The file.
        path: RelPath,
    },
    /// Spec number `spec` has a kind this intent does not use.
    #[error("spec {spec} is a {kind}, which this intent does not use")]
    WrongKind {
        /// Index of the spec.
        spec: usize,
        /// Its kind.
        kind: SpecKind,
    },
    /// The base text under spec number `spec` is not what the intent names.
    #[error("spec {spec} does not touch the name the intent renames")]
    BaseTextMismatch {
        /// Index of the spec.
        spec: usize,
    },
    /// The new text of spec number `spec` is not what the intent says.
    #[error("spec {spec} writes text other than the intent's")]
    NewTextMismatch {
        /// Index of the spec.
        spec: usize,
    },
    /// No insert binds the new variable with `let`.
    #[error("no single insert binds the new variable with let")]
    NoLetInsert,
    /// Nothing replaces an expression with the new variable.
    #[error("no replace puts the new variable in place of an expression")]
    NoUseReplace,
    /// Spec number `spec` is not a derive attribute or a derive list tail.
    #[error("spec {spec} is not a derive")]
    NotADerive {
        /// Index of the spec.
        spec: usize,
    },
    /// The derives written differ from the intent's list.
    #[error("the derives written differ from the intent's list")]
    DeriveListMismatch,
}

fn base_slice<'a>(base: &'a str, spec: &super::MutationSpec) -> Option<&'a str> {
    base.get(spec.range().start() as usize..spec.range().end() as usize)
}

/// Whether `text` contains `let <name>` followed by a non-ident char or the end.
fn binds_with_let(text: &str, name: &str) -> bool {
    let pat = format!("let {name}");
    text.match_indices(&pat).any(|(i, _)| {
        text[i + pat.len()..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'))
    })
}

/// Parses `A, B::C` into names; `None` for an empty list or item.
fn derive_list(s: &str) -> Option<Vec<DeriveName>> {
    s.split(',')
        .map(|p| DeriveName::parse(p.trim()).ok())
        .collect()
}

/// The names a derive spec text adds: `#[derive(L)]` or `, L`.
fn derive_text(text: &str) -> Option<Vec<DeriveName>> {
    let t = text.trim();
    if let Some(inner) = t
        .strip_prefix("#[derive(")
        .and_then(|r| r.strip_suffix(")]"))
    {
        derive_list(inner)
    } else {
        derive_list(t.strip_prefix(',')?)
    }
}

impl IntentCall {
    /// The intent being called.
    pub fn kind(&self) -> IntentKind {
        match self {
            IntentCall::RenameLocal { .. } => IntentKind::RenameLocal,
            IntentCall::ExtractVariable { .. } => IntentKind::ExtractVariable,
            IntentCall::AddDerive { .. } => IntentKind::AddDerive,
        }
    }

    /// Checks that `specs` carry out this call against the `base` texts.
    ///
    /// # Contract
    /// - C1: all specs are on one path and `base` has a text for it, else `ManyFiles` or
    ///   `MissingBase`.
    ///   tested: domain::intent::tests::conforms_needs_one_file_and_its_base
    /// - C2: `RenameLocal`: every spec is a `Replace` of the text `from` by the text `to`.
    ///   tested: domain::intent::tests::rename_local_rejects_replace_of_other_text
    /// - C3: `ExtractVariable`: no `Delete`; exactly one `Insert`, containing `let <name>`
    ///   not followed by an identifier char; at least one `Replace` whose text is `name`.
    ///   tested: domain::intent::tests::extract_variable_needs_let_and_use
    /// - C4: `AddDerive`: every spec is an `Insert` of `#[derive(L)]` or `, L`, and the
    ///   concatenated lists equal `derives` in order.
    ///   tested: domain::intent::tests::add_derive_checks_list
    pub fn conforms(
        &self,
        specs: &[super::MutationSpec],
        base: &BTreeMap<RelPath, String>,
    ) -> Result<(), Nonconformance> {
        let Some(first) = specs.first() else {
            return Err(Nonconformance::ManyFiles);
        };
        if specs.iter().any(|s| s.path() != first.path()) {
            return Err(Nonconformance::ManyFiles);
        }
        let Some(text) = base.get(first.path()) else {
            return Err(Nonconformance::MissingBase {
                path: first.path().clone(),
            });
        };
        match self {
            IntentCall::RenameLocal { from, to } => {
                for (i, s) in specs.iter().enumerate() {
                    if s.kind() != SpecKind::Replace {
                        return Err(Nonconformance::WrongKind {
                            spec: i,
                            kind: s.kind(),
                        });
                    }
                    if base_slice(text, s) != Some(from.as_str()) {
                        return Err(Nonconformance::BaseTextMismatch { spec: i });
                    }
                    if s.text() != to.as_str() {
                        return Err(Nonconformance::NewTextMismatch { spec: i });
                    }
                }
                Ok(())
            }
            IntentCall::ExtractVariable { name } => {
                if let Some((i, s)) = specs
                    .iter()
                    .enumerate()
                    .find(|(_, s)| s.kind() == SpecKind::Delete)
                {
                    return Err(Nonconformance::WrongKind {
                        spec: i,
                        kind: s.kind(),
                    });
                }
                let mut inserts = specs.iter().filter(|s| s.kind() == SpecKind::Insert);
                match (inserts.next(), inserts.next()) {
                    (Some(ins), None) if binds_with_let(ins.text(), name.as_str()) => {}
                    _ => return Err(Nonconformance::NoLetInsert),
                }
                if !specs
                    .iter()
                    .any(|s| s.kind() == SpecKind::Replace && s.text() == name.as_str())
                {
                    return Err(Nonconformance::NoUseReplace);
                }
                Ok(())
            }
            IntentCall::AddDerive { derives } => {
                let mut written = Vec::new();
                for (i, s) in specs.iter().enumerate() {
                    if s.kind() != SpecKind::Insert {
                        return Err(Nonconformance::WrongKind {
                            spec: i,
                            kind: s.kind(),
                        });
                    }
                    let Some(names) = derive_text(s.text()) else {
                        return Err(Nonconformance::NotADerive { spec: i });
                    };
                    written.extend(names);
                }
                if written.as_slice() == derives.as_slice() {
                    Ok(())
                } else {
                    Err(Nonconformance::DeriveListMismatch)
                }
            }
        }
    }
}

impl std::fmt::Display for IntentCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IntentCall::RenameLocal { from, to } => write!(f, "rename-local {from} -> {to}"),
            IntentCall::ExtractVariable { name } => write!(f, "extract-variable {name}"),
            IntentCall::AddDerive { derives } => {
                let names: Vec<&str> = derives.iter().map(DeriveName::as_str).collect();
                write!(f, "add-derive {}", names.join(", "))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{MutationSpec, TextRange};

    fn rel() -> RelPath {
        RelPath::parse("a.rs").unwrap()
    }
    fn id(s: &str) -> Ident {
        Ident::parse(s).unwrap()
    }
    fn rep(a: u32, b: u32, t: &str) -> MutationSpec {
        MutationSpec::replace(rel(), TextRange::new(a, b).unwrap(), t).unwrap()
    }
    fn ins(at: u32, t: &str) -> MutationSpec {
        MutationSpec::insert(rel(), at, t).unwrap()
    }
    fn base(text: &str) -> BTreeMap<RelPath, String> {
        BTreeMap::from([(rel(), text.to_owned())])
    }
    fn derives(names: &[&str]) -> NonEmpty<DeriveName> {
        NonEmpty::new(
            names
                .iter()
                .map(|n| DeriveName::parse(n).unwrap())
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn ident_rejects_keywords() {
        for ok in [
            "x", "_x", "_1", "Foo9", "r#type", "r#match", "r#x", "async_",
        ] {
            assert!(Ident::parse(ok).is_ok(), "{ok}");
        }
        for kw in KEYWORDS.iter() {
            assert!(Ident::parse(kw).is_err(), "{kw}");
        }
        for bad in [
            "", "_", "1a", "a-b", "a b", "é", "r#", "r#1", "r#self", "r#Self", "r#super",
            "r#crate", "r#_", "r#r#a",
        ] {
            assert!(Ident::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(KEYWORDS.len(), 52);
    }

    #[test]
    fn intent_ids_round_trip() {
        for k in IntentKind::ALL {
            assert_eq!(IntentKind::parse(k.id()), Some(k));
            assert_eq!(k.spec().kind, k);
        }
        assert_eq!(IntentKind::parse("rename"), None);
        assert_eq!(IntentKind::AddDerive.to_string(), "add-derive");
    }

    #[test]
    fn extract_variable_carries_borrow_and_behaviour() {
        assert!(IntentKind::RenameLocal.spec().risks.is_empty());
        assert!(IntentKind::AddDerive.spec().risks.is_empty());
        assert_eq!(
            IntentKind::ExtractVariable.spec().risks,
            BTreeSet::from([RiskMark::Borrow, RiskMark::Behaviour])
        );
    }

    #[test]
    fn conforms_needs_one_file_and_its_base() {
        let call = IntentCall::RenameLocal {
            from: id("a"),
            to: id("b"),
        };
        let other = MutationSpec::replace(
            RelPath::parse("b.rs").unwrap(),
            TextRange::new(0, 1).unwrap(),
            "b",
        )
        .unwrap();
        assert_eq!(
            call.conforms(&[rep(0, 1, "b"), other], &base("a")),
            Err(Nonconformance::ManyFiles)
        );
        assert_eq!(
            call.conforms(&[rep(0, 1, "b")], &BTreeMap::new()),
            Err(Nonconformance::MissingBase { path: rel() })
        );
        assert_eq!(
            call.conforms(&[], &base("a")),
            Err(Nonconformance::ManyFiles)
        );
        assert_eq!(call.kind(), IntentKind::RenameLocal);
        assert_eq!(call.to_string(), "rename-local a -> b");
    }

    #[test]
    fn rename_local_rejects_replace_of_other_text() {
        let call = IntentCall::RenameLocal {
            from: id("a"),
            to: id("bb"),
        };
        let text = "let a = a + c;";
        assert_eq!(
            call.conforms(&[rep(4, 5, "bb"), rep(8, 9, "bb")], &base(text)),
            Ok(())
        );
        assert_eq!(
            call.conforms(&[rep(4, 5, "bb"), rep(12, 13, "bb")], &base(text)),
            Err(Nonconformance::BaseTextMismatch { spec: 1 })
        );
        assert_eq!(
            call.conforms(&[rep(4, 5, "cc")], &base(text)),
            Err(Nonconformance::NewTextMismatch { spec: 0 })
        );
        assert_eq!(
            call.conforms(&[ins(4, "bb")], &base(text)),
            Err(Nonconformance::WrongKind {
                spec: 0,
                kind: SpecKind::Insert
            })
        );
        // out of range or off a char boundary is a mismatch, not a panic
        assert_eq!(
            call.conforms(&[rep(40, 41, "bb")], &base(text)),
            Err(Nonconformance::BaseTextMismatch { spec: 0 })
        );
    }

    #[test]
    fn extract_variable_needs_let_and_use() {
        let call = IntentCall::ExtractVariable { name: id("t") };
        let text = "f(1 + 2);";
        let ok = [ins(0, "let t = 1 + 2;\n"), rep(2, 7, "t")];
        assert_eq!(call.conforms(&ok, &base(text)), Ok(()));
        assert_eq!(
            call.conforms(&[ins(0, "let tt = 1;"), rep(2, 7, "t")], &base(text)),
            Err(Nonconformance::NoLetInsert)
        );
        assert_eq!(
            call.conforms(
                &[ins(0, "let t;"), ins(1, "x"), rep(2, 7, "t")],
                &base(text)
            ),
            Err(Nonconformance::NoLetInsert)
        );
        assert_eq!(
            call.conforms(&[ins(0, "let t")], &base(text)),
            Err(Nonconformance::NoUseReplace)
        );
        let del = MutationSpec::delete(rel(), TextRange::new(0, 1).unwrap()).unwrap();
        assert_eq!(
            call.conforms(&[ok[0].clone(), del], &base(text)),
            Err(Nonconformance::WrongKind {
                spec: 1,
                kind: SpecKind::Delete
            })
        );
    }

    #[test]
    fn add_derive_checks_list() {
        assert!(DeriveName::parse("serde::Serialize").is_ok());
        for bad in ["", "::A", "A::", "A:B", "a b", "fn", "A,B"] {
            assert!(DeriveName::parse(bad).is_err(), "{bad}");
        }
        let call = IntentCall::AddDerive {
            derives: derives(&["Clone", "Debug", "serde::Serialize"]),
        };
        let text = "struct S;";
        assert_eq!(
            call.conforms(
                &[
                    ins(0, "#[derive(Clone, Debug)]\n"),
                    ins(5, " , serde::Serialize")
                ],
                &base(text)
            ),
            Ok(())
        );
        assert_eq!(
            call.conforms(&[ins(0, "#[derive(Clone)]")], &base(text)),
            Err(Nonconformance::DeriveListMismatch)
        );
        assert_eq!(
            call.conforms(
                &[ins(0, "#[derive(Debug, Clone, serde::Serialize)]")],
                &base(text)
            ),
            Err(Nonconformance::DeriveListMismatch)
        );
        for bad in [
            "#[derive()]",
            "#[derive(A,)]",
            "#[inline]",
            ", ",
            "Clone",
            ",A B",
        ] {
            assert_eq!(
                call.conforms(&[ins(0, bad)], &base(text)),
                Err(Nonconformance::NotADerive { spec: 0 }),
                "{bad}"
            );
        }
        assert_eq!(
            call.conforms(&[rep(0, 1, "#[derive(Clone)]")], &base(text)),
            Err(Nonconformance::WrongKind {
                spec: 0,
                kind: SpecKind::Replace
            })
        );
    }
}
