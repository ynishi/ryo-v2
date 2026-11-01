//! The catalog of refactoring commands, and the check that a set of splices carries one out.
//!
//! A Blueprint names a command (`Command`) and gives the splices. Nothing here trusts the
//! pairing: [`Command::conforms`] reads the splices against the base text and refuses a
//! Blueprint whose edits are not what its command says.

use super::{DomainError, NonEmpty, RelPath, SpliceKind};
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
    ///   tested: domain::command::tests::ident_rejects_keywords
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
    ///   tested: domain::command::tests::add_derive_checks_list
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

/// The closed set of commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CommandKind {
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

/// What can go wrong with a command that the verifier alone does not show.
///
/// Why each command carries the marks it does. Extract-function moves statements into a new body,
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
    ///   tested: domain::command::tests::risk_mark_unseen_class_maps_each_mark
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
///
/// # Contract
/// - C1: the display name of each class is its kebab-case name, `borrow`, `build-script`,
///   `proc-macro-crate`, `visibility`, `const-eval`, `struct-literal-path`, `unused-type-param`,
///   `derive-without-server`, `unresolved-path`, `duplicate-name`, `field-bounds`, `test-code`
///   and `unexpanded-macro`.
///   tested: domain::command::tests::unseen_class_names_are_stable
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum UnseenClass {
    /// Borrow checking is not run by the analyzer.
    Borrow,
    /// A build script changed.
    BuildScript,
    /// A proc-macro crate changed.
    ProcMacroCrate,
    /// Privacy and cfg-gated paths across crates are not judged by the analyzer: an item moved.
    Visibility,
    /// Constant evaluation is not run by the analyzer (E0080).
    ConstEval,
    /// A struct literal that names a path that is not a struct is not judged (E0422).
    StructLiteralPath,
    /// A type parameter no field uses is not judged (E0392).
    UnusedTypeParam,
    /// The proc-macro server is off, so a derive is not expanded.
    DeriveWithoutServer,
    /// The gap detector of unresolved names and path prefixes is off (E0405, E0425, E0433).
    UnresolvedPath,
    /// The gap detector of names an item and a `use` both define is off (E0255).
    DuplicateName,
    /// The gap detector of the traits a field owes to a derived impl is off (E0204, E0277, E0369).
    FieldBounds,
    /// Code under `#[cfg(test)]` and in tests is not diagnosed.
    TestCode,
    /// A file has a macro call the analyzer could not expand, so the unresolved-path detector
    /// stayed silent on it. Observed in a check, never declared.
    UnexpandedMacro,
}

impl std::fmt::Display for UnseenClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            UnseenClass::Borrow => "borrow",
            UnseenClass::BuildScript => "build-script",
            UnseenClass::ProcMacroCrate => "proc-macro-crate",
            UnseenClass::Visibility => "visibility",
            UnseenClass::ConstEval => "const-eval",
            UnseenClass::StructLiteralPath => "struct-literal-path",
            UnseenClass::UnusedTypeParam => "unused-type-param",
            UnseenClass::DeriveWithoutServer => "derive-without-server",
            UnseenClass::UnresolvedPath => "unresolved-path",
            UnseenClass::DuplicateName => "duplicate-name",
            UnseenClass::FieldBounds => "field-bounds",
            UnseenClass::TestCode => "test-code",
            UnseenClass::UnexpandedMacro => "unexpanded-macro",
        })
    }
}

/// An entry of the catalog: a command kind and the risks it always carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// The command kind.
    pub kind: CommandKind,
    /// The risks every use of it carries.
    pub risks: BTreeSet<RiskMark>,
}

impl CommandKind {
    /// Every command kind.
    pub const ALL: [CommandKind; 5] = [
        CommandKind::RenameLocal,
        CommandKind::ExtractVariable,
        CommandKind::AddDerive,
        CommandKind::ExtractFunction,
        CommandKind::MoveItem,
    ];

