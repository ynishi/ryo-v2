//! Byte ranges of a text, fingerprints of what was read, and the splice that applies edits.
//!
//! A Blueprint says where to edit as byte ranges against the text it read. A [`ReadRange`]
//! remembers the fingerprint of those bytes, so a later change of the same bytes is noticed
//! ([`Freshness::Stale`]) instead of being edited blindly.

use super::{DomainError, MutationSpec, RelPath};
use std::collections::{BTreeMap, BTreeSet};

/// A half-open byte range `[start, end)` of a text. `start <= end` always holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TextRange {
    start: u32,
    end: u32,
}

impl TextRange {
    /// A range from `start` to `end`.
    ///
    /// # Contract
    /// - C1: refuses `start > end` with [`DomainError::Range`].
    ///   tested: domain::text::tests::text_range_rules
    pub fn new(start: u32, end: u32) -> Result<Self, DomainError> {
        if start > end {
            return Err(DomainError::Range(format!("{start}..{end}")));
        }
        Ok(Self { start, end })
    }

    /// The empty range at `at`.
    pub fn empty_at(at: u32) -> Self {
        Self { start: at, end: at }
    }

    /// First byte of the range.
    pub fn start(&self) -> u32 {
        self.start
    }

    /// One past the last byte of the range.
    pub fn end(&self) -> u32 {
        self.end
    }

    /// Number of bytes.
    pub fn len(&self) -> u32 {
        self.end - self.start
    }

    /// Whether the range holds no byte.
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Whether `other` lies inside `self`.
    ///
    /// # Contract
    /// - C1: `self.start <= other.start && other.end <= self.end`.
    ///   tested: domain::text::tests::text_range_rules
    pub fn contains(&self, other: &TextRange) -> bool {
        self.start <= other.start && other.end <= self.end
    }

    /// Whether editing both ranges would collide.
    ///
    /// # Contract
    /// - C1: two non-empty ranges overlap if and only if `a.start < b.end && b.start < a.end`.
    ///   tested: domain::text::tests::text_range_rules
    /// - C2: an empty range at `p` overlaps a non-empty `[s, e)` if and only if `s <= p < e`:
    ///   inserting at the end of a replaced range is allowed, at its start is not.
    ///   tested: domain::text::tests::text_range_rules
    /// - C3: two empty ranges overlap if and only if they are equal.
    ///   tested: domain::text::tests::text_range_rules
    pub fn overlaps(&self, other: &TextRange) -> bool {
        match (self.is_empty(), other.is_empty()) {
            (false, false) => self.start < other.end && other.start < self.end,
            (true, false) => other.start <= self.start && self.start < other.end,
            (false, true) => self.start <= other.start && other.start < self.end,
            (true, true) => self == other,
        }
    }
}

impl std::fmt::Display for TextRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}..{}", self.start, self.end)
    }
}

/// The hash function of a [`Fingerprint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FpAlgo {
    /// FNV-1a 64 over the length (8 bytes, little endian) followed by the bytes.
    Fnv1a64,
}

/// A fingerprint of a byte string. Not a security hash: it notices change, not tampering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Fingerprint {
    algo: FpAlgo,
    value: u64,
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
const FNV_PREFIX: &str = "fnv1a64:";

impl Fingerprint {
    /// Fingerprint of `bytes`.
    ///
    /// # Contract
    /// - C1: FNV-1a 64 (offset `0xcbf29ce484222325`, prime `0x100000001b3`) over
    ///   `(bytes.len() as u64).to_le_bytes()` followed by `bytes`; the shared vectors hold.
    ///   tested: domain::text::tests::fingerprint_matches_shared_vectors
    pub fn of(bytes: &[u8]) -> Self {
        let mut h = FNV_OFFSET;
        let len = (bytes.len() as u64).to_le_bytes();
        for b in len.iter().chain(bytes) {
            h ^= u64::from(*b);
            h = h.wrapping_mul(FNV_PRIME);
        }
        Self {
            algo: FpAlgo::Fnv1a64,
            value: h,
        }
    }

    /// Parses the [`Display`](std::fmt::Display) form.
    ///
    /// # Contract
    /// - C1: accepts exactly `fnv1a64:` followed by 16 lowercase hex digits, and `Display`
    ///   prints the same text back.
    ///   tested: domain::text::tests::fingerprint_parse_is_strict
    pub fn parse(s: &str) -> Result<Self, DomainError> {
        let bad = || DomainError::Fingerprint(s.to_owned());
        let hex = s.strip_prefix(FNV_PREFIX).ok_or_else(bad)?;
        if hex.len() != 16 || !hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(bad());
        }
        let value = u64::from_str_radix(hex, 16).map_err(|_| bad())?;
        Ok(Self {
            algo: FpAlgo::Fnv1a64,
            value,
        })
    }

    /// The hash function.
    pub fn algo(&self) -> FpAlgo {
        self.algo
    }

    /// The hash value.
    pub fn value(&self) -> u64 {
        self.value
    }
}

