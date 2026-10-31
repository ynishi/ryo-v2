//! The trait a field owes to its type's derived impls and to a written `Copy` impl, which
//! rust-analyzer 0.0.358 does not check.
//!
//! A builtin derive asks every field to implement the derived trait. ra builds the derived impls
//! without their bodies and checks no such obligation, and it checks no written `Copy` impl
//! either, so an edit that adds a derive to a type with a field that lacks the trait (the
//! add-derive edit kind proposes `Debug`, `Clone`, `Copy`, `PartialEq`, `Eq` and `Default`)
//! passes the verifier and fails `cargo check` (CHECK_LEDGER "Gaps").
//!
//! What rustc 1.98.1 reports, as measured on the oracle corpus: for `Copy` (derived, or
//! `impl Copy for T {}`) E0204 at the head of the type, or at the trait of the written impl;
//! for a derived `Clone`, `Debug`, `Default` and `Eq` E0277 at the field; for a derived
//! `PartialEq` E0369 at the field.
//!
//! A derived impl is an impl of the type for one of these traits of a `core`/`std` crate that ra
//! holds without a source (`Impl::source` is `None` for builtin derives only), so a written impl
//! of `Debug` or a proc-macro derive is never read as derived. A field fails when its type is
//! known and ra's trait solver says it does not implement the trait.
//!
//! A field is judged in the param env of the impl that asks for the trait. For a type with type
//! parameters the field type is instantiated with the arguments of the derived impl's self type,
//! so the solver sees the bounds the derive puts on the parameters (`T: Trait` for each type
//! parameter, plus the predicates the type declares in its bounds and where clause): `a: T`,
//! `Vec<T>`, `Option<T>` and `PhantomData<T>` pass, a field that lacks the trait whatever `T` is
//! fails. A written `Copy` impl of a generic type is judged the same way in the impl's own env
//! (`impl<T: Copy> Copy for W<T> {}`), also for a concrete instance (`impl Copy for W<u8> {}`).
//! A type with const parameters and no type parameter needs no instantiation (a const parameter
//! adds no bound), so its fields are judged as they are.
//!
//! A derived `Clone` next to a derived `Copy` is judged like any derived `Clone`: rustc reports
//! E0277 at each field without `Clone` beside the E0204 at the head, on a type with or without
//! type or const parameters (the shallow-copy `Clone` a type without parameters gets still asserts
//! `Clone` of every field; measured on breakage-lab `derive-copy-clone-0..2`). A field that is
//! `Clone` but not `Copy` gives only the E0204.
//!
//! Skipped, which loses recall and never precision: a field that mentions a const parameter of a
//! type that also has type parameters (the instantiation leaves the const argument unknown), a
//! written `Copy` impl with const parameters, `Eq` on a field that already fails `PartialEq`,
//! `Default` on an enum (only its `#[default]` variant counts), the traits above that were not
//! measured (`Hash`, `PartialOrd`, `Ord`), negative impls, cfg-disabled code and code inside macro
//! calls.

use crate::duplicate::head;
use crate::unresolved::Hit;
use ra_ap_hir::{Adt, Field, HasSource, Impl, Semantics, Trait, Type};
use ra_ap_ide::{TextRange, TextSize};
use ra_ap_ide_db::RootDatabase;
use ra_ap_syntax::SyntaxKind;
use ra_ap_syntax::ast::{self, AstNode, HasGenericParams};
use ra_ap_vfs::FileId;
use ryo_v2_core::domain::{ErrorCode, FindingCode};
use std::collections::BTreeMap;

/// The traits whose derive this detector checks, and the code rustc gives a field without it.
const DERIVED: [(&str, &str); 5] = [
    ("Clone", "E0277"),
    ("Debug", "E0277"),
    ("Default", "E0277"),
    ("PartialEq", "E0369"),
    ("Eq", "E0277"),
];

/// The field-bound errors of one file.
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
        let mut hits: Vec<Hit> = Vec::new();
        let mut push = |code: &str, message: String, at: TextSize| {
            let Ok(code) = ErrorCode::parse(code) else {
                return;
            };
            let hit = (FindingCode::Rustc(code), message, line_of(at));
            if !hits.contains(&hit) {
                hits.push(hit);
            }
        };
        for node in file.syntax().descendants() {
            if skipped(&node, inactive) {
                continue;
            }
            if let Some(adt) = ast::Adt::cast(node.clone()) {
                let Some(def) = sema.to_def(&adt) else {
                    continue;
                };
                let derived = derived_traits(db, def);
                if derived.is_empty() {
                    continue;
                }
                let fields = fields_of(&sema, &adt);
                let generic = has_param(&adt, true);
                let name = def.name(db).as_str().to_owned();
                if let Some(&(copy, imp)) = derived.get("Copy")
                    && fields
                        .iter()
                        .any(|(_, f)| lacks(db, &field_ty(db, f, imp, generic), copy))
                {
                    push("E0204", copy_message(&name), head(adt.syntax()));
                }
                for (trait_name, code) in DERIVED {
                    let Some(&(t, imp)) = derived.get(trait_name) else {
                        continue;
                    };
                    if trait_name == "Default" && matches!(def, Adt::Enum(_)) {
                        continue;
                    }
                    for (at, f) in &fields {
                        let ty = field_ty(db, f, imp, generic);
                        if !lacks(db, &ty, t) {
                            continue;
                        }
                        if trait_name == "Eq"
                            && derived.get("PartialEq").is_some_and(|&(pe, pe_imp)| {
                                lacks(db, &field_ty(db, f, pe_imp, generic), pe)
                            })
                        {
                            continue;
                        }
                        push(
                            code,
                            format!("a field of `{name}` does not implement `{trait_name}`"),
                            *at,
                        );
                    }
                }
            } else if let Some(imp) = ast::Impl::cast(node) {
                if imp.excl_token().is_some() || has_param(&imp, false) {
                    continue;
                }
                let Some(trait_ty) = imp.trait_() else {
                    continue;
                };
                let Some(def) = sema.to_def(&imp) else {
                    continue;
                };
                let Some(t) = def.trait_(db).filter(|t| is_core_trait(db, *t, "Copy")) else {
                    continue;
                };
                let Some(adt) = def.self_ty(db).as_adt() else {
                    continue;
                };
                // The arguments of the written self type (`W<T>`, or `W<u8>`), in the impl's env.
                let args: Vec<Type<'_>> = def.self_ty(db).type_arguments().collect();
                if adt_fields(db, adt).iter().any(|f| {
                    let ty = if args.is_empty() {
                        f.ty(db)
                    } else {
                        f.ty(db).instantiate(&args)
                    };
                    lacks(db, &ty, t)
                }) {
                    push(
                        "E0204",
                        copy_message(adt.name(db).as_str()),
                        trait_ty.syntax().text_range().start(),
                    );
                }
            }
        }
        hits
    })
}

