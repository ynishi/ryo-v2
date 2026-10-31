//! The `Expander` of the rust-analyzer adapter: from the code of a search state it proposes
//! Blueprints for the five intents (rename-local, extract-variable, add-derive,
//! extract-function, move-item).
//!
//! rust-analyzer does the semantic work (rename, extract assists, names, module paths); this
//! file reads its text edits as mutation specs and keeps only the Blueprints that the intent's
//! conformance check accepts, so a shape that rust-analyzer produces differently is dropped, not
//! forced. Candidates are produced lazily, round-robin over the targets and then the kinds, so a
//! small `max` costs few analyzer calls.

use crate::RaAnalyzer;
use crate::check::{fingerprints, map_error};
use crate::diagnostics::{diagnostics_config, panic_message};
use crate::fork::db_text;
use crate::session::{ExpanderPanic, RaSession};
use ra_ap_hir::{Crate, PrefixKind};
use ra_ap_ide::TextRange as RaRange;
use ra_ap_ide::{
    Analysis, AssistConfig, AssistKind, AssistResolveStrategy, DiagnosticsConfig,
    ExprFillDefaultMode, FilePosition, FileRange, RenameConfig, RootDatabase, Semantics,
    SingleResolve, SourceChange, TextSize,
};
use ra_ap_ide_db::imports::insert_use::{ImportGranularity, InsertUseConfig};
use ra_ap_ide_db::syntax_helpers::suggest_name::NameGenerator;
use ra_ap_syntax::ast::{self, HasModuleItem, HasName};
use ra_ap_syntax::{AstNode, SyntaxKind, SyntaxNode};
use ra_ap_vfs::FileId;
use ryo_v2_core::app::ports::{Analyzer, AnalyzerError, CheckError, ExpandError, Expander};
use ryo_v2_core::domain::text::revert_intact;
use ryo_v2_core::domain::{
    BaseFp, Blueprint, BlueprintId, BlueprintParts, DecisionRecord, DeriveName, Edit, Fingerprint,
    Ident, IntentCall, IntentKind, MutationSpec, NonEmpty, ReadRange, RelPath, SpecKind, StateFp,
    Target, TextRange,
};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;

/// Who the Blueprints are proposed by.
const PROPOSER: &str = "ryo-v2-ra/expand";
/// Longest run of consecutive statements offered to extract-function.
const MAX_RUN: usize = 4;
/// New names tried per binding by rename-local.
const MAX_RENAMES: usize = 2;
/// Destination files tried per item by move-item.
const MAX_DESTS: usize = 3;
/// Derives add-derive proposes, in order.
const DERIVES: [&str; 6] = ["Debug", "Clone", "Copy", "PartialEq", "Eq", "Default"];
/// The name rust-analyzer gives an extracted function.
const FUN_NAME: &str = "fun_name";
/// The name rust-analyzer falls back to when it finds none for an extracted variable.
const VAR_NAME: &str = "var_name";

/// Assist settings: spelled out field by field, like [`diagnostics_config`].
fn assist_config() -> AssistConfig {
    AssistConfig {
        // No snippets: the edits must be plain text.
        snippet_cap: None,
        // Only extract refactorings; this also keeps the quick fixes (and so a full diagnostics
        // run per range) out of `assists_with_fixes`.
        allowed: Some(vec![AssistKind::RefactorExtract]),
        // Import insertion style: the same as the diagnostics' (extract-function may add a `use`).
        insert_use: InsertUseConfig {
            granularity: ImportGranularity::Item,
            enforce_granularity: false,
            prefix_kind: PrefixKind::Plain,
            group: false,
            skip_glob_imports: false,
        },
        // Import path choice: as the diagnostics' config.
        prefer_no_std: false,
        // Same.
        prefer_prelude: true,
        // Same.
        prefer_absolute: false,
        // No `#[must_use]` on generated fns.
        assist_emit_must_use: false,
        // Term search budget: as the diagnostics' config (no assist used here searches terms).
        term_search_fuel: 400,
        // Grouping is a menu matter.
        code_action_grouping: false,
        // Fill style: as the diagnostics' config.
        expr_fill_default: ExprFillDefaultMode::Todo,
        // Keep `Self` out of generated signatures.
        prefer_self_ty: false,
        // Conflicting renames are reported (and refused by this file).
        show_rename_conflicts: true,
    }
}

/// Rename settings.
fn rename_config() -> RenameConfig {
    RenameConfig {
        // Import path choice of the paths a rename rewrites: as the diagnostics' config.
        prefer_no_std: false,
        // Same.
        prefer_prelude: true,
        // Same.
        prefer_absolute: false,
        // A rename that would change the program's meaning carries an annotation; this file
        // refuses those.
        show_conflicts: true,
    }
}

/// A candidate before it is checked and turned into a Blueprint.
struct Cand {
    intent: IntentCall,
    specs: Vec<MutationSpec>,
    rationale: String,
}

/// A target resolved to a file of the session.
#[derive(Clone)]
struct TInfo {
    file: FileId,
    range: Option<(u32, u32)>,
}

impl TInfo {
    /// Whether the byte range `r` meets the target.
    fn hits(&self, r: RaRange) -> bool {
        let Some((lo, hi)) = self.range else {
            return true;
        };
        let (s, e) = (u32::from(r.start()), u32::from(r.end()));
        if lo == hi {
            s <= lo && lo <= e
        } else {
            s < hi && lo < e
        }
    }
}

/// Module paths of the files of the session, by crate.
struct ModuleMap {
    by_file: HashMap<FileId, (Crate, Vec<String>)>,
    by_path: HashMap<(Crate, Vec<String>), FileId>,
}

