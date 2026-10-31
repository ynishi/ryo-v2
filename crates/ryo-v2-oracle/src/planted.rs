//! A deterministic, line-based generator that plants the inverse of a refactor into clean code,
//! and the corpus cases built from its output.
//!
//! Each kind injects the inverse of one Intent, so the best Blueprint of the planted state (the
//! one that restores the clean text) is known by construction, independent of every metric:
//!
//! | kind | injection | best restores with |
//! |---|---|---|
//! | `obscure-name` | `let area = shape.area();` and its uses become `v1` | rename-local |
//! | `inline-variable` | `let bounds = e;` used once on the next line is folded into the use | extract-variable |
//! | `inline-function` | a private helper `fn h(a) { let x = e; x }` right after its only caller `let x = h(a);` is folded into the call | extract-function |
//! | `misplace-item` | a top-level item is moved to another module file of its crate, leaving a re-export | move-item |
//!
//! Add-derive is not planted: its best has no definition independent of the metrics (a KNOWN
//! LIMITATION of the harness, see `SEARCH_LEDGER.md`).
//!
//! The generator is pure: it reads the sources it is given and returns the whole new text of the
//! files it changes. Only [`read_clean`] and [`write_case`] touch the file system. Nothing here
//! uses randomness or time, so the same input always gives the same cases. A site is chosen
//! conservatively: where the name a rust-analyzer expander would suggest (`NameGenerator`) is the
//! name the clean code uses, so a plant never asks for a restore the expander could not name.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use ryo_v2_core::domain::RelPath;

use crate::breakage::{BreakageError, Site, SourceFile, read_sources};

/// The kind of a plant. `id()` is kebab-case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PlantKind {
    /// Rename a local to `v<k>` (inverse of rename-local).
    ObscureName,
    /// Fold a variable into its only use (inverse of extract-variable).
    InlineVariable,
    /// Fold a private helper into its only call (inverse of extract-function).
    InlineFunction,
    /// Move an item to another module file, leaving a re-export (inverse of move-item).
    MisplaceItem,
}

impl PlantKind {
    /// Every kind, in declaration order.
    pub const ALL: [PlantKind; 4] = [
        PlantKind::ObscureName,
        PlantKind::InlineVariable,
        PlantKind::InlineFunction,
        PlantKind::MisplaceItem,
    ];

    /// The kebab-case id used in case ids and on the command line.
    pub fn id(self) -> &'static str {
        match self {
            PlantKind::ObscureName => "obscure-name",
            PlantKind::InlineVariable => "inline-variable",
            PlantKind::InlineFunction => "inline-function",
            PlantKind::MisplaceItem => "misplace-item",
        }
    }

    /// The kind with this id, if any.
    pub fn parse(s: &str) -> Option<PlantKind> {
        PlantKind::ALL.into_iter().find(|k| k.id() == s)
    }

    /// The id of the Intent whose call restores the clean text.
    pub fn best_intent(self) -> &'static str {
        match self {
            PlantKind::ObscureName => "rename-local",
            PlantKind::InlineVariable => "extract-variable",
            PlantKind::InlineFunction => "extract-function",
            PlantKind::MisplaceItem => "move-item",
        }
    }
}

/// The whole new text of one file a plant changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlantedFile {
    /// Path relative to the workspace root.
    pub path: RelPath,
    /// The planted text.
    pub text: String,
}

/// A byte range of the planted text of one file: a function or item a plant touched.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TargetRange {
    /// Path relative to the workspace root.
    pub path: RelPath,
    /// Start byte (inclusive).
    pub start: u32,
    /// End byte (exclusive).
    pub end: u32,
}

/// One planted decision: one plant (best at depth 1) or two in different functions (depth 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plant {
    /// The kinds planted, in order.
    pub kinds: Vec<PlantKind>,
    /// The seed that picked the sites.
    pub seed: u32,
    /// The site of each plant (the second is a line of the text after the first plant).
    pub sites: Vec<Site>,
    /// The files that differ from the clean ones.
    pub files: Vec<PlantedFile>,
    /// The touched functions and items, in the planted text.
    pub targets: Vec<TargetRange>,
    /// The helper `inline-function` folded, when one was.
    pub original_fn: Option<String>,
    /// One line per plant describing the edit.
    pub notes: Vec<String>,
}

impl Plant {
    /// The depth of the best: the number of plants.
    pub fn depth(&self) -> u8 {
        u8::try_from(self.kinds.len()).unwrap_or(u8::MAX)
    }
}

/// One edit of a run of lines of one file (0-based, half open).
#[derive(Debug, Clone)]
struct Splice {
    path: RelPath,
    start: usize,
    end: usize,
    replacement: String,
    region: RegionKind,
}

/// What a splice marks as touched.
#[derive(Debug, Clone, Copy)]
enum RegionKind {
    None,
    /// The replacement, without its first `skip` lines.
    Exact {
        skip: usize,
    },
    /// The function that contains the replacement.
    EnclosingFn,
}

/// A candidate plant: where, what, and the edits.
#[derive(Debug, Clone)]
struct Raw {
    site: Site,
    splices: Vec<Splice>,
    original_fn: Option<String>,
    note: String,
}

/// Touched lines of one file in the current text (0-based, half open).
#[derive(Debug, Clone)]
struct Region {
    path: RelPath,
    start: usize,
    end: usize,
}

// ---------------------------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------------------------

/// The sites of `kind` in `files`, in (path, line) order.
pub fn sites(kind: PlantKind, files: &[SourceFile]) -> Vec<Site> {
    candidates(kind, files)
        .into_iter()
        .map(|r| r.site)
        .collect()
}

/// The plant for `seed`. With `len = sites.len()`, a seed `>= len` is `None`. Otherwise the
/// search starts at `idx = (seed * 7) % len` and steps `+1` (mod `len`) past any index an earlier
/// seed (`0..seed`) took, like the breakage generator.
pub fn generate(kind: PlantKind, seed: u32, files: &[SourceFile]) -> Option<Plant> {
    let cands = candidates(kind, files);
    let raw = cands.get(pick(cands.len(), seed)?)?;
    let applied = apply(files, raw)?;
    Some(assemble(
        files,
        vec![kind],
        seed,
        vec![raw],
        &applied.files,
        applied.regions,
    ))
}

/// Two plants, `first` then `second`, in different functions or items: the best is at depth 2.
/// The first site is picked like [`generate`]; the second is the first site of `second` in the
/// planted text, from a seed-dependent start, whose edits touch none of the first's functions.
/// Two `inline-function` plants are refused (a case names one helper).
pub fn generate_pair(
    first: PlantKind,
    second: PlantKind,
    seed: u32,
    files: &[SourceFile],
) -> Option<Plant> {
    if first == PlantKind::InlineFunction && second == PlantKind::InlineFunction {
        return None;
    }
    let cands = candidates(first, files);
    let raw1 = cands.get(pick(cands.len(), seed)?)?;
    let a1 = apply(files, raw1)?;
    let cands2 = candidates(second, &a1.files);
    let n = cands2.len();
    if n == 0 {
        return None;
    }
    let start = (seed as usize * 7 + 3) % n;
    let raw2 = (0..n)
        .map(|k| &cands2[(start + k) % n])
        .find(|r| !conflicts(&a1.regions, r))?;
    if raw1.original_fn.is_some() && raw2.original_fn.is_some() {
        return None;
    }
    let a2 = apply(&a1.files, raw2)?;
    let mut regions = a1.regions;
    shift_regions(&mut regions, raw2);
    regions.extend(a2.regions);
    Some(assemble(
        files,
        vec![first, second],
        seed,
        vec![raw1, raw2],
        &a2.files,
        regions,
    ))
}

/// `.rs` files under `ws` like [`read_sources`], without a `planted` directory (a workspace may
/// hold its own plants).
pub fn read_clean(ws: &Path) -> Result<Vec<SourceFile>, BreakageError> {
    let mut files = read_sources(ws)?;
    files.retain(|f| f.path.as_str().split('/').next() != Some("planted"));
    Ok(files)
}

/// Writes `<ws_dir>/planted/<id>/<path>` (the whole planted text of every changed file) and
/// `<ws_dir>/cases/<id>.toml`. Overwrites. `workspace` is the case's `workspace` value
/// (corpus-relative or absolute). Returns the toml path.
pub fn write_case(
    ws_dir: &Path,
    workspace: &str,
    id: &str,
    plant: &Plant,
) -> Result<PathBuf, BreakageError> {
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
    let overlay = ws_dir.join("planted").join(id);
    for f in &plant.files {
        write(&overlay.join(f.path.as_str()), &f.text)?;
    }
    let toml_path = ws_dir.join("cases").join(format!("{id}.toml"));
    write(&toml_path, &render_case(workspace, id, plant))?;
    Ok(toml_path)
}

