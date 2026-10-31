//! A syntax-only `Assessor`: it measures a child state against its parent with the parser alone,
//! so no session is needed and a call costs microseconds.
//!
//! The cleanliness cost is a weighted count of smells. The weights below are first guesses
//! (initial values 1, 2, 1, 3, 2, 1); the harness of the search (T4) tunes them later against
//! recall, so nothing outside this file may depend on their values.

use ra_ap_syntax::ast::{self, HasModuleItem, HasName};
use ra_ap_syntax::{AstNode, Edition, SourceFile, SyntaxKind};
use ryo_v2_core::app::ports::Assessor;
use ryo_v2_core::domain::{ChangeProbe, CleanDelta, Edit, ExtDelta, StaticMetrics};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Cost per line of a fn beyond [`LONG_FN_LINES`].
pub(crate) const COST_LONG_FN: i64 = 1;
/// Cost per block level of a fn beyond [`NESTING_LEVELS`].
pub(crate) const COST_NESTING: i64 = 2;
/// Cost per `if`, `match` arm or loop of a fn beyond [`BRANCHES`].
pub(crate) const COST_BRANCH: i64 = 1;
/// Cost per binding or fn with a bad name.
pub(crate) const COST_BAD_NAME: i64 = 3;
/// Cost per repeat of an expression text (at least [`DUP_EXPR_LEN`] chars) inside one fn.
pub(crate) const COST_DUP_EXPR: i64 = 2;
/// Cost per identifier of a top-level item that names an item of another touched file.
pub(crate) const COST_FOREIGN_REF: i64 = 1;

const LONG_FN_LINES: usize = 25;
const NESTING_LEVELS: usize = 3;
const BRANCHES: usize = 5;
const DUP_EXPR_LEN: usize = 12;
/// One-char names that are fine.
const SHORT_OK: [&str; 6] = ["i", "j", "k", "n", "x", "y"];
/// Names that say nothing.
const EMPTY_NAMES: [&str; 5] = ["tmp", "var_name", "fun_name", "foo", "bar"];

/// The `Assessor` of the rust-analyzer adapter: parser only, no session.
///
/// # Contract
/// - C1: `touched_files` counts the paths whose text differs, `touched_crates` the distinct
///   prefixes of them before the last `/src/` (or the empty prefix), and `edit_bytes` sums per
///   path the lengths of both texts minus twice the common prefix and twice the common suffix.
///   tested: assess::tests::touched_counts_files_crates_and_bytes
/// - C2: `clean` is the cost of the parent minus the cost of the child, so a state with fewer
///   bad names, shorter fns, fewer repeated expressions and fewer cross-file names is higher.
///   tested: assess::tests::bad_names_cost_clean
///   tested: assess::tests::long_fn_split_raises_clean
/// - C3: `extensibility` is, summed over the probes and the files, the number of sites in the
///   parent minus the number in the child: matches over an enum without a catch-all arm,
///   record literals and patterns of a struct without `..`, or `impl` blocks of a trait.
///   tested: assess::tests::enum_probe_counts_exhaustive_matches
///   tested: assess::tests::struct_probe_counts_literals_without_rest
/// - C4: without probes `extensibility` is zero.
///   tested: assess::tests::no_probes_no_extensibility
#[derive(Debug, Clone, Copy, Default)]
pub struct SyntaxAssessor;

