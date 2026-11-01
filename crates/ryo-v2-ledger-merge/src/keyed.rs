//! Three-way merge of a list of keyed items: the sections of a document, the rows of a table.
//!
//! Each list holds every key at most once (the callers number repeated keys). An item is
//! compared as a whole: a change anywhere in it is a change of the item.

use std::collections::HashMap;

/// What becomes of one key.
#[derive(Debug, PartialEq)]
pub(crate) enum Outcome<'a, T> {
    /// The item to write.
    Take(&'a T),
    /// Both sides changed the item, differently. A `None` side removed it (or, for `base`, the
    /// item is new on both sides).
    Conflict {
        base: Option<&'a T>,
        ours: Option<&'a T>,
        theirs: Option<&'a T>,
    },
}

/// One list: `(key, item)` pairs in document order, every key once.
pub(crate) type Keyed<T> = [(String, T)];

/// The merged list, in output order.
///
/// # Contract
/// - C1: a key changed (added, removed or edited) on one side only takes that side's state; a
///   key both sides left as in `base`, or changed to the same item, is kept as it is.
///   tested: keyed::tests::one_sided_changes_are_taken
/// - C2: a key both sides changed to different states (two edits, an edit and a removal, two
///   different additions) is one `Outcome::Conflict`.
///   tested: keyed::tests::different_changes_of_one_key_conflict
/// - C3: the order is that of `ours`; a key `ours` does not have is placed after its nearest
///   predecessor in `theirs` that is in the output, and after the keys that only `ours` added
///   right behind that predecessor (at the start when it has none).
///   tested: keyed::tests::keys_of_theirs_follow_their_predecessor_and_the_additions_of_ours
pub(crate) fn merge<'a, T: PartialEq>(
    base: &'a Keyed<T>,
    ours: &'a Keyed<T>,
    theirs: &'a Keyed<T>,
) -> Vec<Outcome<'a, T>> {
    let index = |list: &'a Keyed<T>| -> HashMap<&'a str, &'a T> {
        list.iter().map(|(k, v)| (k.as_str(), v)).collect()
    };
    let (b, o, t) = (index(base), index(ours), index(theirs));
    let decide = |k: &str| -> Option<Outcome<'a, T>> {
        let (bv, ov, tv) = (b.get(k).copied(), o.get(k).copied(), t.get(k).copied());
        match (bv, ov, tv) {
            (_, Some(x), Some(y)) if x == y => Some(Outcome::Take(x)),
            (Some(bv), Some(x), Some(y)) if x == bv => Some(Outcome::Take(y)),
            (Some(bv), Some(x), Some(y)) if y == bv => Some(Outcome::Take(x)),
            (Some(bv), Some(x), None) | (Some(bv), None, Some(x)) if x == bv => None,
            (None, Some(x), None) | (None, None, Some(x)) => Some(Outcome::Take(x)),
            (_, None, None) => None,
            _ => Some(Outcome::Conflict {
                base: bv,
                ours: ov,
                theirs: tv,
            }),
        }
    };
    let ours_only = |k: &str| !b.contains_key(k) && !t.contains_key(k);

    let mut order: Vec<&str> = ours
        .iter()
        .map(|(k, _)| k.as_str())
        .filter(|k| decide(k).is_some())
        .collect();
    let mut prev: Option<usize> = None;
    for (k, _) in theirs {
        if let Some(i) = order.iter().position(|x| x == k) {
            prev = Some(i);
            continue;
        }
        if decide(k).is_none() {
            continue;
        }
        let mut at = prev.map_or(0, |i| i + 1);
        while at < order.len() && ours_only(order[at]) {
            at += 1;
        }
        order.insert(at, k.as_str());
        prev = Some(at);
    }
    order.into_iter().filter_map(decide).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// The merged list as `key=item` for taken keys and `!key` for conflicts.
    fn show(base: &Keyed<String>, ours: &Keyed<String>, theirs: &Keyed<String>) -> Vec<String> {
        merge(base, ours, theirs)
            .into_iter()
            .map(|o| match o {
                Outcome::Take(v) => v.clone(),
                Outcome::Conflict { ours, theirs, .. } => format!(
                    "!{}|{}",
                    ours.map_or("-", String::as_str),
                    theirs.map_or("-", String::as_str)
                ),
            })
            .collect()
    }

    #[test]
    fn one_sided_changes_are_taken() {
        let base = list(&[("a", "a0"), ("b", "b0"), ("c", "c0"), ("d", "d0")]);
        // ours edits a and removes c; theirs edits b and adds e; d is edited alike on both.
        let ours = list(&[("a", "a1"), ("b", "b0"), ("d", "d1")]);
        let theirs = list(&[
            ("a", "a0"),
            ("b", "b1"),
            ("c", "c0"),
            ("d", "d1"),
            ("e", "e1"),
        ]);
        assert_eq!(show(&base, &ours, &theirs), ["a1", "b1", "d1", "e1"]);
        assert_eq!(show(&base, &base, &base), ["a0", "b0", "c0", "d0"]);
    }

    #[test]
    fn different_changes_of_one_key_conflict() {
        let base = list(&[("a", "a0"), ("b", "b0")]);
        let ours = list(&[("a", "a1"), ("b", "b1"), ("n", "n1")]);
        let theirs = list(&[("a", "a2"), ("n", "n2")]);
        assert_eq!(show(&base, &ours, &theirs), ["!a1|a2", "!b1|-", "!n1|n2"]);
        // A removal against an edit conflicts from either side.
        assert_eq!(
            show(
                &base,
                &list(&[("a", "a0")]),
                &list(&[("a", "a0"), ("b", "b2")])
            ),
            ["a0", "!-|b2"]
        );
    }

    #[test]
    fn keys_of_theirs_follow_their_predecessor_and_the_additions_of_ours() {
        let base = list(&[("a", "a"), ("b", "b"), ("c", "c")]);
        // ours adds x after a; theirs adds y after a and z, w at the very start and the end.
        let ours = list(&[("a", "a"), ("x", "x"), ("b", "b"), ("c", "c")]);
        let theirs = list(&[
            ("z", "z"),
            ("a", "a"),
            ("y", "y"),
            ("y2", "y2"),
            ("b", "b"),
            ("c", "c"),
            ("w", "w"),
        ]);
        assert_eq!(
            show(&base, &ours, &theirs),
            ["z", "a", "x", "y", "y2", "b", "c", "w"]
        );
        // The predecessor of a new key of theirs was removed by ours: the next earlier one counts.
        let ours = list(&[("a", "a"), ("c", "c")]);
        let theirs = list(&[("a", "a"), ("b", "b"), ("y", "y"), ("c", "c")]);
        assert_eq!(show(&base, &ours, &theirs), ["a", "y", "c"]);
    }
}