struct RenameSeed {
    file: FileId,
    offset: u32,
    from: String,
    tos: Vec<(String, &'static str)>,
}

struct ExtractVarSeed {
    file: FileId,
    lo: u32,
    hi: u32,
    /// Start of the line of the statement the `let` goes before.
    at: u32,
    /// Whitespace before that statement on its line.
    indent: String,
    /// When the expression is the value of a record-literal field `k: v`: the start of the field
    /// and `k`.
    field: Option<(u32, String)>,
}

struct ExtractFnSeed {
    file: FileId,
    lo: u32,
    hi: u32,
    name: String,
}

/// Everything one expansion reads: the analyzer views of the session, with caches.
struct Env<'a> {
    sess: &'a RaSession,
    db: &'a RootDatabase,
    analysis: Analysis,
    sema: Semantics<'a, RootDatabase>,
    state_fp: StateFp,
    assist_cfg: AssistConfig,
    diag_cfg: DiagnosticsConfig,
    rename_cfg: RenameConfig,
    by_path: HashMap<RelPath, FileId>,
    by_file: HashMap<FileId, RelPath>,
    texts: RefCell<HashMap<FileId, Rc<str>>>,
    trees: RefCell<HashMap<FileId, ast::SourceFile>>,
    modules: RefCell<Option<Rc<ModuleMap>>>,
    fail: RefCell<Option<String>>,
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// `text` with every whole-word `from` replaced by `to`.
fn replace_word(text: &str, from: &str, to: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for (i, _) in text.match_indices(from) {
        let before_ok = text[..i]
            .chars()
            .next_back()
            .is_none_or(|c| !is_ident_char(c));
        let after_ok = text[i + from.len()..]
            .chars()
            .next()
            .is_none_or(|c| !is_ident_char(c));
        if before_ok && after_ok && i >= last {
            out.push_str(&text[last..i]);
            out.push_str(to);
            last = i + from.len();
        }
    }
    out.push_str(&text[last..]);
    out
}

/// Whether `text` has `fn <name>` as a whole word.
fn declares_fn_named(text: &str, name: &str) -> bool {
    let pat = format!("fn {name}");
    text.match_indices(&pat).any(|(i, _)| {
        text[i + pat.len()..]
            .chars()
            .next()
            .is_none_or(|c| !is_ident_char(c))
    })
}

/// The name of the `let <name> = <init>;` that an edit adds: the one name whose statement occurs
/// more often in `applied` (the text after the edit) than in `text`. `None` when there is no such
/// name or more than one; `let mut` and patterns are never read. The name is read from the edited
/// text, not from the edit's indels: rust-analyzer's edit is a diff, and a `let` inserted in front
/// of another `let` lines up on their shared `let `, so no indel holds the new statement.
fn added_let_name(text: &str, applied: &str, init: &str) -> Option<String> {
    let tail = format!(" = {init};");
    let mut names: BTreeSet<String> = BTreeSet::new();
    for (i, _) in applied.match_indices(&tail) {
        let before = &applied[..i];
        let len = before
            .chars()
            .rev()
            .take_while(|c| is_ident_char(*c))
            .map(char::len_utf8)
            .sum::<usize>();
        let (head, name) = before.split_at(before.len() - len);
        let keyword = head
            .strip_suffix("let ")
            .is_some_and(|h| h.chars().next_back().is_none_or(|c| !is_ident_char(c)));
        if name.is_empty() || !keyword || name == "mut" {
            continue;
        }
        let stmt = format!("let {name}{tail}");
        if applied.matches(&stmt).count() > text.matches(&stmt).count() {
            names.insert(name.to_owned());
        }
    }
    let mut it = names.into_iter();
    match (it.next(), it.next()) {
        (Some(n), None) => Some(n),
        _ => None,
    }
}

/// Whether any attribute of `node` or of an ancestor marks test or cfg-dependent code.
fn in_test_code(node: &SyntaxNode) -> bool {
    node.ancestors().any(|a| {
        a.children()
            .filter_map(ast::Attr::cast)
            .any(|attr| is_test_attr(&attr.syntax().text().to_string()))
    })
}

fn is_test_attr(attr: &str) -> bool {
    let a: String = attr.split_whitespace().collect();
    a.starts_with("#[cfg(test)") || a == "#[test]" || a.starts_with("#[tokio::test")
}

/// Attributes that stop an item from being moved.
fn blocks_move(node: &SyntaxNode) -> bool {
    node.children().filter_map(ast::Attr::cast).any(|attr| {
        let a: String = attr
            .syntax()
            .text()
            .to_string()
            .split_whitespace()
            .collect();
        a.starts_with("#[cfg")
            || a.starts_with("#[test")
            || a.starts_with("#[macro")
            || a.starts_with("#[proc_macro")
            || a.starts_with("#[no_mangle")
            || a.starts_with("#[export_name")
    })
}

/// Start of the first element of an item that is not an attribute, comment or whitespace: where
/// its visibility (or keyword) begins.
fn keyword_offset(node: &SyntaxNode) -> u32 {
    node.children_with_tokens()
        .find(|e| {
            !matches!(
                e.kind(),
                SyntaxKind::ATTR | SyntaxKind::COMMENT | SyntaxKind::WHITESPACE
            )
        })
        .map_or_else(
            || u32::from(node.text_range().start()),
            |e| u32::from(e.text_range().start()),
        )
}

/// Text of the visibility of an item node, if it has one.
fn visibility_of(node: &SyntaxNode) -> Option<ast::Visibility> {
    node.children().find_map(ast::Visibility::cast)
}

/// The deterministic UUID version 7 of a Blueprint: 80 bits folded from the state and the
/// content (intent and specs), under timestamp 0.
fn blueprint_id(
    state: StateFp,
    intent: &IntentCall,
    specs: &[MutationSpec],
) -> Option<BlueprintId> {
    let mut key = intent.to_string();
    for s in specs {
        key.push('\n');
        key.push_str(&s.to_string());
        key.push('\0');
        key.push_str(s.text());
    }
    let mut all = [0u8; 16];
    all[..8].copy_from_slice(&state.fingerprint().value().to_le_bytes());
    all[8..].copy_from_slice(&Fingerprint::of(key.as_bytes()).value().to_le_bytes());
    let mut bytes = [0u8; 10];
    bytes.copy_from_slice(&all[..10]);
    for (i, b) in all[10..].iter().enumerate() {
        bytes[i] ^= b;
    }
    let uuid = uuid::Builder::from_unix_timestamp_millis(0, &bytes).into_uuid();
    BlueprintId::from_uuid(uuid).ok()
}

/// The callee name of a call expression, or the method name of a method call.
fn called_name(n: &SyntaxNode) -> Option<(u32, String)> {
    if let Some(m) = ast::MethodCallExpr::cast(n.clone()) {
        let name = m.name_ref()?;
        return Some((
            u32::from(name.syntax().text_range().start()),
            name.text().to_string(),
        ));
    }
    if let Some(c) = ast::CallExpr::cast(n.clone())
        && let Some(ast::Expr::PathExpr(p)) = c.expr()
    {
        let name = p.path()?.segment()?.name_ref()?;
        return Some((
            u32::from(name.syntax().text_range().start()),
            name.text().to_string(),
        ));
    }
    None
}

/// The name for a function extracted from bytes `lo..hi` of `body`: the last method or fn called
/// there, else the name bound by the last `let`.
fn extract_name(body: &SyntaxNode, lo: u32, hi: u32) -> Option<String> {
    let inside = |n: &SyntaxNode| {
        u32::from(n.text_range().start()) >= lo && u32::from(n.text_range().end()) <= hi
    };
    let mut call: Option<(u32, String)> = None;
    let mut bound: Option<(u32, String)> = None;
    for n in body.descendants().filter(|n| inside(n)) {
        if let Some(c) = called_name(&n)
            && call.as_ref().is_none_or(|(o, _)| c.0 > *o)
        {
            call = Some(c);
        }
        if let Some(l) = ast::LetStmt::cast(n.clone())
            && let Some(ast::Pat::IdentPat(p)) = l.pat()
            && let Some(name) = p.name()
        {
            let at = u32::from(name.syntax().text_range().start());
            if bound.as_ref().is_none_or(|(o, _)| at > *o) {
                bound = Some((at, name.text().to_string()));
            }
        }
    }
    let name = call.or(bound).map(|(_, s)| s)?;
    let first = name.chars().next()?;
    (first.is_ascii_lowercase() || first == '_').then_some(name)
}

impl<'a> Env<'a> {
    fn new(sess: &'a RaSession, state_fp: StateFp) -> Env<'a> {
        let db = sess.host.raw_database();
        let mut diag_cfg = diagnostics_config();
        // No diagnostics quick fixes are wanted in `assists_with_fixes`.
        diag_cfg.enabled = false;
        Env {
            sess,
            db,
            analysis: sess.host.analysis(),
            sema: Semantics::new(db),
            state_fp,
            assist_cfg: assist_config(),
            diag_cfg,
            rename_cfg: rename_config(),
            by_path: sess.files.iter().map(|(i, r)| (r.clone(), *i)).collect(),
            by_file: sess.files.iter().map(|(i, r)| (*i, r.clone())).collect(),
            texts: RefCell::default(),
            trees: RefCell::default(),
            modules: RefCell::default(),
            fail: RefCell::default(),
        }
    }

    fn set_fail(&self, msg: &str) {
        let mut f = self.fail.borrow_mut();
        if f.is_none() {
            *f = Some(msg.to_owned());
        }
    }

    fn failed(&self) -> bool {
        self.fail.borrow().is_some()
    }

    fn text(&self, id: FileId) -> Option<Rc<str>> {
        if let Some(t) = self.texts.borrow().get(&id) {
            return Some(t.clone());
        }
        let t: Rc<str> = Rc::from(db_text(&self.sess.host, id).ok()?);
        self.texts.borrow_mut().insert(id, t.clone());
        Some(t)
    }

    fn text_of_path(&self, path: &RelPath) -> Option<Rc<str>> {
        self.text(*self.by_path.get(path)?)
    }

    fn tree(&self, id: FileId) -> Option<ast::SourceFile> {
        if let Some(t) = self.trees.borrow().get(&id) {
            return Some(t.clone());
        }
        let t = self.sema.parse(self.sema.attach_first_edition(id));
        self.trees.borrow_mut().insert(id, t.clone());
        Some(t)
    }

    fn tinfo(&self, t: &Target) -> Option<TInfo> {
        Some(TInfo {
            file: *self.by_path.get(t.path())?,
            range: t.range().map(|r| (r.start(), r.end())),
        })
    }

    /// Functions with a body in the target, outside test code.
    fn fns_in(&self, tree: &ast::SourceFile, t: &TInfo) -> Vec<ast::Fn> {
        tree.syntax()
            .descendants()
            .filter_map(ast::Fn::cast)
            .filter(|f| {
                f.body().is_some() && t.hits(f.syntax().text_range()) && !in_test_code(f.syntax())
            })
            .collect()
    }

    /// The smallest item enclosing bytes `lo..hi`, or the whole file.
    fn read_range(&self, id: FileId, text: &str, lo: u32, hi: u32) -> TextRange {
        let whole = TextRange::new(0, u32::try_from(text.len()).unwrap_or(u32::MAX))
            .unwrap_or_else(|_| TextRange::empty_at(0));
        if lo > hi || hi as usize > text.len() {
            return whole;
        }
        let Some(tree) = self.tree(id) else {
            return whole;
        };
        let r = RaRange::new(TextSize::from(lo), TextSize::from(hi));
        let el = tree.syntax().covering_element(r);
        let node = match el {
            ra_ap_syntax::NodeOrToken::Node(n) => Some(n),
            ra_ap_syntax::NodeOrToken::Token(t) => t.parent(),
        };
        node.into_iter()
            .flat_map(|n| n.ancestors())
            .find(|a| ast::Item::can_cast(a.kind()) && a.text_range().contains_range(r))
            .and_then(|a| {
                TextRange::new(
                    u32::from(a.text_range().start()),
                    u32::from(a.text_range().end()),
                )
                .ok()
            })
            .unwrap_or(whole)
    }

    /// Checks a candidate against its intent and builds the Blueprint.
    fn finish(&self, c: Cand) -> Option<Blueprint> {
        let mut base: BTreeMap<RelPath, String> = BTreeMap::new();
        for s in &c.specs {
            if !base.contains_key(s.path()) {
                base.insert(s.path().clone(), self.text_of_path(s.path())?.to_string());
            }
        }
        c.intent.conforms(&c.specs, &base).ok()?;
        let mut reads = Vec::new();
        for (path, text) in &base {
            let (lo, hi) = c
                .specs
                .iter()
                .filter(|s| s.path() == path)
                .fold((u32::MAX, 0u32), |(lo, hi), s| {
                    (lo.min(s.range().start()), hi.max(s.range().end()))
                });
            let id = *self.by_path.get(path)?;
            let range = self.read_range(id, text, lo, hi);
            reads.push(ReadRange::capture(path.clone(), text, range).ok()?);
        }
        let id = blueprint_id(self.state_fp, &c.intent, &c.specs)?;
        let record = DecisionRecord::new(PROPOSER, &c.rationale).ok()?;
        let bases = base
            .iter()
            .map(|(p, t)| BaseFp::capture(p.clone(), t))
            .collect();
        Blueprint::new(BlueprintParts {
            id,
            intent: c.intent,
            specs: c.specs,
            reads,
            bases,
            spec_refs: Vec::new(),
            record,
        })
        .ok()
    }

    /// The indels of a change that touches one file, as mutation specs.
    fn single_file_specs(
        &self,
        change: &SourceChange,
        refuse_annotations: bool,
    ) -> Option<Vec<MutationSpec>> {
        if !change.file_system_edits.is_empty()
            || change.is_snippet
            || change.source_file_edits.len() != 1
            || (refuse_annotations && !change.annotations.is_empty())
        {
            return None;
        }
        let (fid, (edit, snippet)) = change.source_file_edits.iter().next()?;
        if snippet.is_some() {
            return None;
        }
        let path = self.by_file.get(fid)?.clone();
        let mut specs = Vec::new();
        for ind in edit.iter() {
            let (s, e) = (u32::from(ind.delete.start()), u32::from(ind.delete.end()));
            let spec = if s == e {
                if ind.insert.is_empty() {
                    continue;
                }
                MutationSpec::insert(path.clone(), s, ind.insert.clone()).ok()?
            } else if ind.insert.is_empty() {
                MutationSpec::delete(path.clone(), TextRange::new(s, e).ok()?).ok()?
            } else {
                MutationSpec::replace(path.clone(), TextRange::new(s, e).ok()?, ind.insert.clone())
                    .ok()?
            };
            specs.push(spec);
        }
        (!specs.is_empty()).then_some(specs)
    }

    /// The resolved assist `id` on `range`, if rust-analyzer offers it there.
    fn run_assist(&self, file: FileId, lo: u32, hi: u32, id: &str) -> Option<ra_ap_ide::Assist> {
        if self.failed() || lo > hi {
            return None;
        }
        let frange = FileRange {
            file_id: file,
            range: RaRange::new(TextSize::from(lo), TextSize::from(hi)),
        };
        let resolve = AssistResolveStrategy::Single(SingleResolve {
            assist_id: id.to_owned(),
            assist_kind: AssistKind::RefactorExtract,
            assist_subtype: None,
        });
        match self
            .analysis
            .assists_with_fixes(&self.assist_cfg, &self.diag_cfg, resolve, frange)
        {
            Ok(list) => list
                .into_iter()
                .find(|a| a.id.0 == id && a.source_change.is_some()),
            Err(_) => {
                self.set_fail("analysis cancelled");
                None
            }
        }
    }

    // ---- rename-local ----

    fn rename_seeds(&self, t: &TInfo) -> Vec<RenameSeed> {
        let Some(tree) = self.tree(t.file) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut seen = BTreeSet::new();
        for f in self.fns_in(&tree, t) {
            let pats: Vec<ast::IdentPat> = f
                .syntax()
                .descendants()
                .filter_map(ast::IdentPat::cast)
                .collect();
            let bound: Vec<String> = pats
                .iter()
                .filter_map(|p| p.name())
                .map(|n| n.text().to_string())
                .collect();
            for p in &pats {
                let Some(name) = p.name() else { continue };
                let from = name.text().to_string();
                let offset = u32::from(name.syntax().text_range().start());
                if from.starts_with("r#")
                    || !t.hits(name.syntax().text_range())
                    || self.sema.to_def(p).is_none()
                    || !seen.insert(offset)
                {
                    continue;
                }
                let tos = self.rename_targets(p, &from, &bound);
                if !tos.is_empty() {
                    out.push(RenameSeed {
                        file: t.file,
                        offset,
                        from,
                        tos,
                    });
                }
            }
        }
        out
    }

    /// Up to [`MAX_RENAMES`] new names for a binding.
    fn rename_targets(
        &self,
        p: &ast::IdentPat,
        from: &str,
        bound: &[String],
    ) -> Vec<(String, &'static str)> {
        let mut tos: Vec<(String, &'static str)> = Vec::new();
        let mut push = |n: String, why: &'static str| {
            let fallback = n == VAR_NAME || n == FUN_NAME;
            if n != from
                && !fallback
                && !bound.contains(&n)
                && Ident::parse(&n).is_ok()
                && !tos.iter().any(|(m, _)| *m == n)
            {
                tos.push((n, why));
            }
        };
        let fresh = || NameGenerator::new_with_names(bound.iter().map(String::as_str));
        let is_simple_let =
            p.syntax().parent().and_then(ast::LetStmt::cast).filter(
                |l| matches!(l.pat(), Some(ast::Pat::IdentPat(q)) if q.syntax() == p.syntax()),
            );
        if let Some(init) = is_simple_let.and_then(|l| l.initializer())
            && let Some(n) = fresh().try_for_variable(&init, &self.sema)
        {
            push(n.to_string(), "name from initializer call");
        }
        if let Some(info) = self.sema.type_of_pat(&ast::Pat::IdentPat(p.clone()))
            && let Some(scope) = self.sema.scope(p.syntax())
        {
            let edition = scope.krate().edition(self.db);
            if let Some(n) = fresh().for_type(&info.original, self.db, edition) {
                push(n.to_string(), "name from type");
            }
        }
        tos.truncate(MAX_RENAMES);
        tos
    }

    fn rename_realize(&self, seed: &RenameSeed) -> Vec<Cand> {
        let mut out = Vec::new();
        let Ok(from) = Ident::parse(&seed.from) else {
            return out;
        };
        for (to, why) in &seed.tos {
            if self.failed() {
                break;
            }
            let pos = FilePosition {
                file_id: seed.file,
                offset: TextSize::from(seed.offset),
            };
            let res = match self.analysis.rename(pos, to, &self.rename_cfg) {
                Ok(r) => r,
                Err(_) => {
                    self.set_fail("analysis cancelled");
                    break;
                }
            };
            let Ok(change) = res else { continue };
            let Some(specs) = self.single_file_specs(&change, true) else {
                continue;
            };
            let Ok(to_id) = Ident::parse(to) else {
                continue;
            };
            out.push(Cand {
                intent: IntentCall::RenameLocal {
                    from: from.clone(),
                    to: to_id,
                },
                specs,
                rationale: (*why).to_owned(),
            });
        }
        out
    }

    // ---- extract-variable ----

    fn extract_var_seeds(&self, t: &TInfo) -> Vec<ExtractVarSeed> {
        let (Some(tree), Some(text)) = (self.tree(t.file), self.text(t.file)) else {
            return Vec::new();
        };
        let mut seen: BTreeSet<(u32, u32)> = BTreeSet::new();
        let mut out = Vec::new();
        for f in self.fns_in(&tree, t) {
            let Some(body) = f.body() else { continue };
            for n in body.syntax().descendants() {
                let Some(e) = ast::Expr::cast(n) else {
                    continue;
                };
                if !is_variable_shape(&e)
                    || is_simple_let_init(&e)
                    || is_assign_target(&e)
                    || !is_nameable_field_value(&e)
                {
                    continue;
                }
                let r = e.syntax().text_range();
                let (lo, hi) = (u32::from(r.start()), u32::from(r.end()));
                if !t.hits(r) || !seen.insert((lo, hi)) {
                    continue;
                }
                let Some(anchor) = variable_anchor(&e) else {
                    continue;
                };
                // The `let` is a line of its own, inserted at the start of the statement's line
                // (before its indentation), so it cannot collide with the replaced expression
                // even when that starts the statement. The statement must be alone on its line
                // and the expression on one line (rust-analyzer re-indents longer ones).
                let stmt = u32::from(anchor.text_range().start());
                let expr_text = &text[lo as usize..hi as usize];
                let line_start = text[..stmt as usize].rfind('\n').map_or(0, |i| i + 1);
                let indent = &text[line_start..stmt as usize];
                if line_start as u32 >= lo
                    || expr_text.contains('\n')
                    || !indent.chars().all(|c| c == ' ' || c == '\t')
                {
                    continue;
                }
                out.push(ExtractVarSeed {
                    file: t.file,
                    lo,
                    hi,
                    at: line_start as u32,
                    indent: indent.to_owned(),
                    field: record_field_of(&e),
                });
            }
        }
        out
    }

    /// rust-analyzer decides whether the expression can be extracted and names it; its edit is a
    /// text diff of unpredictable shape, so the specs are built here: `let <name> = <expr>;` before
    /// the statement and the expression replaced by the name. The name is the one of the
    /// `let <name> = <expr>;` that applying rust-analyzer's own edit adds ([`added_let_name`]); a
    /// candidate is kept only when there is exactly one. The value of a record-literal field
    /// named like the variable (`k: v` with name `k`) is replaced, with its field name, by the
    /// shorthand `k`, as rust-analyzer writes it.
    fn extract_var_realize(&self, seed: &ExtractVarSeed) -> Vec<Cand> {
        let Some(assist) = self.run_assist(seed.file, seed.lo, seed.hi, "extract_variable") else {
            return Vec::new();
        };
        let (Some(change), Some(path), Some(text)) = (
            assist.source_change,
            self.by_file.get(&seed.file),
            self.text(seed.file),
        ) else {
            return Vec::new();
        };
        let Some((edit, _)) = change.source_file_edits.get(&seed.file) else {
            return Vec::new();
        };
        let mut applied = text.to_string();
        edit.apply(&mut applied);
        let init = &text[seed.lo as usize..seed.hi as usize];
        let name = added_let_name(&text, &applied, init);
        let Some(name) = name.filter(|n| n != VAR_NAME) else {
            return Vec::new();
        };
        let lo = match &seed.field {
            Some((start, field)) if *field == name => *start,
            _ => seed.lo,
        };
        let (Ok(ident), Ok(range)) = (Ident::parse(&name), TextRange::new(lo, seed.hi)) else {
            return Vec::new();
        };
        let (Ok(ins), Ok(rep)) = (
            MutationSpec::insert(
                path.clone(),
                seed.at,
                format!("{}let {name} = {init};\n", seed.indent),
            ),
            MutationSpec::replace(path.clone(), range, name),
        ) else {
            return Vec::new();
        };
        vec![Cand {
            intent: IntentCall::ExtractVariable { name: ident },
            specs: vec![ins, rep],
            rationale: "name from the expression".to_owned(),
        }]
    }

    // ---- extract-function ----

    fn extract_fn_seeds(&self, t: &TInfo) -> Vec<ExtractFnSeed> {
        let Some(tree) = self.tree(t.file) else {
            return Vec::new();
        };
        let Some(text) = self.text(t.file) else {
            return Vec::new();
        };
        let mut seen: BTreeSet<(u32, u32)> = BTreeSet::new();
        let mut out = Vec::new();
        let mut add = |body: &SyntaxNode, lo: u32, hi: u32| {
            if lo >= hi || !seen.insert((lo, hi)) {
                return;
            }
            if let Some(name) = extract_name(body, lo, hi)
                && Ident::parse(&name).is_ok()
                && !declares_fn_named(&text, &name)
            {
                out.push(ExtractFnSeed {
                    file: t.file,
                    lo,
                    hi,
                    name,
                });
            }
        };
        for f in self.fns_in(&tree, t) {
            let Some(body) = f.body() else { continue };
            let stmts: Vec<ast::Stmt> = body.statements().collect();
            let tail = body.tail_expr();
            for i in 0..stmts.len() {
                for len in 1..=MAX_RUN.min(stmts.len() - i) {
                    let run = &stmts[i..i + len];
                    if run.iter().any(|s| matches!(s, ast::Stmt::Item(_))) {
                        continue;
                    }
                    if i == 0 && len == stmts.len() && tail.is_none() {
                        continue;
                    }
                    let lo = u32::from(run[0].syntax().text_range().start());
                    let hi = u32::from(run[len - 1].syntax().text_range().end());
                    if t.hits(RaRange::new(TextSize::from(lo), TextSize::from(hi))) {
                        add(body.syntax(), lo, hi);
                    }
                }
                if let ast::Stmt::LetStmt(l) = &stmts[i]
                    && let Some(init) = l.initializer()
                {
                    let r = init.syntax().text_range();
                    if t.hits(r) {
                        add(body.syntax(), u32::from(r.start()), u32::from(r.end()));
                    }
                }
            }
            if let Some(tail) = tail
                && !stmts.is_empty()
            {
                let r = tail.syntax().text_range();
                if t.hits(r) {
                    add(body.syntax(), u32::from(r.start()), u32::from(r.end()));
                }
            }
        }
        out
    }

    fn extract_fn_realize(&self, seed: &ExtractFnSeed) -> Vec<Cand> {
        let Some(assist) = self.run_assist(seed.file, seed.lo, seed.hi, "extract_function") else {
            return Vec::new();
        };
        let Some(change) = assist.source_change else {
            return Vec::new();
        };
        let Some(specs) = self.single_file_specs(&change, false) else {
            return Vec::new();
        };
        let Ok(name) = Ident::parse(&seed.name) else {
            return Vec::new();
        };
        let mut renamed = Vec::new();
        for s in &specs {
            let spec = match s.kind() {
                SpecKind::Insert => MutationSpec::insert(
                    s.path().clone(),
                    s.range().start(),
                    replace_word(s.text(), FUN_NAME, &seed.name),
                ),
                SpecKind::Replace => MutationSpec::replace(
                    s.path().clone(),
                    s.range(),
                    replace_word(s.text(), FUN_NAME, &seed.name),
                ),
                SpecKind::Delete => Ok(s.clone()),
            };
            match spec {
                Ok(s) => renamed.push(s),
                Err(_) => return Vec::new(),
            }
        }
        vec![Cand {
            intent: IntentCall::ExtractFunction { name },
            specs: renamed,
            rationale: "name of the last call in the extracted range".to_owned(),
        }]
    }

    // ---- add-derive ----

    fn add_derive_cands(&self, t: &TInfo) -> Vec<Cand> {
        let (Some(tree), Some(text)) = (self.tree(t.file), self.text(t.file)) else {
            return Vec::new();
        };
        let Some(path) = self.by_file.get(&t.file) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for adt in tree.syntax().descendants() {
            if !matches!(adt.kind(), SyntaxKind::STRUCT | SyntaxKind::ENUM)
                || !t.hits(adt.text_range())
                || in_test_code(&adt)
            {
                continue;
            }
            // The first derive attribute: its names and the offset of its closing paren.
            let mut have: BTreeSet<String> = BTreeSet::new();
            let mut close: Option<(u32, bool)> = None;
            for attr in adt.children().filter_map(ast::Attr::cast) {
                let Some((name, tt)) = attr.as_simple_call() else {
                    continue;
                };
                if name != "derive" {
                    continue;
                }
                let inner = tt.syntax().text().to_string();
                let inner = inner.trim_start_matches('(').trim_end_matches(')');
                for n in inner.split(',') {
                    let n = n.trim().rsplit("::").next().unwrap_or("").trim();
                    if !n.is_empty() {
                        have.insert(n.to_owned());
                    }
                }
                if close.is_none()
                    && let Some(r) = tt.right_delimiter_token()
                {
                    let at = u32::from(r.text_range().start());
                    let trailing_comma = text[..at as usize].trim_end().ends_with(',');
                    close = Some((at, trailing_comma));
                }
            }
            let start = u32::from(adt.text_range().start());
            let line_start = text[..start as usize].rfind('\n').map_or(0, |i| i + 1);
            let indent = &text[line_start..start as usize];
            let indent_ok = indent.chars().all(|c| c == ' ' || c == '\t');
            for d in DERIVES {
                if have.contains(d)
                    || (d == "Copy" && !have.contains("Clone"))
                    || (d == "Eq" && !have.contains("PartialEq"))
                {
                    continue;
                }
                let spec = match close {
                    Some((_, true)) => continue,
                    Some((at, false)) => MutationSpec::insert(path.clone(), at, format!(", {d}")),
                    None if indent_ok => MutationSpec::insert(
                        path.clone(),
                        start,
                        format!("#[derive({d})]\n{indent}"),
                    ),
                    None => continue,
                };
                let (Ok(spec), Ok(name)) = (spec, DeriveName::parse(d)) else {
                    continue;
                };
                let Some(derives) = NonEmpty::new(vec![name]) else {
                    continue;
                };
                out.push(Cand {
                    intent: IntentCall::AddDerive { derives },
                    specs: vec![spec],
                    rationale: "derive missing on the type".to_owned(),
                });
            }
        }
        out
    }

    // ---- move-item ----

    fn module_map(&self) -> Rc<ModuleMap> {
        if let Some(m) = self.modules.borrow().as_ref() {
            return m.clone();
        }
        let mut by_file = HashMap::new();
        let mut by_path = HashMap::new();
        for (id, _) in &self.sess.files {
            let Some(m) = self.sema.file_to_module_def(*id) else {
                continue;
            };
            let krate = m.krate(self.db);
            let segs: Vec<String> = m
                .path_segments(self.db)
                .map(|n| n.as_str().to_owned())
                .collect();
            by_path.entry((krate, segs.clone())).or_insert(*id);
            by_file.insert(*id, (krate, segs));
        }
        let map = Rc::new(ModuleMap { by_file, by_path });
        *self.modules.borrow_mut() = Some(map.clone());
        map
    }

    fn move_item_cands(&self, t: &TInfo) -> Vec<Cand> {
        let (Some(tree), Some(text)) = (self.tree(t.file), self.text(t.file)) else {
            return Vec::new();
        };
        let Some(src_path) = self.by_file.get(&t.file) else {
            return Vec::new();
        };
        let map = self.module_map();
        let Some((krate, _)) = map.by_file.get(&t.file) else {
            return Vec::new();
        };
        let mut out = Vec::new();

        // Destinations: other module files of the crate, same directory first.
        let src_dir = src_path.as_str().rsplit_once('/').map_or("", |(d, _)| d);
        let mut dests: Vec<(&FileId, &RelPath, &Vec<String>)> = map
            .by_file
            .iter()
            .filter(|(id, (k, _))| *id != &t.file && k == krate)
            .filter_map(|(id, (_, segs))| Some((id, self.by_file.get(id)?, segs)))
            .collect();
        dests.sort_by_key(|(_, p, _)| {
            let dir = p.as_str().rsplit_once('/').map_or("", |(d, _)| d);
            (dir != src_dir, p.as_str().to_owned())
        });
        dests.truncate(MAX_DESTS);

        // Forward shape: the item becomes a re-export, its text lands in the destination.
        for item in tree.items() {
            let node = item.syntax();
            let Some((name, start, end)) = movable_item(&item, &text, t) else {
                continue;
            };
            let vis = visibility_of(node);
            let vis_text = vis
                .as_ref()
                .map(|v| format!("{} ", v.syntax().text()))
                .unwrap_or_default();
            let item_text = &text[start as usize..end as usize];
            let moved = if vis.is_some() {
                item_text.to_owned()
            } else {
                let at = (keyword_offset(node) - start) as usize;
                format!("{}pub(crate) {}", &item_text[..at], &item_text[at..])
            };
            let (Ok(item_id), Ok(range)) = (Ident::parse(&name), TextRange::new(start, end)) else {
                continue;
            };
            for (dest_id, dest_path, segs) in &dests {
                let Some(dest_text) = self.text(**dest_id) else {
                    continue;
                };
                if dest_text.is_empty() {
                    continue;
                }
                let target = if segs.is_empty() {
                    format!("crate::{name}")
                } else {
                    format!("crate::{}::{name}", segs.join("::"))
                };
                let prefix = if dest_text.ends_with('\n') {
                    "\n"
                } else {
                    "\n\n"
                };
                let Ok(len) = u32::try_from(dest_text.len()) else {
                    continue;
                };
                let (Ok(use_spec), Ok(add_spec)) = (
                    MutationSpec::replace(
                        src_path.clone(),
                        range,
                        format!("{vis_text}use {target};"),
                    ),
                    MutationSpec::insert((*dest_path).clone(), len, format!("{prefix}{moved}\n")),
                ) else {
                    continue;
                };
                out.push(Cand {
                    intent: IntentCall::MoveItem {
                        item: item_id.clone(),
                        to: (*dest_path).clone(),
                    },
                    specs: vec![use_spec, add_spec],
                    rationale: "move to a sibling module and re-export".to_owned(),
                });
            }
        }

        // Collapse shape: a `use` of an item of another module file is replaced by the item.
        for item in tree.items() {
            let ast::Item::Use(u) = &item else { continue };
            if !t.hits(u.syntax().text_range()) {
                continue;
            }
            // A comment right above the `use` is part of its node; it stays where it is.
            let Some(use_start) = after_leading_comments(u.syntax()) else {
                continue;
            };
            let use_end = u32::from(u.syntax().text_range().end());
            let use_text = &text[use_start as usize..use_end as usize];
            let Some((vis, mod_path, name)) = parse_crate_use(use_text) else {
                continue;
            };
            let Some(&m_id) = map.by_path.get(&(*krate, mod_path)) else {
                continue;
            };
            if m_id == t.file {
                continue;
            }
            let (Some(m_tree), Some(m_text), Some(m_path)) =
                (self.tree(m_id), self.text(m_id), self.by_file.get(&m_id))
            else {
                continue;
            };
            let Some(m_item) = m_tree.items().find(|i| {
                movable_name(i).as_deref() == Some(name.as_str())
                    && !blocks_move(i.syntax())
                    && !in_test_code(i.syntax())
            }) else {
                continue;
            };
            let node = m_item.syntax();
            let is = u32::from(node.text_range().start()) as usize;
            // The item runs to its line end: a comment after it on its last line moves with it.
            let Some(ie) =
                line_end_after_comment(&m_text, u32::from(node.text_range().end()) as usize)
            else {
                continue;
            };
            if is == 0 || m_text.as_bytes()[is - 1] != b'\n' {
                continue;
            }
            // The item with its visibility set to the one of the `use`.
            let kw = keyword_offset(node) as usize - is;
            let item_text = &m_text[is..ie];
            let old_end = visibility_of(node).map_or(kw, |v| {
                let after = u32::from(v.syntax().text_range().end()) as usize - is;
                let ws = item_text[after..]
                    .chars()
                    .take_while(|c| c.is_whitespace())
                    .map(char::len_utf8)
                    .sum::<usize>();
                after + ws
            });
            let restored = format!("{}{}{}", &item_text[..kw], vis, &item_text[old_end..]);
            let (Ok(item_id), Ok(use_range), Ok(del_range)) = (
                Ident::parse(&name),
                TextRange::new(use_start, use_end),
                TextRange::new(
                    u32::try_from(is - 1).unwrap_or(0),
                    u32::try_from(ie + 1).unwrap_or(0),
                ),
            ) else {
                continue;
            };
            let (Ok(rep), Ok(del)) = (
                MutationSpec::replace(src_path.clone(), use_range, restored),
                MutationSpec::delete(m_path.clone(), del_range),
            ) else {
                continue;
            };
            out.push(Cand {
                intent: IntentCall::MoveItem {
                    item: item_id,
                    to: src_path.clone(),
                },
                specs: vec![rep, del],
                rationale: "move back into the module that re-exports it".to_owned(),
            });
        }
        out
    }

    // ---- driver ----

    /// Runs one call unit of `kind` on `file`: the seeds of a target, or a call that reaches
    /// rust-analyzer (`call`: realizing one seed, or the candidates of a target). A panic inside
    /// it is recorded on the session and gives `T::default()` (no seeds, no candidates), so one
    /// bad shape costs that unit and not the other kinds' Blueprints nor the whole generation.
    /// The injected test fault fires only in `call` units.
    fn contained<T: Default>(
        &self,
        kind: IntentKind,
        file: FileId,
        call: bool,
        f: impl FnOnce() -> T,
    ) -> T {
        let fault = call && self.sess.expander_fault == Some(kind);
        let got = catch_unwind(AssertUnwindSafe(|| {
            assert!(!fault, "expander fault injected on {kind:?}");
            f()
        }));
        match got {
            Ok(v) => v,
            Err(p) => {
                self.sess
                    .expander_panics
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(ExpanderPanic {
                        kind: Some(kind),
                        path: self.by_file.get(&file).cloned(),
                        message: panic_message(p),
                    });
                T::default()
            }
        }
    }

    fn lazy<'e>(&'e self, kind: IntentKind, t: &TInfo) -> Box<dyn Iterator<Item = Blueprint> + 'e> {
        let file = t.file;
        match kind {
            IntentKind::RenameLocal => Box::new(
                self.contained(kind, file, false, || self.rename_seeds(t))
                    .into_iter()
                    .flat_map(move |s| {
                        self.contained(kind, s.file, true, || self.rename_realize(&s))
                    })
                    .filter_map(move |c| self.finish(c)),
            ),
            IntentKind::ExtractVariable => Box::new(
                self.contained(kind, file, false, || self.extract_var_seeds(t))
                    .into_iter()
                    .flat_map(move |s| {
                        self.contained(kind, s.file, true, || self.extract_var_realize(&s))
                    })
                    .filter_map(move |c| self.finish(c)),
            ),
            IntentKind::AddDerive => Box::new(
                self.contained(kind, file, true, || self.add_derive_cands(t))
                    .into_iter()
                    .filter_map(move |c| self.finish(c)),
            ),
            IntentKind::ExtractFunction => Box::new(
                self.contained(kind, file, false, || self.extract_fn_seeds(t))
                    .into_iter()
                    .flat_map(move |s| {
                        self.contained(kind, s.file, true, || self.extract_fn_realize(&s))
                    })
                    .filter_map(move |c| self.finish(c)),
            ),
            IntentKind::MoveItem => Box::new(
                self.contained(kind, file, true, || self.move_item_cands(t))
                    .into_iter()
                    .filter_map(move |c| self.finish(c)),
            ),
        }
    }

