//! Doc contracts: every promise written under a `# Contract` heading in the workspace's RustDoc
//! names the tests that check it, and `cargo test -p ryo-v2-core` fails when a named test does not
//! exist. This file is the single statement of the convention.
//!
//! The check reads sources with `syn` only: no rustdoc JSON, no rust-analyzer, no build of other
//! targets. It checks that a named test *exists*; that it really checks the promise is a review
//! question (see `DEVELOPMENT.md`).
//!
//! Example (the fence keeps this example out of the check, see rule 1):
//!
//! ```text
//! /// Errors of `checked` that the baseline does not account for.
//! ///
//! /// # Contract
//! /// - C1: an error of the baseline outside `checked.files` is ignored.
//! ///   tested: domain::baseline::tests::baseline_outside_scope_is_ignored
//! /// - C2: an added copy of an existing error is new.
//! ///   tested: domain::baseline::tests::added_copy_of_existing_error_is_new
//! ///   tested: ryo-v2-ra tests/baseline.rs some_integration_test
//! /// - C3: the result is sorted.
//! ///   untested: needs a case with several keys; owed by the backfill
//! /// - C4 [invariant]: no value of the type holds an empty list.
//! ///   tested: domain::baseline::tests::never_empty
//! /// - C5 [free]: the order of equal keys in the result.
//! ```
//!
//! Rules:
//!
//! 1. Section: a doc line whose trimmed text is exactly `# Contract`. It ends at the next line
//!    starting with `#` (any heading) or at the end of the doc block. Lines inside fenced code
//!    blocks (three backticks) are ignored. Valid in any doc block: `///` on any item (public or
//!    private: fn, struct, enum, trait, impl block, impl or trait item, const, static, type, mod,
//!    field, variant) and `//!` module docs.
//! 2. Clause: a line `- C<n>: <promise>` or `- C<n> [<kind>]: <promise>` at the start of the doc
//!    text, with `n` matching `[1-9][0-9]*`; exactly one space before `[`, no spaces inside the
//!    brackets, `:` right after `]` (anything else is a malformed clause line). The kinds are a
//!    closed set; any other word, including an empty one, is a problem
//!    (``unknown clause kind `<word>` (known: invariant, free)``) but the clause is still recorded.
//!    - no kind: a promise about what this item does; a caller may rely on it.
//!    - `invariant`: holds for every value of the type (or every state of the module), whichever
//!      operation made it; a caller may rely on it everywhere. It belongs on a type or module doc.
//!    - `free`: deliberately unspecified; the behaviour the text names may change without notice
//!      and callers must not rely on it.
//!
//!    There is no `pre` or `post` kind: an untagged clause already is the post-condition, and a
//!    precondition is written as a type (a value with rules is a type) or as a refusal clause of its
//!    constructor. The text of a clause is the text after the `:` plus the following lines that
//!    start with whitespace, up to its first `tested:` / `untested:` line, joined with one space; it
//!    must not be empty. Ids are unique within one doc block and never renumbered (a deleted clause
//!    leaves a gap). The global name of a clause is `<owner>#C<n>`, where `owner` is the module
//!    path within the crate plus the item path, for example `domain::baseline::new_findings#C2`
//!    or `domain::verdict::Verdict::passed#C1`; for a module doc it is the module path
//!    (`domain::baseline#C1`; the crate root is `crate`). Methods use the last segment of the
//!    impl's self type (`Verdict::passed`); fields and variants use `Type::name`; an impl block
//!    itself is `impl Type` or `impl Trait for Type`. The promise may continue on following
//!    indented lines.
//! 3. Each clause that is not `[free]` carries at least one `tested: <ref>` line, or exactly one
//!    `untested: <reason>` line with a non-empty reason, never both. A `[free]` clause carries
//!    neither (a `tested:` or `untested:` line under it is a problem) and is not counted as
//!    untested. One ref per `tested:` line; repeat the line for
//!    more tests. A ref may be wrapped in backticks.
//! 4. `<ref>` is whitespace-separated tokens, the last one being the test path:
//!    - `path`: a test in the same package, in the `src/` tree (lib or main).
//!    - `<target-file> path`: the target file ends in `.rs` and is relative to the package root
//!      (`tests/baseline.rs`, `src/bin/x.rs`): a test in that target of the same package.
//!    - `<package> path`: a test in the `src/` tree of another workspace package.
//!    - `<package> <target-file> path`.
//!
//!    `path` is the test's path as `cargo test -- --list` prints it for that target, without
//!    `crate::`: for `src/` the module path from the crate root (`domain::baseline::tests::name`),
//!    for `tests/x.rs` the path inside that file (`name` or `mod_a::name`). A target file of
//!    `src/lib.rs` or `src/main.rs` means the `src/` tree. Doc tests cannot be referenced (their
//!    names carry line numbers).
//! 5. A referenced function must exist, carry an attribute whose path ends in `test`
//!    (`#[test]`, `#[tokio::test]`), and must not carry `#[ignore]`.
//!
//! Untested clauses are allowed but counted: [`UNTESTED_CEILING`] equals their number exactly. It
//! only goes down; both a new `untested:` clause and a removed one fail the check until the
//! ceiling is updated.
//!
//! 6. Glossary: the `# Glossary` section of the `//!` docs of package `ryo-v2-core`, file
//!    `src/domain/mod.rs` ([`GLOSSARY_AT`]), and nowhere else (a `# Glossary` heading in any
//!    other doc block is a problem). The section ends as in rule 1. An entry is a line
//!    `- <Term>: <definition>` plus following lines starting with whitespace; `<Term>` matches
//!    `[A-Z][A-Za-z0-9]*`. If the joined text contains `includes:`, the part after its last
//!    occurrence is a comma-separated list of names (each optionally in backquotes, each matching
//!    `[A-Z][A-Za-z0-9]*`) and the definition is the part before it, which must not be empty.
//!    Other lines of the section are ignored. A malformed entry, an empty definition, a bad
//!    `includes` name and a duplicate term are problems. A name may be included by several terms.
//!    No glossary at all is an empty glossary.
//!
//!    A gap: in the text of any clause (any kind, any package) a backquoted span whose whole
//!    content is one CamelCase identifier `[A-Z][A-Za-z0-9]*` that is none of (1) a glossary term
//!    or a name in some `includes:`, (2) the name of a struct, enum, enum variant, union, trait or
//!    type alias declared anywhere in the same source file as the clause, (3) one of
//!    [`STD_NAMES`]. Paths (`a::B`), expressions, snake_case, file names and lowercase words are
//!    never gaps. A name defined in the file a reader is looking at needs no glossary; a CamelCase
//!    name used across a file boundary is a word of the shared language and is defined once. A
//!    gap is a counted warning, one per (clause, word): [`GLOSSARY_GAP_CEILING`] equals their
//!    number exactly and, like [`UNTESTED_CEILING`], only goes down.
//! 7. Duplicate text: two clauses anywhere in the workspace whose text (tag excluded, lowercased,
//!    runs of whitespace collapsed to one space, trimmed; nothing else is folded) is equal are a
//!    problem, whichever owners they have. `[free]` clauses take part. The problem is reported
//!    on every member after the first of the group sorted by (package, file, owner, id).
//!
//! Not checked, and left to review (see `DEVELOPMENT.md`): which change request touches which
//! clause; a clause that is vacuous or contradicts another with different words; whether a
//! glossary term names a real item; whether a change of an `includes:` list was intended.
//!
//! Limitations of this version: sources are read with `syn` regardless of `cfg` (so
//! `#[cfg(test)] mod tests` is indexed, and text inside string literals is not); files below a
//! `tests/` subdirectory, `src/bin/` files other than `src/bin/<name>.rs`, `build.rs` and
//! `benches/` are not scanned; out-of-line `mod x;` is resolved by file path only; tests inside
//! fn bodies or impl blocks cannot be referenced; names declared and clauses written inside
//! `macro_rules!` bodies (for example in `ids.rs`) are invisible to `syn`, so such clauses are not
//! checked and such names are not declared.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::time::Instant;
use syn::visit::Visit;

