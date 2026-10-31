//! A three-way merge of sequences (diff3): the line-level fallback outside tables.
//!
//! Items are aligned by their key (a longest common subsequence of keys, base against each
//! side) and a changed stretch is compared by the whole items, so an item whose key stays and
//! whose content changed is aligned with its old self and handed back to the caller to merge.
//! For a text line the key is the line itself.

/// An item of a sequence: what it is aligned by.
pub(crate) trait Item: PartialEq {
    /// Items with equal keys are the same item, possibly changed.
    fn key(&self) -> &str;
}

impl Item for String {
    fn key(&self) -> &str {
        self
    }
}

/// One stretch of the merge, in order.
#[derive(Debug, PartialEq)]
pub(crate) enum Chunk<'a, T> {
    /// An item that all three sequences have, aligned (its content may differ).
    Stable {
        base: &'a T,
        ours: &'a T,
        theirs: &'a T,
    },
    /// Items to write as they are: the stretch only one side changed, or both changed alike.
    Take(&'a [T]),
    /// A stretch both sides changed differently.
    Conflict { ours: &'a [T], theirs: &'a [T] },
}

/// Above this many cells (base items times side items, after the common prefix and suffix) the
/// alignment gives up on the middle and leaves it unmatched, so the stretch becomes one conflict
/// instead of a table of that size. A ledger section is far below it.
pub(crate) const MAX_CELLS: usize = 16_000_000;