    fn run(&self, targets: &[Target], max: usize) -> Result<Vec<Blueprint>, String> {
        let infos: Vec<TInfo> = targets.iter().filter_map(|t| self.tinfo(t)).collect();
        let mut gens: Vec<Vec<std::iter::Fuse<Box<dyn Iterator<Item = Blueprint> + '_>>>> = infos
            .iter()
            .map(|t| {
                IntentKind::ALL
                    .iter()
                    .map(|k| self.lazy(*k, t).fuse())
                    .collect()
            })
            .collect();
        let mut out: Vec<Blueprint> = Vec::new();
        let mut seen = BTreeSet::new();
        'rounds: loop {
            let mut progressed = false;
            for per_target in &mut gens {
                for it in per_target {
                    if out.len() >= max || self.failed() {
                        break 'rounds;
                    }
                    if let Some(bp) = it.next() {
                        progressed = true;
                        if seen.insert(bp.id()) {
                            out.push(bp);
                        }
                    }
                }
            }
            if !progressed {
                break;
            }
        }
        if let Some(msg) = self.fail.borrow().clone() {
            return Err(msg);
        }
        Ok(out)
    }
}

/// Whether an expression is worth offering to extract-variable.
fn is_variable_shape(e: &ast::Expr) -> bool {
    match e {
        ast::Expr::CallExpr(_) | ast::Expr::MethodCallExpr(_) | ast::Expr::IndexExpr(_) => true,
        ast::Expr::BinExpr(b) => !matches!(b.op_kind(), Some(ast::BinaryOp::Assignment { .. })),
        ast::Expr::FieldExpr(f) => matches!(f.expr(), Some(ast::Expr::FieldExpr(_))),
        _ => false,
    }
}