    /// The catalog entry.
    ///
    /// # Contract
    /// - C1: `RenameLocal` and `AddDerive` carry no risk; `ExtractVariable` carries `Borrow`
    ///   and `Behaviour`.
    ///   tested: domain::command::tests::extract_variable_carries_borrow_and_behaviour
    /// - C2: `ExtractFunction` carries `Borrow` and `Behaviour` (parameters become borrows and
    ///   drop order changes); `MoveItem` carries `Visibility` alone, so its candidate names
    ///   `Visibility` as an unseen class.
    ///   tested: domain::command::tests::new_commands_carry_their_risks
    pub fn spec(self) -> CommandSpec {
        let risks = match self {
            CommandKind::RenameLocal | CommandKind::AddDerive => BTreeSet::new(),
            CommandKind::ExtractVariable | CommandKind::ExtractFunction => {
                BTreeSet::from([RiskMark::Borrow, RiskMark::Behaviour])
            }
            CommandKind::MoveItem => BTreeSet::from([RiskMark::Visibility]),
        };
        CommandSpec { kind: self, risks }
    }

    /// The stable name: `rename-local`, `extract-variable`, `add-derive`, `extract-function` or
    /// `move-item`.
    pub fn id(self) -> &'static str {
        match self {
            CommandKind::RenameLocal => "rename-local",
            CommandKind::ExtractVariable => "extract-variable",
            CommandKind::AddDerive => "add-derive",
            CommandKind::ExtractFunction => "extract-function",
            CommandKind::MoveItem => "move-item",
        }
    }

    /// The command kind whose [`id`](CommandKind::id) is `s`.
    ///
    /// # Contract
    /// - C1: `parse(k.id()) == Some(k)` for every kind, and `None` for any other text.
    ///   tested: domain::command::tests::command_ids_round_trip
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.id() == s)
    }
}

impl std::fmt::Display for CommandKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.id())
    }
}

/// A command with its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
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

/// Why splices do not carry out their command.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Nonconformance {
    /// The splices touch more than one file (or none).
    #[error("the specs do not touch exactly one file")]
    ManyFiles,
    /// No base text was given for the file.
    #[error("no base text for {path}")]
    MissingBase {
        /// The file.
        path: RelPath,
    },
    /// Splice number `splice` has a kind this command does not use.
    #[error("spec {splice} is a {kind}, which this intent does not use")]
    WrongKind {
        /// Index of the splice.
        splice: usize,
        /// Its kind.
        kind: SpliceKind,
    },
    /// The base text under splice number `splice` is not what the command names.
    #[error("spec {splice} does not touch the name the intent renames")]
    BaseTextMismatch {
        /// Index of the splice.
        splice: usize,
    },
    /// The new text of splice number `splice` is not what the command says.
    #[error("spec {splice} writes text other than the intent's")]
    NewTextMismatch {
        /// Index of the splice.
        splice: usize,
    },
    /// No insert binds the new variable with `let`.
    #[error("no single insert binds the new variable with let")]
    NoLetInsert,
    /// Nothing replaces an expression with the new variable.
    #[error("no replace puts the new variable in place of an expression")]
    NoUseReplace,
    /// Splice number `splice` is not a derive attribute or a derive list tail.
    #[error("spec {splice} is not a derive")]
    NotADerive {
        /// Index of the splice.
        splice: usize,
    },
    /// The derives written differ from the command's list.
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
    /// The splices of a move do not touch exactly the source file and the destination file.
    #[error("the specs do not touch exactly the file moved from and the file moved to")]
    MoveFiles,
    /// The base text under splice number `splice` does not declare the item moved.
    #[error("spec {splice} does not declare the item moved")]
    NotAnItem {
        /// Index of the splice.
        splice: usize,
    },
    /// The new text of splice number `splice` is not a re-export of the item moved.
    #[error("spec {splice} does not re-export the item moved")]
    NoReexport {
        /// Index of the splice.
        splice: usize,
    },
    /// The destination does not receive the text of the item.
    #[error("the destination does not receive the item's text")]
    NotMoved,
    /// Splice number `splice` replaces text by the same text.
    #[error("spec {splice} writes the bytes it replaces: it changes nothing")]
    NoOp {
        /// Index of the splice.
        splice: usize,
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

fn base_slice<'a>(base: &'a str, splice: &super::Splice) -> Option<&'a str> {
    base.get(splice.range().start() as usize..splice.range().end() as usize)
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

/// The names a derive splice text adds: `#[derive(L)]` or `, L`.
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

