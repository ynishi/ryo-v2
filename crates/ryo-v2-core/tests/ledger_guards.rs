//! Ledger guards: every name in the guard column of a ledger's decision table exists in the code,
//! so a decision cannot keep citing a test, a clause owner or a constant that was deleted
//! (`DEVELOPMENT.md`, "Where it is recorded": a row names the guard that fails when it is broken).
//! The companion of `doc_contract.rs`, which does the same for the `tested:` lines of contracts.
//!
//! Rules:
//!
//! 1. Tables: every Markdown table of a `*_LEDGER.md` file at the workspace root whose header has a
//!    column whose name starts with `guard`. Only that column is read; the other columns are
//!    history and measurement and may name what no longer exists.
//! 2. Names: each backquoted span of a guard cell whose last whitespace-separated token is a name,
//!    after at most two leading tokens that are a package (`ryo-v2-ra`) or a target file (`.rs`).
//!    A name is identifier segments joined by `::` or `.` (`a_test`, `app::search::tests::a_test`,
//!    `RaConfig.assert_revert_every`, `Forks::width`, `DEFAULT_SEARCH_BUDGET`). Any other span
//!    (`beam 20`, `--engine`, `Some(1)`, a path `vendor/`, a file name `UNSAFE.tsv`) is not a name
//!    and is not checked.
//! 3. A name exists when its last segment is declared in a source file of `crates/` (`src/` and
//!    `tests/`) or `vendor/` (fn, method, const, static, field, variant, type, trait, mod, macro).
//!    Only the last segment is resolved: the check catches a deleted or renamed item, not a guard
//!    cited under the wrong module (that is a review question).
//! 4. A name that is recorded as deleted is written with a leading `-` inside the span (`-repath`)
//!    and is not checked; it says in the row that the guard went with the code.

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use syn::visit::Visit;

/// Declared identifiers of every visited source.
#[derive(Default)]
struct Declared(HashSet<String>);

impl Declared {
    fn add(&mut self, ident: &syn::Ident) {
        self.0.insert(ident.to_string());
    }
}

impl<'ast> Visit<'ast> for Declared {
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        self.add(&i.sig.ident);
        syn::visit::visit_item_fn(self, i);
    }
    fn visit_impl_item_fn(&mut self, i: &'ast syn::ImplItemFn) {
        self.add(&i.sig.ident);
        syn::visit::visit_impl_item_fn(self, i);
    }
    fn visit_trait_item_fn(&mut self, i: &'ast syn::TraitItemFn) {
        self.add(&i.sig.ident);
        syn::visit::visit_trait_item_fn(self, i);
    }
    fn visit_item_const(&mut self, i: &'ast syn::ItemConst) {
        self.add(&i.ident);
        syn::visit::visit_item_const(self, i);
    }
    fn visit_impl_item_const(&mut self, i: &'ast syn::ImplItemConst) {
        self.add(&i.ident);
        syn::visit::visit_impl_item_const(self, i);
    }
    fn visit_trait_item_const(&mut self, i: &'ast syn::TraitItemConst) {
        self.add(&i.ident);
        syn::visit::visit_trait_item_const(self, i);
    }
    fn visit_item_static(&mut self, i: &'ast syn::ItemStatic) {
        self.add(&i.ident);
        syn::visit::visit_item_static(self, i);
    }
    fn visit_field(&mut self, i: &'ast syn::Field) {
        if let Some(id) = &i.ident {
            self.add(id);
        }
        syn::visit::visit_field(self, i);
    }
    fn visit_variant(&mut self, i: &'ast syn::Variant) {
        self.add(&i.ident);
        syn::visit::visit_variant(self, i);
    }
    fn visit_item_struct(&mut self, i: &'ast syn::ItemStruct) {
        self.add(&i.ident);
        syn::visit::visit_item_struct(self, i);
    }
    fn visit_item_enum(&mut self, i: &'ast syn::ItemEnum) {
        self.add(&i.ident);
        syn::visit::visit_item_enum(self, i);
    }
    fn visit_item_union(&mut self, i: &'ast syn::ItemUnion) {
        self.add(&i.ident);
        syn::visit::visit_item_union(self, i);
    }
    fn visit_item_trait(&mut self, i: &'ast syn::ItemTrait) {
        self.add(&i.ident);
        syn::visit::visit_item_trait(self, i);
    }
    fn visit_item_type(&mut self, i: &'ast syn::ItemType) {
        self.add(&i.ident);
        syn::visit::visit_item_type(self, i);
    }
    fn visit_impl_item_type(&mut self, i: &'ast syn::ImplItemType) {
        self.add(&i.ident);
        syn::visit::visit_impl_item_type(self, i);
    }
    fn visit_trait_item_type(&mut self, i: &'ast syn::TraitItemType) {
        self.add(&i.ident);
        syn::visit::visit_trait_item_type(self, i);
    }
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        self.add(&i.ident);
        syn::visit::visit_item_mod(self, i);
    }
    fn visit_item_macro(&mut self, i: &'ast syn::ItemMacro) {
        if let Some(id) = &i.ident {
            self.add(id);
        }
        syn::visit::visit_item_macro(self, i);
    }
}

