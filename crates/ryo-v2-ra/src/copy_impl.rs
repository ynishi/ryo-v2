//! A `Copy` impl on a type with a field that is not `Copy` (E0204), which rust-analyzer 0.0.358
//! does not diagnose.
//!
//! rustc checks that every field of a type with a `Copy` impl is `Copy` and reports E0204 at the
//! head of the type for `#[derive(.., Copy)]` (rustc 1.98.1: the `struct` / `enum` line, not the
//! attribute) and at the trait of `impl Copy for T {}`. ra runs no
//! such check, so an edit that adds the derive to a type holding a `String` (the add-derive edit
//! kind proposes `Copy` wherever `Clone` is derived) passes the verifier and fails `cargo check`
//! (CHECK_LEDGER "Gaps").
//!
//! A field counts as not `Copy` when ra's trait solver says the field type does not implement
//! `core::marker::Copy`. Skipped, which loses recall and never precision: types with type or
//! const parameters (the derived impl adds a `Copy` bound per parameter, which the field types
//! are not checked under), fields whose type ra does not know, negative impls, cfg-disabled
//! code, and code inside macro calls.

use crate::duplicate::head;
use crate::unresolved::Hit;
use ra_ap_hir::{Adt, Semantics};
use ra_ap_ide::{TextRange, TextSize};
use ra_ap_ide_db::RootDatabase;
use ra_ap_ide_db::famous_defs::FamousDefs;
use ra_ap_syntax::SyntaxKind;
use ra_ap_syntax::ast::{self, AstNode, HasAttrs, HasGenericParams};
use ra_ap_vfs::FileId;
use ryo_v2_core::domain::{ErrorCode, FindingCode};

/// The E0204 errors of one file.
///
/// `inactive` holds the ranges ra marked as cfg-disabled; `line_of` maps an offset to its
/// 1-based line. An empty result also means "cannot tell" (the file is outside the module tree,
/// or the crate has no `core`).
pub(crate) fn find(
    db: &RootDatabase,
    file_id: FileId,
    inactive: &[TextRange],
    line_of: &dyn Fn(TextSize) -> u32,
) -> Vec<Hit> {
    ra_ap_hir::attach_db(db, || {
        let sema = Semantics::new(db);
        let Some(module) = sema.file_to_module_def(file_id) else {
            return Vec::new();
        };
        let Some(copy) = FamousDefs(&sema, module.krate(db)).core_marker_Copy() else {
            return Vec::new();
        };
        let Ok(code) = ErrorCode::parse("E0204") else {
            return Vec::new();
        };
        let file = sema.parse_guess_edition(file_id);
        let mut hits = Vec::new();
        let mut push = |at: TextSize, name: String| {
            let hit = (
                FindingCode::Rustc(code.clone()),
                format!("the trait `Copy` cannot be implemented for type `{name}`"),
                line_of(at),
            );
            if !hits.contains(&hit) {
                hits.push(hit);
            }
        };
        for node in file.syntax().descendants() {
            if skipped(&node, inactive) {
                continue;
            }
            if let Some(adt) = ast::Adt::cast(node.clone()) {
                if !derives_copy(&adt) || has_type_params(&adt) {
                    continue;
                }
                let Some(def) = sema.to_def(&adt) else {
                    continue;
                };
                if has_non_copy_field(db, def) {
                    push(head(adt.syntax()), def.name(db).as_str().to_owned());
                }
            } else if let Some(imp) = ast::Impl::cast(node) {
                if imp.excl_token().is_some() || has_type_params(&imp) {
                    continue;
                }
                let Some(trait_ty) = imp.trait_() else {
                    continue;
                };
                let Some(def) = sema.to_def(&imp) else {
                    continue;
                };
                if def.trait_(db) != Some(copy) {
                    continue;
                }
                let Some(adt) = def.self_ty(db).as_adt() else {
                    continue;
                };
                if has_non_copy_field(db, adt) {
                    push(
                        trait_ty.syntax().text_range().start(),
                        adt.name(db).as_str().to_owned(),
                    );
                }
            }
        }
        hits
    })
}

/// Whether a `#[derive(..)]` of the type names `Copy` (as `Copy` or a path ending in it).
fn derives_copy(adt: &ast::Adt) -> bool {
    adt.attrs().any(|attr| {
        attr.as_simple_call().is_some_and(|(name, tt)| {
            name == "derive"
                && tt
                    .syntax()
                    .children_with_tokens()
                    .filter_map(|e| e.into_token())
                    .any(|t| t.kind() == SyntaxKind::IDENT && t.text() == "Copy")
        })
    })
}

/// Whether the item has a type or const parameter (lifetimes alone do not count).
fn has_type_params(item: &impl HasGenericParams) -> bool {
    item.generic_param_list()
        .is_some_and(|l| l.type_or_const_params().next().is_some())
}

/// Whether some field of the type is known and not `Copy`.
fn has_non_copy_field(db: &RootDatabase, adt: Adt) -> bool {
    let fields = match adt {
        Adt::Struct(s) => s.fields(db),
        Adt::Union(u) => u.fields(db),
        Adt::Enum(e) => e
            .variants(db)
            .into_iter()
            .flat_map(|v| v.fields(db))
            .collect(),
    };
    fields.into_iter().any(|f| {
        let ty = f.ty(db);
        !ty.contains_unknown() && !ty.is_copy(db)
    })
}

/// Whether the node is cfg-disabled or inside a macro call or definition.
fn skipped(node: &ra_ap_syntax::SyntaxNode, inactive: &[TextRange]) -> bool {
    let start = node.text_range().start();
    inactive.iter().any(|r| r.contains(start))
        || node.ancestors().skip(1).any(|a| {
            matches!(
                a.kind(),
                SyntaxKind::MACRO_CALL
                    | SyntaxKind::TOKEN_TREE
                    | SyntaxKind::MACRO_RULES
                    | SyntaxKind::MACRO_DEF
            )
        })
}