impl Command {
    /// The command kind being called.
    pub fn kind(&self) -> CommandKind {
        match self {
            Command::RenameLocal { .. } => CommandKind::RenameLocal,
            Command::ExtractVariable { .. } => CommandKind::ExtractVariable,
            Command::AddDerive { .. } => CommandKind::AddDerive,
            Command::ExtractFunction { .. } => CommandKind::ExtractFunction,
            Command::MoveItem { .. } => CommandKind::MoveItem,
        }
    }

    /// Checks that `splices` carry out this call against the `base` texts.
    ///
    /// Known limit of rename-local and add-derive: they are checked by matching text only, so a
    /// Blueprint can pass with a rename inside a string literal, a name capture, one side of a
    /// shadowing pair or a struct-field rename.
    ///
    /// # Contract
    /// - C7: checked before every other clause and for every command, `MoveItem` included: the
    ///   first splice that is a `Replace` writing the bytes of the base text it replaces (when
    ///   `base` has its path and the range lies inside it) gives `NoOp`; inserts and deletes
    ///   are never no-ops.
    ///   tested: domain::command::tests::no_op_replace_is_refused
    /// - C1: for every command except `MoveItem`, all splices are on one path and `base` has a
    ///   text for it, else `ManyFiles` or `MissingBase`.
    ///   tested: domain::command::tests::conforms_needs_one_file_and_its_base
    /// - C2: `RenameLocal`: every splice is a `Replace` of the text `from` by the text `to`.
    ///   tested: domain::command::tests::rename_local_rejects_replace_of_other_text
    /// - C3: `ExtractVariable`: no `Delete`; exactly one `Insert`, containing `let <name>`
    ///   not followed by an identifier char; at least one `Replace` whose text is `name`.
    ///   tested: domain::command::tests::extract_variable_needs_let_and_use
    /// - C4: `AddDerive`: every splice is an `Insert` of `#[derive(L)]` or `, L`, and the
    ///   concatenated lists equal `derives` in order.
    ///   tested: domain::command::tests::add_derive_checks_list
    /// - C5: `ExtractFunction`: no `Delete` (`WrongKind`); exactly one `Insert` whose text has
    ///   `fn`, whitespace, the name, optional whitespace and `(` or `<` (`NoFnInsert`); exactly
    ///   one `Replace` whose new text contains `name(` not preceded by an identifier char
    ///   (`NoCallReplace`); every identifier token of that `Replace`'s base text that is not a
    ///   keyword occurs among the tokens of the `Insert` text or of the `Replace` text, the
    ///   first missing one giving `LostIdent`.
    ///   tested: domain::command::tests::extract_function_needs_fn_insert_and_call
    ///   tested: domain::command::tests::extract_function_rejects_lost_identifiers
    /// - C6: `MoveItem`: the splices touch exactly two paths, one of them `to`, both with a base
    ///   text (`MoveFiles`, `MissingBase`). The other path, the source, has exactly one splice, a
    ///   `Replace` or a `Delete` (`WrongKind`, else `NotMoved`), whose base text declares the
    ///   item as `<kw> <item>` with `kw` one of `fn struct enum trait type const static union`
    ///   (`NotAnItem`); a `Replace` there writes `[<vis> ]use <path>::<item>;` with `vis` one of
    ///   `pub`, `pub(crate)` or `pub(super)` (`NoReexport`). The destination `to` has exactly one
    ///   splice, an `Insert`, or a `Replace` of such a `use` line (`WrongKind`, `NotMoved`,
    ///   `BaseTextMismatch`), and its new text, with whitespace collapsed and every visibility
    ///   token removed, contains the source's base text treated the same way (`NotMoved`).
    ///   tested: domain::command::tests::move_item_needs_two_files_and_the_item
    ///   tested: domain::command::tests::move_item_accepts_reexport_and_collapse
    pub fn conforms(
        &self,
        splices: &[super::Splice],
        base: &BTreeMap<RelPath, String>,
    ) -> Result<(), Nonconformance> {
        if let Some(splice) = splices.iter().position(|s| {
            s.kind() == SpliceKind::Replace
                && base
                    .get(s.path())
                    .and_then(|text| base_slice(text, s))
                    .is_some_and(|old| old == s.text())
        }) {
            return Err(Nonconformance::NoOp { splice });
        }
        if let Command::MoveItem { item, to } = self {
            return conforms_move(item, to, splices, base);
        }
        let Some(first) = splices.first() else {
            return Err(Nonconformance::ManyFiles);
        };
        if splices.iter().any(|s| s.path() != first.path()) {
            return Err(Nonconformance::ManyFiles);
        }
        let Some(text) = base.get(first.path()) else {
            return Err(Nonconformance::MissingBase {
                path: first.path().clone(),
            });
        };
        match self {
            Command::RenameLocal { from, to } => {
                for (i, s) in splices.iter().enumerate() {
                    if s.kind() != SpliceKind::Replace {
                        return Err(Nonconformance::WrongKind {
                            splice: i,
                            kind: s.kind(),
                        });
                    }
                    if base_slice(text, s) != Some(from.as_str()) {
                        return Err(Nonconformance::BaseTextMismatch { splice: i });
                    }
                    if s.text() != to.as_str() {
                        return Err(Nonconformance::NewTextMismatch { splice: i });
                    }
                }
                Ok(())
            }
            Command::ExtractVariable { name } => {
                if let Some((i, s)) = splices
                    .iter()
                    .enumerate()
                    .find(|(_, s)| s.kind() == SpliceKind::Delete)
                {
                    return Err(Nonconformance::WrongKind {
                        splice: i,
                        kind: s.kind(),
                    });
                }
                let mut inserts = splices.iter().filter(|s| s.kind() == SpliceKind::Insert);
                match (inserts.next(), inserts.next()) {
                    (Some(ins), None) if binds_with_let(ins.text(), name.as_str()) => {}
                    _ => return Err(Nonconformance::NoLetInsert),
                }
                if !splices
                    .iter()
                    .any(|s| s.kind() == SpliceKind::Replace && s.text() == name.as_str())
                {
                    return Err(Nonconformance::NoUseReplace);
                }
                Ok(())
            }
            Command::AddDerive { derives } => {
                let mut written = Vec::new();
                for (i, s) in splices.iter().enumerate() {
                    if s.kind() != SpliceKind::Insert {
                        return Err(Nonconformance::WrongKind {
                            splice: i,
                            kind: s.kind(),
                        });
                    }
                    let Some(names) = derive_text(s.text()) else {
                        return Err(Nonconformance::NotADerive { splice: i });
                    };
                    written.extend(names);
                }
                if written.as_slice() == derives.as_slice() {
                    Ok(())
                } else {
                    Err(Nonconformance::DeriveListMismatch)
                }
            }
            Command::ExtractFunction { name } => {
                if let Some((i, s)) = splices
                    .iter()
                    .enumerate()
                    .find(|(_, s)| s.kind() == SpliceKind::Delete)
                {
                    return Err(Nonconformance::WrongKind {
                        splice: i,
                        kind: s.kind(),
                    });
                }
                let mut inserts = splices.iter().filter(|s| s.kind() == SpliceKind::Insert);
                let ins = match (inserts.next(), inserts.next()) {
                    (Some(ins), None) if declares_fn(ins.text(), name.as_str()) => ins,
                    _ => return Err(Nonconformance::NoFnInsert),
                };
                let mut replaces = splices
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.kind() == SpliceKind::Replace);
                let (ri, rep) = match (replaces.next(), replaces.next()) {
                    (Some((i, r)), None) if calls_fn(r.text(), name.as_str()) => (i, r),
                    _ => return Err(Nonconformance::NoCallReplace),
                };
                let Some(replaced) = base_slice(text, rep) else {
                    return Err(Nonconformance::BaseTextMismatch { splice: ri });
                };
                match first_lost_ident(replaced, [ins.text(), rep.text()]) {
                    Some(ident) => Err(Nonconformance::LostIdent { ident }),
                    None => Ok(()),
                }
            }
            Command::MoveItem { item, to } => conforms_move(item, to, splices, base),
        }
    }
}