/// For each item of `a`, the index of the item of `b` it is aligned with.
fn align<T: Item>(a: &[T], b: &[T]) -> Vec<Option<usize>> {
    let mut out = vec![None; a.len()];
    let pre = a
        .iter()
        .zip(b)
        .take_while(|(x, y)| x.key() == y.key())
        .count();
    let suf = a[pre..]
        .iter()
        .rev()
        .zip(b[pre..].iter().rev())
        .take_while(|(x, y)| x.key() == y.key())
        .count();
    for (i, slot) in out.iter_mut().enumerate().take(pre) {
        *slot = Some(i);
    }
    for k in 0..suf {
        out[a.len() - 1 - k] = Some(b.len() - 1 - k);
    }
    let (am, bm) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    let (n, m) = (am.len(), bm.len());
    if n == 0 || m == 0 || n.saturating_mul(m) > MAX_CELLS {
        return out;
    }
    // lcs[i][j]: the longest common subsequence of am[i..] and bm[j..], row-major.
    let w = m + 1;
    let mut lcs = vec![0u32; (n + 1) * w];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i * w + j] = if am[i].key() == bm[j].key() {
                lcs[(i + 1) * w + j + 1] + 1
            } else {
                lcs[(i + 1) * w + j].max(lcs[i * w + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if am[i].key() == bm[j].key() {
            out[pre + i] = Some(pre + j);
            i += 1;
            j += 1;
        } else if lcs[(i + 1) * w + j] >= lcs[i * w + j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

/// The merge of `ours` and `theirs` against `base`.
///
/// # Contract
/// - C1: a stretch only one side changed is that side's items; a stretch both sides changed to
///   the same items is those items; a stretch both changed differently is one `Chunk::Conflict`,
///   changes that touch (no unchanged item between them) included.
///   tested: diff3::tests::one_sided_and_equal_changes_merge_and_touching_ones_conflict
/// - C2: an item whose key all three sequences keep at aligned places is a `Chunk::Stable`, so
///   its content is merged by the caller.
///   tested: diff3::tests::an_item_kept_by_key_is_stable_whatever_its_content
pub(crate) fn merge<'a, T: Item>(
    base: &'a [T],
    ours: &'a [T],
    theirs: &'a [T],
) -> Vec<Chunk<'a, T>> {
    let (mo, mt) = (align(base, ours), align(base, theirs));
    let mut out = Vec::new();
    let (mut i, mut a, mut b) = (0, 0, 0);
    loop {
        while i < base.len() && mo[i] == Some(a) && mt[i] == Some(b) {
            out.push(Chunk::Stable {
                base: &base[i],
                ours: &ours[a],
                theirs: &theirs[b],
            });
            i += 1;
            a += 1;
            b += 1;
        }
        if i == base.len() && a == ours.len() && b == theirs.len() {
            return out;
        }
        // The next base item both sides kept ends the unstable stretch.
        let next = (i..base.len()).find_map(|k| Some((k, mo[k]?, mt[k]?)));
        let (i2, a2, b2) = next.unwrap_or((base.len(), ours.len(), theirs.len()));
        let (bs, os, ts) = (&base[i..i2], &ours[a..a2], &theirs[b..b2]);
        if os == bs {
            push_take(&mut out, ts);
        } else if ts == bs || os == ts {
            push_take(&mut out, os);
        } else {
            out.push(Chunk::Conflict {
                ours: os,
                theirs: ts,
            });
        }
        (i, a, b) = (i2, a2, b2);
    }
}

fn push_take<'a, T>(out: &mut Vec<Chunk<'a, T>>, items: &'a [T]) {
    if !items.is_empty() {
        out.push(Chunk::Take(items));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    /// The merge written out, conflicts as `<ours|theirs>`.
    fn show(base: &str, ours: &str, theirs: &str) -> String {
        let (b, o, t) = (lines(base), lines(ours), lines(theirs));
        let mut out = Vec::new();
        for c in merge(&b, &o, &t) {
            match c {
                Chunk::Stable { ours, .. } => out.push(ours.clone()),
                Chunk::Take(xs) => out.extend(xs.iter().cloned()),
                Chunk::Conflict { ours, theirs } => {
                    out.push(format!("<{}|{}>", ours.join(" "), theirs.join(" ")))
                }
            }
        }
        out.join(" ")
    }

    #[test]
    fn one_sided_and_equal_changes_merge_and_touching_ones_conflict() {
        assert_eq!(show("a b c d e", "a B c d e", "a b c D e"), "a B c D e");
        assert_eq!(show("a b c", "a x b c", "a b c y"), "a x b c y");
        assert_eq!(show("a b c", "a c", "a b c"), "a c");
        assert_eq!(show("a b c", "a X c", "a X c"), "a X c");
        assert_eq!(show("a b c", "a X c", "a Y c"), "a <X|Y> c");
        // Touching changes: b and c changed with no unchanged line between them.
        assert_eq!(show("a b c d", "a B c d", "a b C d"), "a <B c|b C> d");
        // Two insertions at one place.
        assert_eq!(show("a b", "a x b", "a y b"), "a <x|y> b");
        assert_eq!(show("", "x", "y"), "<x|y>");
        assert_eq!(show("a", "", "a"), "");
    }

    /// A key with a payload: aligned by `k`, compared by both.
    #[derive(Debug, PartialEq)]
    struct Kv(&'static str, &'static str);

    impl Item for Kv {
        fn key(&self) -> &str {
            self.0
        }
    }

    #[test]
    fn an_item_kept_by_key_is_stable_whatever_its_content() {
        let base = [Kv("a", "0"), Kv("t", "0"), Kv("z", "0")];
        let ours = [Kv("a", "0"), Kv("t", "1"), Kv("z", "0")];
        let theirs = [Kv("a", "0"), Kv("t", "2"), Kv("z", "0")];
        let merged = merge(&base, &ours, &theirs);
        assert_eq!(
            merged[1],
            Chunk::Stable {
                base: &base[1],
                ours: &ours[1],
                theirs: &theirs[1]
            }
        );
        // Removed by ours, changed by theirs: the stretch differs on both sides.
        let ours = [Kv("a", "0"), Kv("z", "0")];
        let merged = merge(&base, &ours, &theirs);
        assert_eq!(
            merged[1],
            Chunk::Conflict {
                ours: &[],
                theirs: &theirs[1..2]
            }
        );
    }

    #[test]
    fn alignment_beyond_the_cell_ceiling_leaves_the_middle_unmatched() {
        let n = 4_001;
        let base: Vec<String> = (0..n).map(|i| i.to_string()).collect();
        let mut ours = base.clone();
        ours.insert(0, "o".to_owned());
        ours.push("o".to_owned());
        assert!(n * (n + 2) > MAX_CELLS);
        assert!(align(&base, &ours).iter().all(Option::is_none));
        assert!(
            align(&base[..100], &ours[..102])
                .iter()
                .skip(1)
                .any(Option::is_some)
        );
    }
}