fn copy_message(name: &str) -> String {
    format!("the trait `Copy` cannot be implemented for type `{name}`")
}

/// The traits of `core`/`std` that `adt` derives (builtin derives, which have no source), by
/// name: `Copy` and the names of [`DERIVED`].
fn derived_traits(db: &RootDatabase, adt: Adt) -> BTreeMap<&'static str, (Trait, Impl)> {
    let mut out = BTreeMap::new();
    for imp in Impl::all_for_type(db, adt.ty(db)) {
        if imp.source(db).is_some() {
            continue;
        }
        let Some(t) = imp.trait_(db) else { continue };
        for name in std::iter::once("Copy").chain(DERIVED.iter().map(|(n, _)| *n)) {
            if is_core_trait(db, t, name) {
                out.insert(name, (t, imp));
            }
        }
    }
    out
}

/// Whether `t` is the trait `name` of a `core`/`std` crate.
fn is_core_trait(db: &RootDatabase, t: Trait, name: &str) -> bool {
    t.name(db).as_str() == name && t.module(db).krate(db).is_builtin(db)
}

/// Where each field of the type starts, with the field (from ra), in source order.
fn fields_of(sema: &Semantics<'_, RootDatabase>, adt: &ast::Adt) -> Vec<(TextSize, Field)> {
    let lists: Vec<ast::FieldList> = match adt {
        ast::Adt::Struct(s) => s.field_list().into_iter().collect(),
        ast::Adt::Union(u) => u
            .record_field_list()
            .map(ast::FieldList::RecordFieldList)
            .into_iter()
            .collect(),
        ast::Adt::Enum(e) => e
            .variant_list()
            .into_iter()
            .flat_map(|l| l.variants())
            .filter_map(|v| v.field_list())
            .collect(),
    };
    let mut out = Vec::new();
    for list in lists {
        match list {
            ast::FieldList::RecordFieldList(l) => {
                for f in l.fields() {
                    if let Some(def) = sema.to_def(&f) {
                        out.push((head(f.syntax()), def));
                    }
                }
            }
            ast::FieldList::TupleFieldList(l) => {
                for f in l.fields() {
                    if let Some(def) = sema.to_def(&f) {
                        out.push((head(f.syntax()), def));
                    }
                }
            }
        }
    }
    out
}

/// The fields of a type ra resolved (for a written `Copy` impl).
fn adt_fields(db: &RootDatabase, adt: Adt) -> Vec<Field> {
    match adt {
        Adt::Struct(s) => s.fields(db),
        Adt::Union(u) => u.fields(db),
        Adt::Enum(e) => e
            .variants(db)
            .into_iter()
            .flat_map(|v| v.fields(db))
            .collect(),
    }
}

/// The type of `f` as the derived impl `imp` sees it. A generic type's fields are instantiated
/// with the arguments of the impl's self type, so the solver runs in the param env of the derived
/// impl: the type's own predicates plus `T: Trait` for each type parameter `T`. A const
/// parameter becomes an unknown argument, which [`lacks`] does not judge.
fn field_ty<'db>(db: &'db RootDatabase, f: &Field, imp: Impl, generic: bool) -> Type<'db> {
    if !generic {
        return f.ty(db);
    }
    let args: Vec<Type<'db>> = imp.self_ty(db).type_arguments().collect();
    f.ty(db).instantiate(&args)
}

/// Whether the type is known and does not implement `t` (with `Self` as the only other
/// argument where the trait has one, as `PartialEq<Rhs = Self>`).
fn lacks(db: &RootDatabase, ty: &Type<'_>, t: Trait) -> bool {
    if ty.contains_unknown() {
        return false;
    }
    let args: Vec<Type<'_>> = if t.name(db).as_str() == "PartialEq" {
        vec![ty.clone()]
    } else {
        Vec::new()
    };
    !ty.impls_trait(db, t, &args)
}

/// Whether the item has a type parameter (`ty`) or a const parameter (lifetimes do not count).
fn has_param(item: &impl HasGenericParams, ty: bool) -> bool {
    item.generic_param_list().is_some_and(|l| {
        l.type_or_const_params()
            .any(|p| matches!(p, ast::TypeOrConstParam::Type(_)) == ty)
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