/// The text of `cases/<id>.toml` for `plant` (`probes` stay empty: the Main AI fills them).
pub fn render_case(workspace: &str, id: &str, plant: &Plant) -> String {
    let mut out = String::new();
    for (kind, (site, note)) in plant
        .kinds
        .iter()
        .zip(plant.sites.iter().zip(plant.notes.iter()))
    {
        let note = note.replace(['\n', '\r'], " ");
        out.push_str(&format!(
            "# planted: {} {}:{} {note}\n",
            kind.id(),
            site.path,
            site.line
        ));
    }
    let kinds: Vec<String> = plant
        .kinds
        .iter()
        .map(|k| format!("{:?}", k.id()))
        .collect();
    out.push_str(&format!("workspace = {workspace:?}\n"));
    out.push_str(&format!("overlay = {:?}\n", format!("planted/{id}")));
    out.push_str(&format!("kinds = [{}]\n", kinds.join(", ")));
    out.push_str(&format!("depth = {}\n", plant.depth()));
    let origin_kinds: Vec<&str> = plant.kinds.iter().map(|k| k.id()).collect();
    out.push_str(&format!(
        "origin = {:?}\n",
        format!("planted:{}:{}", origin_kinds.join("+"), plant.seed)
    ));
    out.push_str("targets = [\n");
    for t in &plant.targets {
        out.push_str(&format!(
            "    {{ path = {:?}, start = {}, end = {} }},\n",
            t.path.as_str(),
            t.start,
            t.end
        ));
    }
    out.push_str("]\n");
    out.push_str("probes = []\n");
    if let Some(f) = &plant.original_fn {
        out.push_str(&format!("original_fn = {f:?}\n"));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Matching planted text with clean text (used by the harness)
// ---------------------------------------------------------------------------------------------

/// `s` with every run of whitespace replaced by one space and the ends trimmed.
pub fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `text` with every whole-word `from` replaced by `to`.
pub fn replace_word(text: &str, from: &str, to: &str) -> String {
    if from.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for pos in word_positions(text, from) {
        out.push_str(&text[at..pos]);
        out.push_str(to);
        at = pos + from.len();
    }
    out.push_str(&text[at..]);
    out
}

/// `text` with the function `from` renamed to `to`: every whole-word `from` followed by `(` that
/// is not a method call (not preceded by `.`). A method or a variable of the same name stays: the
/// expander names an extracted function after the last call or the name it binds.
pub fn rename_fn(text: &str, from: &str, to: &str) -> String {
    if from.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for pos in word_positions(text, from) {
        if text[..pos].ends_with('.') || !text[pos + from.len()..].starts_with('(') {
            continue;
        }
        out.push_str(&text[at..pos]);
        out.push_str(to);
        at = pos + from.len();
    }
    out.push_str(&text[at..]);
    out
}

/// The names of the functions declared in `text` (`fn <name>`).
pub fn fn_names(text: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for pos in word_positions(text, "fn") {
        let rest = text[pos + 2..].trim_start_matches([' ', '\t']);
        if rest.len() == text[pos + 2..].len() {
            continue;
        }
        let name: String = rest.chars().take_while(|c| is_ident_char(*c)).collect();
        if !name.is_empty() {
            names.insert(name);
        }
    }
    names
}

/// Whether `candidate` equals `clean` up to whitespace runs and, when `original_fn` is given, the
/// name of the one function `candidate` declares that `planted` did not (the name the expander
/// gave the extracted helper), which is read as `original_fn`.
pub fn texts_match(candidate: &str, clean: &str, planted: &str, original_fn: Option<&str>) -> bool {
    let mut cand = candidate.to_string();
    if let Some(orig) = original_fn {
        let before = fn_names(planted);
        let new: Vec<String> = fn_names(candidate)
            .into_iter()
            .filter(|n| !before.contains(n))
            .collect();
        if let [generated] = new.as_slice() {
            cand = rename_fn(&cand, generated, orig);
        }
    }
    collapse_ws(&cand) == collapse_ws(clean)
}

// ---------------------------------------------------------------------------------------------
// Picking, applying, assembling
// ---------------------------------------------------------------------------------------------

/// The index for `seed` among `len` sites (see [`generate`]).
fn pick(len: usize, seed: u32) -> Option<usize> {
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
    Some(idx)
}

struct Applied {
    files: Vec<SourceFile>,
    regions: Vec<Region>,
}

fn line_count(text: &str) -> usize {
    text.split_inclusive('\n').count()
}

/// `raw`'s splices over `files`: the new files and the touched regions in them.
fn apply(files: &[SourceFile], raw: &Raw) -> Option<Applied> {
    let mut by_path: BTreeMap<&RelPath, Vec<&Splice>> = BTreeMap::new();
    for s in &raw.splices {
        by_path.entry(&s.path).or_default().push(s);
    }
    let mut out_files = files.to_vec();
    let mut regions = Vec::new();
    for (path, mut splices) in by_path {
        splices.sort_by_key(|s| (s.start, s.end));
        let file = out_files.iter_mut().find(|f| &f.path == path)?;
        let lines: Vec<&str> = file.text.split_inclusive('\n').collect();
        let mut text = String::new();
        let mut at = 0usize;
        let mut delta: isize = 0;
        // (new start line, new line count, kind) per splice, in the new text.
        let mut marks: Vec<(usize, usize, RegionKind)> = Vec::new();
        for s in &splices {
            if s.start < at || s.end < s.start || s.end > lines.len() {
                return None;
            }
            text.push_str(&lines[at..s.start].concat());
            text.push_str(&s.replacement);
            let new_len = line_count(&s.replacement);
            let new_start = (s.start as isize + delta) as usize;
            marks.push((new_start, new_len, s.region));
            delta += new_len as isize - (s.end - s.start) as isize;
            at = s.end;
        }
        text.push_str(&lines[at..].concat());
        let new_lines: Vec<&str> = text.split_inclusive('\n').collect();
        let new_in_test = test_mask(&new_lines);
        let fns = parse_fns(&new_lines, &new_in_test);
        for (start, len, kind) in marks {
            match kind {
                RegionKind::None => {}
                RegionKind::Exact { skip } => {
                    if len > skip {
                        regions.push(Region {
                            path: path.clone(),
                            start: start + skip,
                            end: start + len,
                        });
                    }
                }
                RegionKind::EnclosingFn => {
                    let enclosing = fns
                        .iter()
                        .filter(|f| f.header <= start && start <= f.end)
                        .max_by_key(|f| f.header);
                    let (a, b) = match enclosing {
                        Some(f) => (f.header, f.end + 1),
                        None => (start, start + len.max(1)),
                    };
                    regions.push(Region {
                        path: path.clone(),
                        start: a,
                        end: b,
                    });
                }
            }
        }
        file.text = text;
    }
    Some(Applied {
        files: out_files,
        regions,
    })
}

/// Whether `raw` edits lines inside any of `regions`.
fn conflicts(regions: &[Region], raw: &Raw) -> bool {
    raw.splices.iter().any(|s| {
        regions
            .iter()
            .any(|r| r.path == s.path && !(r.end <= s.start || r.start >= s.end.max(s.start)))
            || regions.iter().any(|r| {
                r.path == s.path && s.start == s.end && r.start < s.start && s.start < r.end
            })
    })
}

/// Moves `regions` (of the text before `raw`) to the text after it. `raw` must not conflict.
fn shift_regions(regions: &mut [Region], raw: &Raw) {
    for r in regions.iter_mut() {
        let mut delta: isize = 0;
        for s in raw.splices.iter().filter(|s| s.path == r.path) {
            if s.end <= r.start {
                delta += line_count(&s.replacement) as isize - (s.end - s.start) as isize;
            }
        }
        r.start = (r.start as isize + delta) as usize;
        r.end = (r.end as isize + delta) as usize;
    }
}

fn assemble(
    clean: &[SourceFile],
    kinds: Vec<PlantKind>,
    seed: u32,
    raws: Vec<&Raw>,
    planted: &[SourceFile],
    regions: Vec<Region>,
) -> Plant {
    let mut files = Vec::new();
    for (before, after) in clean.iter().zip(planted) {
        if before.text != after.text {
            files.push(PlantedFile {
                path: after.path.clone(),
                text: after.text.clone(),
            });
        }
    }
    let mut targets = Vec::new();
    for r in &regions {
        let Some(f) = planted.iter().find(|f| f.path == r.path) else {
            continue;
        };
        let mut offsets: Vec<usize> = Vec::new();
        let mut at = 0usize;
        for l in f.text.split_inclusive('\n') {
            offsets.push(at);
            at += l.len();
        }
        offsets.push(at);
        let start = offsets.get(r.start).copied().unwrap_or(at);
        let end = offsets.get(r.end).copied().unwrap_or(at);
        targets.push(TargetRange {
            path: r.path.clone(),
            start: start as u32,
            end: end as u32,
        });
    }
    targets.sort();
    Plant {
        kinds,
        seed,
        sites: raws.iter().map(|r| r.site.clone()).collect(),
        files,
        targets,
        original_fn: raws.iter().find_map(|r| r.original_fn.clone()),
        notes: raws.iter().map(|r| r.note.clone()).collect(),
    }
}

// ---------------------------------------------------------------------------------------------
// Line parsing
// ---------------------------------------------------------------------------------------------

fn body(line: &str) -> &str {
    line.trim_end_matches(['\n', '\r'])
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn indent_of(line: &str) -> usize {
    let b = body(line);
    b.len() - b.trim_start().len()
}

fn is_blank(line: &str) -> bool {
    body(line).trim().is_empty()
}

fn eol_of(line: &str) -> &str {
    &line[body(line).len()..]
}

/// Byte offsets of the whole-word occurrences of `word` in `text`.
fn word_positions(text: &str, word: &str) -> Vec<usize> {
    if word.is_empty() {
        return Vec::new();
    }
    text.match_indices(word)
        .map(|(i, _)| i)
        .filter(|&i| {
            let before = text[..i].chars().next_back();
            let after = text[i + word.len()..].chars().next();
            !before.is_some_and(is_ident_char) && !after.is_some_and(is_ident_char)
        })
        .collect()
}

/// Per byte of `s`: whether it is inside a string or char literal (quotes included).
fn string_mask(s: &str) -> Vec<bool> {
    let mut mask = vec![false; s.len()];
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let end_of = |j: usize| chars.get(j).map_or(s.len(), |&(p, ch)| p + ch.len_utf8());
    let mut i = 0;
    while i < chars.len() {
        let (pos, c) = chars[i];
        if c == '"' {
            let mut j = i + 1;
            while j < chars.len() {
                match chars[j].1 {
                    '\\' => j += 2,
                    '"' => break,
                    _ => j += 1,
                }
            }
            for m in &mut mask[pos..end_of(j)] {
                *m = true;
            }
            i = j + 1;
        } else if c == '\'' {
            let close = if chars.get(i + 1).map(|x| x.1) == Some('\\') {
                (i + 3..chars.len()).find(|&j| chars[j].1 == '\'')
            } else if chars.get(i + 2).map(|x| x.1) == Some('\'') {
                Some(i + 2)
            } else {
                None
            };
            match close {
                Some(j) => {
                    for m in &mut mask[pos..end_of(j)] {
                        *m = true;
                    }
                    i = j + 1;
                }
                None => i += 1,
            }
        } else {
            i += 1;
        }
    }
    mask
}

/// The line without its terminator and without a trailing `//` comment.
fn code_part(line: &str) -> &str {
    let b = body(line);
    let mask = string_mask(b);
    let bytes = b.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'/' && bytes[i + 1] == b'/' && !mask[i] {
            return &b[..i];
        }
        i += 1;
    }
    b
}

/// Whether any of `chars` occurs in `s` outside string and char literals.
fn contains_outside_strings(s: &str, chars: &[char]) -> bool {
    let mask = string_mask(s);
    s.char_indices()
        .any(|(i, c)| !mask[i] && chars.contains(&c))
}

/// Whether the brackets of `s` (outside strings) are balanced.
fn balanced(s: &str) -> bool {
    let mask = string_mask(s);
    let mut stack: Vec<char> = Vec::new();
    for (i, c) in s.char_indices() {
        if mask[i] {
            continue;
        }
        match c {
            '(' | '[' | '{' => stack.push(c),
            ')' | ']' | '}' => {
                let want = match c {
                    ')' => '(',
                    ']' => '[',
                    _ => '{',
                };
                if stack.pop() != Some(want) {
                    return false;
                }
            }
            _ => {}
        }
    }
    stack.is_empty()
}

/// The open brackets (char, byte index) before byte `pos` of `code`, outside strings.
fn open_brackets_before(code: &str, pos: usize) -> Vec<(char, usize)> {
    let mask = string_mask(code);
    let mut stack: Vec<(char, usize)> = Vec::new();
    for (i, c) in code.char_indices() {
        if i >= pos {
            break;
        }
        if mask[i] {
            continue;
        }
        match c {
            '(' | '[' | '{' => stack.push((c, i)),
            ')' | ']' | '}' => {
                stack.pop();
            }
            _ => {}
        }
    }
    stack
}

const KEYWORDS: [&str; 38] = [
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while",
];

/// ra's `USELESS_NAMES` of `suggest_name.rs`: never suggested.
const USELESS_NAMES: [&str; 11] = [
    "new", "default", "option", "some", "none", "ok", "err", "str", "string", "from", "into",
];

/// ra's `USELESS_METHODS`: stripped from the expression the name is taken from.
const USELESS_METHODS: [&str; 18] = [
    "to_string",
    "as_str",
    "to_owned",
    "as_ref",
    "clone",
    "cloned",
    "expect",
    "expect_none",
    "unwrap",
    "unwrap_none",
    "unwrap_or",
    "unwrap_or_default",
    "unwrap_or_else",
    "unwrap_unchecked",
    "iter",
    "into_iter",
    "iter_mut",
    "into_future",
];

/// ra's `normalize` for names that are already lower snake case: `None` for a name the expander
/// never suggests (and for a name this generator does not model: upper case, keywords).
fn normalize_name(name: &str) -> Option<String> {
    let first = name.chars().next()?;
    if !(first.is_ascii_lowercase()) {
        return None;
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return None;
    }
    if USELESS_NAMES.contains(&name) || KEYWORDS.contains(&name) {
        return None;
    }
    if ["from_", "with_", "into_"]
        .iter()
        .any(|p| name.starts_with(p))
    {
        return None;
    }
    Some(name.to_string())
}

/// The name ra's `NameGenerator::for_variable` takes from a plain call expression `init`
/// (`f(..)`, `a::f(..)` or `recv.m(..)`), if it is one.
fn call_name(init: &str) -> Option<String> {
    let init = init.trim();
    if !init.ends_with(')')
        || !init
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
    {
        return None;
    }
    let mask = string_mask(init);
    let mut depth = 0i32;
    let mut open_of_last: Option<usize> = None;
    let mut last_group: Option<(char, usize, usize)> = None;
    let mut start_stack: Vec<(char, usize)> = Vec::new();
    for (i, c) in init.char_indices() {
        if mask[i] {
            continue;
        }
        match c {
            '(' | '[' | '{' => {
                start_stack.push((c, i));
                depth += 1;
            }
            ')' | ']' | '}' => {
                depth -= 1;
                let (oc, oi) = start_stack.pop()?;
                if depth == 0 {
                    last_group = Some((oc, oi, i));
                    open_of_last = Some(oi);
                }
            }
            _ => {}
        }
    }
    let (oc, oi, ci) = last_group?;
    if oc != '(' || ci != init.len() - 1 || open_of_last != Some(oi) || depth != 0 {
        return None;
    }
    let callee = &init[..oi];
    if has_top_level_space(callee) || callee.contains(['!', '&', '?']) && !callee.contains('(') {
        return None;
    }
    let ident: String = callee
        .chars()
        .rev()
        .take_while(|c| is_ident_char(*c))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if ident.is_empty() {
        return None;
    }
    let head = &callee[..callee.len() - ident.len()];
    if head.is_empty() {
        return normalize_name(&ident);
    }
    if let Some(recv) = head.strip_suffix('.') {
        if recv.is_empty() || USELESS_METHODS.contains(&ident.as_str()) {
            return None;
        }
        let mut name = ident.as_str();
        for prefix in ["try_into_", "into_", "as_", "to_"] {
            if let Some(rest) = name.strip_prefix(prefix) {
                name = rest;
                break;
            }
        }
        return normalize_name(name);
    }
    if head.ends_with("::") && callee.chars().all(|c| is_ident_char(c) || c == ':') {
        return normalize_name(&ident);
    }
    None
}

fn has_top_level_space(s: &str) -> bool {
    let mask = string_mask(s);
    let mut depth = 0i32;
    for (i, c) in s.char_indices() {
        if mask[i] {
            continue;
        }
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ' ' | '\t' if depth == 0 => return true,
            _ => {}
        }
    }
    false
}

/// A parsed `let [mut] name = init;` line.
struct LetStmt {
    name: String,
    mutable: bool,
    init: String,
    /// Byte offset of the name in the line.
    name_at: usize,
}

fn parse_let(line: &str) -> Option<LetStmt> {
    let code = code_part(line);
    let trimmed = code.trim_end();
    let lead = trimmed.len() - trimmed.trim_start().len();
    let t = &trimmed[lead..];
    let rest = t.strip_prefix("let ")?;
    let (mutable, after_mut) = match rest.strip_prefix("mut ") {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let name: String = after_mut
        .chars()
        .take_while(|c| is_ident_char(*c))
        .collect();
    if name.is_empty() {
        return None;
    }
    let after_name = &after_mut[name.len()..];
    let init = after_name.strip_prefix(" = ")?.strip_suffix(';')?.trim();
    if init.is_empty() || !balanced(init) {
        return None;
    }
    let name_at = lead + (t.len() - after_mut.len());
    Some(LetStmt {
        name,
        mutable,
        init: init.to_string(),
        name_at,
    })
}

/// `name` as the full argument `index` of a call on the line: the callee identifier, whether
/// it is a method call, and the argument index.
fn arg_site(code: &str, pos: usize, word_len: usize) -> Option<(String, bool, usize)> {
    let before = code[..pos].trim_end();
    let after = code[pos + word_len..].trim_start();
    if !(before.ends_with('(') || before.ends_with(','))
        || !(after.starts_with(')') || after.starts_with(','))
    {
        return None;
    }
    let stack = open_brackets_before(code, pos);
    let &(c, open) = stack.last()?;
    if c != '(' {
        return None;
    }
    let head = &code[..open];
    let ident: String = head
        .chars()
        .rev()
        .take_while(|c| is_ident_char(*c))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if ident.is_empty() || KEYWORDS.contains(&ident.as_str()) {
        return None;
    }
    let before_ident = &head[..head.len() - ident.len()];
    let method = if before_ident.ends_with('.') {
        true
    } else if before_ident.ends_with("::") || !before_ident.ends_with(is_ident_char) {
        false
    } else {
        return None;
    };
    let mask = string_mask(code);
    let mut depth = 0i32;
    let mut index = 0usize;
    for (i, ch) in code[open + 1..pos].char_indices() {
        if mask[open + 1 + i] {
            continue;
        }
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => index += 1,
            _ => {}
        }
    }
    Some((ident, method, index))
}

// ---------------------------------------------------------------------------------------------
// Files and functions
// ---------------------------------------------------------------------------------------------

/// A parameter written `name: Type`.
#[derive(Debug, Clone)]
struct Param {
    name: String,
    ty: String,
}

/// A function with a block body.
#[derive(Debug, Clone)]
struct FnBlock {
    header: usize,
    open: usize,
    end: usize,
    indent: usize,
    name: String,
    params: Vec<Param>,
    simple_params: bool,
    has_self: bool,
    is_pub: bool,
    generic: bool,
}

/// From the first `#[cfg(test)]` line to the end of the file.
fn test_mask(lines: &[&str]) -> Vec<bool> {
    let start = lines
        .iter()
        .position(|l| body(l).trim() == "#[cfg(test)]")
        .unwrap_or(lines.len());
    let mut mask = vec![false; lines.len()];
    mask[start..].fill(true);
    mask
}

fn parse_fns(lines: &[&str], in_test: &[bool]) -> Vec<FnBlock> {
    let mut out = Vec::new();
    for i in 0..lines.len() {
        if in_test[i] {
            continue;
        }
        let code = code_part(lines[i]);
        let t = code.trim_start();
        let (is_pub, rest) = match t.strip_prefix("pub ") {
            Some(r) => (true, r),
            None => match t.strip_prefix("pub(crate) ") {
                Some(r) => (true, r),
                None => (false, t),
            },
        };
        let mut rest = rest;
        for q in ["const ", "async ", "unsafe "] {
            if let Some(r) = rest.strip_prefix(q) {
                rest = r;
            }
        }
        let Some(rest) = rest.strip_prefix("fn ") else {
            continue;
        };
        let name: String = rest.chars().take_while(|c| is_ident_char(*c)).collect();
        if name.is_empty() {
            continue;
        }
        let generic = rest[name.len()..].starts_with('<');
        // The block opens on the first line that ends with `{`; a `;` first means no body.
        let mut open = None;
        for (j, line) in lines.iter().enumerate().skip(i).take(16) {
            let c = code_part(line).trim_end();
            if c.ends_with('{') {
                open = Some(j);
                break;
            }
            if c.ends_with(';') || c.ends_with('}') {
                break;
            }
        }
        let Some(open) = open else { continue };
        let indent = indent_of(lines[i]);
        let closer = format!("{}}}", " ".repeat(indent));
        let Some(end) = (open + 1..lines.len()).find(|&k| body(lines[k]).trim_end() == closer)
        else {
            continue;
        };
        let header_text: String = (i..=open)
            .map(|j| code_part(lines[j]).trim())
            .collect::<Vec<_>>()
            .join(" ");
        let (params, simple_params, has_self) = parse_params(&header_text, &name);
        out.push(FnBlock {
            header: i,
            open,
            end,
            indent,
            name,
            params,
            simple_params,
            has_self,
            is_pub,
            generic,
        });
    }
    out
}

fn parse_params(header: &str, name: &str) -> (Vec<Param>, bool, bool) {
    let Some(at) = header.find(&format!("fn {name}")) else {
        return (Vec::new(), false, false);
    };
    let after = &header[at + 3 + name.len()..];
    let Some(open) = after.find('(') else {
        return (Vec::new(), false, false);
    };
    let mask = string_mask(after);
    let mut depth = 0i32;
    let mut close = None;
    for (i, c) in after.char_indices().skip_while(|(i, _)| *i < open) {
        if mask[i] {
            continue;
        }
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(close) = close else {
        return (Vec::new(), false, false);
    };
    let inner = &after[open + 1..close];
    let mut parts: Vec<&str> = Vec::new();
    let mut d = 0i32;
    let mut from = 0usize;
    let ib = inner.as_bytes();
    for (i, c) in inner.char_indices() {
        match c {
            '(' | '[' | '{' | '<' => d += 1,
            ')' | ']' | '}' => d -= 1,
            '>' if i == 0 || ib[i - 1] != b'-' => d -= 1,
            ',' if d == 0 => {
                parts.push(&inner[from..i]);
                from = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&inner[from..]);
    let mut params = Vec::new();
    let mut simple = true;
    let mut has_self = false;
    for p in parts {
        let p = p.trim();
        if p.is_empty() {
            continue;
        }
        if matches!(p, "self" | "&self" | "&mut self" | "mut self") || p.starts_with("self:") {
            has_self = true;
            continue;
        }
        match p.split_once(':') {
            Some((n, ty)) if !n.trim().is_empty() && n.trim().chars().all(is_ident_char) => {
                params.push(Param {
                    name: n.trim().to_string(),
                    ty: ty.trim().to_string(),
                });
            }
            _ => simple = false,
        }
    }
    (params, simple, has_self)
}

/// A parsed file.
struct Parsed<'a> {
    path: &'a RelPath,
    lines: Vec<&'a str>,
    in_test: Vec<bool>,
    fns: Vec<FnBlock>,
}

impl<'a> Parsed<'a> {
    fn new(f: &'a SourceFile) -> Self {
        let lines: Vec<&str> = f.text.split_inclusive('\n').collect();
        let in_test = test_mask(&lines);
        let fns = parse_fns(&lines, &in_test);
        Self {
            path: &f.path,
            lines,
            in_test,
            fns,
        }
    }
}

/// The functions of the workspace by name, with their parameters.
struct FnIndex {
    defs: BTreeMap<String, Vec<(Vec<Param>, bool, bool)>>,
}

impl FnIndex {
    fn new(parsed: &[Parsed]) -> Self {
        let mut defs: BTreeMap<String, Vec<(Vec<Param>, bool, bool)>> = BTreeMap::new();
        for p in parsed {
            for f in &p.fns {
                defs.entry(f.name.clone()).or_default().push((
                    f.params.clone(),
                    f.has_self,
                    f.simple_params,
                ));
            }
        }
        Self { defs }
    }

    /// The name of parameter `index` of the one function called `callee`, when it is unique and
    /// takes `self` exactly when the call is a method call.
    fn param_name(&self, callee: &str, method: bool, index: usize) -> Option<&str> {
        let defs = self.defs.get(callee)?;
        let [(params, has_self, simple)] = defs.as_slice() else {
            return None;
        };
        if !simple || *has_self != method {
            return None;
        }
        params.get(index).map(|p| p.name.as_str())
    }
}

// ---------------------------------------------------------------------------------------------
// Candidates per kind
// ---------------------------------------------------------------------------------------------

fn candidates(kind: PlantKind, files: &[SourceFile]) -> Vec<Raw> {
    let parsed: Vec<Parsed> = files.iter().map(Parsed::new).collect();
    let mut out = match kind {
        PlantKind::ObscureName => obscure_name(&parsed, files),
        PlantKind::InlineVariable => inline_variable(&parsed),
        PlantKind::InlineFunction => inline_function(&parsed),
        PlantKind::MisplaceItem => misplace_item(&parsed, files),
    };
    out.sort_by(|a, b| a.site.cmp(&b.site));
    out
}

/// Whether the file declares `name` at its top level (`fn`, `use`, `const`, `static`, `mod`).
fn file_declares(p: &Parsed, name: &str) -> bool {
    p.lines.iter().enumerate().any(|(i, l)| {
        if p.in_test[i] || indent_of(l) != 0 {
            return false;
        }
        let t = code_part(l).trim_start();
        [
            "fn ",
            "pub fn ",
            "use ",
            "pub use ",
            "const ",
            "pub const ",
            "static ",
            "pub static ",
            "mod ",
            "pub mod ",
        ]
        .iter()
        .any(|k| t.starts_with(k))
            && !word_positions(t, name).is_empty()
    })
}

fn obscure_name(parsed: &[Parsed], files: &[SourceFile]) -> Vec<Raw> {
    let mut out = Vec::new();
    for p in parsed {
        for f in &p.fns {
            for l in f.open + 1..f.end {
                if let Some(raw) = obscure_at(p, f, l, files) {
                    out.push(raw);
                }
            }
        }
    }
    out
}

fn obscure_at(p: &Parsed, f: &FnBlock, l: usize, files: &[SourceFile]) -> Option<Raw> {
    let stmt = parse_let(p.lines[l])?;
    let name = &stmt.name;
    if normalize_name(name).as_deref() != Some(name)
        || call_name(&stmt.init).as_deref() != Some(name)
    {
        return None;
    }
    if file_declares(p, name) {
        return None;
    }
    // Names before the let: the header and the earlier lines of the fn.
    if (f.header..l).any(|j| !word_positions(code_part(p.lines[j]), name).is_empty()) {
        return None;
    }
    let ind = indent_of(p.lines[l]);
    let m = (l + 1..=f.end)
        .find(|&j| !is_blank(p.lines[j]) && indent_of(p.lines[j]) < ind)
        .unwrap_or(f.end);
    // No use of the name after its block.
    if (m..=f.end).any(|j| !word_positions(code_part(p.lines[j]), name).is_empty()) {
        return None;
    }
    let mut uses = 0usize;
    for j in l + 1..m {
        let code = code_part(p.lines[j]);
        let mask = string_mask(code);
        for pos in word_positions(code, name) {
            uses += 1;
            let before = &code[..pos];
            let after = &code[pos + name.len()..];
            let bad = mask[pos]
                || before.ends_with('.')
                || before.ends_with("::")
                || before.ends_with('\'')
                || (after.starts_with(':') && !after.starts_with("::"))
                || after.starts_with('(')
                || after.starts_with('!')
                || open_brackets_before(code, pos)
                    .last()
                    .is_some_and(|b| b.0 == '{')
                || code.trim() == format!("{name},");
            if bad {
                return None;
            }
        }
    }
    if uses == 0 {
        return None;
    }
    let fresh = (1..).map(|k| format!("v{k}")).find(|v| {
        files
            .iter()
            .all(|sf| word_positions(&sf.text, v).is_empty())
    })?;
    let mut replacement = String::new();
    for j in l..m {
        let line = p.lines[j];
        if j == l {
            let at = stmt.name_at;
            replacement.push_str(&format!(
                "{}{}{}",
                &line[..at],
                fresh,
                &line[at + name.len()..]
            ));
        } else {
            let code_len = code_part(line).len();
            let (code, comment) = line.split_at(code_len);
            replacement.push_str(&replace_word(code, name, &fresh));
            replacement.push_str(comment);
        }
    }
    Some(Raw {
        site: Site {
            path: p.path.clone(),
            line: l as u32 + 1,
        },
        splices: vec![Splice {
            path: p.path.clone(),
            start: l,
            end: m,
            replacement,
            region: RegionKind::EnclosingFn,
        }],
        original_fn: None,
        note: format!("rename local `{name}` to `{fresh}` in fn {}", f.name),
    })
}

fn inline_variable(parsed: &[Parsed]) -> Vec<Raw> {
    let index = FnIndex::new(parsed);
    let mut out = Vec::new();
    for p in parsed {
        for f in &p.fns {
            for l in f.open + 1..f.end {
                if let Some(raw) = inline_variable_at(p, f, l, &index) {
                    out.push(raw);
                }
            }
        }
    }
    out
}

fn inline_variable_at(p: &Parsed, f: &FnBlock, l: usize, index: &FnIndex) -> Option<Raw> {
    let stmt = parse_let(p.lines[l])?;
    let name = &stmt.name;
    if stmt.mutable || normalize_name(name).as_deref() != Some(name) {
        return None;
    }
    let next = l + 1;
    if next >= f.end {
        return None;
    }
    // A statement, or the tail expression of the fn.
    let ncode = code_part(p.lines[next]);
    let is_tail = next + 1 == f.end && !ncode.trim().is_empty();
    if !(ncode.trim_end().ends_with(';') || is_tail) || !balanced(ncode) {
        return None;
    }
    // The name occurs exactly once in the fn besides its let: on the next line.
    let mut found: Vec<(usize, usize)> = Vec::new();
    for j in f.header..=f.end {
        if j == l {
            continue;
        }
        for pos in word_positions(code_part(p.lines[j]), name) {
            found.push((j, pos));
        }
    }
    let [(line_idx, pos)] = found.as_slice() else {
        return None;
    };
    if *line_idx != next {
        return None;
    }
    let mask = string_mask(ncode);
    let before = &ncode[..*pos];
    let after = &ncode[*pos + name.len()..];
    if mask[*pos] || before.ends_with('.') || before.ends_with("::") || after.starts_with(':') {
        return None;
    }
    let by_rhs = parse_let(p.lines[next]).is_some_and(|s2| &s2.init == name);
    if by_rhs {
        if call_name(&stmt.init).as_deref() != Some(name) {
            return None;
        }
    } else {
        let (callee, method, arg) = arg_site(ncode, *pos, name.len())?;
        let param = index.param_name(&callee, method, arg)?;
        if param != name {
            return None;
        }
    }
    let line = p.lines[next];
    let replacement = format!(
        "{}{}{}",
        &line[..*pos],
        stmt.init,
        &line[*pos + name.len()..]
    );
    Some(Raw {
        site: Site {
            path: p.path.clone(),
            line: l as u32 + 1,
        },
        splices: vec![Splice {
            path: p.path.clone(),
            start: l,
            end: l + 2,
            replacement,
            region: RegionKind::EnclosingFn,
        }],
        original_fn: None,
        note: format!("inline `{name}` into the next line of fn {}", f.name),
    })
}

fn inline_function(parsed: &[Parsed]) -> Vec<Raw> {
    let mut out = Vec::new();
    for p in parsed {
        for h in &p.fns {
            if let Some(raw) = inline_function_at(parsed, p, h) {
                out.push(raw);
            }
        }
    }
    out
}

fn inline_function_at(all: &[Parsed], p: &Parsed, h: &FnBlock) -> Option<Raw> {
    if h.indent != 0
        || h.is_pub
        || h.header != h.open
        || h.generic
        || !h.simple_params
        || h.has_self
        || h.params.is_empty()
        || h.end != h.open + 3
        || p.in_test[h.header]
    {
        return None;
    }
    // The body is what rust-analyzer's extract-function writes for a statement that binds a
    // name used afterwards: `let x = <expr>;` and `x`.
    let bound = parse_let(p.lines[h.open + 1])?;
    if bound.mutable || code_part(p.lines[h.open + 2]).trim() != bound.name {
        return None;
    }
    let expr = bound.init.as_str();
    if expr.is_empty()
        || expr.ends_with(';')
        || contains_outside_strings(expr, &['?', '{', '}'])
        || !word_positions(expr, "return").is_empty()
        || !word_positions(expr, "let").is_empty()
        || !balanced(expr)
    {
        return None;
    }
    // Every parameter appears in the body (rust-analyzer passes what the range uses).
    let mask = string_mask(expr);
    for param in &h.params {
        let pos = *word_positions(expr, &param.name).first()?;
        if mask[pos] {
            return None;
        }
    }
    // The helper follows its caller after one blank line.
    let caller = p
        .fns
        .iter()
        .find(|c| c.indent == 0 && c.end + 2 == h.header && is_blank(p.lines[c.end + 1]))?;
    let files_count: usize = all
        .iter()
        .map(|q| {
            q.lines
                .iter()
                .map(|l| word_positions(code_part(l), &h.name).len())
                .sum::<usize>()
        })
        .sum();
    if files_count != 2 {
        return None;
    }
    let l = (caller.open + 1..caller.end).find(|&l| {
        parse_let(p.lines[l])
            .is_some_and(|s| !s.mutable && s.name == bound.name && is_call_of(&s.init, h))
    })?;
    // rust-analyzer lists the parameters of an extracted fn in the order the caller defines
    // them: the first occurrence of a name in the caller is its definition.
    let caller_text: String = p.lines[caller.header..=l]
        .iter()
        .map(|s| code_part(s))
        .collect::<Vec<_>>()
        .join("\n");
    let mut last = None;
    for param in &h.params {
        let pos = *word_positions(&caller_text, &param.name).first()?;
        if last.is_some_and(|prev| pos <= prev) {
            return None;
        }
        last = Some(pos);
    }
    let line = p.lines[l];
    let new_line = format!(
        "{}let {} = {};{}",
        " ".repeat(indent_of(line)),
        bound.name,
        expr,
        eol_of(line)
    );
    Some(Raw {
        site: Site {
            path: p.path.clone(),
            line: l as u32 + 1,
        },
        splices: vec![
            Splice {
                path: p.path.clone(),
                start: l,
                end: l + 1,
                replacement: new_line,
                region: RegionKind::EnclosingFn,
            },
            Splice {
                path: p.path.clone(),
                start: caller.end + 1,
                end: h.end + 1,
                replacement: String::new(),
                region: RegionKind::None,
            },
        ],
        original_fn: Some(h.name.clone()),
        note: format!("inline helper `{}` into fn {}", h.name, caller.name),
    })
}

/// Whether `init` is the call `h(a, b)` of the helper with each argument its parameter's name,
/// or `&name` for a parameter taken by shared reference.
fn is_call_of(init: &str, h: &FnBlock) -> bool {
    let Some(inner) = init
        .strip_prefix(h.name.as_str())
        .and_then(|r| r.strip_prefix('('))
        .and_then(|r| r.strip_suffix(')'))
    else {
        return false;
    };
    let args: Vec<&str> = inner.split(", ").collect();
    args.len() == h.params.len()
        && args.iter().zip(&h.params).all(|(a, p)| {
            *a == p.name
                || (p.ty.starts_with('&')
                    && !p.ty.starts_with("&mut")
                    && *a == format!("&{}", p.name))
        })
}

/// A top-level item of a file.
struct Item {
    start: usize,
    header: usize,
    end: usize,
    name: String,
    is_pub: bool,
}

/// `(pub?, keyword, name)` of a top-level movable item header.
fn item_header(raw: &str) -> Option<(bool, &'static str, String)> {
    let (is_pub, rest) = match raw.strip_prefix("pub ") {
        Some(r) => (true, r),
        None => {
            if raw.starts_with("pub(") {
                return None;
            }
            (false, raw)
        }
    };
    for kw in ["fn", "const", "struct", "enum", "type"] {
        let Some(after) = rest.strip_prefix(kw).and_then(|r| r.strip_prefix(' ')) else {
            continue;
        };
        if kw == "const" && after.starts_with("fn ") {
            return None;
        }
        let name: String = after.chars().take_while(|c| is_ident_char(*c)).collect();
        if name.is_empty() || name == "main" || name == "_" {
            return None;
        }
        return Some((is_pub, kw, name));
    }
    None
}

fn items_of(p: &Parsed) -> Vec<Item> {
    let n = p.lines.len();
    let mut out = Vec::new();
    let mut attr_start: Option<usize> = None;
    let mut i = 0;
    while i < n {
        if p.in_test[i] {
            break;
        }
        let raw = body(p.lines[i]);
        if indent_of(p.lines[i]) == 0 && (raw.starts_with("#[") || raw.starts_with("///")) {
            attr_start.get_or_insert(i);
            i += 1;
            continue;
        }
        if indent_of(p.lines[i]) == 0
            && let Some((is_pub, kw, name)) = item_header(raw)
            && let Some(end) = item_end(p, i, kw)
        {
            out.push(Item {
                start: attr_start.unwrap_or(i),
                header: i,
                end,
                name,
                is_pub,
            });
            attr_start = None;
            i = end + 1;
            continue;
        }
        attr_start = None;
        i += 1;
    }
    out
}

/// The last line of the item whose header is line `h`; `None` for an item with private fields.
fn item_end(p: &Parsed, h: usize, kw: &str) -> Option<usize> {
    let code = code_part(p.lines[h]).trim_end();
    if code.ends_with(';') {
        if kw == "struct"
            && let Some(open) = code.find('(')
        {
            let inner = &code[open + 1..code.rfind(')')?];
            if !inner
                .split(',')
                .all(|f| f.trim().is_empty() || f.trim().starts_with("pub "))
            {
                return None;
            }
        }
        return Some(h);
    }
    let mut open = None;
    for j in h..p.lines.len().min(h + 16) {
        let c = code_part(p.lines[j]).trim_end();
        if c.ends_with('{') {
            open = Some(j);
            break;
        }
        if c.ends_with(';') {
            return None;
        }
    }
    let open = open?;
    let end = (open + 1..p.lines.len()).find(|&k| body(p.lines[k]).trim_end() == "}")?;
    if kw == "struct" {
        for l in &p.lines[open + 1..end] {
            let t = body(l).trim();
            if !(t.is_empty()
                || t.starts_with("pub ")
                || t.starts_with("#[")
                || t.starts_with("///"))
            {
                return None;
            }
        }
    }
    Some(end)
}

/// The identifiers in `text`.
fn idents(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !is_ident_char(c))
        .filter(|w| {
            w.chars()
                .next()
                .is_some_and(|c| c.is_alphabetic() || c == '_')
        })
        .map(str::to_string)
        .collect()
}

/// The names a file's top level binds (declared items, modules, imports), or `None` when it has a
/// glob import (the scope is then unknown).
fn scope_names(p: &Parsed) -> Option<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    for (i, l) in p.lines.iter().enumerate() {
        if p.in_test[i] || indent_of(l) != 0 {
            continue;
        }
        let t = code_part(l).trim();
        let t = t
            .strip_prefix("pub(crate) ")
            .or_else(|| t.strip_prefix("pub "))
            .unwrap_or(t);
        if let Some(u) = t.strip_prefix("use ") {
            if u.contains('*') {
                return None;
            }
            names.extend(idents(u));
        } else if let Some(m) = t.strip_prefix("mod ") {
            names.extend(idents(m));
        } else if let Some((_, _, name)) = item_header(t) {
            names.insert(name);
        } else {
            for kw in ["trait ", "static ", "union ", "macro_rules! "] {
                if let Some(r) = t.strip_prefix(kw) {
                    names.extend(idents(r).into_iter().take(1));
                }
            }
        }
    }
    for k in ["crate", "self", "super", "std", "core", "alloc"] {
        names.remove(k);
    }
    Some(names)
}

fn mod_declared(lib: &Parsed, stem: &str) -> bool {
    lib.lines.iter().any(|l| {
        let t = code_part(l).trim();
        let t = t
            .strip_prefix("pub(crate) ")
            .or_else(|| t.strip_prefix("pub "))
            .unwrap_or(t);
        t.strip_prefix("mod ")
            .and_then(|r| r.strip_suffix(';'))
            .is_some_and(|m| m.trim() == stem)
    })
}

/// `(crate dir prefix with trailing slash or empty, stem)` of `<prefix>src/<stem>.rs`.
fn split_src_path(path: &str) -> Option<(String, String)> {
    let stem = path.strip_suffix(".rs")?;
    let (dir, file) = stem.rsplit_once('/').unwrap_or(("", stem));
    if file.is_empty() {
        return None;
    }
    let prefix = if dir == "src" {
        String::new()
    } else {
        format!("{}/", dir.strip_suffix("/src")?)
    };
    Some((prefix, file.to_string()))
}

fn misplace_item(parsed: &[Parsed], files: &[SourceFile]) -> Vec<Raw> {
    let mut out = Vec::new();
    for p in parsed {
        let Some((prefix, stem)) = split_src_path(p.path.as_str()) else {
            continue;
        };
        let lib_path = format!("{prefix}src/lib.rs");
        let Some(lib) = parsed.iter().find(|q| q.path.as_str() == lib_path) else {
            continue;
        };
        if stem != "lib" && !mod_declared(lib, &stem) {
            continue;
        }
        let Some(names) = scope_names(p) else {
            continue;
        };
        let mut dests: Vec<(&Parsed, &SourceFile)> = Vec::new();
        for (q, sf) in parsed.iter().zip(files) {
            let Some((qp, qs)) = split_src_path(q.path.as_str()) else {
                continue;
            };
            if qp == prefix
                && qs != stem
                && qs != "main"
                && (qs == "lib" || mod_declared(lib, &qs))
                && sf.text.ends_with('\n')
            {
                dests.push((q, sf));
            }
        }
        if dests.is_empty() {
            continue;
        }
        for item in items_of(p) {
            if let Some(raw) = misplace_at(p, &item, &names, &dests) {
                out.push(raw);
            }
        }
    }
    out
}

fn misplace_at(
    p: &Parsed,
    item: &Item,
    names: &BTreeSet<String>,
    dests: &[(&Parsed, &SourceFile)],
) -> Option<Raw> {
    let text: String = p.lines[item.start..=item.end].concat();
    if p.lines[item.start..item.header]
        .iter()
        .any(|l| body(l).contains("cfg"))
        || text.contains("super::")
        || text.contains("self::")
    {
        return None;
    }
    let mut words = idents(&text);
    words.remove(&item.name);
    if words.iter().any(|w| names.contains(w)) {
        return None;
    }
    let usable: Vec<&(&Parsed, &SourceFile)> = dests
        .iter()
        .filter(|(_, sf)| word_positions(&sf.text, &item.name).is_empty())
        .collect();
    if usable.is_empty() {
        return None;
    }
    let (dest, dest_file) = usable[item.header % usable.len()];
    let (_, dest_stem) = split_src_path(dest.path.as_str())?;
    let module = if dest_stem == "lib" {
        "crate".to_string()
    } else {
        format!("crate::{dest_stem}")
    };
    let vis = if item.is_pub { "pub " } else { "" };
    let reexport = format!("{vis}use {module}::{};\n", item.name);
    let mut moved = String::new();
    for (k, l) in p.lines[item.start..=item.end].iter().enumerate() {
        if item.start + k == item.header && !item.is_pub {
            moved.push_str("pub(crate) ");
        }
        moved.push_str(l);
    }
    let moved = moved.trim_end_matches(['\n', '\r']);
    let dest_len = dest_file.text.split_inclusive('\n').count();
    Some(Raw {
        site: Site {
            path: p.path.clone(),
            line: item.header as u32 + 1,
        },
        splices: vec![
            Splice {
                path: p.path.clone(),
                start: item.start,
                end: item.end + 1,
                replacement: reexport,
                region: RegionKind::Exact { skip: 0 },
            },
            Splice {
                path: dest.path.clone(),
                start: dest_len,
                end: dest_len,
                replacement: format!("\n{moved}\n"),
                region: RegionKind::Exact { skip: 1 },
            },
        ],
        original_fn: None,
        note: format!("move `{}` to {}", item.name, dest.path),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(list: &[(&str, &str)]) -> Vec<SourceFile> {
        list.iter()
            .map(|(p, t)| SourceFile {
                path: RelPath::parse(p).unwrap(),
                text: (*t).to_string(),
            })
            .collect()
    }

    fn text_of<'a>(plant: &'a Plant, path: &str) -> &'a str {
        &plant
            .files
            .iter()
            .find(|f| f.path.as_str() == path)
            .unwrap_or_else(|| panic!("{path} not changed: {:?}", plant.files))
            .text
    }

    const TOTAL: &str = "pub struct S {
    pub w: u32,
}

impl S {
    pub fn area(&self) -> u32 {
        self.w
    }
}

pub fn total(items: &[S]) -> u32 {
    let mut sum = 0;
    for item in items {
        let area = item.area();
        sum += area;
    }
    sum
}
";

    #[test]
    fn obscure_name_renames_every_use_in_fn() {
        let fs = files(&[("a/src/lib.rs", TOTAL)]);
        let s = sites(PlantKind::ObscureName, &fs);
        assert_eq!(s.len(), 1, "{s:?}");
        assert_eq!(s[0].line, 14);
        let p = generate(PlantKind::ObscureName, 0, &fs).unwrap();
        let t = text_of(&p, "a/src/lib.rs");
        assert!(
            t.contains("        let v1 = item.area();\n        sum += v1;\n"),
            "{t}"
        );
        assert!(!t.contains("let area"), "{t}");
        assert!(t.contains("pub fn area(&self)"), "{t}");
        assert_eq!(p.depth(), 1);
        assert_eq!(p.targets.len(), 1);
        let range = &p.targets[0];
        let region = &t[range.start as usize..range.end as usize];
        assert!(region.starts_with("pub fn total("), "{region}");
        assert!(region.ends_with("}\n"), "{region}");
        // The clean text comes back by renaming the other way.
        assert_eq!(replace_word(t, "v1", "area"), TOTAL);
    }

    #[test]
    fn obscure_name_refuses_fields_and_method_names() {
        let src = "pub fn f(items: &[S]) -> u32 {
    let area = items.area();
    area + items.area()
}
";
        assert!(sites(PlantKind::ObscureName, &files(&[("a/src/lib.rs", src)])).is_empty());
        let shorthand = "pub fn g(s: &S) -> T {
    let area = s.area();
    T { area }
}
";
        assert!(
            sites(
                PlantKind::ObscureName,
                &files(&[("a/src/lib.rs", shorthand)])
            )
            .is_empty()
        );
        let named = "pub fn h(s: &S) -> u32 {
    let total = total_area(s);
    total
}
";
        assert!(sites(PlantKind::ObscureName, &files(&[("a/src/lib.rs", named)])).is_empty());
    }

    const INLINE_VAR: &str = "pub fn run(items: &Items) -> String {
    let label = items.label();
    let text = show(label);
    text
}