/// The checks of `MoveItem`, which spans two files.
fn conforms_move(
    item: &Ident,
    to: &RelPath,
    splices: &[super::Splice],
    base: &BTreeMap<RelPath, String>,
) -> Result<(), Nonconformance> {
    let paths: BTreeSet<&RelPath> = splices.iter().map(|s| s.path()).collect();
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
    let on = |p: &RelPath| -> Vec<(usize, &super::Splice)> {
        splices
            .iter()
            .enumerate()
            .filter(|(_, s)| s.path() == p)
            .collect()
    };
    let item = item.as_str();
    let [(si, s)] = on(src)[..] else {
        return Err(Nonconformance::NotMoved);
    };
    if s.kind() == SpliceKind::Insert {
        return Err(Nonconformance::WrongKind {
            splice: si,
            kind: s.kind(),
        });
    }
    let Some(declared) = base_slice(&base[src], s).filter(|d| declares_item(d, item)) else {
        return Err(Nonconformance::NotAnItem { splice: si });
    };
    if s.kind() == SpliceKind::Replace && !is_use_of(s.text(), item) {
        return Err(Nonconformance::NoReexport { splice: si });
    }
    let [(di, d)] = on(to)[..] else {
        return Err(Nonconformance::NotMoved);
    };
    match d.kind() {
        SpliceKind::Delete => {
            return Err(Nonconformance::WrongKind {
                splice: di,
                kind: d.kind(),
            });
        }
        SpliceKind::Replace => {
            if !base_slice(&base[to], d).is_some_and(|b| is_use_of(b, item)) {
                return Err(Nonconformance::BaseTextMismatch { splice: di });
            }
        }
        SpliceKind::Insert => {}
    }
    if without_vis(d.text()).contains(&without_vis(declared)) {
        Ok(())
    } else {
        Err(Nonconformance::NotMoved)
    }
}

