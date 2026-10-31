//! A deterministic, line-based generator that breaks code, and the corpus
//! cases built from its output.
//!
//! The generator is pure: it reads the sources it is given and returns the
//! whole new text of one file. Only [`read_sources`] and [`write_case`] touch
//! the file system. Nothing here uses randomness or time, so the same input
//! always gives the same cases.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use ryo_v2_core::domain::{Diagnostic, DomainError, RelPath};

use crate::case::parse_expected;
use crate::groups::Group;

/// The kind of a breaking edit. `id()` is kebab-case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// Delete a `use` line.
    RemoveImport,
    /// Rename a function (`fn x(` to `fn x_x(`).
    RenameSymbol,
    /// Comment out an `impl Trait for Type { .. }` block.
    DropTraitImpl,
    /// Turn `<T: Bound>` into `<T>`.
    RemoveBound,
    /// Delete a struct field line.
    RemoveField,
    /// Change a `-> u32` return type to `-> String`.
    ChangeType,
    /// Make a `pub fn` private.
    CallPrivate,
    /// Misspell a struct name at a use site.
    TypoType,
    /// Anchored: a use after a move.
    UseAfterMove,
    /// Anchored: two live mutable borrows.
    DoubleMutBorrow,
    /// Anchored: a mutation while a shared borrow lives.
    SharedThenMut,
    /// Anchored: a move while borrowed.
    MoveWhileBorrowed,
    /// Anchored: an assignment while borrowed.
    AssignWhileBorrowed,
    /// Anchored: a constant that fails to evaluate.
    ConstEval,
    /// Anchored: a reference to a cfg-disabled item.
    CfgGate,
}

impl Kind {
    /// Every kind, in declaration order.
    pub const ALL: [Kind; 15] = [
        Kind::RemoveImport,
        Kind::RenameSymbol,
        Kind::DropTraitImpl,
        Kind::RemoveBound,
        Kind::RemoveField,
        Kind::ChangeType,
        Kind::CallPrivate,
        Kind::TypoType,
        Kind::UseAfterMove,
        Kind::DoubleMutBorrow,
        Kind::SharedThenMut,
        Kind::MoveWhileBorrowed,
        Kind::AssignWhileBorrowed,
        Kind::ConstEval,
        Kind::CfgGate,
    ];

    /// The kebab-case id used in case ids, anchors and on the command line.
    pub fn id(self) -> &'static str {
        match self {
            Kind::RemoveImport => "remove-import",
            Kind::RenameSymbol => "rename-symbol",
            Kind::DropTraitImpl => "drop-trait-impl",
            Kind::RemoveBound => "remove-bound",
            Kind::RemoveField => "remove-field",
            Kind::ChangeType => "change-type",
            Kind::CallPrivate => "call-private",
            Kind::TypoType => "typo-type",
            Kind::UseAfterMove => "use-after-move",
            Kind::DoubleMutBorrow => "double-mut-borrow",
            Kind::SharedThenMut => "shared-then-mut",
            Kind::MoveWhileBorrowed => "move-while-borrowed",
            Kind::AssignWhileBorrowed => "assign-while-borrowed",
            Kind::ConstEval => "const-eval",
            Kind::CfgGate => "cfg-gate",
        }
    }

    /// The kind with this id, if any.
    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.id() == s)
    }

    /// The group written into the case file at gen time; regroup rewrites it
    /// from the expected diagnostics.
    pub fn default_group(self) -> Group {
        match self {
            Kind::RemoveImport => Group::UnresolvedImport,
            Kind::RenameSymbol => Group::UnresolvedName,
            Kind::DropTraitImpl | Kind::RemoveBound => Group::TraitBound,
            Kind::RemoveField => Group::Field,
            Kind::ChangeType => Group::TypeMismatch,
            Kind::CallPrivate => Group::Privacy,
            Kind::TypoType => Group::UnresolvedType,
            Kind::UseAfterMove
            | Kind::DoubleMutBorrow
            | Kind::SharedThenMut
            | Kind::MoveWhileBorrowed
            | Kind::AssignWhileBorrowed => Group::Borrow,
            Kind::ConstEval => Group::ConstEval,
            Kind::CfgGate => Group::Cfg,
        }
    }

    /// `Some` only for [`Kind::CfgGate`] ([`Group::Cfg`]); regroup leaves it alone.
    pub fn fixed_group(self) -> Option<Group> {
        (self == Kind::CfgGate).then_some(Group::Cfg)
    }

    /// Whether the kind edits a `// @brk <kind>: <payload>` anchor.
    fn is_anchored(self) -> bool {
        matches!(
            self,
            Kind::UseAfterMove
                | Kind::DoubleMutBorrow
                | Kind::SharedThenMut
                | Kind::MoveWhileBorrowed
                | Kind::AssignWhileBorrowed
                | Kind::ConstEval
                | Kind::CfgGate
        )
    }
}