/// The statement (or tail expression) of a block that an extracted `let` goes before. `None`
/// when the expression sits where hoisting it would change what runs: in a closure, a match arm,
/// a `while` condition or the right side of `&&` and `||`.
fn variable_anchor(e: &ast::Expr) -> Option<SyntaxNode> {
    let mut prev = e.syntax().clone();
    for a in e.syntax().ancestors().skip(1) {
        match a.kind() {
            SyntaxKind::CLOSURE_EXPR | SyntaxKind::MATCH_ARM | SyntaxKind::WHILE_EXPR => {
                return None;
            }
            SyntaxKind::BIN_EXPR => {
                let b = ast::BinExpr::cast(a.clone())?;
                let short_circuit = matches!(
                    b.op_kind(),
                    Some(ast::BinaryOp::LogicOp(ast::LogicOp::And | ast::LogicOp::Or))
                );
                if short_circuit && b.rhs().is_some_and(|r| r.syntax() == &prev) {
                    return None;
                }
            }
            SyntaxKind::STMT_LIST => return Some(prev),
            SyntaxKind::FN | SyntaxKind::CONST | SyntaxKind::STATIC => return None,
            _ => {}
        }
        prev = a;
    }
    None
}

/// Whether `e` is the whole initializer of a `let` of a plain name.
fn is_simple_let_init(e: &ast::Expr) -> bool {
    e.syntax()
        .parent()
        .and_then(ast::LetStmt::cast)
        .is_some_and(|l| {
            matches!(l.pat(), Some(ast::Pat::IdentPat(_)))
                && l.initializer().is_some_and(|i| i.syntax() == e.syntax())
        })
}