impl Assessor for SyntaxAssessor {
    fn assess(&self, before: &[Edit], after: &[Edit], probes: &[ChangeProbe]) -> StaticMetrics {
        let pairs = pair_texts(before, after);
        let changed: Vec<&(String, String, String)> =
            pairs.iter().filter(|(_, b, a)| b != a).collect();

        let touched_files = u16::try_from(changed.len()).unwrap_or(u16::MAX);
        let crates: BTreeSet<&str> = changed.iter().map(|(p, _, _)| crate_prefix(p)).collect();
        let touched_crates = u8::try_from(crates.len()).unwrap_or(u8::MAX);
        let edit_bytes = changed
            .iter()
            .map(|(_, b, a)| edit_bytes(b, a))
            .fold(0u32, u32::saturating_add);

        let cost = |pick: fn(&(String, String, String)) -> &str| -> i64 {
            let parsed: Vec<(SourceFile, BTreeSet<String>)> = changed
                .iter()
                .map(|t| {
                    let file = parse(pick(t));
                    let decls = declared_names(&file);
                    (file, decls)
                })
                .collect();
            parsed
                .iter()
                .enumerate()
                .map(|(i, (file, own))| {
                    let foreign: BTreeSet<&str> = parsed
                        .iter()
                        .enumerate()
                        .filter(|(j, _)| *j != i)
                        .flat_map(|(_, (_, d))| d.iter().map(String::as_str))
                        .filter(|n| !own.contains(*n))
                        .collect();
                    file_cost(file, &foreign)
                })
                .sum()
        };
        let clean = cost(|t| &t.1) - cost(|t| &t.2);

        let sites = |pick: fn(&(String, String, String)) -> &str| -> i64 {
            pairs
                .iter()
                .map(|t| {
                    let file = parse(pick(t));
                    probes.iter().map(|p| sites_of(&file, p)).sum::<i64>()
                })
                .sum()
        };
        let extensibility = if probes.is_empty() {
            0
        } else {
            sites(|t| &t.1) - sites(|t| &t.2)
        };

        StaticMetrics {
            touched_files,
            touched_crates,
            edit_bytes,
            clean: CleanDelta(clamp_i32(clean)),
            extensibility: ExtDelta(clamp_i32(extensibility)),
        }
    }
}

fn clamp_i32(v: i64) -> i32 {
    i32::try_from(v).unwrap_or(if v < 0 { i32::MIN } else { i32::MAX })
}

/// `(path, before text, after text)` per path of either side; a missing side or a deleted file
/// is the empty text.
fn pair_texts(before: &[Edit], after: &[Edit]) -> Vec<(String, String, String)> {
    let text = |e: &Edit| e.new_text.clone().unwrap_or_default();
    let b: BTreeMap<&str, String> = before.iter().map(|e| (e.path.as_str(), text(e))).collect();
    let a: BTreeMap<&str, String> = after.iter().map(|e| (e.path.as_str(), text(e))).collect();
    let paths: BTreeSet<&str> = b.keys().chain(a.keys()).copied().collect();
    paths
        .into_iter()
        .map(|p| {
            (
                p.to_owned(),
                b.get(p).cloned().unwrap_or_default(),
                a.get(p).cloned().unwrap_or_default(),
            )
        })
        .collect()
}

/// The part of `path` before its last `/src/`; empty when it has none.
fn crate_prefix(path: &str) -> &str {
    match path.rfind("/src/") {
        Some(i) => &path[..i],
        None => "",
    }
}

fn edit_bytes(before: &str, after: &str) -> u32 {
    let (b, a) = (before.as_bytes(), after.as_bytes());
    let prefix = b.iter().zip(a).take_while(|(x, y)| x == y).count();
    let max_suffix = b.len().min(a.len()) - prefix;
    let suffix = b
        .iter()
        .rev()
        .zip(a.iter().rev())
        .take(max_suffix)
        .take_while(|(x, y)| x == y)
        .count();
    let total = (b.len() + a.len()).saturating_sub(2 * prefix + 2 * suffix);
    u32::try_from(total).unwrap_or(u32::MAX)
}

fn parse(text: &str) -> SourceFile {
    SourceFile::parse(text, Edition::Edition2024).tree()
}

/// Names of the top-level items of a file.
fn declared_names(file: &SourceFile) -> BTreeSet<String> {
    file.items()
        .filter_map(|item| {
            item.syntax()
                .children()
                .find_map(ast::Name::cast)
                .map(|n| n.text().to_string())
        })
        .collect()
}

fn is_bad_name(name: &str) -> bool {
    let name = name.strip_prefix("r#").unwrap_or(name);
    let mut cs = name.chars();
    let first = cs.next();
    let rest = cs.as_str();
    if first.is_some() && rest.is_empty() {
        return !SHORT_OK.contains(&name);
    }
    if EMPTY_NAMES.contains(&name) {
        return true;
    }
    first.is_some_and(|c| c.is_alphanumeric() || c == '_')
        && !rest.is_empty()
        && rest.chars().all(|c| c.is_ascii_digit())
}