/// One `.rs` file of the base workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    /// Path relative to the workspace root.
    pub path: RelPath,
    /// The file's text.
    pub text: String,
}

/// A line a kind can edit (1-based).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Site {
    /// The file.
    pub path: RelPath,
    /// The 1-based line.
    pub line: u32,
}

/// One generated case: the whole new text of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Breakage {
    /// The kind of the edit.
    pub kind: Kind,
    /// The seed that picked the site.
    pub seed: u32,
    /// The edited line.
    pub site: Site,
    /// The new text of `site.path`.
    pub new_text: String,
    /// A short description of the edit.
    pub note: String,
}

/// Why a file could not be read or a case could not be written.
#[derive(Debug, thiserror::Error)]
pub enum BreakageError {
    /// A file or directory could not be read or written.
    #[error("io {path}: {source}")]
    Io {
        /// The path.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A path under the workspace is not a workspace-relative path.
    #[error(transparent)]
    Path(#[from] DomainError),
    /// An argument is well-formed but wrong.
    #[error("{path}: {reason}")]
    Invalid {
        /// The path.
        path: PathBuf,
        /// What is wrong.
        reason: String,
    },
}

/// Classes whose errors rust-analyzer is not expected to report come first.
///
/// A case with mixed codes cannot pass anyway, so putting the G-B candidates
/// (borrow, const-eval, unresolved-type, privacy, field, arity) ahead of the
/// primary groups keeps that failure out of the primary groups' recall.
pub const REGROUP_PRIORITY: [Group; 11] = [
    Group::Borrow,
    Group::ConstEval,
    Group::UnresolvedType,
    Group::Privacy,
    Group::Field,
    Group::Arity,
    Group::TraitBound,
    Group::TypeMismatch,
    Group::NoMethod,
    Group::UnresolvedName,
    Group::UnresolvedImport,
];

/// The group of a case from its expected diagnostics.
///
/// A fixed group wins; no expected error is [`Group::Valid`]; otherwise each
/// code maps to its group (first match in [`Group::ALL`] order, cause groups
/// ([`Group::is_cause`]) excluded, unknown codes ignored) and the first group present in
/// [`REGROUP_PRIORITY`] is taken, falling back to the kind's default group.
pub fn group_for(kind: Kind, expected: &[Diagnostic]) -> Group {
    if let Some(g) = kind.fixed_group() {
        return g;
    }
    if expected.is_empty() {
        return Group::Valid;
    }
    let present: BTreeSet<Group> = expected
        .iter()
        .filter_map(|d| {
            Group::ALL
                .into_iter()
                .filter(|g| !g.is_cause())
                .find(|g| g.codes().contains(&d.code.as_str()))
        })
        .collect();
    REGROUP_PRIORITY
        .into_iter()
        .find(|g| present.contains(g))
        .unwrap_or_else(|| kind.default_group())
}

/// The sites of `kind` in `files`, in (path, line) order.
pub fn sites(kind: Kind, files: &[SourceFile]) -> Vec<Site> {
    let names = if kind == Kind::TypoType {
        declared_names(files)
    } else {
        BTreeSet::new()
    };
    let mut out = Vec::new();
    for f in files {
        let p = Parsed::new(&f.text);
        for i in 0..p.lines.len() {
            if edit_line(kind, &p, i, &names).is_some() {
                out.push(Site {
                    path: f.path.clone(),
                    line: i as u32 + 1,
                });
            }
        }
    }
    out.sort();
    out
}

/// The case for `seed`. With `len = sites.len()`, a seed `>= len` is `None`.
/// Otherwise the search starts at `idx = (seed * 7) % len` and steps `+1`
/// (mod `len`) past any index an earlier seed (`0..seed`) took.
pub fn generate(kind: Kind, seed: u32, files: &[SourceFile]) -> Option<Breakage> {
    let all = sites(kind, files);
    let len = all.len();
    if seed as usize >= len {
        return None;
    }
    let mut taken: Vec<usize> = Vec::new();
    let mut idx = 0;
    for s in 0..=u64::from(seed) {
        idx = (s * 7 % len as u64) as usize;
        while taken.contains(&idx) {
            idx = (idx + 1) % len;
        }
        taken.push(idx);
    }
    let site = all.get(idx)?.clone();
    let file = files.iter().find(|f| f.path == site.path)?;
    let names = if kind == Kind::TypoType {
        declared_names(files)
    } else {
        BTreeSet::new()
    };
    let p = Parsed::new(&file.text);
    let e = edit_line(kind, &p, site.line as usize - 1, &names)?;
    let new_text = format!(
        "{}{}{}",
        p.lines[..e.start].concat(),
        e.replacement,
        p.lines[e.end..].concat()
    );
    Some(Breakage {
        kind,
        seed,
        site,
        new_text,
        note: e.note,
    })
}

/// `.rs` files under `ws`, skipping `target` `cases` `expected` `breakage`
/// `overlays` `.git` `.ryo`, in path order.
pub fn read_sources(ws: &Path) -> Result<Vec<SourceFile>, BreakageError> {
    const SKIP: [&str; 7] = [
        "target", "cases", "expected", "breakage", "overlays", ".git", ".ryo",
    ];
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source| BreakageError::Io { path, source }
    };
    let mut out = Vec::new();
    let mut stack = vec![ws.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).map_err(io(&dir))? {
            let path = entry.map_err(io(&dir))?.path();
            if path.is_dir() {
                if path
                    .file_name()
                    .is_some_and(|n| SKIP.iter().any(|s| n == *s))
                {
                    continue;
                }
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let Ok(rel) = path.strip_prefix(ws) else {
                    continue;
                };
                let rel = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                let text = std::fs::read_to_string(&path).map_err(io(&path))?;
                out.push(SourceFile {
                    path: RelPath::parse(&rel)?,
                    text,
                });
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Writes `<corpus>/<ws_rel>/breakage/<kind>-<seed>/<site.path>` (the whole
/// new text) and `<corpus>/<ws_rel>/cases/<kind>-<seed>.toml`. Overwrites.
/// Returns the toml path.
///
/// The `group` line of the toml is, in order: the group of the existing toml
/// (when the file exists and has a parseable `group = "<id>"` line); else
/// [`group_for`] when `expected/<kind>-<seed>.check.json` exists and parses;
/// else [`Kind::default_group`]. Everything else is rewritten as before.
pub fn write_case(corpus: &Path, ws_rel: &str, b: &Breakage) -> Result<PathBuf, BreakageError> {
    if RelPath::parse(ws_rel).is_err() {
        return Err(BreakageError::Invalid {
            path: PathBuf::from(ws_rel),
            reason: "workspace is not a corpus-relative path".into(),
        });
    }
    let id = format!("{}-{}", b.kind.id(), b.seed);
    let ws = corpus.join(ws_rel);
    let write = |path: &Path, text: &str| -> Result<(), BreakageError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| BreakageError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        std::fs::write(path, text).map_err(|source| BreakageError::Io {
            path: path.to_path_buf(),
            source,
        })
    };
    write(
        &ws.join("breakage").join(&id).join(b.site.path.as_str()),
        &b.new_text,
    )?;
    let note = b.note.replace(['\n', '\r'], " ");
    let toml_path = ws.join("cases").join(format!("{id}.toml"));
    let group = existing_group(&toml_path)
        .or_else(|| {
            let text =
                std::fs::read_to_string(ws.join("expected").join(format!("{id}.check.json")))
                    .ok()?;
            let expected = parse_expected(&text).ok()?;
            Some(group_for(b.kind, &expected))
        })
        .unwrap_or_else(|| b.kind.default_group());
    let toml = format!(
        "# breakage: {}:{} {note}\nworkspace = \"{ws_rel}\"\noverlay = \"{ws_rel}/breakage/{id}\"\ngroup = \"{}\"\norigin = \"breakage:{}:{}\"\n",
        b.site.path,
        b.site.line,
        group.id(),
        b.kind.id(),
        b.seed,
    );
    write(&toml_path, &toml)?;
    Ok(toml_path)
}

/// The group of an existing case toml: its `group = "<id>"` line, if the file
/// exists and the id is a known group.
fn existing_group(toml: &Path) -> Option<Group> {
    let text = std::fs::read_to_string(toml).ok()?;
    text.lines().find_map(|l| {
        let id = l.trim().strip_prefix("group = \"")?.strip_suffix('"')?;
        Group::parse(id)
    })
}

/// A file split into lines, with the regions no kind edits marked.
struct Parsed<'a> {
    /// `split_inclusive('\n')`, so the line terminator survives.
    lines: Vec<&'a str>,
    /// From the first `#[cfg(test)]` line to the end of the file.
    in_test: Vec<bool>,
    /// Inside a `trait X {` or an `impl .. for .. {` block (header included).
    in_block: Vec<bool>,
    /// Strictly inside a `struct X {` block.
    in_struct: Vec<bool>,
}

/// One edit of a run of lines.
struct LineEdit {
    start: usize,
    end: usize,
    replacement: String,
    note: String,
}

fn body(line: &str) -> &str {
    line.trim_end_matches(['\n', '\r'])
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn is_trait_header(t: &str) -> bool {
    (t.starts_with("trait ") || t.starts_with("pub trait ")) && t.ends_with('{')
}

fn is_impl_for_header(t: &str) -> bool {
    t.starts_with("impl") && t.ends_with('{') && t.contains(" for ")
}

fn is_struct_header(t: &str) -> bool {
    (t.starts_with("struct ") || t.starts_with("pub struct ")) && t.ends_with('{')
}

/// The index of the first line after `start` that is `}` at the indent of `start`.
fn block_end(lines: &[&str], start: usize) -> Option<usize> {
    let head = body(lines.get(start)?);
    let indent = &head[..head.len() - head.trim_start().len()];
    let target = format!("{indent}}}");
    (start + 1..lines.len()).find(|&j| body(lines[j]).trim_end() == target)
}

impl<'a> Parsed<'a> {
    fn new(text: &'a str) -> Self {
        let lines: Vec<&str> = text.split_inclusive('\n').collect();
        let n = lines.len();
        let test_start = lines
            .iter()
            .position(|l| body(l).trim() == "#[cfg(test)]")
            .unwrap_or(n);
        let mut in_test = vec![false; n];
        in_test[test_start..].fill(true);
        let mut in_block = vec![false; n];
        let mut in_struct = vec![false; n];
        for (i, l) in lines.iter().enumerate().take(test_start) {
            let t = body(l).trim();
            if is_trait_header(t) || is_impl_for_header(t) {
                if let Some(end) = block_end(&lines, i) {
                    in_block[i..=end].fill(true);
                }
            } else if is_struct_header(t)
                && let Some(end) = block_end(&lines, i)
            {
                in_struct[i + 1..end].fill(true);
            }
        }
        Self {
            lines,
            in_test,
            in_block,
            in_struct,
        }
    }
}

/// The first whole-word occurrence of `word` in `text`.
fn find_word(text: &str, word: &str) -> Option<usize> {
    text.match_indices(word).map(|(i, _)| i).find(|&i| {
        let before = text[..i].chars().next_back();
        let after = text[i + word.len()..].chars().next();
        !before.is_some_and(is_ident_char) && !after.is_some_and(is_ident_char)
    })
}

/// The identifier after `struct ` / `pub struct `, if the line declares one.
fn struct_decl_name(t: &str) -> Option<String> {
    let rest = t
        .strip_prefix("pub struct ")
        .or_else(|| t.strip_prefix("struct "))?;
    let name: String = rest.chars().take_while(|c| is_ident_char(*c)).collect();
    (!name.is_empty()).then_some(name)
}

/// Every struct name declared outside a test region.
fn declared_names(files: &[SourceFile]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for f in files {
        let p = Parsed::new(&f.text);
        for (i, l) in p.lines.iter().enumerate() {
            if !p.in_test[i]
                && let Some(n) = struct_decl_name(body(l).trim())
            {
                names.insert(n);
            }
        }
    }
    names
}

/// The byte range of the identifier in the first `fn <ident>(` of `text`.
fn fn_ident(text: &str) -> Option<(usize, usize)> {
    text.match_indices("fn ").find_map(|(i, _)| {
        if text[..i].chars().next_back().is_some_and(is_ident_char) {
            return None;
        }
        let start = i + 3;
        let len: usize = text[start..]
            .chars()
            .take_while(|c| is_ident_char(*c))
            .map(char::len_utf8)
            .sum();
        (len > 0 && text[start + len..].starts_with('(')).then_some((start, start + len))
    })
}

/// The edit `kind` makes at line `i` (0-based), if the line is a site.
fn edit_line(kind: Kind, p: &Parsed, i: usize, names: &BTreeSet<String>) -> Option<LineEdit> {
    if p.in_test[i] {
        return None;
    }
    let raw = p.lines[i];
    let text = body(raw);
    let eol = &raw[text.len()..];
    let t = text.trim();
    let one = |replacement: String, note: String| {
        Some(LineEdit {
            start: i,
            end: i + 1,
            replacement,
            note,
        })
    };

    if kind.is_anchored() {
        if p.in_block[i] {
            return None;
        }
        let payload = t
            .strip_prefix("// @brk ")?
            .strip_prefix(kind.id())?
            .strip_prefix(": ")?;
        let indent = &text[..text.len() - text.trim_start().len()];
        return one(
            format!("{indent}{payload}{eol}"),
            format!("activate {}: {payload}", kind.id()),
        );
    }
    if t.starts_with("//") {
        return None;
    }
    if kind != Kind::DropTraitImpl && p.in_block[i] {
        return None;
    }

    match kind {
        Kind::RemoveImport => {
            if !((t.starts_with("use ") || t.starts_with("pub use ")) && t.ends_with(';')) {
                return None;
            }
            one(String::new(), format!("remove `{t}`"))
        }
        Kind::RenameSymbol => {
            if t.ends_with(';') {
                return None;
            }
            let (s, e) = fn_ident(text)?;
            let name = &text[s..e];
            one(
                format!("{}_x{}{eol}", &text[..e], &text[e..]),
                format!("rename fn {name} -> {name}_x"),
            )
        }
        Kind::DropTraitImpl => {
            if !is_impl_for_header(t) {
                return None;
            }
            let end = block_end(&p.lines, i)?;
            let replacement: String = p.lines[i..=end].iter().map(|l| format!("// {l}")).collect();
            Some(LineEdit {
                start: i,
                end: end + 1,
                replacement,
                note: format!("comment out `{}`", t.trim_end_matches('{').trim()),
            })
        }
        Kind::RemoveBound => {
            if !(t.contains("fn ") || t.starts_with("impl")) {
                return None;
            }
            let lt = text.find('<')?;
            let gt = lt + text[lt..].find('>')?;
            let inner = &text[lt + 1..gt];
            if inner.contains(['<', ',', '=']) || text[..gt].ends_with('-') {
                return None;
            }
            let ident: String = inner.chars().take_while(|c| is_ident_char(*c)).collect();
            if !ident.chars().next().is_some_and(char::is_alphabetic)
                || !inner[ident.len()..].starts_with(':')
            {
                return None;
            }
            one(
                format!("{}{ident}{}{eol}", &text[..=lt], &text[gt..]),
                format!("remove bound `{inner}`"),
            )
        }
        Kind::RemoveField => {
            if !p.in_struct[i] {
                return None;
            }
            let field = t.strip_suffix(',')?;
            let field = field.strip_prefix("pub ").unwrap_or(field);
            let (ident, ty) = field.split_once(": ")?;
            if ident.is_empty() || !ident.chars().all(is_ident_char) || ty.trim().is_empty() {
                return None;
            }
            one(String::new(), format!("remove field {ident}"))
        }
        Kind::ChangeType => {
            if !t.contains("fn ") {
                return None;
            }
            let at = text.find("-> u32 {")?;
            let name = fn_ident(text).map_or("?", |(s, e)| &text[s..e]);
            one(
                format!(
                    "{}-> String{}{eol}",
                    &text[..at],
                    &text[at + "-> u32".len()..]
                ),
                format!("change return type of fn {name} to String"),
            )
        }
        Kind::CallPrivate => {
            if !t.starts_with("pub fn ") {
                return None;
            }
            let at = text.find("pub fn")?;
            let name = fn_ident(text).map_or("?", |(s, e)| &text[s..e]);
            one(
                format!("{}fn{}{eol}", &text[..at], &text[at + "pub fn".len()..]),
                format!("make fn {name} private"),
            )
        }
        Kind::TypoType => {
            let decl = struct_decl_name(t);
            let name = names
                .iter()
                .find(|n| Some(n.as_str()) != decl.as_deref() && find_word(text, n).is_some())?;
            let at = find_word(text, name)? + name.len();
            let last = name.chars().next_back()?;
            one(
                format!("{}{last}{}{eol}", &text[..at], &text[at..]),
                format!("typo {name} -> {name}{last}"),
            )
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryo_v2_core::domain::ErrorCode;

    fn src(text: &str) -> Vec<SourceFile> {
        vec![SourceFile {
            path: RelPath::parse("a/src/lib.rs").unwrap(),
            text: text.into(),
        }]
    }

    fn d(code: &str) -> Diagnostic {
        Diagnostic {
            code: ErrorCode::parse(code).unwrap(),
            path: RelPath::parse("a/src/lib.rs").unwrap(),
            line: 1,
        }
    }

    const SAMPLE: &str = "use std::fmt;
pub trait Area {
    fn area(&self) -> u32;
}
pub struct Rect {
    pub w: u32,
}
impl Area for Rect {
    fn area(&self) -> u32 {
        self.w
    }
}
// fn commented() {}
pub fn make() -> Rect {
    Rect { w: 1 }
}
#[cfg(test)]
mod tests {
    fn in_test() {}
}
";

    #[test]
    fn sites_skip_test_region_comments_and_trait_impls() {
        let s = sites(Kind::RenameSymbol, &src(SAMPLE));
        let lines: Vec<u32> = s.iter().map(|s| s.line).collect();
        assert_eq!(lines, [14], "only `pub fn make`: {s:?}");
    }

    #[test]
    fn same_seed_same_case() {
        let files = src(SAMPLE);
        for kind in Kind::ALL {
            assert_eq!(generate(kind, 0, &files), generate(kind, 0, &files));
        }
        assert!(generate(Kind::RenameSymbol, 0, &files).is_some());
    }

    #[test]
    fn seed_beyond_sites_is_none() {
        let files = src("use a::b;\nuse c::d;\nuse e::f;\nfn x() {}\n");
        let len = sites(Kind::RemoveImport, &files).len();
        assert_eq!(len, 3);
        let picked: BTreeSet<Site> = (0..len as u32)
            .map(|seed| generate(Kind::RemoveImport, seed, &files).unwrap().site)
            .collect();
        assert_eq!(picked.len(), len, "distinct sites");
        assert_eq!(generate(Kind::RemoveImport, len as u32, &files), None);
    }

    #[test]
    fn remove_import_deletes_the_use_line() {
        let b = generate(Kind::RemoveImport, 0, &src("use std::fmt;\nfn x() {}\n")).unwrap();
        assert_eq!(b.new_text, "fn x() {}\n");
        assert_eq!(b.site.line, 1);
    }

    #[test]
    fn drop_trait_impl_comments_out_block() {
        let b = generate(Kind::DropTraitImpl, 0, &src(SAMPLE)).unwrap();
        assert_eq!(b.site.line, 8);
        assert!(b.new_text.contains(
            "// impl Area for Rect {\n//     fn area(&self) -> u32 {\n//         self.w\n//     }\n// }\n"
        ));
        assert!(b.new_text.contains("pub fn make() -> Rect {"));
    }

    #[test]
    fn typo_type_appends_last_char() {
        let b = generate(Kind::TypoType, 0, &src(SAMPLE)).unwrap();
        assert_eq!(b.site.line, 14);
        assert!(
            b.new_text.contains("pub fn make() -> Rectt {"),
            "{}",
            b.new_text
        );
        assert!(b.new_text.contains("pub struct Rect {"));
    }

    #[test]
    fn anchor_replaced_with_payload_at_indent() {
        let text = "fn f(b: Vec<u32>) {\n    // @brk use-after-move: let _ = b.len();\n}\n";
        let b = generate(Kind::UseAfterMove, 0, &src(text)).unwrap();
        assert_eq!(b.new_text, "fn f(b: Vec<u32>) {\n    let _ = b.len();\n}\n");
        // Another kind's anchor is not a site.
        assert!(generate(Kind::ConstEval, 0, &src(text)).is_none());
    }

    #[test]
    fn group_for_prefers_g_b_suspects() {
        assert_eq!(
            group_for(Kind::TypoType, &[d("E0599"), d("E0412")]),
            Group::UnresolvedType
        );
        assert_eq!(group_for(Kind::RemoveImport, &[]), Group::Valid);
        assert_eq!(group_for(Kind::CfgGate, &[d("E0425")]), Group::Cfg);
        assert_eq!(
            group_for(Kind::DropTraitImpl, &[d("E0277"), d("E0599")]),
            Group::TraitBound
        );
        assert_eq!(
            group_for(Kind::ChangeType, &[d("E0369")]),
            Group::TypeMismatch
        );
        assert_eq!(
            group_for(Kind::ConstEval, &[d("E0999")]),
            Group::ConstEval,
            "unknown codes fall back to the default group"
        );
    }

    #[test]
    fn write_case_keeps_existing_group() {
        let corpus = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../target/oracle-test-tmp"
        ))
        .join(format!("keep-group-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&corpus);
        let b = generate(Kind::RemoveImport, 0, &src("use a::b;\nfn x() {}\n")).unwrap();
        let toml = write_case(&corpus, "ws", &b).unwrap();
        let first = std::fs::read_to_string(&toml).unwrap();
        assert!(first.contains("group = \"unresolved-import\""), "{first}");
        let edited = first.replace("group = \"unresolved-import\"", "group = \"borrow\"");
        std::fs::write(&toml, &edited).unwrap();
        write_case(&corpus, "ws", &b).unwrap();
        let second = std::fs::read_to_string(&toml).unwrap();
        let _ = std::fs::remove_dir_all(&corpus);
        assert_eq!(second, edited);
        assert!(second.contains("group = \"borrow\""));
    }

    #[test]
    fn kind_ids_round_trip() {
        for k in Kind::ALL {
            assert_eq!(Kind::parse(k.id()), Some(k));
        }
        assert_eq!(Kind::parse("nope"), None);
    }
}