/// The cells of one table row, split on `|` outside backquotes; `None` if the line is no row.
fn cells(line: &str) -> Option<Vec<String>> {
    let body = line.trim().strip_prefix('|')?.strip_suffix('|')?;
    let mut out = vec![String::new()];
    let mut in_code = false;
    for c in body.chars() {
        match c {
            '`' => {
                in_code = !in_code;
                out.last_mut().expect("one cell").push(c);
            }
            '|' if !in_code => out.push(String::new()),
            _ => out.last_mut().expect("one cell").push(c),
        }
    }
    Some(out.into_iter().map(|c| c.trim().to_owned()).collect())
}

/// One guard cell: (file, line number, cell text).
type GuardCell = (String, usize, String);

/// The guard cells of every table in `text` whose header has a column starting with `guard`.
fn guard_cells(file: &str, text: &str) -> Vec<GuardCell> {
    let mut out = Vec::new();
    let mut column: Option<usize> = None;
    let mut header_seen = false;
    for (n, line) in text.lines().enumerate() {
        let Some(row) = cells(line) else {
            column = None;
            header_seen = false;
            continue;
        };
        if !header_seen {
            header_seen = true;
            column = row.iter().position(|c| c.starts_with("guard"));
            continue;
        }
        let Some(col) = column else { continue };
        if row
            .iter()
            .all(|c| c.chars().all(|ch| matches!(ch, '-' | ':' | ' ')))
        {
            continue; // the delimiter row
        }
        if let Some(cell) = row.get(col) {
            out.push((file.to_owned(), n + 1, cell.clone()));
        }
    }
    out
}

/// A span ending in one of these after its last `.` is a file name, not a name (rule 2).
const FILE_EXTENSIONS: &[&str] = &["rs", "md", "toml", "tsv", "json", "txt", "sh", "py", "yml"];

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The last segment of the name a backquoted span cites, or `None` when the span is no name or is
/// marked deleted (rule 4).
fn cited(span: &str) -> Option<&str> {
    let tokens: Vec<&str> = span.split_whitespace().collect();
    let (last, prefix) = tokens.split_last()?;
    if prefix.len() > 2
        || !prefix
            .iter()
            .all(|t| t.ends_with(".rs") || t.starts_with("ryo-v2"))
    {
        return None;
    }
    if last.starts_with('-') {
        return None;
    }
    if last
        .rsplit_once('.')
        .is_some_and(|(_, ext)| FILE_EXTENSIONS.contains(&ext))
    {
        return None;
    }
    let segments: Vec<&str> = last.split("::").flat_map(|s| s.split('.')).collect();
    if segments.iter().all(|s| is_ident(s)) {
        segments.last().copied()
    } else {
        None
    }
}

/// Problems of the guard cells against the declared names, one line each, sorted.
fn check(cells: &[GuardCell], declared: &HashSet<String>) -> Vec<String> {
    let mut problems = Vec::new();
    for (file, line, cell) in cells {
        for span in cell.split('`').skip(1).step_by(2) {
            if let Some(name) = cited(span)
                && !declared.contains(name)
            {
                problems.push(format!(
                    "{file}:{line}: `{span}`: `{name}` is declared nowhere"
                ));
            }
        }
    }
    problems.sort();
    problems.dedup();
    problems
}

