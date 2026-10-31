//! Unresolved type names and path prefixes, which rust-analyzer 0.0.358 does not diagnose.
//!
//! ra reports neither a single-segment type name that resolves to nothing nor a first path
//! segment that resolves to nothing (`Missing` in `fn f(x: Missing)`, `fmt` in `fmt::Display`,
//! `Binn` in `Binn::new()`). rustc reports them as E0405 (trait position), E0425 (any other
//! type position) and E0433 (path prefix), so this module walks the syntax tree of a file,
//! asks the semantic layer to resolve each candidate and emits what stays unresolved with the
//! rustc code.
//!
//! A candidate is skipped whenever the answer cannot be trusted: inside attributes, macro
//! calls and macro definitions, in cfg-disabled code, in the input of an attribute macro, for
//! a name some `use` of the file introduces (rustc reports only the failed import), and in a
//! file with a glob import ra cannot resolve. Skipping loses recall, never precision.
//!
//! Residual false positives: a name a child module gets through `use super::*` from a parent
//! whose import failed, and items produced by `include!` when out dirs are not loaded. Both
//! exist in the baseline too, so the baseline difference cancels them unless an edit adds a
//! new use.

use ra_ap_hir::{InFile, Semantics};
use ra_ap_ide::{TextRange, TextSize};
use ra_ap_ide_db::RootDatabase;
use ra_ap_syntax::SyntaxKind;
use ra_ap_syntax::ast::{self, AstNode, HasName, PathSegmentKind};
use ra_ap_vfs::FileId;
use ryo_v2_core::domain::{ErrorCode, FindingCode};
use std::collections::HashSet;

/// One unresolved name: the rustc code, the message and the 1-based line.
pub(crate) type Hit = (FindingCode, String, u32);

/// Which diagnostic a candidate would raise.
#[derive(Clone, Copy)]
enum Kind {
    /// A single-segment name in a trait position: E0405.
    Trait,
    /// A single-segment name in any other type position: E0425.
    Type,
    /// The first segment of a multi-segment path: E0433.
    Prefix,
}

/// A name to resolve: its path and the `NameRef` of the segment that is reported.
struct Candidate {
    kind: Kind,
    path: ast::Path,
    name: ast::NameRef,
}

/// The unresolved type names and path prefixes of one file.
///
/// `inactive` holds the ranges ra marked as cfg-disabled; `line_of` maps an offset to its
/// 1-based line. An empty result also means "cannot tell" (the file is outside the module
/// tree, or has a glob import ra cannot resolve).
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
        if has_unresolved_glob(&sema, &file) {
            return Vec::new();
        }
        let imported = imported_names(&file);
        let mut hits = Vec::new();
        for c in candidates(&file) {
            let name = c.name.text().to_string();
            if skipped_syntactically(&c, &name, inactive, &imported) {
                continue;
            }
            if sema.resolve_path(&c.path).is_some() {
                continue;
            }
            if in_attr_macro_input(&sema, &c.path) || in_scope(&sema, &c.path, &name) {
                continue;
            }
            let Some((code, message)) = describe(c.kind, &name) else {
                continue;
            };
            hits.push((code, message, line_of(c.name.syntax().text_range().start())));
        }
        hits
    })
}

/// The rustc code and the message of a candidate; `None` when the code does not parse.
fn describe(kind: Kind, name: &str) -> Option<(FindingCode, String)> {
    let (code, message) = match kind {
        Kind::Trait => ("E0405", format!("cannot find trait `{name}` in this scope")),
        Kind::Type => ("E0425", format!("cannot find type `{name}` in this scope")),
        Kind::Prefix => (
            "E0433",
            format!("failed to resolve: use of undeclared crate, module or type `{name}`"),
        ),
    };
    Some((FindingCode::Rustc(ErrorCode::parse(code).ok()?), message))
}

/// Single-segment type paths and first segments of multi-segment paths, in document order.
fn candidates(file: &ast::SourceFile) -> Vec<Candidate> {
    let mut out = Vec::new();
    for node in file.syntax().descendants() {
        if let Some(ty) = ast::PathType::cast(node.clone()) {
            let Some(path) = ty.path() else { continue };
            if path.qualifier().is_some() {
                continue;
            }
            if let Some(PathSegmentKind::Name(name)) = path.segment().and_then(|s| s.kind()) {
                let kind = if is_trait_position(&ty) {
                    Kind::Trait
                } else {
                    Kind::Type
                };
                out.push(Candidate { kind, path, name });
            }
        } else if let Some(path) = ast::Path::cast(node)
            && path.qualifier().is_none()
            && path.parent_path().is_some()
            && let Some(PathSegmentKind::Name(name)) = path.segment().and_then(|s| s.kind())
        {
            out.push(Candidate {
                kind: Kind::Prefix,
                path,
                name,
            });
        }
    }
    out
}