/// One source file handed to the checker.
struct Source {
    package: String,
    /// Relative to the package root, '/' separators.
    rel_path: String,
    text: String,
}

/// What the checker found.
#[derive(Debug, Default)]
struct Report {
    /// Hard errors, one line each: "<package>/<rel_path>: <owner>#C<n>: <what>". Sorted.
    problems: Vec<String>,
    /// Clauses marked `untested:`, as "<package>::<owner>#C<n>: <reason>". Sorted.
    untested: Vec<String>,
    /// Gaps of the glossary rule, as "<package>::<owner>#C<n>: `<Word>`". Sorted.
    glossary_gaps: Vec<String>,
}

/// Counters printed by the workspace test.
#[derive(Debug, Default)]
struct Stats {
    files: usize,
    tests: usize,
    free: usize,
    terms: usize,
}

/// Only goes down. Equals the number of `untested:` clauses in the workspace.
const UNTESTED_CEILING: usize = 0;

/// Only goes down. Equals the number of glossary gaps in the workspace.
const GLOSSARY_GAP_CEILING: usize = 0;

/// (package, file) holding the `# Glossary`; its module doc is owned by `domain`.
const GLOSSARY_AT: (&str, &str) = ("ryo-v2-core", "src/domain/mod.rs");

/// CamelCase names that need no glossary entry.
const STD_NAMES: &[&str] = &[
    "None",
    "Some",
    "Ok",
    "Err",
    "Option",
    "Result",
    "Vec",
    "String",
    "Box",
    "Self",
    "Display",
    "Debug",
    "Default",
    "Clone",
    "Copy",
    "PartialEq",
    "Eq",
    "PartialOrd",
    "Ord",
    "Hash",
    "From",
    "Into",
    "Iterator",
    "Send",
    "Sync",
];

/// (package, group, path): group is `src` or the target file (`tests/x.rs`, `src/bin/x.rs`).
type TestKey = (String, String, String);

#[derive(Clone, Copy)]
struct FnInfo {
    is_test: bool,
    ignored: bool,
}

/// The doc lines of one item (or module), with the name of its owner.
struct DocBlock {
    package: String,
    rel_path: String,
    owner: String,
    lines: Vec<String>,
}

fn doc_value(attr: &syn::Attribute) -> Option<String> {
    if !attr.path().is_ident("doc") {
        return None;
    }
    let syn::Meta::NameValue(nv) = &attr.meta else {
        return None;
    };
    let syn::Expr::Lit(lit) = &nv.value else {
        return None;
    };
    let syn::Lit::Str(s) = &lit.lit else {
        return None;
    };
    Some(s.value())
}

/// The `#[doc = "..."]` lines of the outer (or inner) attributes, one leading space stripped.
fn doc_lines(attrs: &[syn::Attribute], inner: bool) -> Vec<String> {
    let mut out = Vec::new();
    for attr in attrs {
        if matches!(attr.style, syn::AttrStyle::Inner(_)) != inner {
            continue;
        }
        if let Some(value) = doc_value(attr) {
            for line in value.split('\n') {
                out.push(line.strip_prefix(' ').unwrap_or(line).to_owned());
            }
        }
    }
    out
}

fn path_ends_in(attr: &syn::Attribute, name: &str) -> bool {
    attr.path().segments.last().is_some_and(|s| s.ident == name)
}

/// Where a file sits: the target group and the module path of its root, or `None` if skipped.
fn locate(rel_path: &str) -> Option<(String, Vec<String>)> {
    let parts: Vec<&str> = rel_path.split('/').collect();
    match parts.as_slice() {
        ["src", "lib.rs"] | ["src", "main.rs"] => Some(("src".to_owned(), Vec::new())),
        ["src", "bin", file] if file.ends_with(".rs") => Some((rel_path.to_owned(), Vec::new())),
        ["src", "bin", ..] => None,
        ["src", rest @ ..] if !rest.is_empty() => {
            let mut mods: Vec<String> = rest.iter().map(|s| (*s).to_owned()).collect();
            let last = mods.pop()?;
            if last == "mod.rs" {
                // `a/mod.rs` is module `a`.
            } else {
                mods.push(last.strip_suffix(".rs")?.to_owned());
            }
            Some(("src".to_owned(), mods))
        }
        ["tests", file] if file.ends_with(".rs") => Some((rel_path.to_owned(), Vec::new())),
        _ => None,
    }
}