fn show(label: String) -> String {
    label
}
";

    #[test]
    fn inline_variable_removes_let_and_substitutes() {
        let fs = files(&[("a/src/lib.rs", INLINE_VAR)]);
        let s = sites(PlantKind::InlineVariable, &fs);
        assert_eq!(s.len(), 1, "{s:?}");
        assert_eq!(s[0].line, 2);
        let p = generate(PlantKind::InlineVariable, 0, &fs).unwrap();
        let t = text_of(&p, "a/src/lib.rs");
        assert!(
            t.starts_with("pub fn run(items: &Items) -> String {\n    let text = show(items.label());\n    text\n}\n"),
            "{t}"
        );
        assert!(!t.contains("let label"), "{t}");
        // The tail expression of the fn can be the use.
        let tail = INLINE_VAR.replacen(
            "    let text = show(label);\n    text\n",
            "    show(label)\n",
            1,
        );
        let p = generate(
            PlantKind::InlineVariable,
            0,
            &files(&[("a/src/lib.rs", &tail)]),
        )
        .unwrap();
        assert!(
            text_of(&p, "a/src/lib.rs")
                .starts_with("pub fn run(items: &Items) -> String {\n    show(items.label())\n}\n"),
            "{}",
            text_of(&p, "a/src/lib.rs")
        );
        // A parameter name that differs from the let name is not a site.
        let other = INLINE_VAR.replace("fn show(label", "fn show(name");
        assert!(
            sites(
                PlantKind::InlineVariable,
                &files(&[("a/src/lib.rs", &other)])
            )
            .is_empty()
        );
    }

    const INLINE_FN: &str = "pub fn run(a: u32, b: u32) -> u32 {
    let sum = add(a, b);
    sum
}