impl std::fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{FNV_PREFIX}{:016x}", self.value)
    }
}

/// Whether the bytes a [`ReadRange`] remembers are still the bytes of the current text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Freshness {
    /// The bytes are unchanged.
    Fresh,
    /// The bytes changed.
    Stale {
        /// Fingerprint at capture time.
        expected: Fingerprint,
        /// Fingerprint now.
        actual: Fingerprint,
    },
    /// The current text is too short, or the range no longer falls on char boundaries.
    OutOfRange {
        /// Byte length of the current text.
        len: u32,
    },
}

impl std::fmt::Display for Freshness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Freshness::Fresh => f.write_str("fresh"),
            Freshness::Stale { expected, actual } => {
                write!(f, "stale (expected {expected}, now {actual})")
            }
            Freshness::OutOfRange { len } => write!(f, "out of range (text is {len} bytes)"),
        }
    }
}

/// The bytes of one range of one file that a Blueprint read, with their fingerprint.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReadRange {
    path: RelPath,
    range: TextRange,
    fingerprint: Fingerprint,
}

/// The slice of `text` for `range`, if it is inside and on char boundaries.
fn slice_of(text: &str, range: TextRange) -> Option<&str> {
    let end = usize::try_from(range.end()).ok()?;
    let start = usize::try_from(range.start()).ok()?;
    text.get(start..end)
}

fn text_len(text: &str) -> u32 {
    u32::try_from(text.len()).unwrap_or(u32::MAX)
}

impl ReadRange {
    /// A read with a fingerprint taken elsewhere.
    pub fn new(path: RelPath, range: TextRange, fp: Fingerprint) -> Self {
        Self {
            path,
            range,
            fingerprint: fp,
        }
    }

    /// Reads `range` of `text` and remembers its fingerprint.
    ///
    /// # Contract
    /// - C1: refuses an `end` past the length of `text`, or either end off a char boundary,
    ///   with [`DomainError::Range`].
    ///   tested: domain::text::tests::capture_refuses_bad_ranges
    pub fn capture(path: RelPath, text: &str, range: TextRange) -> Result<Self, DomainError> {
        let bytes = slice_of(text, range).ok_or_else(|| {
            DomainError::Range(format!("{range} of {} bytes in {path}", text.len()))
        })?;
        Ok(Self {
            path,
            range,
            fingerprint: Fingerprint::of(bytes.as_bytes()),
        })
    }

    /// Compares the remembered bytes with `current`, the current text of the same file.
    ///
    /// # Contract
    /// - C1: `OutOfRange{len}` for an end past the length or off a char boundary, `Stale` when
    ///   the bytes differ, `Fresh` otherwise; bytes outside the range never matter.
    ///   tested: domain::text::tests::read_range_is_stale_after_edit_inside
    pub fn check(&self, current: &str) -> Freshness {
        match slice_of(current, self.range) {
            None => Freshness::OutOfRange {
                len: text_len(current),
            },
            Some(s) => {
                let actual = Fingerprint::of(s.as_bytes());
                if actual == self.fingerprint {
                    Freshness::Fresh
                } else {
                    Freshness::Stale {
                        expected: self.fingerprint,
                        actual,
                    }
                }
            }
        }
    }

    /// The file.
    pub fn path(&self) -> &RelPath {
        &self.path
    }

    /// The byte range.
    pub fn range(&self) -> TextRange {
        self.range
    }

    /// The fingerprint of the bytes at capture time.
    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }
}

/// `after` must equal `before` for every path.
///
/// # Contract
/// - C1: returns `Err(paths)`, sorted and unique, for every path whose fingerprint differs or
///   that is missing from one side; `Ok` when there is none.
///   tested: domain::text::tests::revert_intact_reports_drifted_path
pub fn revert_intact(
    before: &[(RelPath, Fingerprint)],
    after: &[(RelPath, Fingerprint)],
) -> Result<(), Vec<RelPath>> {
    let b: BTreeMap<&RelPath, Fingerprint> = before.iter().map(|(p, f)| (p, *f)).collect();
    let a: BTreeMap<&RelPath, Fingerprint> = after.iter().map(|(p, f)| (p, *f)).collect();
    let paths: BTreeSet<&RelPath> = b.keys().chain(a.keys()).copied().collect();
    let bad: Vec<RelPath> = paths
        .into_iter()
        .filter(|p| b.get(p) != a.get(p))
        .cloned()
        .collect();
    if bad.is_empty() { Ok(()) } else { Err(bad) }
}