struct Collector<'a> {
    package: &'a str,
    group: &'a str,
    rel_path: &'a str,
    mods: Vec<String>,
    tests: &'a mut HashMap<TestKey, FnInfo>,
    blocks: &'a mut Vec<DocBlock>,
    declared: &'a mut HashSet<String>,
}

impl Collector<'_> {
    fn declare(&mut self, name: &syn::Ident) {
        self.declared.insert(name.to_string());
    }

    fn path_of(&self, tail: &[&str]) -> String {
        let mut segs: Vec<&str> = self.mods.iter().map(String::as_str).collect();
        segs.extend_from_slice(tail);
        if segs.is_empty() {
            "crate".to_owned()
        } else {
            segs.join("::")
        }
    }

    fn add_docs(&mut self, tail: &[&str], lines: Vec<String>) {
        if lines.is_empty() {
            return;
        }
        let owner = self.path_of(tail);
        self.blocks.push(DocBlock {
            package: self.package.to_owned(),
            rel_path: self.rel_path.to_owned(),
            owner,
            lines,
        });
    }

    fn item_docs(&mut self, attrs: &[syn::Attribute], tail: &[&str]) {
        self.add_docs(tail, doc_lines(attrs, false));
    }

    fn index_fn(&mut self, attrs: &[syn::Attribute], name: &str) {
        let is_test = attrs.iter().any(|a| path_ends_in(a, "test"));
        let ignored = attrs.iter().any(|a| a.path().is_ident("ignore"));
        let mut segs: Vec<&str> = self.mods.iter().map(String::as_str).collect();
        segs.push(name);
        let path = segs.join("::");
        self.tests
            .entry((self.package.to_owned(), self.group.to_owned(), path))
            .or_insert(FnInfo { is_test, ignored });
    }
}

fn self_ty_name(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(p) => p
            .path
            .segments
            .last()
            .map_or_else(|| "_".to_owned(), |s| s.ident.to_string()),
        _ => "_".to_owned(),
    }
}

impl<'ast> Visit<'ast> for Collector<'_> {
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        let name = i.sig.ident.to_string();
        self.item_docs(&i.attrs, &[&name]);
        self.index_fn(&i.attrs, &name);
    }

    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        let name = i.ident.to_string();
        self.add_docs(&[&name], doc_lines(&i.attrs, false));
        self.add_docs(&[&name], doc_lines(&i.attrs, true));
        if let Some((_, items)) = &i.content {
            self.mods.push(name);
            for item in items {
                self.visit_item(item);
            }
            self.mods.pop();
        }
    }

    fn visit_item_struct(&mut self, i: &'ast syn::ItemStruct) {
        let name = i.ident.to_string();
        self.declare(&i.ident);
        self.item_docs(&i.attrs, &[&name]);
        for field in &i.fields {
            if let Some(id) = &field.ident {
                self.item_docs(&field.attrs, &[&name, &id.to_string()]);
            }
        }
    }

    fn visit_item_union(&mut self, i: &'ast syn::ItemUnion) {
        let name = i.ident.to_string();
        self.declare(&i.ident);
        self.item_docs(&i.attrs, &[&name]);
        for field in &i.fields.named {
            if let Some(id) = &field.ident {
                self.item_docs(&field.attrs, &[&name, &id.to_string()]);
            }
        }
    }

    fn visit_item_enum(&mut self, i: &'ast syn::ItemEnum) {
        let name = i.ident.to_string();
        self.item_docs(&i.attrs, &[&name]);
        self.declare(&i.ident);
        for variant in &i.variants {
            self.declare(&variant.ident);
            self.item_docs(&variant.attrs, &[&name, &variant.ident.to_string()]);
        }
    }

    fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
        let name = i.ident.to_string();
        self.declare(&i.ident);
        self.item_docs(&i.attrs, &[&name]);
        for item in &i.items {
            let (attrs, id) = match item {
                syn::TraitItem::Fn(f) => (&f.attrs, f.sig.ident.to_string()),
                syn::TraitItem::Const(c) => (&c.attrs, c.ident.to_string()),
                syn::TraitItem::Type(t) => (&t.attrs, t.ident.to_string()),
                _ => continue,
            };
            self.item_docs(attrs, &[&name, &id]);
        }
    }

    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        let ty = self_ty_name(&i.self_ty);
        let head = match &i.trait_ {
            Some((_, path, _)) => {
                let tr = path
                    .segments
                    .last()
                    .map_or_else(|| "_".to_owned(), |s| s.ident.to_string());
                format!("impl {tr} for {ty}")
            }
            None => format!("impl {ty}"),
        };
        self.item_docs(&i.attrs, &[&head]);
        for item in &i.items {
            let (attrs, id) = match item {
                syn::ImplItem::Fn(f) => (&f.attrs, f.sig.ident.to_string()),
                syn::ImplItem::Const(c) => (&c.attrs, c.ident.to_string()),
                syn::ImplItem::Type(t) => (&t.attrs, t.ident.to_string()),
                _ => continue,
            };
            self.item_docs(attrs, &[&ty, &id]);
        }
    }

    fn visit_item_const(&mut self, i: &'ast syn::ItemConst) {
        self.item_docs(&i.attrs, &[&i.ident.to_string()]);
    }

    fn visit_item_static(&mut self, i: &'ast syn::ItemStatic) {
        self.item_docs(&i.attrs, &[&i.ident.to_string()]);
    }

    fn visit_item_type(&mut self, i: &'ast syn::ItemType) {
        self.declare(&i.ident);
        self.item_docs(&i.attrs, &[&i.ident.to_string()]);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Plain,
    Invariant,
    Free,
}

struct Clause {
    id: String,
    kind: Kind,
    /// Text after the `:` and the indented lines up to the first `tested:`/`untested:`.
    text: String,
    tested: Vec<String>,
    untested: Vec<String>,
}

/// `C<n>: rest` or `C<n> [<kind>]: rest` with n in `[1-9][0-9]*`: the id (`C<n>`), the kind word
/// if there is a bracket, and the text after the `:`.
fn parse_id(s: &str) -> Option<(String, Option<String>, &str)> {
    let rest = s.strip_prefix('C')?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || digits.starts_with('0') {
        return None;
    }
    let after = &rest[digits.len()..];
    let id = format!("C{digits}");
    if let Some(text) = after.strip_prefix(':') {
        return Some((id, None, text));
    }
    let bracket = after.strip_prefix(" [")?;
    let (word, tail) = bracket.split_once(']')?;
    if word.contains(char::is_whitespace) {
        return None;
    }
    let text = tail.strip_prefix(':')?;
    Some((id, Some(word.to_owned()), text))
}