fn fn_cost(f: &ast::Fn) -> i64 {
    let node = f.syntax();
    let mut cost = 0;

    let lines = node.text().to_string().lines().count();
    cost += COST_LONG_FN * lines.saturating_sub(LONG_FN_LINES) as i64;

    let depth = node
        .descendants()
        .filter(|n| n.kind() == SyntaxKind::BLOCK_EXPR)
        .map(|n| {
            n.ancestors()
                .take_while(|a| a != node)
                .filter(|a| a.kind() == SyntaxKind::BLOCK_EXPR)
                .count()
        })
        .max()
        .unwrap_or(0);
    cost += COST_NESTING * depth.saturating_sub(NESTING_LEVELS) as i64;

    let branches = node
        .descendants()
        .filter(|n| {
            matches!(
                n.kind(),
                SyntaxKind::IF_EXPR
                    | SyntaxKind::MATCH_ARM
                    | SyntaxKind::FOR_EXPR
                    | SyntaxKind::WHILE_EXPR
                    | SyntaxKind::LOOP_EXPR
            )
        })
        .count();
    cost += COST_BRANCH * branches.saturating_sub(BRANCHES) as i64;

    let bad_bindings = node
        .descendants()
        .filter_map(ast::IdentPat::cast)
        .filter_map(|p| p.name())
        .filter(|n| is_bad_name(n.text()))
        .count();
    let bad_fn = f.name().is_some_and(|n| is_bad_name(n.text()));
    cost += COST_BAD_NAME * (bad_bindings + usize::from(bad_fn)) as i64;

    let mut seen: HashMap<String, usize> = HashMap::new();
    for n in node.descendants() {
        if matches!(
            n.kind(),
            SyntaxKind::METHOD_CALL_EXPR
                | SyntaxKind::CALL_EXPR
                | SyntaxKind::BIN_EXPR
                | SyntaxKind::INDEX_EXPR
                | SyntaxKind::FIELD_EXPR
        ) {
            let t = n.text().to_string();
            if t.len() >= DUP_EXPR_LEN {
                *seen.entry(t).or_default() += 1;
            }
        }
    }
    let repeats: usize = seen.values().map(|c| c - 1).sum();
    cost + COST_DUP_EXPR * repeats as i64
}

/// Cost of one file; `foreign` are the names declared in the other touched files and not here.
fn file_cost(file: &SourceFile, foreign: &BTreeSet<&str>) -> i64 {
    let mut cost: i64 = file
        .syntax()
        .descendants()
        .filter_map(ast::Fn::cast)
        .map(|f| fn_cost(&f))
        .sum();
    if !foreign.is_empty() {
        for item in file.items() {
            let refs = item
                .syntax()
                .descendants()
                .filter_map(ast::NameRef::cast)
                .filter(|r| foreign.contains(r.text()))
                .count();
            cost += COST_FOREIGN_REF * refs as i64;
        }
    }
    cost
}

/// Last segment of a path as text.
fn last_segment(path: &ast::Path) -> Option<String> {
    path.segment()
        .and_then(|s| s.name_ref())
        .map(|n| n.text().to_string())
}

/// First segment of a path as text.
fn first_segment(path: &ast::Path) -> Option<String> {
    path.first_segment()
        .and_then(|s| s.name_ref())
        .map(|n| n.text().to_string())
}

/// Whether an arm pattern (or one alternative of it) accepts everything.
fn is_catch_all(p: &ast::Pat) -> bool {
    match p {
        ast::Pat::WildcardPat(_) => true,
        ast::Pat::IdentPat(i) => i.pat().is_none(),
        ast::Pat::OrPat(o) => o.pats().any(|q| is_catch_all(&q)),
        ast::Pat::ParenPat(q) => q.pat().is_some_and(|q| is_catch_all(&q)),
        _ => false,
    }
}