impl std::fmt::Display for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Command::RenameLocal { from, to } => write!(f, "rename-local {from} -> {to}"),
            Command::ExtractVariable { name } => write!(f, "extract-variable {name}"),
            Command::AddDerive { derives } => {
                let names: Vec<&str> = derives.iter().map(DeriveName::as_str).collect();
                write!(f, "add-derive {}", names.join(", "))
            }
            Command::ExtractFunction { name } => write!(f, "extract-function {name}"),
            Command::MoveItem { item, to } => write!(f, "move-item {item} -> {to}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Splice, TextRange};

    fn rel() -> RelPath {
        RelPath::parse("a.rs").unwrap()
    }
    fn id(s: &str) -> Ident {
        Ident::parse(s).unwrap()
    }
    fn rep(a: u32, b: u32, t: &str) -> Splice {
        Splice::replace(rel(), TextRange::new(a, b).unwrap(), t).unwrap()
    }
    fn ins(at: u32, t: &str) -> Splice {
        Splice::insert(rel(), at, t).unwrap()
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
    fn command_ids_round_trip() {
        for k in CommandKind::ALL {
            assert_eq!(CommandKind::parse(k.id()), Some(k));
            assert_eq!(k.spec().kind, k);
        }
        assert_eq!(CommandKind::parse("rename"), None);
        assert_eq!(CommandKind::AddDerive.to_string(), "add-derive");
        assert_eq!(CommandKind::ALL.len(), 5);
        assert_eq!(
            CommandKind::parse("extract-function"),
            Some(CommandKind::ExtractFunction)
        );
        assert_eq!(CommandKind::parse("move-item"), Some(CommandKind::MoveItem));
        assert_eq!(CommandKind::MoveItem.to_string(), "move-item");
        assert_eq!(RiskMark::Visibility.to_string(), "visibility");
    }

    #[test]
    fn new_commands_carry_their_risks() {
        assert_eq!(
            CommandKind::ExtractFunction.spec().risks,
            BTreeSet::from([RiskMark::Borrow, RiskMark::Behaviour])
        );
        assert_eq!(
            CommandKind::MoveItem.spec().risks,
            BTreeSet::from([RiskMark::Visibility])
        );
        let ef = Command::ExtractFunction { name: id("h") };
        let mv = Command::MoveItem {
            item: id("foo"),
            to: RelPath::parse("b.rs").unwrap(),
        };
        assert_eq!(ef.kind(), CommandKind::ExtractFunction);
        assert_eq!(mv.kind(), CommandKind::MoveItem);
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
    fn unseen_class_names_are_stable() {
        let names = [
            (UnseenClass::Borrow, "borrow"),
            (UnseenClass::BuildScript, "build-script"),
            (UnseenClass::ProcMacroCrate, "proc-macro-crate"),
            (UnseenClass::Visibility, "visibility"),
            (UnseenClass::ConstEval, "const-eval"),
            (UnseenClass::StructLiteralPath, "struct-literal-path"),
            (UnseenClass::UnusedTypeParam, "unused-type-param"),
            (UnseenClass::DeriveWithoutServer, "derive-without-server"),
            (UnseenClass::UnresolvedPath, "unresolved-path"),
            (UnseenClass::DuplicateName, "duplicate-name"),
            (UnseenClass::FieldBounds, "field-bounds"),
            (UnseenClass::TestCode, "test-code"),
            (UnseenClass::UnexpandedMacro, "unexpanded-macro"),
        ];
        for (class, name) in names {
            assert_eq!(class.to_string(), name);
        }
        let distinct: std::collections::BTreeSet<String> =
            names.iter().map(|(c, _)| c.to_string()).collect();
        assert_eq!(distinct.len(), names.len());
    }

    #[test]
    fn extract_variable_carries_borrow_and_behaviour() {
        assert!(CommandKind::RenameLocal.spec().risks.is_empty());
        assert!(CommandKind::AddDerive.spec().risks.is_empty());
        assert_eq!(
            CommandKind::ExtractVariable.spec().risks,
            BTreeSet::from([RiskMark::Borrow, RiskMark::Behaviour])
        );
    }

    #[test]
    fn conforms_needs_one_file_and_its_base() {
        let call = Command::RenameLocal {
            from: id("a"),
            to: id("b"),
        };
        let other = Splice::replace(
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
        assert_eq!(call.kind(), CommandKind::RenameLocal);
        assert_eq!(call.to_string(), "rename-local a -> b");
    }

    #[test]
    fn no_op_replace_is_refused() {
        let same = Command::RenameLocal {
            from: id("a"),
            to: id("a"),
        };
        assert_eq!(
            same.conforms(&[rep(4, 5, "a")], &base("let a = 1;")),
            Err(Nonconformance::NoOp { splice: 0 })
        );
        // the lowest index wins, before every other check
        let call = Command::RenameLocal {
            from: id("a"),
            to: id("bb"),
        };
        assert_eq!(
            call.conforms(&[rep(4, 5, "bb"), rep(8, 9, "a")], &base("let a = a + c;")),
            Err(Nonconformance::NoOp { splice: 1 })
        );
        // an insert or a delete is never a no-op; a range outside the base is not judged here
        assert_ne!(
            call.conforms(&[ins(0, "z")], &base("a")),
            Err(Nonconformance::NoOp { splice: 0 })
        );
        assert_ne!(
            call.conforms(&[rep(40, 41, "a")], &base("a")),
            Err(Nonconformance::NoOp { splice: 0 })
        );
        // a no-op is found before the file count and the missing base are
        let other = Splice::replace(
            RelPath::parse("b.rs").unwrap(),
            TextRange::new(0, 1).unwrap(),
            "z",
        )
        .unwrap();
        assert_eq!(
            same.conforms(&[rep(4, 5, "a"), other.clone()], &base("let a = 1;")),
            Err(Nonconformance::NoOp { splice: 0 })
        );
        assert_eq!(
            same.conforms(&[other, rep(4, 5, "a")], &base("let a = 1;")),
            Err(Nonconformance::NoOp { splice: 1 })
        );
        // a move whose source Replace writes back its own text
        let b = two("pub fn foo() {}\n", "");
        let back = [
            rep_at("a.rs", 0, 15, "pub fn foo() {}"),
            ins_at("b.rs", 0, "pub fn foo() {}\n"),
        ];
        assert_eq!(
            mv("foo", "b.rs").conforms(&back, &b),
            Err(Nonconformance::NoOp { splice: 0 })
        );
    }

    #[test]
    fn rename_local_rejects_replace_of_other_text() {
        let call = Command::RenameLocal {
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
            Err(Nonconformance::BaseTextMismatch { splice: 1 })
        );
        assert_eq!(
            call.conforms(&[rep(4, 5, "cc")], &base(text)),
            Err(Nonconformance::NewTextMismatch { splice: 0 })
        );
        assert_eq!(
            call.conforms(&[ins(4, "bb")], &base(text)),
            Err(Nonconformance::WrongKind {
                splice: 0,
                kind: SpliceKind::Insert
            })
        );
        // out of range or off a char boundary is a mismatch, not a panic
        assert_eq!(
            call.conforms(&[rep(40, 41, "bb")], &base(text)),
            Err(Nonconformance::BaseTextMismatch { splice: 0 })
        );
    }

    #[test]
    fn extract_variable_needs_let_and_use() {
        let call = Command::ExtractVariable { name: id("t") };
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
        let del = Splice::delete(rel(), TextRange::new(0, 1).unwrap()).unwrap();
        assert_eq!(
            call.conforms(&[ok[0].clone(), del], &base(text)),
            Err(Nonconformance::WrongKind {
                splice: 1,
                kind: SpliceKind::Delete
            })
        );
    }

    #[test]
    fn add_derive_checks_list() {
        assert!(DeriveName::parse("serde::Serialize").is_ok());
        for bad in ["", "::A", "A::", "A:B", "a b", "fn", "A,B"] {
            assert!(DeriveName::parse(bad).is_err(), "{bad}");
        }
        let call = Command::AddDerive {
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
                Err(Nonconformance::NotADerive { splice: 0 }),
                "{bad}"
            );
        }
        assert_eq!(
            call.conforms(&[rep(0, 1, "#[derive(Clone)]")], &base(text)),
            Err(Nonconformance::WrongKind {
                splice: 0,
                kind: SpliceKind::Replace
            })
        );
    }

    #[test]
    fn extract_function_needs_fn_insert_and_call() {
        let call = Command::ExtractFunction { name: id("h") };
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
        let del = Splice::delete(rel(), TextRange::new(0, 2).unwrap()).unwrap();
        assert_eq!(
            call.conforms(&[ins(0, decl), rep(15, 24, "h(x);"), del], &base(text)),
            Err(Nonconformance::WrongKind {
                splice: 2,
                kind: SpliceKind::Delete
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
            Err(Nonconformance::BaseTextMismatch { splice: 1 })
        );
    }

    #[test]
    fn extract_function_rejects_lost_identifiers() {
        let call = Command::ExtractFunction { name: id("h") };
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
    fn rep_at(p: &str, a: u32, b: u32, t: &str) -> Splice {
        Splice::replace(rp(p), TextRange::new(a, b).unwrap(), t).unwrap()
    }
    fn ins_at(p: &str, at: u32, t: &str) -> Splice {
        Splice::insert(rp(p), at, t).unwrap()
    }
    fn del_at(p: &str, a: u32, b: u32) -> Splice {
        Splice::delete(rp(p), TextRange::new(a, b).unwrap()).unwrap()
    }
    fn two(a: &str, b: &str) -> BTreeMap<RelPath, String> {
        BTreeMap::from([(rp("a.rs"), a.to_owned()), (rp("b.rs"), b.to_owned())])
    }
    fn mv(item: &str, to: &str) -> Command {
        Command::MoveItem {
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
        // one file, a third file, or `to` missing from the splices
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
            Err(Nonconformance::NotAnItem { splice: 0 })
        );
        assert_eq!(
            mv("fo", "b.rs").conforms(&ok, &b),
            Err(Nonconformance::NotAnItem { splice: 0 })
        );
        // an index counts in `splices`, whichever file comes first
        assert_eq!(
            call.conforms(&[ok[1].clone(), rep_at("a.rs", 16, 27, reexport)], &b),
            Err(Nonconformance::NotAnItem { splice: 1 })
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
                Err(Nonconformance::NoReexport { splice: 0 }),
                "{bad}"
            );
        }
        // writing the item back is a no-op, which is refused first
        assert_eq!(
            call.conforms(
                &[rep_at("a.rs", 0, 15, "pub fn foo() {}"), ok[1].clone()],
                &b
            ),
            Err(Nonconformance::NoOp { splice: 0 })
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
        // the source has one splice, a replace or a delete
        assert_eq!(
            call.conforms(&[ins_at("a.rs", 0, "x"), ok[1].clone()], &b),
            Err(Nonconformance::WrongKind {
                splice: 0,
                kind: SpliceKind::Insert
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
                splice: 1,
                kind: SpliceKind::Delete
            })
        );
        assert_eq!(
            call.conforms(&[ok[0].clone(), rep_at("b.rs", 0, 9, item)], &b2),
            Err(Nonconformance::BaseTextMismatch { splice: 1 })
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