/// Parses the `# Contract` sections of one doc block. Problems are `(clause id, what)`.
fn parse_block(lines: &[String]) -> (Vec<Clause>, Vec<(Option<String>, String)>) {
    let mut clauses: Vec<Clause> = Vec::new();
    let mut problems = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut in_fence = false;
    let mut in_section = false;
    let mut open = false; // the last element of `clauses` takes tested:/untested: lines
    let mut in_text = false; // the last clause has seen no tested:/untested: line yet
    for line in lines {
        let t = line.trim();
        if t.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if t.starts_with('#') {
            in_section = t == "# Contract";
            open = false;
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some(rest) = line.strip_prefix("- ") {
            match parse_id(rest) {
                Some((id, word, text)) => {
                    if !seen.insert(id.clone()) {
                        problems.push((Some(id.clone()), "duplicate clause id".to_owned()));
                    }
                    let kind = match word.as_deref() {
                        None => Kind::Plain,
                        Some("invariant") => Kind::Invariant,
                        Some("free") => Kind::Free,
                        Some(other) => {
                            problems.push((
                                Some(id.clone()),
                                format!("unknown clause kind `{other}` (known: invariant, free)"),
                            ));
                            Kind::Plain
                        }
                    };
                    clauses.push(Clause {
                        id,
                        kind,
                        text: text.trim().to_owned(),
                        tested: Vec::new(),
                        untested: Vec::new(),
                    });
                    open = true;
                    in_text = true;
                }
                None => {
                    problems.push((None, format!("malformed clause line `{line}`")));
                    open = false;
                }
            }
            continue;
        }
        let (is_tested, value) = if let Some(v) = t.strip_prefix("tested:") {
            (true, v)
        } else if let Some(v) = t.strip_prefix("untested:") {
            (false, v)
        } else {
            if open && in_text && line.starts_with(char::is_whitespace) && !t.is_empty() {
                let c = clauses.last_mut().expect("open implies a clause");
                if !c.text.is_empty() {
                    c.text.push(' ');
                }
                c.text.push_str(t);
            }
            continue;
        };
        in_text = false;
        let value = value.trim().trim_matches('`').trim().to_owned();
        match clauses.last_mut().filter(|_| open) {
            Some(c) if is_tested => c.tested.push(value),
            Some(c) => c.untested.push(value),
            None => problems.push((
                None,
                format!(
                    "`{}:` line before any clause",
                    if is_tested { "tested" } else { "untested" }
                ),
            )),
        }
    }
    (clauses, problems)
}

fn is_camel(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_uppercase()) && chars.all(|c| c.is_ascii_alphanumeric())
}

/// A glossary entry: term, definition, included names.
struct Entry {
    term: String,
    includes: Vec<String>,
}

/// The `# Glossary` sections of one doc block: whether there was one, the entries, the problems.
fn parse_glossary(lines: &[String]) -> (bool, Vec<Entry>, Vec<String>) {
    let mut found = false;
    let mut problems = Vec::new();
    let mut raw: Vec<(String, String)> = Vec::new(); // (first line, joined text after the term's ':')
    let mut in_fence = false;
    let mut in_section = false;
    let mut open = false;
    for line in lines {
        let t = line.trim();
        if t.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if t.starts_with('#') {
            in_section = t == "# Glossary";
            found |= in_section;
            open = false;
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some(rest) = line.strip_prefix("- ") {
            raw.push((line.clone(), rest.to_owned()));
            open = true;
        } else if open && line.starts_with(char::is_whitespace) && !t.is_empty() {
            if let Some((_, joined)) = raw.last_mut() {
                joined.push(' ');
                joined.push_str(t);
            }
        } else {
            open = false;
        }
    }
    let mut entries = Vec::new();
    for (first, joined) in raw {
        let Some((term, text)) = joined.split_once(':') else {
            problems.push(format!("malformed glossary entry `{first}`"));
            continue;
        };
        if !is_camel(term) {
            problems.push(format!("malformed glossary entry `{first}`"));
            continue;
        }
        let (definition, list) = match text.rsplit_once("includes:") {
            Some((d, l)) => (d, Some(l)),
            None => (text, None),
        };
        if definition.trim().is_empty() {
            problems.push(format!("glossary term `{term}` has an empty definition"));
            continue;
        }
        let mut includes = Vec::new();
        for name in list.into_iter().flat_map(|l| l.split(',')) {
            let name = name.trim().trim_matches('`');
            if is_camel(name) {
                includes.push(name.to_owned());
            } else {
                problems.push(format!(
                    "glossary term `{term}`: bad name `{name}` in includes"
                ));
            }
        }
        entries.push(Entry {
            term: term.to_owned(),
            includes,
        });
    }
    (found, entries, problems)
}

/// Backquoted spans of a clause text that are one CamelCase identifier.
fn camel_spans(text: &str) -> impl Iterator<Item = &str> {
    text.split('`')
        .skip(1)
        .step_by(2)
        .filter(|span| is_camel(span))
}

/// One clause with where it lives, kept for the workspace-wide passes.
struct Fact {
    package: String,
    rel_path: String,
    owner: String,
    id: String,
    text: String,
}

struct Index<'a> {
    tests: &'a HashMap<TestKey, FnInfo>,
    packages: &'a HashSet<String>,
}

