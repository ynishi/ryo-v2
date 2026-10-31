//! A name an item and a `use` both define in one module (E0255), which rust-analyzer 0.0.358
//! does not diagnose.
//!
//! rustc refuses a module in which an item and a non-glob `use` bind the same name in the same
//! namespace ("the name is defined multiple times", E0255) and reports it at the second of the
//! two in the file. ra lets the item shadow the import and reports nothing, so an edit that puts
//! an item into a module that still imports the name (a move-item that keeps the re-export,
//! CHECK_LEDGER "Gaps") passes the verifier and fails `cargo check`.
//!
//! Per module of the file (the file itself and each inline `mod m { .. }`), the names its items
//! define are matched with the names its `use` leaves bind, namespace by namespace. An item
//! defines a type (struct, enum, union, trait, type alias, module) or a value (fn, const,
//! static), and a tuple or unit struct both. A leaf binds the namespaces its path resolves in.
//!
//! Skipped, which loses recall and never precision: glob imports, `as _`, `extern crate`, the
//! macro namespace, a leaf whose path resolves in no namespace (rustc reports the import
//! instead), cfg-disabled items and uses, items that are the input of an attribute macro, items
//! made by macros, and items inside function bodies.

use crate::unresolved::{Hit, self_import_base};
use ra_ap_hir::{InFile, Semantics};
use ra_ap_ide::{TextRange, TextSize};
use ra_ap_ide_db::RootDatabase;
use ra_ap_syntax::SyntaxKind;
use ra_ap_syntax::ast::{self, AstNode, HasModuleItem, HasName, PathSegmentKind};
use ra_ap_vfs::FileId;
use ryo_v2_core::domain::{ErrorCode, FindingCode};
use std::collections::BTreeSet;

/// The namespaces a definition occupies.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Ns {
    types: bool,
    values: bool,
}

impl Ns {
    fn overlaps(self, other: Ns) -> bool {
        (self.types && other.types) || (self.values && other.values)
    }
}

/// A name defined in a module: the name, its namespaces and where its definition starts.
struct Def {
    name: String,
    ns: Ns,
    at: TextSize,
}

/// The E0255 conflicts of one file.
///
/// `inactive` holds the ranges ra marked as cfg-disabled; `line_of` maps an offset to its
/// 1-based line. An empty result also means "cannot tell" (the file is outside the module tree).
pub(crate) fn find(
    db: &RootDatabase,
    file_id: FileId,
    inactive: &[TextRange],
    line_of: &dyn Fn(TextSize) -> u32,
) -> Vec<Hit> {
    ra_ap_hir::attach_db(db, || {
        let sema = Semantics::new(db);
        if sema.file_to_module_def(file_id).is_none() {
            return Vec::new();
        }
        let file = sema.parse_guess_edition(file_id);
        let mut scopes: Vec<Vec<ast::Item>> = vec![file.items().collect()];
        for m in file.syntax().descendants().filter_map(ast::Module::cast) {
            if in_body_or_macro(m.syntax()) || is_inactive(m.syntax(), inactive) {
                continue;
            }
            if let Some(list) = m.item_list() {
                scopes.push(list.items().collect());
            }
        }
        let mut seen: BTreeSet<(u32, String)> = BTreeSet::new();
        let mut hits = Vec::new();
        for items in &scopes {
            for (name, line) in conflicts(&sema, items, inactive, line_of) {
                if seen.insert((line, name.clone())) {
                    let Ok(code) = ErrorCode::parse("E0255") else {
                        continue;
                    };
                    hits.push((
                        FindingCode::Rustc(code),
                        format!("the name `{name}` is defined multiple times"),
                        line,
                    ));
                }
            }
        }
        hits
    })
}

/// The `(name, line)` of each conflict among the items of one module.
fn conflicts(
    sema: &Semantics<'_, RootDatabase>,
    items: &[ast::Item],
    inactive: &[TextRange],
    line_of: &dyn Fn(TextSize) -> u32,
) -> Vec<(String, u32)> {
    let defs: Vec<Def> = items
        .iter()
        .filter(|i| !is_inactive(i.syntax(), inactive))
        .filter(|i| !is_attr_macro_input(sema, i))
        .filter_map(item_def)
        .collect();
    if defs.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for item in items {
        let ast::Item::Use(u) = item else { continue };
        if is_inactive(u.syntax(), inactive) {
            continue;
        }
        let Some(tree) = u.use_tree() else { continue };
        for (name, path, at) in leaves(&tree) {
            let clashing: Vec<&Def> = defs.iter().filter(|d| d.name == name).collect();
            if clashing.is_empty() {
                continue;
            }
            let ns = imported_ns(sema, &path);
            for d in clashing.into_iter().filter(|d| d.ns.overlaps(ns)) {
                // rustc reports the second of the two definitions in the file.
                out.push((name.clone(), line_of(d.at.max(at))));
            }
        }
    }
    out
}