/// Whether extract-variable may be offered `e`. False for the value of a record-expression field
/// whose text does not begin with the field's name (an attribute or comment comes first):
/// rust-analyzer names the variable after the field's whole text, which is then no identifier,
/// and its syntax factory panics on it.
fn is_nameable_field_value(e: &ast::Expr) -> bool {
    let Some(field) = e.syntax().parent().and_then(ast::RecordExprField::cast) else {
        return true;
    };
    let Some(name) = field.name_ref() else {
        return true;
    };
    field.syntax().text().to_string().starts_with(name.text())
}

/// `(start of the field, field name)` when `e` is the value of a record-literal field `k: v`
/// whose text starts with its name (see [`is_nameable_field_value`]).
fn record_field_of(e: &ast::Expr) -> Option<(u32, String)> {
    let field = e.syntax().parent().and_then(ast::RecordExprField::cast)?;
    let name = field.name_ref()?;
    let start = u32::from(name.syntax().text_range().start());
    (start == u32::from(field.syntax().text_range().start()))
        .then(|| (start, name.text().to_string()))
}

/// Start of the code of `node` after the comments rust-analyzer attaches in front of it; `None`
/// when an attribute comes first.
fn after_leading_comments(node: &SyntaxNode) -> Option<u32> {
    let first = node
        .children_with_tokens()
        .find(|e| !matches!(e.kind(), SyntaxKind::COMMENT | SyntaxKind::WHITESPACE))?;
    (first.kind() != SyntaxKind::ATTR).then(|| u32::from(first.text_range().start()))
}