impl Index<'_> {
    fn resolve(&self, own_package: &str, reference: &str) -> Result<(), String> {
        let tokens: Vec<&str> = reference.split_whitespace().collect();
        let Some((path, prefix)) = tokens.split_last() else {
            return Err("empty ref".to_owned());
        };
        let (package, target) = match prefix {
            [] => (own_package, None),
            [x] if x.ends_with(".rs") => (own_package, Some(*x)),
            [p] => (*p, None),
            [p, f] if f.ends_with(".rs") => (*p, Some(*f)),
            _ => return Err("malformed ref".to_owned()),
        };
        if !self.packages.contains(package) {
            return Err(format!("unknown package `{package}`"));
        }
        let group = match target {
            None | Some("src/lib.rs") | Some("src/main.rs") => "src",
            Some(f) => f,
        };
        match self
            .tests
            .get(&(package.to_owned(), group.to_owned(), (*path).to_owned()))
        {
            None => Err("no such test".to_owned()),
            Some(info) if !info.is_test => Err("not a #[test]".to_owned()),
            Some(info) if info.ignored => Err("ignored test cannot support a clause".to_owned()),
            Some(_) => Ok(()),
        }
    }
}

/// Parses every source with syn, indexes the tests, parses the `# Contract` sections, resolves refs.
/// A file syn cannot parse is a problem (not a panic).
fn check_sources(sources: &[Source]) -> Report {
    check_sources_with_stats(sources).0
}

fn check_sources_with_stats(sources: &[Source]) -> (Report, Stats) {
    let mut report = Report::default();
    let mut stats = Stats::default();
    let mut tests: HashMap<TestKey, FnInfo> = HashMap::new();
    let mut blocks: Vec<DocBlock> = Vec::new();
    let mut declared: HashMap<(String, String), HashSet<String>> = HashMap::new();
    let packages: HashSet<String> = sources.iter().map(|s| s.package.clone()).collect();

    for source in sources {
        let Some((group, mods)) = locate(&source.rel_path) else {
            continue;
        };
        stats.files += 1;
        let file = match syn::parse_file(&source.text) {
            Ok(f) => f,
            Err(e) => {
                report.problems.push(format!(
                    "{}/{}: parse error: {e}",
                    source.package, source.rel_path
                ));
                continue;
            }
        };
        let mut names: HashSet<String> = HashSet::new();
        let mut collector = Collector {
            package: &source.package,
            group: &group,
            rel_path: &source.rel_path,
            mods,
            tests: &mut tests,
            blocks: &mut blocks,
            declared: &mut names,
        };
        collector.add_docs(&[], doc_lines(&file.attrs, true));
        for item in &file.items {
            collector.visit_item(item);
        }
        declared.insert((source.package.clone(), source.rel_path.clone()), names);
    }
    stats.tests = tests.values().filter(|i| i.is_test).count();

    let index = Index {
        tests: &tests,
        packages: &packages,
    };
    let mut facts: Vec<Fact> = Vec::new();
    let mut glossary: HashSet<String> = HashSet::new();
    let mut seen_terms: HashSet<String> = HashSet::new();
    for block in &blocks {
        let at = format!("{}/{}", block.package, block.rel_path);
        let (found, entries, gloss_problems) = parse_glossary(&block.lines);
        if found {
            if (block.package.as_str(), block.rel_path.as_str()) == GLOSSARY_AT
                && block.owner == "domain"
            {
                stats.terms += entries.len();
                for what in gloss_problems {
                    report
                        .problems
                        .push(format!("{at}: {}: {what}", block.owner));
                }
                for entry in entries {
                    if !seen_terms.insert(entry.term.clone()) {
                        report.problems.push(format!(
                            "{at}: {}: duplicate glossary term `{}`",
                            block.owner, entry.term
                        ));
                    }
                    glossary.insert(entry.term);
                    glossary.extend(entry.includes);
                }
            } else {
                report.problems.push(format!(
                    "{at}: {}: `# Glossary` belongs in {} {} only",
                    block.owner, GLOSSARY_AT.0, GLOSSARY_AT.1
                ));
            }
        }
        let (clauses, parse_problems) = parse_block(&block.lines);
        for (id, what) in parse_problems {
            report.problems.push(match id {
                Some(id) => format!("{at}: {}#{id}: {what}", block.owner),
                None => format!("{at}: {}: {what}", block.owner),
            });
        }
        for clause in &clauses {
            let name = format!("{}#{}", block.owner, clause.id);
            let mut problem = |what: String| report.problems.push(format!("{at}: {name}: {what}"));
            if clause.text.is_empty() {
                problem("clause has no text".to_owned());
            }
            facts.push(Fact {
                package: block.package.clone(),
                rel_path: block.rel_path.clone(),
                owner: block.owner.clone(),
                id: clause.id.clone(),
                text: clause.text.clone(),
            });
            if clause.kind == Kind::Free {
                stats.free += 1;
                if !clause.tested.is_empty() || !clause.untested.is_empty() {
                    problem("a `[free]` clause takes no `tested:` or `untested:` line".to_owned());
                }
                continue;
            }
            match (clause.tested.is_empty(), clause.untested.len()) {
                (false, 1..) => problem("has both `tested:` and `untested:`".to_owned()),
                (true, 0) => problem("needs a `tested:` line or one `untested:` line".to_owned()),
                (_, 2..) => problem("has more than one `untested:` line".to_owned()),
                _ => {}
            }
            if let [reason] = clause.untested.as_slice() {
                if reason.is_empty() {
                    problem("`untested:` needs a reason".to_owned());
                } else if clause.tested.is_empty() {
                    report
                        .untested
                        .push(format!("{}::{name}: {reason}", block.package));
                }
            }
            for reference in &clause.tested {
                if let Err(what) = index.resolve(&block.package, reference) {
                    problem(format!("tested: {reference}: {what}"));
                }
            }
        }
    }

    // L2: equal normalised text.
    let mut by_text: HashMap<String, Vec<&Fact>> = HashMap::new();
    for fact in &facts {
        let norm = fact
            .text
            .to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if !norm.is_empty() {
            by_text.entry(norm).or_default().push(fact);
        }
    }
    for group in by_text.values_mut().filter(|g| g.len() > 1) {
        group.sort_by_key(|f| (&f.package, &f.rel_path, &f.owner, &f.id));
        let first = group[0];
        for f in &group[1..] {
            report.problems.push(format!(
                "{}/{}: {}#{}: same text as {}::{}#{}",
                f.package, f.rel_path, f.owner, f.id, first.package, first.owner, first.id
            ));
        }
    }

    // L1: CamelCase words of clauses that are neither glossary, local nor std.
    let mut gaps: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let no_names = HashSet::new();
    for fact in &facts {
        let local = declared
            .get(&(fact.package.clone(), fact.rel_path.clone()))
            .unwrap_or(&no_names);
        for word in camel_spans(&fact.text) {
            if !glossary.contains(word) && !local.contains(word) && !STD_NAMES.contains(&word) {
                gaps.insert(format!(
                    "{}::{}#{}: `{word}`",
                    fact.package, fact.owner, fact.id
                ));
            }
        }
    }
    report.glossary_gaps = gaps.into_iter().collect();

    report.problems.sort();
    report.untested.sort();
    (report, stats)
}