/// The name, namespaces and head offset of an item that defines a name; `None` for the rest.
fn item_def(item: &ast::Item) -> Option<Def> {
    let types = Ns {
        types: true,
        values: false,
    };
    let values = Ns {
        types: false,
        values: true,
    };
    let (name, ns) = match item {
        ast::Item::Struct(s) => {
            let record = matches!(s.field_list(), Some(ast::FieldList::RecordFieldList(_)));
            (
                s.name()?,
                Ns {
                    types: true,
                    values: !record,
                },
            )
        }
        ast::Item::Enum(e) => (e.name()?, types),
        ast::Item::Union(u) => (u.name()?, types),
        ast::Item::Trait(t) => (t.name()?, types),
        ast::Item::TypeAlias(t) => (t.name()?, types),
        ast::Item::Module(m) => (m.name()?, types),
        ast::Item::Fn(f) => (f.name()?, values),
        ast::Item::Const(c) => (c.name()?, values),
        ast::Item::Static(s) => (s.name()?, values),
        _ => return None,
    };
    Some(Def {
        name: name.text().to_string(),
        ns,
        at: head(item.syntax()),
    })
}

/// Where rustc's span of an item starts: the first token after its attributes and comments.
pub(crate) fn head(node: &ra_ap_syntax::SyntaxNode) -> TextSize {
    node.children_with_tokens()
        .find(|e| {
            !matches!(
                e.kind(),
                SyntaxKind::ATTR | SyntaxKind::COMMENT | SyntaxKind::WHITESPACE
            )
        })
        .map_or_else(|| node.text_range().start(), |e| e.text_range().start())
}

/// The leaves of a use tree that bind a name: the name, the path to resolve and where the leaf
/// starts. Globs and `as _` bind nothing.
fn leaves(tree: &ast::UseTree) -> Vec<(String, ast::Path, TextSize)> {
    let mut out = Vec::new();
    for t in tree.syntax().descendants().filter_map(ast::UseTree::cast) {
        if t.use_tree_list().is_some() || t.star_token().is_some() {
            continue;
        }
        let Some(path) = t.path() else { continue };
        let at = t.syntax().text_range().start();
        let renamed = t.rename();
        if renamed
            .as_ref()
            .is_some_and(|r| r.underscore_token().is_some())
        {
            continue;
        }
        let Some(segment) = path.segment() else {
            continue;
        };
        // `use a::b::{self}` binds `b` and resolves as `a::b`.
        let (bound, target) = match segment.kind() {
            Some(PathSegmentKind::Name(n)) => (n.text().to_string(), path.clone()),
            Some(PathSegmentKind::SelfKw) => {
                let Some(base) = self_import_base(&t, &path) else {
                    continue;
                };
                let Some(PathSegmentKind::Name(n)) = base.segment().and_then(|s| s.kind()) else {
                    continue;
                };
                (n.text().to_string(), base)
            }
            _ => continue,
        };
        let name = match renamed.and_then(|r| r.name()) {
            Some(n) => n.text().to_string(),
            None => bound,
        };
        out.push((name, target, at));
    }
    out
}

/// The namespaces a `use` path resolves in (macros left out); empty when it resolves in none.
fn imported_ns(sema: &Semantics<'_, RootDatabase>, path: &ast::Path) -> Ns {
    let Some(per_ns) = sema.resolve_path_per_ns(path) else {
        return Ns::default();
    };
    Ns {
        types: per_ns.type_ns.is_some(),
        values: per_ns.value_ns.is_some(),
    }
}

/// Whether the item is the input of an attribute macro (its expansion is unknown).
fn is_attr_macro_input(sema: &Semantics<'_, RootDatabase>, item: &ast::Item) -> bool {
    sema.is_attr_macro_call(InFile::new(sema.hir_file_for(item.syntax()), item))
}

/// Whether the node starts inside a cfg-disabled range.
fn is_inactive(node: &ra_ap_syntax::SyntaxNode, inactive: &[TextRange]) -> bool {
    let start = node.text_range().start();
    inactive.iter().any(|r| r.contains(start))
}

/// Whether the node sits inside a function body, a block or a macro call: not a module scope
/// this detector reads.
fn in_body_or_macro(node: &ra_ap_syntax::SyntaxNode) -> bool {
    node.ancestors().skip(1).any(|a| {
        matches!(
            a.kind(),
            SyntaxKind::BLOCK_EXPR
                | SyntaxKind::MACRO_CALL
                | SyntaxKind::TOKEN_TREE
                | SyntaxKind::MACRO_RULES
                | SyntaxKind::MACRO_DEF
        )
    })
}