fn add(a: u32, b: u32) -> u32 {
    let sum = a + b;
    sum
}

pub fn other() -> u32 {
    1
}
";

    #[test]
    fn inline_function_replaces_call_and_removes_helper() {
        let fs = files(&[("a/src/lib.rs", INLINE_FN)]);
        let p = generate(PlantKind::InlineFunction, 0, &fs).unwrap();
        let t = text_of(&p, "a/src/lib.rs");
        assert_eq!(
            t,
            "pub fn run(a: u32, b: u32) -> u32 {\n    let sum = a + b;\n    sum\n}\n\npub fn other() -> u32 {\n    1\n}\n"
        );
        assert_eq!(p.original_fn.as_deref(), Some("add"));
        // A helper called twice is not a site.
        let twice = INLINE_FN.replacen("    sum\n}", "    sum + add(a, b)\n}", 1);
        assert!(
            sites(
                PlantKind::InlineFunction,
                &files(&[("a/src/lib.rs", &twice)])
            )
            .is_empty()
        );
        // Parameters listed in another order than the caller defines them are not a site.
        let swapped = INLINE_FN
            .replace("fn add(a: u32, b: u32)", "fn add(b: u32, a: u32)")
            .replace("add(a, b)", "add(b, a)");
        assert!(
            sites(
                PlantKind::InlineFunction,
                &files(&[("a/src/lib.rs", &swapped)])
            )
            .is_empty()
        );
        // A parameter taken by shared reference is called with `&name`.
        let by_ref = "pub fn run(r: Report) -> String {\n    let text = show(&r);\n    text\n}\n\nfn show(r: &Report) -> String {\n    let text = r.render();\n    text\n}\n";
        let p = generate(
            PlantKind::InlineFunction,
            0,
            &files(&[("a/src/lib.rs", by_ref)]),
        )
        .unwrap();
        assert_eq!(
            text_of(&p, "a/src/lib.rs"),
            "pub fn run(r: Report) -> String {\n    let text = r.render();\n    text\n}\n"
        );
        // The generated name is read as the original one.
        let generated = "pub fn run(a: u32, b: u32) -> u32 {\n    let sum = fun_name(a, b);\n    sum\n}\n\nfn fun_name(a: u32, b: u32) -> u32 {\n    let sum = a + b;\n    sum\n}\n";
        assert!(texts_match(
            generated,
            INLINE_FN.split("\npub fn other").next().unwrap(),
            t,
            Some("add")
        ));
    }

    const LIB: &str = "mod other;