fn walk(pkg_dir: &Path, dir: &Path, package: &str, out: &mut Vec<Source>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            walk(pkg_dir, &path, package, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let Ok(rel) = path.strip_prefix(pkg_dir) else {
                continue;
            };
            let rel_path = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            out.push(Source {
                package: package.to_owned(),
                rel_path,
                text,
            });
        }
    }
}

/// Reads `crates/*/src/**/*.rs` and `crates/*/tests/**/*.rs` under the workspace root
/// (`CARGO_MANIFEST_DIR/../..`), package = the directory name under `crates/`, skipping build output.
fn workspace_sources() -> Vec<Source> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut out = Vec::new();
    let crates = fs::read_dir(root.join("crates")).expect("crates/ directory");
    for entry in crates.flatten() {
        let pkg_dir = entry.path();
        if !pkg_dir.is_dir() {
            continue;
        }
        let package = entry.file_name().to_string_lossy().into_owned();
        for sub in ["src", "tests"] {
            walk(&pkg_dir, &pkg_dir.join(sub), &package, &mut out);
        }
    }
    out.sort_by(|a, b| (&a.package, &a.rel_path).cmp(&(&b.package, &b.rel_path)));
    out
}

#[test]
#[allow(clippy::absurd_extreme_comparisons)] // the ceiling is 0 when no glossary gap remains; the check stays so a gap cannot return
fn workspace_doc_contracts_hold() {
    let started = Instant::now();
    let sources = workspace_sources();
    let (report, stats) = check_sources_with_stats(&sources);
    eprintln!(
        "doc contracts: {} files scanned, {} tests indexed, {} untested clauses, {} free clauses, {} glossary terms, {} glossary gaps, {:?}",
        stats.files,
        stats.tests,
        report.untested.len(),
        stats.free,
        stats.terms,
        report.glossary_gaps.len(),
        started.elapsed()
    );
    assert!(
        report.problems.is_empty(),
        "doc contract problems:\n{}",
        report.problems.join("\n")
    );
    // Only goes down, like the oracle floors go only up.
    assert!(
        report.untested.len() <= UNTESTED_CEILING,
        "new untested clauses (ceiling {UNTESTED_CEILING}):\n{}",
        report.untested.join("\n")
    );
    assert!(
        report.untested.len() >= UNTESTED_CEILING,
        "untested clauses went down to {}: lower UNTESTED_CEILING to that",
        report.untested.len()
    );
    assert!(
        report.glossary_gaps.len() <= GLOSSARY_GAP_CEILING,
        "new glossary gaps (ceiling {GLOSSARY_GAP_CEILING}); define the word in the glossary of {} {} or declare it in the clause's file:\n{}",
        GLOSSARY_AT.0,
        GLOSSARY_AT.1,
        report.glossary_gaps.join("\n")
    );
    assert!(
        report.glossary_gaps.len() >= GLOSSARY_GAP_CEILING,
        "glossary gaps went down to {}: lower GLOSSARY_GAP_CEILING to that",
        report.glossary_gaps.len()
    );
}

// ---- fixtures: in-memory sources, no disk ----

fn src(package: &str, rel_path: &str, text: &str) -> Source {
    Source {
        package: package.to_owned(),
        rel_path: rel_path.to_owned(),
        text: text.to_owned(),
    }
}

fn doc_fn(refs: &str) -> String {
    format!("/// Doc.\n///\n/// # Contract\n/// - C1: promise.\n{refs}\npub fn f() {{}}\n")
}

#[test]
fn missing_test_is_a_problem() {
    let lib = doc_fn("///   tested: tests::nope");
    let r = check_sources(&[src("p", "src/lib.rs", &lib)]);
    assert_eq!(r.problems.len(), 1, "{:?}", r.problems);
    assert!(r.problems[0].contains("nope"), "{:?}", r.problems);
    assert!(r.problems[0].contains("no such test"), "{:?}", r.problems);
}

#[test]
fn existing_unit_test_resolves() {
    let lib = doc_fn("///   tested: m::tests::t");
    let m = "#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n";
    let r = check_sources(&[src("p", "src/lib.rs", &lib), src("p", "src/m.rs", m)]);
    assert!(r.problems.is_empty(), "{:?}", r.problems);
    // also with backticks and an explicit lib.rs target
    let lib = doc_fn("///   tested: `src/lib.rs m::tests::t`");
    let m = "#[cfg(test)]\nmod tests {\n    #[tokio::test]\n    async fn t() {}\n}\n";
    let r = check_sources(&[src("p", "src/lib.rs", &lib), src("p", "src/m.rs", m)]);
    assert!(r.problems.is_empty(), "{:?}", r.problems);
}

#[test]
fn integration_and_cross_package_refs_resolve() {
    let lib = doc_fn(
        "///   tested: tests/x.rs inner::t\n\
         ///   tested: q tests/y.rs top\n\
         ///   tested: q a::b::t",
    );
    let x = "mod inner {\n    #[test]\n    fn t() {}\n}\n";
    let y = "#[test]\nfn top() {}\n";
    let q = "pub mod b {\n    #[test]\n    fn t() {}\n}\n";
    let sources = [
        src("p", "src/lib.rs", &lib),
        src("p", "tests/x.rs", x),
        src("q", "src/lib.rs", ""),
        src("q", "tests/y.rs", y),
        src("q", "src/a/mod.rs", q),
    ];
    let r = check_sources(&sources);
    assert!(r.problems.is_empty(), "{:?}", r.problems);

    let bad = doc_fn("///   tested: zzz a::b::t\n///   tested: q tests/x.rs inner::t");
    let r = check_sources(&[
        src("p", "src/lib.rs", &bad),
        src("q", "src/a/mod.rs", q),
        src("p", "tests/x.rs", x),
    ]);
    assert_eq!(r.problems.len(), 2, "{:?}", r.problems);
    assert!(
        r.problems
            .iter()
            .any(|p| p.contains("unknown package `zzz`"))
    );
    assert!(r.problems.iter().any(|p| p.contains("no such test")));
}