/// The end of the line `end` is on, when the rest of that line is empty or a `//` comment, as
/// in `const N: u32 = 3; // per slot`; `None` otherwise or without a line end.
fn line_end_after_comment(text: &str, end: usize) -> Option<usize> {
    let eol = end + text.get(end..)?.find('\n')?;
    let rest = text[end..eol].trim_start_matches([' ', '\t']);
    (rest.is_empty() || (rest.starts_with("//") && !rest.starts_with("///"))).then_some(eol)
}

/// Whether `e` is the left side of an assignment.
fn is_assign_target(e: &ast::Expr) -> bool {
    e.syntax()
        .parent()
        .and_then(ast::BinExpr::cast)
        .is_some_and(|b| {
            matches!(b.op_kind(), Some(ast::BinaryOp::Assignment { .. }))
                && b.lhs().is_some_and(|l| l.syntax() == e.syntax())
        })
}

/// The name of a top-level item that move-item can move.
fn movable_name(item: &ast::Item) -> Option<String> {
    match item {
        ast::Item::Fn(_)
        | ast::Item::Struct(_)
        | ast::Item::Enum(_)
        | ast::Item::Trait(_)
        | ast::Item::TypeAlias(_)
        | ast::Item::Const(_)
        | ast::Item::Static(_) => item
            .syntax()
            .children()
            .find_map(ast::Name::cast)
            .map(|n| n.text().to_string()),
        _ => None,
    }
}