pub const LIMIT: u32 = 10;
const SECRET: u32 = 1;

pub fn go() -> u32 {
    LIMIT + SECRET
}
";
    const OTHER: &str = "pub fn helper() -> u32 {
    7
}
";

    #[test]
    fn misplace_item_writes_forward_shape() {
        let fs = files(&[("x/src/lib.rs", LIB), ("x/src/other.rs", OTHER)]);
        let all = sites(PlantKind::MisplaceItem, &fs);
        let lines: Vec<(&str, u32)> = all.iter().map(|s| (s.path.as_str(), s.line)).collect();
        assert_eq!(
            lines,
            [
                ("x/src/lib.rs", 3),
                ("x/src/lib.rs", 4),
                ("x/src/other.rs", 1)
            ],
            "go() mentions LIMIT and SECRET, so it stays"
        );
        let public = (0..3)
            .filter_map(|s| generate(PlantKind::MisplaceItem, s, &fs))
            .find(|p| p.sites[0].line == 3 && p.sites[0].path.as_str() == "x/src/lib.rs")
            .unwrap();
        assert_eq!(
            text_of(&public, "x/src/lib.rs"),
            "mod other;\n\npub use crate::other::LIMIT;\nconst SECRET: u32 = 1;\n\npub fn go() -> u32 {\n    LIMIT + SECRET\n}\n"
        );
        assert_eq!(
            text_of(&public, "x/src/other.rs"),
            "pub fn helper() -> u32 {\n    7\n}\n\npub const LIMIT: u32 = 10;\n"
        );
        assert_eq!(public.targets.len(), 2);
        let private = (0..3)
            .filter_map(|s| generate(PlantKind::MisplaceItem, s, &fs))
            .find(|p| p.sites[0].line == 4)
            .unwrap();
        assert!(text_of(&private, "x/src/lib.rs").contains("use crate::other::SECRET;\n"));
        assert!(!text_of(&private, "x/src/lib.rs").contains("pub use crate::other::SECRET"));
        assert!(
            text_of(&private, "x/src/other.rs").ends_with("\npub(crate) const SECRET: u32 = 1;\n")
        );
        // From a module file back to the crate root.
        let up = (0..3)
            .filter_map(|s| generate(PlantKind::MisplaceItem, s, &fs))
            .find(|p| p.sites[0].path.as_str() == "x/src/other.rs")
            .unwrap();
        assert!(text_of(&up, "x/src/other.rs").starts_with("pub use crate::helper;\n"));
        assert!(text_of(&up, "x/src/lib.rs").ends_with("\npub fn helper() -> u32 {\n    7\n}\n"));
    }

    #[test]
    fn plants_are_deterministic() {
        let fs = files(&[("a/src/lib.rs", TOTAL)]);
        for kind in PlantKind::ALL {
            assert_eq!(generate(kind, 0, &fs), generate(kind, 0, &fs));
        }
        assert!(generate(PlantKind::ObscureName, 0, &fs).is_some());
        assert_eq!(generate(PlantKind::ObscureName, 1, &fs), None);
        let second = TOTAL
            .split_once("pub fn total")
            .map(|(_, f)| format!("pub fn total2{f}"))
            .unwrap();
        let two = format!("{TOTAL}\n{second}");
        let fs2 = files(&[("a/src/lib.rs", &two)]);
        let n = sites(PlantKind::ObscureName, &fs2).len();
        assert_eq!(n, 2, "{two}");
        let picked: BTreeSet<Site> = (0..n as u32)
            .map(|s| generate(PlantKind::ObscureName, s, &fs2).unwrap().sites[0].clone())
            .collect();
        assert_eq!(picked.len(), n, "distinct sites");
    }

    #[test]
    fn pair_plants_two_sites() {
        let src = format!(
            "{INLINE_FN}\n{}",
            TOTAL.replace("pub fn total", "pub fn total_of")
        );
        let fs = files(&[("a/src/lib.rs", &src)]);
        let p = generate_pair(PlantKind::InlineFunction, PlantKind::ObscureName, 0, &fs).unwrap();
        assert_eq!(p.depth(), 2);
        assert_eq!(p.kinds, [PlantKind::InlineFunction, PlantKind::ObscureName]);
        let t = text_of(&p, "a/src/lib.rs");
        assert!(t.contains("let sum = a + b;"), "{t}");
        assert!(t.contains("let v1 = item.area();"), "{t}");
        assert!(!t.contains("fn add("), "{t}");
        assert_eq!(p.original_fn.as_deref(), Some("add"));
        assert_eq!(p.targets.len(), 2, "{:?}", p.targets);
        assert!(p.targets[0].end <= p.targets[1].start, "{:?}", p.targets);
        let first = &t[p.targets[0].start as usize..p.targets[0].end as usize];
        let second = &t[p.targets[1].start as usize..p.targets[1].end as usize];
        assert!(first.starts_with("pub fn run("), "{first}");
        assert!(second.starts_with("pub fn total_of("), "{second}");
        // Two plants in the same function are refused.
        let one = files(&[("a/src/lib.rs", TOTAL)]);
        assert_eq!(
            generate_pair(PlantKind::ObscureName, PlantKind::ObscureName, 0, &one),
            None
        );
        // The case file names both plants and the overlay is the planted text.
        let toml = render_case("search/base/x", "pair-0", &p);
        assert!(
            toml.contains("kinds = [\"inline-function\", \"obscure-name\"]\n"),
            "{toml}"
        );
        assert!(
            toml.contains("depth = 2\n") && toml.contains("probes = []\n"),
            "{toml}"
        );
        assert!(toml.contains("original_fn = \"add\"\n"), "{toml}");
    }

    #[test]
    fn text_helpers_collapse_whitespace_and_words() {
        assert_eq!(collapse_ws("  a \n  b\t c "), "a b c");
        assert_eq!(
            replace_word("fun_name(fun_name2, fun_name)", "fun_name", "h"),
            "h(fun_name2, h)"
        );
        assert_eq!(
            fn_names("pub fn a() {}\nfn b<T>() {}\nlet fnord = 1; // fn c"),
            BTreeSet::from(["a".to_string(), "b".to_string(), "c".to_string()])
        );
        for (init, want) in [
            ("shape.area()", Some("area")),
            ("self.items.len()", Some("len")),
            ("name.to_string()", None),
            ("x.as_slice()", Some("slice")),
            ("Rect::new(1, 2)", None),
            ("Line::build(1)", Some("build")),
            ("a.len() + b.len()", None),
            ("foo(1)?", None),
            ("report_for(\"a (b\", &shapes)", Some("report_for")),
        ] {
            assert_eq!(call_name(init).as_deref(), want, "{init}");
        }
        // A method of the extracted function's name in its body is not renamed with it.
        assert_eq!(
            rename_fn(
                "let t = render(r);\nfn render(r: R) -> S {\n    let t = r.render();\n    t\n}",
                "render",
                "finish"
            ),
            "let t = finish(r);\nfn finish(r: R) -> S {\n    let t = r.render();\n    t\n}"
        );
        // ... nor is a variable that the function is named after.
        assert_eq!(
            rename_fn(
                "let wide = wide(a);\nfn wide(a: B) -> bool {\n    let wide = a.x;\n    wide\n}",
                "wide",
                "wider"
            ),
            "let wide = wider(a);\nfn wider(a: B) -> bool {\n    let wide = a.x;\n    wide\n}"
        );
    }
}