#[test]
fn string_literal_test_is_not_indexed() {
    let lib = doc_fn("///   tested: tests::ghost");
    let other = r##"
pub const SRC: &str = r#"
#[cfg(test)]
mod tests {
    #[test]
    fn ghost() {}
}
"#;
"##;
    let r = check_sources(&[
        src("p", "src/lib.rs", &lib),
        src("p", "src/lib_other.rs", other),
    ]);
    assert_eq!(r.problems.len(), 1, "{:?}", r.problems);
    assert!(r.problems[0].contains("ghost"));
}

#[test]
fn ignored_and_non_test_fns_are_problems() {
    let lib = doc_fn("///   tested: m::skipped\n///   tested: m::helper");
    let m = "#[test]\n#[ignore]\nfn skipped() {}\nfn helper() {}\n";
    let r = check_sources(&[src("p", "src/lib.rs", &lib), src("p", "src/m.rs", m)]);
    assert_eq!(r.problems.len(), 2, "{:?}", r.problems);
    assert!(
        r.problems
            .iter()
            .any(|p| p.contains("ignored test cannot support a clause"))
    );
    assert!(r.problems.iter().any(|p| p.contains("not a #[test]")));
}

#[test]
fn clause_without_tested_or_untested_is_a_problem() {
    let lib = "/// # Contract\n/// - C1: nothing named.\n/// - C2: both.\n///   tested: x\n///   untested: why\n/// - C3: empty reason.\n///   untested:\npub fn f() {}\n";
    let r = check_sources(&[src("p", "src/lib.rs", lib)]);
    assert!(
        r.problems
            .iter()
            .any(|p| p.contains("f#C1") && p.contains("needs a"))
    );
    assert!(
        r.problems
            .iter()
            .any(|p| p.contains("f#C2") && p.contains("both"))
    );
    assert!(
        r.problems
            .iter()
            .any(|p| p.contains("f#C3") && p.contains("needs a reason"))
    );
    assert!(r.untested.is_empty(), "{:?}", r.untested);
}

#[test]
fn untested_is_counted_not_a_problem() {
    let lib = "/// # Contract\n/// - C1: later.\n///   untested: owed by the backfill\nfn f() {}\n";
    let r = check_sources(&[src("p", "src/lib.rs", lib)]);
    assert!(r.problems.is_empty(), "{:?}", r.problems);
    assert_eq!(r.untested, vec!["p::f#C1: owed by the backfill".to_owned()]);
}

#[test]
fn malformed_and_duplicate_ids_are_problems() {
    let lib = "/// # Contract\n/// ///\n/// tested: early\n/// - no id here\n/// - C0: zero.\n/// - C1: ok.\n///   untested: r\n/// - C1: again.\n///   untested: r\nfn f() {}\n";
    let r = check_sources(&[src("p", "src/lib.rs", lib)]);
    assert!(
        r.problems
            .iter()
            .any(|p| p.contains("`tested:` line before any clause"))
    );
    assert!(
        r.problems
            .iter()
            .any(|p| p.contains("malformed clause line `- no id here`"))
    );
    assert!(
        r.problems
            .iter()
            .any(|p| p.contains("malformed clause line `- C0: zero.`"))
    );
    assert!(
        r.problems
            .iter()
            .any(|p| p.contains("f#C1: duplicate clause id"))
    );
}

#[test]
fn headings_and_fences_bound_the_section() {
    let lib = "/// # Contract\n/// - C1: a.\n///   untested: r\n///\n/// # Notes\n/// - C2: not a clause.\n///\n/// ```text\n/// # Contract\n/// - C9: in a fence.\n/// ```\nfn f() {}\n";
    let r = check_sources(&[src("p", "src/lib.rs", lib)]);
    assert!(r.problems.is_empty(), "{:?}", r.problems);
    assert_eq!(r.untested.len(), 1, "{:?}", r.untested);
}

#[test]
fn unparsable_file_is_a_problem_not_a_panic() {
    let r = check_sources(&[src("p", "src/lib.rs", "fn (")]);
    assert_eq!(r.problems.len(), 1, "{:?}", r.problems);
    assert!(r.problems[0].contains("parse error"));
}

#[test]
fn module_doc_and_method_owners() {
    let a = "//! # Contract\n//! - C1: module promise.\n//!   untested: r\n\n\
             pub struct T {\n    /// # Contract\n    /// - C1: field promise.\n    ///   untested: r\n    pub x: u8,\n}\n\n\
             impl T {\n    /// # Contract\n    /// - C1: method promise.\n    ///   untested: r\n    pub fn m(&self) {}\n}\n";
    let r = check_sources(&[src("p", "src/a.rs", a), src("p", "src/lib.rs", "")]);
    assert!(r.problems.is_empty(), "{:?}", r.problems);
    assert_eq!(
        r.untested,
        vec![
            "p::a#C1: r".to_owned(),
            "p::a::T::m#C1: r".to_owned(),
            "p::a::T::x#C1: r".to_owned(),
        ]
    );
}

fn clause_fn(clauses: &str) -> String {
    format!("/// # Contract\n{clauses}\npub fn f() {{}}\n")
}

