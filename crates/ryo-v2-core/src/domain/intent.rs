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
    /// Move statements into a new function and call it.
    ExtractFunction,
    /// Move an item to another file and leave a re-export where it was.
    MoveItem,
}

/// What can go wrong with an intent that the verifier alone does not show.
///
/// Why each intent carries the marks it does. Extract-function moves statements into a new body,
/// so parameters become borrows or moves (the analyzer runs no borrow check), and temporaries
/// and guards are dropped at the new function's end instead of the old scope's: drop order is
/// behaviour, the same reason extract-variable carries `Behaviour`. Move-item changes where an
/// item lives and who can see it: the analyzer misses cross-crate privacy (E0603) and cfg-gated
/// paths in another crate, so its candidate names `Visibility` as an unseen class, which the
/// build after the commit settles. It does not reorder evaluation or drops and does not change
/// borrows, so it carries no `Borrow` and no `Behaviour`.
///
/// Known limit of move-item: a glob import (`use a::*`) that reached the item through its old
/// file is not followed, so a name can stop resolving there; the build after the commit shows
/// it, the conformance check does not.
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
    /// The edit changes where an item lives and who can see it: only a real build shows a
    /// privacy or cfg error across crates.
    Visibility,
}

impl std::fmt::Display for RiskMark {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.unseen_class() {
            Some(class) => class.fmt(f),
            None => f.write_str("behaviour"),
        }
    }
}

impl RiskMark {
    /// The class of errors the in-process check cannot see that this mark names, or `None`.
    ///
    /// # Contract
    /// - C1: `Borrow`, `BuildScript`, `ProcMacroCrate` and `Visibility` give the class of the
    ///   same name; `Behaviour` gives `None`, as it is not a kind of error.
    ///   tested: domain::intent::tests::risk_mark_unseen_class_maps_each_mark
    pub fn unseen_class(self) -> Option<UnseenClass> {
        match self {
            RiskMark::Borrow => Some(UnseenClass::Borrow),
            RiskMark::BuildScript => Some(UnseenClass::BuildScript),
            RiskMark::ProcMacroCrate => Some(UnseenClass::ProcMacroCrate),
            RiskMark::Visibility => Some(UnseenClass::Visibility),
            RiskMark::Behaviour => None,
        }
    }
}

/// A kind of error the in-process check does not see. A candidate names it in its result, and
/// the build after the commit settles it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UnseenClass {
    /// Borrow checking is not run by the analyzer.
    Borrow,
    /// A build script changed.
    BuildScript,
    /// A proc-macro crate changed.
    ProcMacroCrate,
    /// An item moved: privacy and cfg-gated paths across crates are not judged by the analyzer.
    Visibility,
}