/// Whether the type is a bound (`T: Bound`, `dyn Bound`) or the trait of an `impl Trait for`.
fn is_trait_position(ty: &ast::PathType) -> bool {
    let Some(parent) = ty.syntax().parent() else {
        return false;
    };
    if ast::TypeBound::can_cast(parent.kind()) {
        return true;
    }
    ast::Impl::cast(parent)
        .and_then(|i| i.trait_())
        .is_some_and(|t| t.syntax() == ty.syntax())
}

/// Checks that need no semantic query.
fn skipped_syntactically(
    c: &Candidate,
    name: &str,
    inactive: &[TextRange],
    imported: &HashSet<String>,
) -> bool {
    let opaque = c.path.syntax().ancestors().any(|a| {
        matches!(
            a.kind(),
            SyntaxKind::ATTR
                | SyntaxKind::TOKEN_TREE
                | SyntaxKind::MACRO_CALL
                | SyntaxKind::MACRO_RULES
                | SyntaxKind::MACRO_DEF
                | SyntaxKind::USE_TREE
                | SyntaxKind::VISIBILITY
                | SyntaxKind::EXTERN_CRATE
        )
    });
    let start = c.name.syntax().text_range().start();
    opaque || inactive.iter().any(|r| r.contains(start)) || imported.contains(name)
}

/// Whether an enclosing item is the input of an attribute macro (its expansion is unknown).
fn in_attr_macro_input(sema: &Semantics<'_, RootDatabase>, path: &ast::Path) -> bool {
    path.syntax()
        .ancestors()
        .filter_map(ast::Item::cast)
        .any(|item| sema.is_attr_macro_call(InFile::new(sema.hir_file_for(item.syntax()), &item)))
}

/// Whether the scope at the path holds `name` in any namespace (generics, preludes included).
/// A path without a scope counts as in scope: it cannot be told from an unanalysable one.
fn in_scope(sema: &Semantics<'_, RootDatabase>, path: &ast::Path, name: &str) -> bool {
    let Some(scope) = sema.scope(path.syntax()) else {
        return true;
    };
    let mut found = false;
    scope.process_all_names(&mut |n, _| {
        if n.as_str() == name {
            found = true;
        }
    });
    found
}

/// Whether a glob import of the file does not resolve; then any name may come from it.
fn has_unresolved_glob(sema: &Semantics<'_, RootDatabase>, file: &ast::SourceFile) -> bool {
    file.syntax()
        .descendants()
        .filter_map(ast::UseTree::cast)
        .filter(|t| t.star_token().is_some())
        .filter_map(|t| t.path())
        .filter(|p| !is_module_keyword(p))
        .any(|p| sema.resolve_path(&p).is_none())
}

/// Whether the path is only `super`, `self` or `crate`: a module of the crate itself, which
/// `resolve_path` does not resolve as a use-tree path (`use super::*`) and which always exists.
fn is_module_keyword(path: &ast::Path) -> bool {
    path.qualifier().is_none()
        && matches!(
            path.segment().and_then(|s| s.kind()),
            Some(PathSegmentKind::SuperKw | PathSegmentKind::SelfKw | PathSegmentKind::CrateKw)
        )
}

/// The names the `use` items of the file introduce. rustc binds a failed import to a dummy
/// and reports only the import, so the name is not reported again where it is used.
fn imported_names(file: &ast::SourceFile) -> HashSet<String> {
    let mut names = HashSet::new();
    for tree in file.syntax().descendants().filter_map(ast::UseTree::cast) {
        if tree.use_tree_list().is_some() || tree.star_token().is_some() {
            continue;
        }
        if let Some(rename) = tree.rename() {
            if let Some(name) = rename.name() {
                names.insert(name.text().to_string());
            }
            continue;
        }
        let Some(path) = tree.path() else { continue };
        let Some(segment) = path.segment() else {
            continue;
        };
        match segment.kind() {
            Some(PathSegmentKind::Name(n)) => {
                names.insert(n.text().to_string());
            }
            Some(PathSegmentKind::SelfKw) => {
                if let Some(base) = self_import_base(&tree, &path)
                    && let Some(PathSegmentKind::Name(n)) = base.segment().and_then(|s| s.kind())
                {
                    names.insert(n.text().to_string());
                }
            }
            _ => {}
        }
    }
    names
}

/// The path a `self` leaf imports: `a::b` in `use a::b::self` and in `use a::b::{self}`.
pub(crate) fn self_import_base(tree: &ast::UseTree, path: &ast::Path) -> Option<ast::Path> {
    path.qualifier().or_else(|| {
        tree.syntax()
            .ancestors()
            .skip(1)
            .find_map(ast::UseTree::cast)
            .and_then(|t| t.path())
    })
}