#[test]
fn unknown_clause_kind_is_a_problem() {
    let m = "#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n";
    let lib = clause_fn("/// - C1 [maybe]: x.\n///   tested: m::tests::t");
    let r = check_sources(&[src("p", "src/lib.rs", &lib), src("p", "src/m.rs", m)]);
    assert_eq!(r.problems.len(), 1, "{:?}", r.problems);
    assert!(
        r.problems[0].contains("unknown clause kind `maybe`"),
        "{:?}",
        r.problems
    );

    let lib =
        clause_fn("/// - C1 [invariant]: x.\n///   untested: r\n/// - C2 [free]: order of y.");
    let r = check_sources(&[src("p", "src/lib.rs", &lib)]);
    assert!(r.problems.is_empty(), "{:?}", r.problems);
    assert_eq!(r.untested.len(), 1, "{:?}", r.untested);

    let lib = clause_fn("/// - C3 [free]: z.\n///   untested: r");
    let r = check_sources(&[src("p", "src/lib.rs", &lib)]);
    assert_eq!(r.problems.len(), 1, "{:?}", r.problems);
    assert!(
        r.problems[0].contains("`[free]` clause takes no"),
        "{:?}",
        r.problems
    );
    assert!(r.untested.is_empty(), "{:?}", r.untested);

    // Other spellings: unknown words are problems, bad shapes are malformed lines.
    for bad in ["[pre]", "[Free]", "[]"] {
        let lib = clause_fn(&format!("/// - C1 {bad}: x.\n///   untested: r"));
        let r = check_sources(&[src("p", "src/lib.rs", &lib)]);
        assert_eq!(r.problems.len(), 1, "{bad}: {:?}", r.problems);
        assert!(r.problems[0].contains("unknown clause kind"), "{bad}");
    }
    for bad in ["C1[free]: x.", "C1 [free] : x.", "C1 [ free ]: x."] {
        let lib = clause_fn(&format!("/// - {bad}\n///   untested: r"));
        let r = check_sources(&[src("p", "src/lib.rs", &lib)]);
        assert!(
            r.problems
                .iter()
                .any(|p| p.contains("malformed clause line")),
            "{bad}: {:?}",
            r.problems
        );
    }

    // A clause with no text is a problem; text may continue on indented lines.
    let lib = clause_fn("/// - C1:\n///   untested: r");
    let r = check_sources(&[src("p", "src/lib.rs", &lib)]);
    assert_eq!(r.problems.len(), 1, "{:?}", r.problems);
    assert!(r.problems[0].contains("clause has no text"));
    let lib = clause_fn("/// - C1:\n///   continued.\n///   untested: r");
    let r = check_sources(&[src("p", "src/lib.rs", &lib)]);
    assert!(r.problems.is_empty(), "{:?}", r.problems);
}

fn gadget_doc(word_clause: &str) -> String {
    format!("/// # Contract\n/// - C1: {word_clause}\n///   untested: r\npub fn f() {{}}\n")
}

#[test]
fn glossary_gap_is_counted_not_a_problem() {
    let lib = gadget_doc("mentions `Widget`, `None`, `a::Gadget` and `OutOfRange{len}`.");
    let one = |extra: &[Source]| {
        let mut all = vec![src("p", "src/lib.rs", &lib)];
        all.extend(extra.iter().map(|s| src(&s.package, &s.rel_path, &s.text)));
        check_sources(&all)
    };
    let r = one(&[]);
    assert!(r.problems.is_empty(), "{:?}", r.problems);
    assert_eq!(r.glossary_gaps, vec!["p::f#C1: `Widget`".to_owned()]);

    // Green three ways: a glossary term, an includes name, a declaration in the same file.
    let term = src(
        "ryo-v2-core",
        "src/domain/mod.rs",
        "//! # Glossary\n//! - Widget: a thing.\n",
    );
    let r = one(std::slice::from_ref(&term));
    assert!(r.problems.is_empty() && r.glossary_gaps.is_empty(), "{r:?}");
    let incl = src(
        "ryo-v2-core",
        "src/domain/mod.rs",
        "//! # Glossary\n//! - Gadget: a thing.\n//!   includes: Widget\n",
    );
    let r = one(std::slice::from_ref(&incl));
    assert!(r.problems.is_empty() && r.glossary_gaps.is_empty(), "{r:?}");
    let own = format!("{lib}pub struct Widget;\n");
    let r = check_sources(&[src("p", "src/lib.rs", &own)]);
    assert!(r.problems.is_empty() && r.glossary_gaps.is_empty(), "{r:?}");

    // A variant of an inline module's enum counts as declared in the file; another file does not.
    let own = format!("{lib}mod m {{ pub enum E {{ Widget }} }}\n");
    let r = check_sources(&[src("p", "src/lib.rs", &own)]);
    assert!(r.glossary_gaps.is_empty(), "{r:?}");
    let r = check_sources(&[
        src("p", "src/lib.rs", &lib),
        src("p", "src/other.rs", "pub struct Widget;\n"),
    ]);
    assert_eq!(r.glossary_gaps.len(), 1, "{r:?}");
}

#[test]
fn glossary_format_problems() {
    let gloss = src(
        "ryo-v2-core",
        "src/domain/mod.rs",
        "//! # Glossary\n//! - lower: x.\n//! - A:\n//! - A: x.\n//! - A: y.\n//! - B: z.\n//!   includes: ok, Fine\n",
    );
    let stray = src(
        "p",
        "src/lib.rs",
        "/// # Glossary\n/// - C: x.\npub fn f() {}\n",
    );
    let r = check_sources(&[gloss, stray]);
    let msgs = r.problems.join("\n");
    assert_eq!(r.problems.len(), 5, "{msgs}");
    assert!(msgs.contains("malformed glossary entry `- lower: x.`"));
    assert!(msgs.contains("glossary term `A` has an empty definition"));
    assert!(msgs.contains("duplicate glossary term `A`"));
    assert!(msgs.contains("bad name `ok` in includes"));
    assert!(msgs.contains("`# Glossary` belongs in ryo-v2-core src/domain/mod.rs only"));

    // No glossary at all is an empty glossary, not a problem.
    let r = check_sources(&[src("p", "src/lib.rs", "")]);
    assert!(r.problems.is_empty());
}

#[test]
fn duplicate_clause_text_is_a_problem() {
    let two = |g_text: &str| {
        format!(
            "/// # Contract\n/// - C1: Same   Promise.\n///   untested: r\npub fn f() {{}}\n\
             /// # Contract\n/// - C1 [invariant]: {g_text}\n///   untested: r\npub fn g() {{}}\n"
        )
    };
    let r = check_sources(&[src("p", "src/lib.rs", &two("same promise."))]);
    assert_eq!(r.problems.len(), 1, "{:?}", r.problems);
    assert!(
        r.problems[0].contains("same text as p::f#C1"),
        "{:?}",
        r.problems
    );
    assert!(r.problems[0].contains("g#C1"), "{:?}", r.problems);
    let r = check_sources(&[src("p", "src/lib.rs", &two("other promise."))]);
    assert!(r.problems.is_empty(), "{:?}", r.problems);
}