impl std::fmt::Display for UnseenClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            UnseenClass::Borrow => "borrow",
            UnseenClass::BuildScript => "build-script",
            UnseenClass::ProcMacroCrate => "proc-macro-crate",
            UnseenClass::Visibility => "visibility",
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
    pub const ALL: [IntentKind; 5] = [
        IntentKind::RenameLocal,
        IntentKind::ExtractVariable,
        IntentKind::AddDerive,
        IntentKind::ExtractFunction,
        IntentKind::MoveItem,
    ];

    /// The catalog entry.
    ///
    /// # Contract
    /// - C1: `RenameLocal` and `AddDerive` carry no risk; `ExtractVariable` carries `Borrow`
    ///   and `Behaviour`.
    ///   tested: domain::intent::tests::extract_variable_carries_borrow_and_behaviour
    /// - C2: `ExtractFunction` carries `Borrow` and `Behaviour` (parameters become borrows and
    ///   drop order changes); `MoveItem` carries `Visibility` alone, so its candidate names
    ///   `Visibility` as an unseen class.
    ///   tested: domain::intent::tests::new_intents_carry_their_risks
    pub fn spec(self) -> IntentSpec {
        let risks = match self {
            IntentKind::RenameLocal | IntentKind::AddDerive => BTreeSet::new(),
            IntentKind::ExtractVariable | IntentKind::ExtractFunction => {
                BTreeSet::from([RiskMark::Borrow, RiskMark::Behaviour])
            }
            IntentKind::MoveItem => BTreeSet::from([RiskMark::Visibility]),
        };
        IntentSpec { kind: self, risks }
    }

    /// The stable name: `rename-local`, `extract-variable`, `add-derive`, `extract-function` or
    /// `move-item`.
    pub fn id(self) -> &'static str {
        match self {
            IntentKind::RenameLocal => "rename-local",
            IntentKind::ExtractVariable => "extract-variable",
            IntentKind::AddDerive => "add-derive",
            IntentKind::ExtractFunction => "extract-function",
            IntentKind::MoveItem => "move-item",
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
    /// Move statements into a new function `name` and call it.
    ExtractFunction {
        /// The new function.
        name: Ident,
    },
    /// Move the item `item` to the file `to`.
    MoveItem {
        /// The item moved.
        item: Ident,
        /// The file it moves to.
        to: RelPath,
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
    /// No single insert declares the new function.
    #[error("no single insert declares the new function")]
    NoFnInsert,
    /// No replace puts a call of the new function in place of the statements.
    #[error("no replace puts a call of the new function in place of the statements")]
    NoCallReplace,
    /// A name used by the replaced statements is in neither the new function nor the call.
    #[error("the name {ident} is lost: neither the new function nor the call has it")]
    LostIdent {
        /// The name.
        ident: String,
    },
    /// The specs of a move do not touch exactly the source file and the destination file.
    #[error("the specs do not touch exactly the file moved from and the file moved to")]
    MoveFiles,
    /// The base text under spec number `spec` does not declare the item moved.
    #[error("spec {spec} does not declare the item moved")]
    NotAnItem {
        /// Index of the spec.
        spec: usize,
    },
    /// The new text of spec number `spec` is not a re-export of the item moved.
    #[error("spec {spec} does not re-export the item moved")]
    NoReexport {
        /// Index of the spec.
        spec: usize,
    },
    /// The destination does not receive the text of the item.
    #[error("the destination does not receive the item's text")]
    NotMoved,
    /// Spec number `spec` replaces text by the same text.
    #[error("spec {spec} writes the bytes it replaces: it changes nothing")]
    NoOp {
        /// Index of the spec.
        spec: usize,
    },
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// The identifier tokens of `text` with their byte offsets: maximal runs of `[A-Za-z0-9_]` that
/// do not start with a digit.
fn ident_tokens(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in text
        .char_indices()
        .chain(std::iter::once((text.len(), ' ')))
    {
        match (start, is_ident_char(c)) {
            (None, true) => start = Some(i),
            (Some(s), false) => {
                let run = &text[s..i];
                if !run.starts_with(|d: char| d.is_ascii_digit()) {
                    out.push((s, run));
                }
                start = None;
            }
            _ => {}
        }
    }
    out
}

/// Whether `text` has `fn`, whitespace, `name`, optional whitespace and `(` or `<`.
fn declares_fn(text: &str, name: &str) -> bool {
    ident_tokens(text).into_iter().any(|(at, tok)| {
        if tok != "fn" {
            return false;
        }
        let rest = &text[at + 2..];
        let after_ws = rest.trim_start();
        after_ws.len() < rest.len()
            && after_ws.strip_prefix(name).is_some_and(|r| {
                let r = r.trim_start();
                r.starts_with('(') || r.starts_with('<')
            })
    })
}

/// Whether `text` has `name(` not preceded by an identifier char.
fn calls_fn(text: &str, name: &str) -> bool {
    let pat = format!("{name}(");
    text.match_indices(&pat)
        .any(|(i, _)| !text[..i].chars().next_back().is_some_and(is_ident_char))
}

/// The first identifier token of `base_text` that is not a keyword and is in no token of
/// `texts`.
fn first_lost_ident(base_text: &str, texts: [&str; 2]) -> Option<String> {
    let known: BTreeSet<&str> = texts
        .iter()
        .flat_map(|t| ident_tokens(t))
        .map(|(_, tok)| tok)
        .collect();
    ident_tokens(base_text)
        .into_iter()
        .map(|(_, tok)| tok)
        .find(|tok| !is_keyword(tok) && !known.contains(tok))
        .map(str::to_owned)
}

const ITEM_KEYWORDS: [&str; 8] = [
    "fn", "struct", "enum", "trait", "type", "const", "static", "union",
];

/// Whether `text` declares `item`: `<kw> <item>` with the name followed by a non-identifier char.
fn declares_item(text: &str, item: &str) -> bool {
    ident_tokens(text).into_iter().any(|(at, tok)| {
        if !ITEM_KEYWORDS.contains(&tok) {
            return false;
        }
        let rest = &text[at + tok.len()..];
        let after_ws = rest.trim_start();
        after_ws.len() < rest.len()
            && after_ws
                .strip_prefix(item)
                .is_some_and(|r| !r.starts_with(is_ident_char))
    })
}

/// Strips a leading `pub`, `pub(crate)` or `pub(super)` that is followed by whitespace.
fn strip_vis(t: &str) -> &str {
    for v in ["pub(crate)", "pub(super)", "pub"] {
        if let Some(r) = t.strip_prefix(v) {
            let rest = r.trim_start();
            if rest.len() < r.len() {
                return rest;
            }
        }
    }
    t
}

/// Whether the trimmed `text` is `[<vis> ]use <path>::<item>;`.
fn is_use_of(text: &str, item: &str) -> bool {
    let Some(rest) = strip_vis(text.trim()).strip_prefix("use") else {
        return false;
    };
    let path = rest.trim_start();
    if path.len() == rest.len() {
        return false;
    }
    let Some(path) = path.trim_end().strip_suffix(';') else {
        return false;
    };
    let segs: Vec<&str> = path.trim().split("::").collect();
    segs.len() >= 2 && segs.iter().all(|s| is_ident_shape(s)) && segs.last() == Some(&item)
}

/// `text` with visibility tokens removed and whitespace collapsed to single spaces.
fn without_vis(text: &str) -> String {
    let stripped = text
        .replace("pub(crate)", "")
        .replace("pub(super)", "")
        .replace("pub", "");
    stripped.split_whitespace().collect::<Vec<_>>().join(" ")
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
            IntentCall::ExtractFunction { .. } => IntentKind::ExtractFunction,
            IntentCall::MoveItem { .. } => IntentKind::MoveItem,
        }
    }

    /// Checks that `specs` carry out this call against the `base` texts.
    ///
    /// Known limit of rename-local and add-derive: they are checked by matching text only, so a
    /// Blueprint can pass with a rename inside a string literal, a name capture, one side of a
    /// shadowing pair or a struct-field rename.
    ///
    /// # Contract
    /// - C7: checked before every other clause and for every intent, `MoveItem` included: the
    ///   first spec that is a `Replace` writing the bytes of the base text it replaces (when
    ///   `base` has its path and the range lies inside it) gives `NoOp`; inserts and deletes
    ///   are never no-ops.
    ///   tested: domain::intent::tests::no_op_replace_is_refused
    /// - C1: for every intent except `MoveItem`, all specs are on one path and `base` has a
    ///   text for it, else `ManyFiles` or `MissingBase`.
    ///   tested: domain::intent::tests::conforms_needs_one_file_and_its_base
    /// - C2: `RenameLocal`: every spec is a `Replace` of the text `from` by the text `to`.
    ///   tested: domain::intent::tests::rename_local_rejects_replace_of_other_text
    /// - C3: `ExtractVariable`: no `Delete`; exactly one `Insert`, containing `let <name>`
    ///   not followed by an identifier char; at least one `Replace` whose text is `name`.
    ///   tested: domain::intent::tests::extract_variable_needs_let_and_use
    /// - C4: `AddDerive`: every spec is an `Insert` of `#[derive(L)]` or `, L`, and the
    ///   concatenated lists equal `derives` in order.
    ///   tested: domain::intent::tests::add_derive_checks_list
    /// - C5: `ExtractFunction`: no `Delete` (`WrongKind`); exactly one `Insert` whose text has
    ///   `fn`, whitespace, the name, optional whitespace and `(` or `<` (`NoFnInsert`); exactly
    ///   one `Replace` whose new text contains `name(` not preceded by an identifier char
    ///   (`NoCallReplace`); every identifier token of that `Replace`'s base text that is not a
    ///   keyword occurs among the tokens of the `Insert` text or of the `Replace` text, the
    ///   first missing one giving `LostIdent`.
    ///   tested: domain::intent::tests::extract_function_needs_fn_insert_and_call
    ///   tested: domain::intent::tests::extract_function_rejects_lost_identifiers
    /// - C6: `MoveItem`: the specs touch exactly two paths, one of them `to`, both with a base
    ///   text (`MoveFiles`, `MissingBase`). The other path, the source, has exactly one spec, a
    ///   `Replace` or a `Delete` (`WrongKind`, else `NotMoved`), whose base text declares the
    ///   item as `<kw> <item>` with `kw` one of `fn struct enum trait type const static union`
    ///   (`NotAnItem`); a `Replace` there writes `[<vis> ]use <path>::<item>;` with `vis` one of
    ///   `pub`, `pub(crate)` or `pub(super)` (`NoReexport`). The destination `to` has exactly one
    ///   spec, an `Insert`, or a `Replace` of such a `use` line (`WrongKind`, `NotMoved`,
    ///   `BaseTextMismatch`), and its new text, with whitespace collapsed and every visibility
    ///   token removed, contains the source's base text treated the same way (`NotMoved`).
    ///   tested: domain::intent::tests::move_item_needs_two_files_and_the_item
    ///   tested: domain::intent::tests::move_item_accepts_reexport_and_collapse
    pub fn conforms(
        &self,
        specs: &[super::MutationSpec],
        base: &BTreeMap<RelPath, String>,
    ) -> Result<(), Nonconformance> {
        if let Some(spec) = specs.iter().position(|s| {
            s.kind() == SpecKind::Replace
                && base
                    .get(s.path())
                    .and_then(|text| base_slice(text, s))
                    .is_some_and(|old| old == s.text())
        }) {
            return Err(Nonconformance::NoOp { spec });
        }
        if let IntentCall::MoveItem { item, to } = self {
            return conforms_move(item, to, specs, base);
        }
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
            IntentCall::ExtractFunction { name } => {
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
                let ins = match (inserts.next(), inserts.next()) {
                    (Some(ins), None) if declares_fn(ins.text(), name.as_str()) => ins,
                    _ => return Err(Nonconformance::NoFnInsert),
                };
                let mut replaces = specs
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.kind() == SpecKind::Replace);
                let (ri, rep) = match (replaces.next(), replaces.next()) {
                    (Some((i, r)), None) if calls_fn(r.text(), name.as_str()) => (i, r),
                    _ => return Err(Nonconformance::NoCallReplace),
                };
                let Some(replaced) = base_slice(text, rep) else {
                    return Err(Nonconformance::BaseTextMismatch { spec: ri });
                };
                match first_lost_ident(replaced, [ins.text(), rep.text()]) {
                    Some(ident) => Err(Nonconformance::LostIdent { ident }),
                    None => Ok(()),
                }
            }
            IntentCall::MoveItem { item, to } => conforms_move(item, to, specs, base),
        }
    }
}