/// `(name, start, end)` of an item move-item may move: on its own lines, no cfg or test attribute,
/// and hit by the target.
fn movable_item(item: &ast::Item, text: &str, t: &TInfo) -> Option<(String, u32, u32)> {
    let name = movable_name(item)?;
    let node = item.syntax();
    if name == "main" || blocks_move(node) || in_test_code(node) || !t.hits(node.text_range()) {
        return None;
    }
    let (s, e) = (
        u32::from(node.text_range().start()),
        u32::from(node.text_range().end()),
    );
    let starts_line = s == 0 || text.as_bytes().get(s as usize - 1) == Some(&b'\n');
    let ends_line = text[e as usize..]
        .chars()
        .next()
        .is_none_or(|c| c == '\n' || c == '\r');
    (starts_line && ends_line).then_some((name, s, e))
}

/// `(visibility with a trailing space or empty, module path, item)` of a statement
/// `[pub ]use crate::a::b::Item;`.
fn parse_crate_use(text: &str) -> Option<(String, Vec<String>, String)> {
    let i = text.find("use ")?;
    let vis = &text[..i];
    if !(vis.is_empty() || (vis.starts_with("pub") && vis.ends_with(' '))) {
        return None;
    }
    let path = text[i + 4..].strip_suffix(';')?;
    let mut segs = path.split("::");
    if segs.next()? != "crate" {
        return None;
    }
    let mut rest: Vec<&str> = segs.collect();
    let item = rest.pop()?;
    let all = rest.iter().chain(std::iter::once(&item));
    if all
        .clone()
        .any(|s| s.is_empty() || !s.chars().all(is_ident_char))
    {
        return None;
    }
    Some((
        vis.to_owned(),
        rest.into_iter().map(str::to_owned).collect(),
        item.to_owned(),
    ))
}

/// Runs the generation on the current database state; the views are dropped before it returns.
fn generate(
    sess: &RaSession,
    state_fp: StateFp,
    targets: &[Target],
    max: usize,
) -> Result<Vec<Blueprint>, ExpandError> {
    let got = catch_unwind(AssertUnwindSafe(|| {
        let env = Env::new(sess, state_fp);
        // hir_ty reads the database through a thread-local that `Analysis` sets for its own
        // queries; the `Semantics` calls of this file need it set too.
        ra_ap_hir::attach_db_allow_change(env.db, || env.run(targets, max))
    }));
    match got {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(msg)) => Err(ExpandError::Analysis(msg)),
        Err(p) => {
            let message = panic_message(p);
            sess.expander_panics
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(ExpanderPanic {
                    kind: None,
                    path: None,
                    message: message.clone(),
                });
            Err(ExpandError::Analysis(format!(
                "rust-analyzer panicked: {message}"
            )))
        }
    }
}