/// Why a splice could not be done.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpliceError {
    /// A range ends past the text.
    #[error("range {range} is outside a text of {len} bytes")]
    OutOfRange {
        /// The range.
        range: TextRange,
        /// Byte length of the text.
        len: u32,
    },
    /// An offset is inside a multi-byte char.
    #[error("offset {offset} is not on a char boundary")]
    NotCharBoundary {
        /// The offset.
        offset: u32,
    },
    /// Two specs touch the same bytes.
    #[error("two specs overlap")]
    Overlap,
}

/// Applies all specs of one file to `base`.
///
/// # Contract
/// - C1: any overlapping pair of ranges gives `Overlap` (inserting at the start of a replaced
///   range overlaps; at its end does not).
///   tested: domain::text::tests::splice_rejects_insert_at_replace_start
/// - C2: a range past the end of `base` gives `OutOfRange`, an offset inside a char gives
///   `NotCharBoundary`.
///   tested: domain::text::tests::splice_rejects_non_char_boundary
/// - C3: otherwise the specs are applied in descending `start` order, so every offset refers
///   to `base`.
///   tested: domain::text::tests::splice_applies_from_end
pub fn splice(base: &str, specs: &[&MutationSpec]) -> Result<String, SpliceError> {
    let len = text_len(base);
    for s in specs {
        let r = s.range();
        if r.end() > len {
            return Err(SpliceError::OutOfRange { range: r, len });
        }
        for off in [r.start(), r.end()] {
            if !base.is_char_boundary(off as usize) {
                return Err(SpliceError::NotCharBoundary { offset: off });
            }
        }
    }
    for (i, a) in specs.iter().enumerate() {
        if specs[i + 1..]
            .iter()
            .any(|b| a.range().overlaps(&b.range()))
        {
            return Err(SpliceError::Overlap);
        }
    }
    let mut order: Vec<&&MutationSpec> = specs.iter().collect();
    order.sort_by_key(|s| std::cmp::Reverse(s.range().start()));
    let mut out = base.to_owned();
    for s in order {
        let r = s.range();
        out.replace_range(r.start() as usize..r.end() as usize, s.text());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(s: &str) -> RelPath {
        RelPath::parse(s).unwrap()
    }
    fn tr(a: u32, b: u32) -> TextRange {
        TextRange::new(a, b).unwrap()
    }
    fn rep(a: u32, b: u32, t: &str) -> MutationSpec {
        MutationSpec::replace(rel("a.rs"), tr(a, b), t).unwrap()
    }
    fn ins(at: u32, t: &str) -> MutationSpec {
        MutationSpec::insert(rel("a.rs"), at, t).unwrap()
    }

    #[test]
    fn fingerprint_matches_shared_vectors() {
        assert_eq!(Fingerprint::of(b"").to_string(), "fnv1a64:a8c7f832281a39c5");
        assert_eq!(
            Fingerprint::of(b"a = 1").to_string(),
            "fnv1a64:c4f954ac18a041ff"
        );
        assert_eq!(
            Fingerprint::of(b"a = 2").to_string(),
            "fnv1a64:c4f955ac18a043b2"
        );
    }

    #[test]
    fn fingerprint_parse_is_strict() {
        let f = Fingerprint::of(b"a = 1");
        assert_eq!(Fingerprint::parse(&f.to_string()), Ok(f));
        assert_eq!(f.algo(), FpAlgo::Fnv1a64);
        assert_eq!(f.value(), 0xc4f9_54ac_18a0_41ff);
        for bad in [
            "",
            "fnv1a64:",
            "fnv1a64:c4f954ac18a041f",
            "fnv1a64:c4f954ac18a041ff0",
            "fnv1a64:C4F954AC18A041FF",
            "fnv1a64:c4f954ac18a041fg",
            "sha:c4f954ac18a041ff",
            "c4f954ac18a041ff",
        ] {
            assert!(Fingerprint::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn text_range_rules() {
        assert!(TextRange::new(3, 2).is_err());
        assert_eq!(tr(2, 5).len(), 3);
        assert!(TextRange::empty_at(4).is_empty());
        assert_eq!(tr(2, 5).to_string(), "2..5");
        assert!(tr(2, 8).contains(&tr(2, 8)));
        assert!(tr(2, 8).contains(&tr(3, 4)));
        assert!(!tr(2, 8).contains(&tr(1, 4)));
        assert!(!tr(2, 8).contains(&tr(5, 9)));
        // non-empty pairs
        assert!(tr(0, 5).overlaps(&tr(4, 6)));
        assert!(!tr(0, 5).overlaps(&tr(5, 6)));
        assert!(!tr(5, 6).overlaps(&tr(0, 5)));
        // empty at p vs [s, e): s <= p < e
        let e = TextRange::empty_at;
        assert!(e(2).overlaps(&tr(2, 5)) && tr(2, 5).overlaps(&e(2)));
        assert!(e(4).overlaps(&tr(2, 5)));
        assert!(!e(5).overlaps(&tr(2, 5)) && !tr(2, 5).overlaps(&e(5)));
        assert!(!e(1).overlaps(&tr(2, 5)));
        // empty vs empty
        assert!(e(3).overlaps(&e(3)));
        assert!(!e(3).overlaps(&e(4)));
    }

    #[test]
    fn capture_refuses_bad_ranges() {
        let text = "é = 1";
        assert!(ReadRange::capture(rel("a.rs"), text, tr(0, 99)).is_err());
        assert!(ReadRange::capture(rel("a.rs"), text, tr(1, 3)).is_err());
        assert!(ReadRange::capture(rel("a.rs"), text, tr(0, 2)).is_ok());
    }

    #[test]
    fn read_range_is_stale_after_edit_inside() {
        let base = "fn f() { let a = 1; }";
        let r = ReadRange::capture(rel("a.rs"), base, tr(13, 18)).unwrap();
        assert_eq!(&base[13..18], "a = 1");
        assert_eq!(r.check(base), Freshness::Fresh);
        assert_eq!(r.path().as_str(), "a.rs");
        assert_eq!(r.range(), tr(13, 18));
        assert_eq!(r.fingerprint(), Fingerprint::of(b"a = 1"));
        let edited = "fn f() { let a = 2; }";
        assert_eq!(
            r.check(edited),
            Freshness::Stale {
                expected: Fingerprint::of(b"a = 1"),
                actual: Fingerprint::of(b"a = 2"),
            }
        );
        assert_eq!(r.check(&format!("{base} // more")), Freshness::Fresh);
        assert_eq!(r.check(&base[..15]), Freshness::OutOfRange { len: 15 });
        assert_eq!(
            r.check("é").to_string(),
            "out of range (text is 2 bytes)".to_owned()
        );
    }

    #[test]
    fn splice_applies_from_end() {
        let base = "let a = 1;";
        let a = rep(4, 5, "bb");
        let b = ins(0, "// c\n");
        let c = ins(10, " // end");
        let d = MutationSpec::delete(rel("a.rs"), tr(6, 8)).unwrap();
        let out = splice(base, &[&a, &b, &c, &d]).unwrap();
        assert_eq!(out, "// c\nlet bb 1; // end");
        // insert at the end of a replaced range is allowed
        let r = rep(4, 5, "x");
        let i = ins(5, "y");
        assert_eq!(splice(base, &[&r, &i]).unwrap(), "let xy = 1;");
    }

    #[test]
    fn splice_rejects_non_char_boundary() {
        let s = rep(1, 2, "x");
        assert_eq!(
            splice("é", &[&s]),
            Err(SpliceError::NotCharBoundary { offset: 1 })
        );
        let far = rep(0, 9, "x");
        assert_eq!(
            splice("ab", &[&far]),
            Err(SpliceError::OutOfRange {
                range: tr(0, 9),
                len: 2
            })
        );
    }

    #[test]
    fn splice_rejects_insert_at_replace_start() {
        let r = rep(4, 5, "x");
        let i = ins(4, "y");
        assert_eq!(splice("let a = 1;", &[&r, &i]), Err(SpliceError::Overlap));
        assert_eq!(splice("let a = 1;", &[&i, &r]), Err(SpliceError::Overlap));
        let r2 = rep(3, 6, "z");
        assert_eq!(splice("let a = 1;", &[&r, &r2]), Err(SpliceError::Overlap));
    }

    #[test]
    fn revert_intact_reports_drifted_path() {
        let f = |s: &str| Fingerprint::of(s.as_bytes());
        let before = vec![
            (rel("b.rs"), f("1")),
            (rel("a.rs"), f("2")),
            (rel("c.rs"), f("3")),
        ];
        assert_eq!(revert_intact(&before, &before), Ok(()));
        let after = vec![
            (rel("b.rs"), f("X")),
            (rel("a.rs"), f("2")),
            (rel("d.rs"), f("3")),
        ];
        assert_eq!(
            revert_intact(&before, &after),
            Err(vec![rel("b.rs"), rel("c.rs"), rel("d.rs")])
        );
    }
}