/// The checks of `MoveItem`, which spans two files.
fn conforms_move(
    item: &Ident,
    to: &RelPath,
    specs: &[super::MutationSpec],
    base: &BTreeMap<RelPath, String>,
) -> Result<(), Nonconformance> {
    let paths: BTreeSet<&RelPath> = specs.iter().map(|s| s.path()).collect();
    if paths.len() != 2 || !paths.contains(to) {
        return Err(Nonconformance::MoveFiles);
    }
    for p in &paths {
        if !base.contains_key(*p) {
            return Err(Nonconformance::MissingBase { path: (*p).clone() });
        }
    }
    let src = paths
        .iter()
        .find(|p| **p != to)
        .copied()
        .ok_or(Nonconformance::MoveFiles)?;
    let on = |p: &RelPath| -> Vec<(usize, &super::MutationSpec)> {
        specs
            .iter()
            .enumerate()
            .filter(|(_, s)| s.path() == p)
            .collect()
    };
    let item = item.as_str();
    let [(si, s)] = on(src)[..] else {
        return Err(Nonconformance::NotMoved);
    };
    if s.kind() == SpecKind::Insert {
        return Err(Nonconformance::WrongKind {
            spec: si,
            kind: s.kind(),
        });
    }
    let Some(declared) = base_slice(&base[src], s).filter(|d| declares_item(d, item)) else {
        return Err(Nonconformance::NotAnItem { spec: si });
    };
    if s.kind() == SpecKind::Replace && !is_use_of(s.text(), item) {
        return Err(Nonconformance::NoReexport { spec: si });
    }
    let [(di, d)] = on(to)[..] else {
        return Err(Nonconformance::NotMoved);
    };
    match d.kind() {
        SpecKind::Delete => {
            return Err(Nonconformance::WrongKind {
                spec: di,
                kind: d.kind(),
            });
        }
        SpecKind::Replace => {
            if !base_slice(&base[to], d).is_some_and(|b| is_use_of(b, item)) {
                return Err(Nonconformance::BaseTextMismatch { spec: di });
            }
        }
        SpecKind::Insert => {}
    }
    if without_vis(d.text()).contains(&without_vis(declared)) {
        Ok(())
    } else {
        Err(Nonconformance::NotMoved)
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
            IntentCall::ExtractFunction { name } => write!(f, "extract-function {name}"),
            IntentCall::MoveItem { item, to } => write!(f, "move-item {item} -> {to}"),
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
        assert_eq!(IntentKind::ALL.len(), 5);
        assert_eq!(
            IntentKind::parse("extract-function"),
            Some(IntentKind::ExtractFunction)
        );
        assert_eq!(IntentKind::parse("move-item"), Some(IntentKind::MoveItem));
        assert_eq!(IntentKind::MoveItem.to_string(), "move-item");
        assert_eq!(RiskMark::Visibility.to_string(), "visibility");
    }

    #[test]
    fn new_intents_carry_their_risks() {
        assert_eq!(
            IntentKind::ExtractFunction.spec().risks,
            BTreeSet::from([RiskMark::Borrow, RiskMark::Behaviour])
        );
        assert_eq!(
            IntentKind::MoveItem.spec().risks,
            BTreeSet::from([RiskMark::Visibility])
        );
        let ef = IntentCall::ExtractFunction { name: id("h") };
        let mv = IntentCall::MoveItem {
            item: id("foo"),
            to: RelPath::parse("b.rs").unwrap(),
        };
        assert_eq!(ef.kind(), IntentKind::ExtractFunction);
        assert_eq!(mv.kind(), IntentKind::MoveItem);
        assert_eq!(ef.to_string(), "extract-function h");
        assert_eq!(mv.to_string(), "move-item foo -> b.rs");
    }

    #[test]
    fn risk_mark_unseen_class_maps_each_mark() {
        assert_eq!(RiskMark::Borrow.unseen_class(), Some(UnseenClass::Borrow));
        assert_eq!(
            RiskMark::BuildScript.unseen_class(),
            Some(UnseenClass::BuildScript)
        );
        assert_eq!(
            RiskMark::ProcMacroCrate.unseen_class(),
            Some(UnseenClass::ProcMacroCrate)
        );
        assert_eq!(
            RiskMark::Visibility.unseen_class(),
            Some(UnseenClass::Visibility)
        );
        assert_eq!(RiskMark::Behaviour.unseen_class(), None);
        assert_eq!(UnseenClass::Borrow.to_string(), "borrow");
        assert_eq!(UnseenClass::BuildScript.to_string(), "build-script");
        assert_eq!(UnseenClass::ProcMacroCrate.to_string(), "proc-macro-crate");
        assert_eq!(UnseenClass::Visibility.to_string(), "visibility");
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
    fn no_op_replace_is_refused() {
        let same = IntentCall::RenameLocal {
            from: id("a"),
            to: id("a"),
        };
        assert_eq!(
            same.conforms(&[rep(4, 5, "a")], &base("let a = 1;")),
            Err(Nonconformance::NoOp { spec: 0 })
        );
        // the lowest index wins, before every other check
        let call = IntentCall::RenameLocal {
            from: id("a"),
            to: id("bb"),
        };
        assert_eq!(
            call.conforms(&[rep(4, 5, "bb"), rep(8, 9, "a")], &base("let a = a + c;")),
            Err(Nonconformance::NoOp { spec: 1 })
        );
        // an insert or a delete is never a no-op; a range outside the base is not judged here
        assert_ne!(
            call.conforms(&[ins(0, "z")], &base("a")),
            Err(Nonconformance::NoOp { spec: 0 })
        );
        assert_ne!(
            call.conforms(&[rep(40, 41, "a")], &base("a")),
            Err(Nonconformance::NoOp { spec: 0 })
        );
        // a no-op is found before the file count and the missing base are
        let other = MutationSpec::replace(
            RelPath::parse("b.rs").unwrap(),
            TextRange::new(0, 1).unwrap(),
            "z",
        )
        .unwrap();
        assert_eq!(
            same.conforms(&[rep(4, 5, "a"), other.clone()], &base("let a = 1;")),
            Err(Nonconformance::NoOp { spec: 0 })
        );
        assert_eq!(
            same.conforms(&[other, rep(4, 5, "a")], &base("let a = 1;")),
            Err(Nonconformance::NoOp { spec: 1 })
        );
        // a move whose source Replace writes back its own text
        let b = two("pub fn foo() {}\n", "");
        let back = [
            rep_at("a.rs", 0, 15, "pub fn foo() {}"),
            ins_at("b.rs", 0, "pub fn foo() {}\n"),
        ];
        assert_eq!(
            mv("foo", "b.rs").conforms(&back, &b),
            Err(Nonconformance::NoOp { spec: 0 })
        );
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

    #[test]
    fn extract_function_needs_fn_insert_and_call() {
        let call = IntentCall::ExtractFunction { name: id("h") };
        let text = "fn f(x: i32) { g(x + 1); }";
        let decl = "fn h(x: i32) { g(x + 1); }\n";
        assert_eq!(
            call.conforms(&[ins(0, decl), rep(15, 24, "h(x);")], &base(text)),
            Ok(())
        );
        // a space before the parameters, generics, and a method-style call are all fine
        assert_eq!(
            call.conforms(
                &[ins(0, "pub fn  h <T>() { g(x); }"), rep(15, 24, "s.h(x);")],
                &base(text)
            ),
            Ok(())
        );
        let del = MutationSpec::delete(rel(), TextRange::new(0, 2).unwrap()).unwrap();
        assert_eq!(
            call.conforms(&[ins(0, decl), rep(15, 24, "h(x);"), del], &base(text)),
            Err(Nonconformance::WrongKind {
                spec: 2,
                kind: SpecKind::Delete
            })
        );
        for bad in [
            "const h: i32 = 1;",
            "fn hh(x: i32) { g(x); }",
            "xfn h() { g(x); }",
        ] {
            assert_eq!(
                call.conforms(&[ins(0, bad), rep(15, 24, "h(x);")], &base(text)),
                Err(Nonconformance::NoFnInsert),
                "{bad}"
            );
        }
        assert_eq!(
            call.conforms(
                &[ins(0, decl), ins(1, decl), rep(15, 24, "h(x);")],
                &base(text)
            ),
            Err(Nonconformance::NoFnInsert)
        );
        assert_eq!(
            call.conforms(&[rep(15, 24, "h(x);")], &base(text)),
            Err(Nonconformance::NoFnInsert)
        );
        for bad in ["hh(x);", "xh(x);", "h;"] {
            assert_eq!(
                call.conforms(&[ins(0, decl), rep(15, 24, bad)], &base(text)),
                Err(Nonconformance::NoCallReplace),
                "{bad}"
            );
        }
        assert_eq!(
            call.conforms(&[ins(0, decl)], &base(text)),
            Err(Nonconformance::NoCallReplace)
        );
        assert_eq!(
            call.conforms(
                &[ins(0, decl), rep(15, 16, "h(x);"), rep(17, 24, "h(x);")],
                &base(text)
            ),
            Err(Nonconformance::NoCallReplace)
        );
        // a replace outside the base text is a mismatch, not a panic
        assert_eq!(
            call.conforms(&[ins(0, decl), rep(60, 70, "h(x);")], &base(text)),
            Err(Nonconformance::BaseTextMismatch { spec: 1 })
        );
    }

    #[test]
    fn extract_function_rejects_lost_identifiers() {
        let call = IntentCall::ExtractFunction { name: id("h") };
        let text = "fn f(x: i32) { g(x + 1); }";
        // `g` is neither in the new function nor in the call
        assert_eq!(
            call.conforms(
                &[ins(0, "fn h(x: i32) { x + 1; }\n"), rep(15, 24, "h(x);")],
                &base(text)
            ),
            Err(Nonconformance::LostIdent {
                ident: "g".to_owned()
            })
        );
        // the first missing one is reported, in the order of the base text
        assert_eq!(
            call.conforms(&[ins(0, "fn h() {}\n"), rep(15, 24, "h();")], &base(text)),
            Err(Nonconformance::LostIdent {
                ident: "g".to_owned()
            })
        );
        // a name found only in the call is kept, and keywords and numbers are not names
        let text = "fn f(x: i32) { return; }";
        assert_eq!(
            call.conforms(&[ins(0, "fn h() {}\n"), rep(15, 22, "h();")], &base(text)),
            Ok(())
        );
        let text = "fn f(x: i32) { g(x, 7u8); }";
        assert_eq!(
            call.conforms(
                &[
                    ins(0, "fn h(x: i32) { g(x, 7u8); }\n"),
                    rep(15, 25, "h(x);")
                ],
                &base(text)
            ),
            Ok(())
        );
    }

    fn rp(s: &str) -> RelPath {
        RelPath::parse(s).unwrap()
    }
    fn rep_at(p: &str, a: u32, b: u32, t: &str) -> MutationSpec {
        MutationSpec::replace(rp(p), TextRange::new(a, b).unwrap(), t).unwrap()
    }
    fn ins_at(p: &str, at: u32, t: &str) -> MutationSpec {
        MutationSpec::insert(rp(p), at, t).unwrap()
    }
    fn del_at(p: &str, a: u32, b: u32) -> MutationSpec {
        MutationSpec::delete(rp(p), TextRange::new(a, b).unwrap()).unwrap()
    }
    fn two(a: &str, b: &str) -> BTreeMap<RelPath, String> {
        BTreeMap::from([(rp("a.rs"), a.to_owned()), (rp("b.rs"), b.to_owned())])
    }
    fn mv(item: &str, to: &str) -> IntentCall {
        IntentCall::MoveItem {
            item: id(item),
            to: rp(to),
        }
    }

    #[test]
    fn move_item_needs_two_files_and_the_item() {
        let b = two("pub fn foo() {}\nfn bar() {}\n", "");
        let call = mv("foo", "b.rs");
        let reexport = "pub use crate::b::foo;";
        let item = "pub fn foo() {}\n";
        let ok = [rep_at("a.rs", 0, 15, reexport), ins_at("b.rs", 0, item)];
        assert_eq!(call.conforms(&ok, &b), Ok(()));
        // one file, a third file, or `to` missing from the specs
        assert_eq!(call.conforms(&ok[..1], &b), Err(Nonconformance::MoveFiles));
        assert_eq!(call.conforms(&[], &b), Err(Nonconformance::MoveFiles));
        let c = ins_at("c.rs", 0, "x");
        assert_eq!(
            call.conforms(&[ok[0].clone(), ok[1].clone(), c.clone()], &b),
            Err(Nonconformance::MoveFiles)
        );
        assert_eq!(
            call.conforms(&[ok[0].clone(), c], &b),
            Err(Nonconformance::MoveFiles)
        );
        // both files need a base text
        let only_a = BTreeMap::from([(rp("a.rs"), "pub fn foo() {}\n".to_owned())]);
        assert_eq!(
            call.conforms(&ok, &only_a),
            Err(Nonconformance::MissingBase { path: rp("b.rs") })
        );
        // the source must declare exactly the item
        assert_eq!(
            call.conforms(&[rep_at("a.rs", 16, 27, reexport), ok[1].clone()], &b),
            Err(Nonconformance::NotAnItem { spec: 0 })
        );
        assert_eq!(
            mv("fo", "b.rs").conforms(&ok, &b),
            Err(Nonconformance::NotAnItem { spec: 0 })
        );
        // an index counts in `specs`, whichever file comes first
        assert_eq!(
            call.conforms(&[ok[1].clone(), rep_at("a.rs", 16, 27, reexport)], &b),
            Err(Nonconformance::NotAnItem { spec: 1 })
        );
        // a replace of the source must leave a re-export of the item
        for bad in [
            "pub use crate::b::bar;",
            "pub use foo;",
            "pub use crate::b::foo",
            "pub(in x) use crate::b::foo;",
            "pub fn foo() { }",
        ] {
            assert_eq!(
                call.conforms(&[rep_at("a.rs", 0, 15, bad), ok[1].clone()], &b),
                Err(Nonconformance::NoReexport { spec: 0 }),
                "{bad}"
            );
        }
        // writing the item back is a no-op, which is refused first
        assert_eq!(
            call.conforms(
                &[rep_at("a.rs", 0, 15, "pub fn foo() {}"), ok[1].clone()],
                &b
            ),
            Err(Nonconformance::NoOp { spec: 0 })
        );
        for good in [
            "use crate::b::foo;",
            "pub(crate) use crate::b::foo;",
            " pub(super) use super::b::foo;\n",
        ] {
            assert_eq!(
                call.conforms(&[rep_at("a.rs", 0, 15, good), ok[1].clone()], &b),
                Ok(()),
                "{good}"
            );
        }
        // the source has one spec, a replace or a delete
        assert_eq!(
            call.conforms(&[ins_at("a.rs", 0, "x"), ok[1].clone()], &b),
            Err(Nonconformance::WrongKind {
                spec: 0,
                kind: SpecKind::Insert
            })
        );
        assert_eq!(
            call.conforms(
                &[ok[0].clone(), rep_at("a.rs", 16, 27, "z"), ok[1].clone()],
                &b
            ),
            Err(Nonconformance::NotMoved)
        );
        // the destination must receive the text of the item
        assert_eq!(
            call.conforms(&[ok[0].clone(), ins_at("b.rs", 0, "pub fn other() {}")], &b),
            Err(Nonconformance::NotMoved)
        );
        assert_eq!(
            call.conforms(&[ok[0].clone(), ok[1].clone(), ins_at("b.rs", 0, "y")], &b),
            Err(Nonconformance::NotMoved)
        );
        let b2 = two("pub fn foo() {}\nfn bar() {}\n", "fn z() {}\n");
        assert_eq!(
            call.conforms(&[ok[0].clone(), del_at("b.rs", 0, 5)], &b2),
            Err(Nonconformance::WrongKind {
                spec: 1,
                kind: SpecKind::Delete
            })
        );
        assert_eq!(
            call.conforms(&[ok[0].clone(), rep_at("b.rs", 0, 9, item)], &b2),
            Err(Nonconformance::BaseTextMismatch { spec: 1 })
        );
    }

    #[test]
    fn move_item_accepts_reexport_and_collapse() {
        // forward: the destination may write the item with another visibility and spacing
        let b = two("pub fn foo() {}\nfn bar() {}\n", "");
        let call = mv("foo", "b.rs");
        let reexport = rep_at("a.rs", 0, 15, "pub use crate::b::foo;");
        for dest in [
            "pub fn foo() {}\n",
            "pub(crate) fn  foo()\n{}",
            "// moved\nfn foo() {}",
        ] {
            assert_eq!(
                call.conforms(&[reexport.clone(), ins_at("b.rs", 0, dest)], &b),
                Ok(()),
                "{dest}"
            );
        }
        // other item kinds
        let b = two("struct S;\n", "");
        assert_eq!(
            mv("S", "b.rs").conforms(
                &[
                    rep_at("a.rs", 0, 9, "use crate::b::S;"),
                    ins_at("b.rs", 0, "pub struct S;")
                ],
                &b
            ),
            Ok(())
        );
        // collapse: the item comes back to the file that re-exports it
        let b = two(
            "pub use crate::b::foo;\nfn bar() {}\n",
            "pub fn foo() {}\nfn baz() {}\n",
        );
        let back = mv("foo", "a.rs");
        assert_eq!(
            back.conforms(
                &[
                    del_at("b.rs", 0, 16),
                    rep_at("a.rs", 0, 22, "pub fn foo() {}")
                ],
                &b
            ),
            Ok(())
        );
        // a collapse that writes something else is not a move
        assert_eq!(
            back.conforms(
                &[
                    del_at("b.rs", 0, 16),
                    rep_at("a.rs", 0, 22, "pub fn foo() { 1 }")
                ],
                &b
            ),
            Err(Nonconformance::NotMoved)
        );
    }
}