/// Blueprints for the five intents, from the code of a search state.
///
/// # Contract
/// - C1: every returned Blueprint passes the conformance check of its intent against the
///   texts of the state, for all five intents.
///   tested: tests/expand.rs every_expansion_conforms
/// - C2: rename-local proposes a rename of a local binding that replaces its definition and
///   every use, and no other change.
///   tested: tests/expand.rs rename_local_expansion_covers_all_uses
/// - C3: after `expand`, whatever the state, the text and baseline of the session are what
///   they were before, and a following `check` works.
///   tested: tests/expand.rs expansion_leaves_session_at_base
/// - C4: the specs, ranges and reads are those of the text of the state, not of the base.
///   tested: tests/expand.rs expansion_reads_the_given_state
/// - C5: at most `max` Blueprints come back, only within the given targets; no target gives
///   nothing.
///   tested: tests/expand.rs expansion_respects_max_and_targets
/// - C6: move-item proposes a move to another module of the crate as a re-export plus the item
///   in the destination, and the reverse of such a move; the reverse restores the text byte
///   for byte.
///   tested: tests/expand.rs move_item_forward_then_collapse_restores_the_text
/// - C7: extract-function names the new function after the last call in the extracted range or
///   the last name it binds, never with the placeholder rust-analyzer writes.
///   tested: tests/expand.rs extract_function_names_are_not_fun_name
/// - C8: ids are UUID version 7 and the same input gives the same ids; a different state gives
///   different ids.
///   tested: tests/expand.rs blueprint_ids_are_v7_and_deterministic
/// - C9: extract-variable never offers the value of a record field whose text does not start with
///   its name (rust-analyzer would build a name from the whole field text and panic); the other
///   expressions of the same function are still offered.
///   tested: tests/expand_panics.rs extract_variable_skips_the_value_of_an_attributed_record_field
/// - C10: extract-variable names the variable after the `let` that rust-analyzer's edit adds,
///   read from the edited text, so an expression inside the initializer of a `let` is extracted
///   too; the value of a record-literal field named like the variable becomes the shorthand.
///   tested: tests/expand_gen.rs extract_variable_restores_a_call_folded_into_a_let_initializer
///   tested: tests/expand_gen.rs extract_variable_restores_a_record_field_value_as_the_shorthand
///   tested: expand::tests::the_added_let_is_read_from_the_edited_text
/// - C11: the reverse move of C6 also restores an item with a comment line above its re-export
///   (the comment stays) and an item whose last line ends with a `//` comment (the comment moves
///   with it).
///   tested: tests/expand_gen.rs move_item_collapses_a_reexport_under_a_comment
///   tested: tests/expand_gen.rs move_item_collapses_an_item_with_a_trailing_comment
///   tested: expand::tests::a_trailing_line_comment_ends_the_item_at_its_line_end
impl Expander for RaAnalyzer {
    type Session = RaSession;

    fn expand(
        &self,
        s: &mut RaSession,
        state: &[Edit],
        targets: &[Target],
        max: usize,
    ) -> Result<Vec<Blueprint>, ExpandError> {
        if let Some(msg) = &s.poisoned {
            return Err(ExpandError::Session(CheckError::Poisoned(msg.clone())));
        }
        if targets.is_empty() || max == 0 {
            return Ok(Vec::new());
        }
        let state_fp = StateFp::of(state);
        if state.is_empty() {
            return generate(s, state_fp, targets, max);
        }

        let mut paths: Vec<RelPath> = state.iter().map(|e| e.path.clone()).collect();
        paths.sort();
        paths.dedup();
        let before = fingerprints(s, &paths);
        let mut f = self
            .fork(s)
            .map_err(|e| ExpandError::Session(map_error(e, &paths)))?;
        let applied = self.apply(s, &mut f, state);
        let out = applied.is_ok().then(|| generate(s, state_fp, targets, max));
        let discarded = self.discard(s, f);

        let after = fingerprints(s, &paths);
        if let Err(bad) = revert_intact(&before, &after) {
            s.poisoned = Some(format!(
                "text of {} file(s) differs after revert",
                bad.len()
            ));
            return Err(ExpandError::Session(CheckError::RevertMismatch {
                paths: bad,
            }));
        }
        if let Err(e) = applied {
            return Err(match e {
                AnalyzerError::Edit { path, reason } => {
                    ExpandError::Analysis(format!("state edit rejected: {path}: {reason}"))
                }
                other => ExpandError::Session(map_error(other, &paths)),
            });
        }
        if let Err(e) = discarded {
            return Err(ExpandError::Session(map_error(e, &paths)));
        }
        out.unwrap_or_else(|| Ok(Vec::new()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first call expression of `src` in a function body.
    fn first_call(src: &str) -> ast::Expr {
        let file = ast::SourceFile::parse(src, ra_ap_syntax::Edition::CURRENT).tree();
        file.syntax()
            .descendants()
            .filter_map(ast::CallExpr::cast)
            .map(ast::Expr::CallExpr)
            .find(|e| e.syntax().text() == "f()")
            .expect("a call f()")
    }

    #[test]
    fn a_field_value_behind_an_attribute_is_not_nameable() {
        let e = first_call("fn g() { S { #[cfg(x)] a: f() }; }");
        assert!(!is_nameable_field_value(&e));
    }

    #[test]
    fn the_added_let_is_read_from_the_edited_text() {
        let text = "    let fitted = fit(needed(v), limit);\n";
        // The shape of rust-analyzer's edit does not matter, only the text after it.
        let applied = "    let width = needed(v);\n    let fitted = fit(width, limit);\n";
        assert_eq!(
            added_let_name(text, applied, "needed(v)").as_deref(),
            Some("width")
        );
        // A statement that was there before is not the added one.
        let before = "    let width = needed(v);\n    g(needed(v));\n";
        let after = "    let width = needed(v);\n    let width1 = needed(v);\n    g(width1);\n";
        assert_eq!(
            added_let_name(before, after, "needed(v)").as_deref(),
            Some("width1")
        );
        // `let mut`, a name that only ends in `let`, and two new names give nothing.
        assert_eq!(added_let_name("", "let mut w = f();", "f()"), None);
        assert_eq!(added_let_name("", "outlet w = f();", "f()"), None);
        assert_eq!(
            added_let_name("", "let a = f();\nlet b = f();", "f()"),
            None
        );
    }

    #[test]
    fn a_trailing_line_comment_ends_the_item_at_its_line_end() {
        let text = "const N: u32 = 3; // per slot\nconst M: u32 = 4;\nfn f() {} g();\n";
        let n_end = text.find(';').unwrap() + 1;
        assert_eq!(line_end_after_comment(text, n_end), Some(29));
        let m_end = text.find("4;").unwrap() + 2;
        assert_eq!(line_end_after_comment(text, m_end), Some(m_end));
        // Code after the item on its line, a doc comment, or no line end: not an item line.
        let f_end = text.find("{}").unwrap() + 2;
        assert_eq!(line_end_after_comment(text, f_end), None);
        assert_eq!(
            line_end_after_comment("const N: u32 = 3; /// d\n", 17),
            None
        );
        assert_eq!(line_end_after_comment("const N: u32 = 3;", 17), None);
    }

    #[test]
    fn a_plain_field_value_and_a_non_field_expression_are_nameable() {
        assert!(is_nameable_field_value(&first_call(
            "fn g() { S { a: f() }; }"
        )));
        assert!(is_nameable_field_value(&first_call(
            "fn g() { let x = f(); }"
        )));
    }
}