/// The path an arm pattern starts with, through `|` alternatives.
fn pat_paths(p: &ast::Pat, out: &mut Vec<ast::Path>) {
    match p {
        ast::Pat::PathPat(q) => out.extend(q.path()),
        ast::Pat::TupleStructPat(q) => out.extend(q.path()),
        ast::Pat::RecordPat(q) => out.extend(q.path()),
        ast::Pat::OrPat(o) => o.pats().for_each(|q| pat_paths(&q, out)),
        ast::Pat::ParenPat(q) => {
            if let Some(q) = q.pat() {
                pat_paths(&q, out);
            }
        }
        _ => {}
    }
}

/// Number of places a change of kind `probe` has to touch in `file`.
fn sites_of(file: &SourceFile, probe: &ChangeProbe) -> i64 {
    let root = file.syntax();
    match probe {
        ChangeProbe::AddVariant { ty } => root
            .descendants()
            .filter_map(ast::MatchExpr::cast)
            .filter(|m| {
                let arms: Vec<ast::MatchArm> = m
                    .match_arm_list()
                    .map(|l| l.arms().collect())
                    .unwrap_or_default();
                let names_ty = arms.iter().any(|a| {
                    let mut paths = Vec::new();
                    if let Some(p) = a.pat() {
                        pat_paths(&p, &mut paths);
                    }
                    paths.iter().any(|p| {
                        p.qualifier().is_some() && first_segment(p).as_deref() == Some(ty.as_str())
                    })
                });
                let catch_all = arms
                    .iter()
                    .any(|a| a.guard().is_none() && a.pat().is_some_and(|p| is_catch_all(&p)));
                names_ty && !catch_all
            })
            .count() as i64,
        ChangeProbe::AddField { ty } => {
            let lits = root
                .descendants()
                .filter_map(ast::RecordExpr::cast)
                .filter(|r| r.path().and_then(|p| last_segment(&p)).as_deref() == Some(ty.as_str()))
                .filter(|r| {
                    r.record_expr_field_list()
                        .is_some_and(|l| l.dotdot_token().is_none() && l.spread().is_none())
                })
                .count();
            let pats = root
                .descendants()
                .filter_map(ast::RecordPat::cast)
                .filter(|r| r.path().and_then(|p| last_segment(&p)).as_deref() == Some(ty.as_str()))
                .filter(|r| {
                    r.record_pat_field_list()
                        .is_some_and(|l| l.rest_pat().is_none())
                })
                .count();
            (lits + pats) as i64
        }
        ChangeProbe::AddMethod { ty } => root
            .descendants()
            .filter_map(ast::Impl::cast)
            .filter(|i| match i.trait_() {
                Some(ast::Type::PathType(t)) => {
                    t.path().and_then(|p| last_segment(&p)).as_deref() == Some(ty.as_str())
                }
                _ => false,
            })
            .count() as i64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryo_v2_core::domain::{Ident, RelPath};

    fn edit(path: &str, text: &str) -> Edit {
        Edit::write(RelPath::parse(path).unwrap(), text)
    }

    fn one(before: &str, after: &str, probes: &[ChangeProbe]) -> StaticMetrics {
        SyntaxAssessor.assess(
            &[edit("src/lib.rs", before)],
            &[edit("src/lib.rs", after)],
            probes,
        )
    }

    fn ty(s: &str) -> Ident {
        Ident::parse(s).unwrap()
    }

    #[test]
    fn bad_names_cost_clean() {
        let before = "fn run() {\n    let tmp = 1;\n    let v1 = 2;\n    let x = tmp + v1;\n}\n";
        let after =
            "fn run() {\n    let width = 1;\n    let height = 2;\n    let x = width + height;\n}\n";
        let m = one(before, after, &[]);
        assert_eq!(m.clean, CleanDelta((2 * COST_BAD_NAME) as i32));
        assert_eq!(
            one(after, before, &[]).clean,
            CleanDelta(-(2 * COST_BAD_NAME) as i32)
        );
        assert!(is_bad_name("a") && is_bad_name("x2") && is_bad_name("var_name"));
        assert!(!is_bad_name("i") && !is_bad_name("width") && !is_bad_name("_x"));
    }

    fn body(prefix: &str, n: usize) -> String {
        (0..n)
            .map(|i| format!("    let {prefix}_{i} = {i};\n"))
            .collect()
    }

    #[test]
    fn long_fn_split_raises_clean() {
        // 40 lines: 15 beyond the limit.
        let before = format!("fn run() {{\n{}}}\n", body("value", 38));
        let after = format!(
            "fn run() {{\n    part_a();\n    part_b();\n}}\n\nfn part_a() {{\n{}}}\n\nfn part_b() {{\n{}}}\n",
            body("left", 19),
            body("right", 19)
        );
        let m = one(&before, &after, &[]);
        assert_eq!(m.clean, CleanDelta((15 * COST_LONG_FN) as i32));
    }

    #[test]
    fn enum_probe_counts_exhaustive_matches() {
        let before = "fn a(s: Shape) -> u8 {\n    match s {\n        Shape::Circle => 1,\n        Shape::Square => 2,\n    }\n}\n\
                      fn b(s: Shape) -> u8 {\n    match s {\n        Shape::Circle => 1,\n        _ => 0,\n    }\n}\n";
        // The exhaustive match gained a catch-all: one site less to touch on a new variant.
        let after = "fn a(s: Shape) -> u8 {\n    match s {\n        Shape::Circle => 1,\n        _ => 2,\n    }\n}\n\
                     fn b(s: Shape) -> u8 {\n    match s {\n        Shape::Circle => 1,\n        _ => 0,\n    }\n}\n";
        let probes = [ChangeProbe::AddVariant { ty: ty("Shape") }];
        assert_eq!(one(before, after, &probes).extensibility, ExtDelta(1));
        assert_eq!(one(after, before, &probes).extensibility, ExtDelta(-1));
        let other = [ChangeProbe::AddVariant { ty: ty("Color") }];
        assert_eq!(one(before, after, &other).extensibility, ExtDelta(0));
        let m = [ChangeProbe::AddMethod { ty: ty("Draw") }];
        let with_impl = format!("{before}impl Draw for Shape {{}}\n");
        assert_eq!(one(&with_impl, before, &m).extensibility, ExtDelta(1));
    }

    #[test]
    fn struct_probe_counts_literals_without_rest() {
        let before = "fn a() -> P {\n    P { x: 1, y: 2 }\n}\nfn b(p: P) -> u8 {\n    let P { x, y } = p;\n    x + y\n}\n";
        let after = "fn a() -> P {\n    P { x: 1, ..P::default() }\n}\nfn b(p: P) -> u8 {\n    let P { x, .. } = p;\n    x\n}\n";
        let probes = [ChangeProbe::AddField { ty: ty("P") }];
        assert_eq!(one(before, after, &probes).extensibility, ExtDelta(2));
    }

    #[test]
    fn touched_counts_files_crates_and_bytes() {
        let before = [
            edit("crates/a/src/lib.rs", "abcdef"),
            edit("crates/a/src/x.rs", "same"),
            edit("crates/b/src/lib.rs", "0123456789"),
            edit("build.rs", "fn main() {}"),
        ];
        let after = [
            edit("crates/a/src/lib.rs", "abXYef"),
            edit("crates/a/src/x.rs", "same"),
            edit("crates/b/src/lib.rs", "0123456789!!"),
            edit("build.rs", "fn main() {}\n"),
        ];
        let m = SyntaxAssessor.assess(&before, &after, &[]);
        assert_eq!(m.touched_files, 3);
        // crates/a, crates/b and "" (build.rs has no /src/)
        assert_eq!(m.touched_crates, 3);
        // abcdef -> abXYef: 2 + 2; 0123456789 -> ...!!: 2; build.rs: 1
        assert_eq!(m.edit_bytes, 4 + 2 + 1);
        assert_eq!(edit_bytes("aa", "aaa"), 1);
        assert_eq!(edit_bytes("abab", "ab"), 2);
        assert_eq!(edit_bytes("same", "same"), 0);
    }

    #[test]
    fn no_probes_no_extensibility() {
        let before =
            "fn a(s: Shape) -> u8 {\n    match s {\n        Shape::Circle => 1,\n    }\n}\n";
        let after = "fn a(s: Shape) -> u8 {\n    match s {\n        Shape::Circle => 1,\n        _ => 0,\n    }\n}\n";
        assert_eq!(one(before, after, &[]).extensibility, ExtDelta(0));
    }
}