fn walk(dir: &Path, declared: &mut Declared) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            walk(&path, declared);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            // A file syn cannot parse (a compile-fail fixture) declares nothing.
            if let Ok(file) = syn::parse_file(&text) {
                declared.visit_file(&file);
            }
        }
    }
}

#[test]
fn ledger_guards_name_what_exists() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut declared = Declared::default();
    walk(&root.join("crates"), &mut declared);
    walk(&root.join("vendor"), &mut declared);
    let mut all = Vec::new();
    let mut ledgers = 0;
    for entry in fs::read_dir(&root).expect("workspace root").flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with("_LEDGER.md") {
            ledgers += 1;
            let text = fs::read_to_string(entry.path()).expect("ledger");
            all.extend(guard_cells(&name, &text));
        }
    }
    assert!(ledgers > 0, "no *_LEDGER.md at the workspace root");
    assert!(!all.is_empty(), "no guard column in any ledger");
    let problems = check(&all, &declared.0);
    assert!(
        problems.is_empty(),
        "ledger guards that name nothing in the code (mark a deleted one `-name`):\n{}",
        problems.join("\n")
    );
}

// ---- fixtures ----

fn names(list: &[&str]) -> HashSet<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

const TABLE: &str = "\
| id | decision | guard (test) | rejected |
|---|---|---|---|
| D1 | x | `a_test`, `tests/x.rs gone_test`, `ryo-v2-ra tests/y.rs b_test` | `not_checked` |
| D2 | y | `Type.field`, `Port::gone_method`, `-deleted`, `beam 20`, `Some(1)`, `a | b` | z |

| id | other |
|---|---|
| D3 | `elsewhere` |
";

#[test]
fn only_the_guard_column_is_read() {
    let cells = guard_cells("L.md", TABLE);
    assert_eq!(cells.len(), 2, "{cells:?}");
    assert_eq!(cells[0].1, 3);
    assert!(
        cells[1].2.contains("`a | b`"),
        "a `|` inside backquotes splits no cell"
    );
}

#[test]
fn a_name_declared_nowhere_is_a_problem() {
    let cells = guard_cells("L.md", TABLE);
    let problems = check(&cells, &names(&["a_test", "b_test", "field"]));
    assert_eq!(problems.len(), 2, "{problems:?}");
    assert!(problems[0].starts_with("L.md:3:"), "{problems:?}");
    assert!(problems[0].contains("`gone_test`"), "{problems:?}");
    assert!(problems[1].contains("`gone_method`"), "{problems:?}");
}

#[test]
fn spans_that_are_no_name_or_marked_deleted_are_skipped() {
    assert_eq!(cited("-repath"), None);
    assert_eq!(cited("beam 20"), None);
    assert_eq!(cited("--engine"), None);
    assert_eq!(cited("Some(1)"), None);
    assert_eq!(cited("a b c d"), None);
    assert_eq!(cited("UNSAFE.tsv"), None);
    assert_eq!(cited("vendor/"), None);
    assert_eq!(cited("missing_docs = \"warn\""), None);
    assert_eq!(cited("ryo-v2-ra tests/x.rs t"), Some("t"));
    assert_eq!(cited("app::search::tests::t"), Some("t"));
    assert_eq!(
        cited("RaConfig.assert_revert_every"),
        Some("assert_revert_every")
    );
}

#[test]
fn declarations_of_every_kind_are_collected() {
    let src = "mod m { #[test] fn t() {} }\n\
               pub struct S { pub f: u8 }\n\
               pub enum E { V }\n\
               pub trait T { fn m(&self); const K: u8; type A; }\n\
               impl S { fn n(&self) {} const L: u8 = 0; }\n\
               const C: u8 = 0; static G: u8 = 0; type Y = u8;\n\
               macro_rules! mac { () => {} }\n";
    let mut d = Declared::default();
    d.visit_file(&syn::parse_file(src).expect("parses"));
    for name in [
        "m", "t", "S", "f", "E", "V", "T", "K", "A", "n", "L", "C", "G", "Y", "mac",
    ] {
        assert!(d.0.contains(name), "{name} not collected");
    }
}
