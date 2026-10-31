//! What a verdict rests on in the loaded text, and how the files can drift from it.
//!
//! A [`Basis`] lists the base fingerprint of every file a change edits and the read ranges of
//! the files it only reads. Checking it against the current files says whether the edits that
//! were verified against the loaded text may still be written.

use super::{BaseFp, Blueprint, Edit, Freshness, ReadRange, RelPath};
use std::collections::BTreeSet;

/// The text of a path in some state: `None` when the state has no such file.
pub(crate) type View<'a> = dyn Fn(&RelPath) -> Option<String> + 'a;

/// What a verdict rests on in the loaded text: the base fingerprint of every file edited and the
/// read ranges of the files only read.
///
/// # Contract
/// - C1 [invariant]: the base fingerprints are sorted by path and there is one per path; the
///   reads are sorted and unique, and none is on a path of a base fingerprint.
///   tested: domain::basis::tests::basis_new_sorts_and_drops_covered_reads
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Basis {
    files: Vec<BaseFp>,
    reads: Vec<ReadRange>,
}

/// One way the files no longer match a basis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drift {
    /// An edited file's text is not its base text.
    File {
        /// The base fingerprint.
        base: BaseFp,
        /// How the current text differs.
        now: Freshness,
    },
    /// A read-only range is not fresh.
    Read {
        /// The read.
        read: ReadRange,
        /// How the current text differs.
        now: Freshness,
    },
    /// A file of the basis is not there.
    Missing(RelPath),
}

impl Drift {
    /// The file the drift is about.
    pub fn path(&self) -> &RelPath {
        match self {
            Drift::File { base, .. } => base.path(),
            Drift::Read { read, .. } => read.path(),
            Drift::Missing(path) => path,
        }
    }
}

impl Basis {
    /// A basis from base fingerprints and reads.
    ///
    /// # Contract
    /// - C1: sorts the base fingerprints by path keeping the first of a path, drops the reads on
    ///   a path of a base fingerprint, and sorts and dedups the reads.
    ///   tested: domain::basis::tests::basis_new_sorts_and_drops_covered_reads
    pub(crate) fn new(mut files: Vec<BaseFp>, mut reads: Vec<ReadRange>) -> Self {
        files.sort_by(|a, b| a.path().cmp(b.path()));
        files.dedup_by(|later, first| later.path() == first.path());
        let based: BTreeSet<&RelPath> = files.iter().map(BaseFp::path).collect();
        reads.retain(|r| !based.contains(r.path()));
        reads.sort();
        reads.dedup();
        Self { files, reads }
    }

    /// The basis of a chain of Blueprints (root first) whose composed whole-file edits are
    /// `edits`, over the loaded text `base`.
    ///
    /// # Contract
    /// - C1: holds `BaseFp::capture` of the text of `base` for every path on which a Blueprint
    ///   of the chain has a spec or a base fingerprint and for every path of `edits`; never a
    ///   fingerprint of an intermediate state or of a Blueprint's own base.
    ///   tested: domain::basis::tests::of_chain_takes_every_base_from_the_loaded_text
    ///   tested: app::search::tests::search_candidate_basis_is_against_the_loaded_text
    ///   tested: app::search::tests::start_state_file_is_in_the_basis
    /// - C2: holds every read of every Blueprint of the chain whose path is not a path of the
    ///   base fingerprints, once.
    ///   tested: domain::basis::tests::of_chain_takes_every_base_from_the_loaded_text
    /// - C3: a path `base` lacks is not in the basis.
    ///   tested: domain::basis::tests::of_chain_leaves_out_a_file_the_base_lacks
    pub(crate) fn of_chain(chain: &[&Blueprint], edits: &[Edit], base: &View<'_>) -> Self {
        let mut paths: BTreeSet<RelPath> = edits.iter().map(|e| e.path.clone()).collect();
        for bp in chain {
            paths.extend(bp.specs().iter().map(|s| s.path().clone()));
            paths.extend(bp.bases().iter().map(|b| b.path().clone()));
        }
        let files: Vec<BaseFp> = paths
            .into_iter()
            .filter_map(|p| base(&p).map(|t| BaseFp::capture(p, &t)))
            .collect();
        let reads: Vec<ReadRange> = chain
            .iter()
            .flat_map(|bp| bp.reads().iter().cloned())
            .collect();
        Self::new(files, reads)
    }

    /// The base fingerprints, in path order.
    pub fn files(&self) -> &[BaseFp] {
        &self.files
    }

    /// The reads of the files that are only read.
    pub fn reads(&self) -> &[ReadRange] {
        &self.reads
    }

    /// Compares the basis with the current files.
    ///
    /// # Contract
    /// - C1: one drift per entry that does not match, the base fingerprints first, then the
    ///   reads, each in basis order; `Missing` for a path `current` lacks; a drift of a file is
    ///   never `OutOfRange`.
    ///   tested: domain::basis::tests::basis_check_lists_every_drift
    pub fn check(&self, current: &View<'_>) -> Vec<Drift> {
        let mut out = Vec::new();
        for base in &self.files {
            match current(base.path()) {
                None => out.push(Drift::Missing(base.path().clone())),
                Some(t) => match base.check(&t) {
                    Freshness::Fresh => {}
                    now => out.push(Drift::File {
                        base: base.clone(),
                        now,
                    }),
                },
            }
        }
        for read in &self.reads {
            match current(read.path()) {
                None => out.push(Drift::Missing(read.path().clone())),
                Some(t) => match read.check(&t) {
                    Freshness::Fresh => {}
                    now => out.push(Drift::Read {
                        read: read.clone(),
                        now,
                    }),
                },
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        BlueprintId, BlueprintParts, DecisionRecord, Ident, IntentCall, MutationSpec, SpecRef,
        TextRange,
    };
    use std::collections::BTreeMap;

    fn rel(s: &str) -> RelPath {
        RelPath::parse(s).unwrap()
    }
    fn tr(a: u32, b: u32) -> TextRange {
        TextRange::new(a, b).unwrap()
    }
    fn bid(n: u8) -> BlueprintId {
        BlueprintId::parse(&format!("01890a5d-ac96-774b-bcce-b302099a80{n:02x}")).unwrap()
    }
    fn view(texts: &BTreeMap<RelPath, String>) -> impl Fn(&RelPath) -> Option<String> + '_ {
        move |p| texts.get(p).cloned()
    }
    fn texts(items: &[(&str, &str)]) -> BTreeMap<RelPath, String> {
        items
            .iter()
            .map(|(p, t)| (rel(p), (*t).to_owned()))
            .collect()
    }
    fn blueprint(
        n: u8,
        specs: Vec<MutationSpec>,
        reads: Vec<ReadRange>,
        bases: Vec<BaseFp>,
    ) -> Blueprint {
        Blueprint::new(BlueprintParts {
            id: bid(n),
            intent: IntentCall::RenameLocal {
                from: Ident::parse("a").unwrap(),
                to: Ident::parse("b").unwrap(),
            },
            specs,
            reads,
            bases,
            spec_refs: vec![SpecRef::parse("spec#1").unwrap()],
            record: DecisionRecord::new("agent", "why").unwrap(),
        })
        .unwrap()
    }

    const A: &str = "let a = 1;";
    const B: &str = "let b = 2;";

    #[test]
    fn basis_new_sorts_and_drops_covered_reads() {
        let rb = ReadRange::capture(rel("b.rs"), B, tr(0, 3)).unwrap();
        let rb2 = ReadRange::capture(rel("b.rs"), B, tr(4, 5)).unwrap();
        let ra = ReadRange::capture(rel("a.rs"), A, tr(0, 3)).unwrap();
        let first = BaseFp::capture(rel("a.rs"), A);
        let other = BaseFp::capture(rel("a.rs"), B);
        let c = BaseFp::capture(rel("c.rs"), A);
        let basis = Basis::new(
            vec![c.clone(), first.clone(), other],
            vec![
                rb2.clone(),
                ra,
                rb.clone(),
                rb2.clone(),
                ReadRange::capture(rel("c.rs"), A, tr(0, 1)).unwrap(),
            ],
        );
        assert_eq!(basis.files(), &[first, c]);
        assert_eq!(basis.reads(), &[rb, rb2]);
    }

    #[test]
    fn basis_check_lists_every_drift() {
        let a_len = u32::try_from(A.len()).unwrap();
        let bp = blueprint(
            1,
            vec![MutationSpec::replace(rel("a.rs"), tr(4, 5), "b").unwrap()],
            vec![
                ReadRange::capture(rel("a.rs"), A, tr(0, a_len)).unwrap(),
                ReadRange::capture(rel("b.rs"), B, tr(0, 3)).unwrap(),
                ReadRange::capture(rel("c.rs"), A, tr(0, 3)).unwrap(),
            ],
            vec![BaseFp::capture(rel("a.rs"), A)],
        );
        let basis = bp.basis();
        assert_eq!(basis.files().len(), 1);
        assert_eq!(basis.reads().len(), 2);
        assert!(basis.reads().iter().all(|r| r.path() != &rel("a.rs")));

        let same = texts(&[("a.rs", A), ("b.rs", B), ("c.rs", A)]);
        assert!(basis.check(&view(&same)).is_empty());

        let drifted = texts(&[("a.rs", "let a = 1; //"), ("b.rs", "xxx b = 2;")]);
        let drift = basis.check(&view(&drifted));
        assert_eq!(drift.len(), 3);
        assert!(
            matches!(&drift[0], Drift::File { base, now: Freshness::Stale { .. } } if base.path() == &rel("a.rs"))
        );
        assert!(
            matches!(&drift[1], Drift::Read { read, now: Freshness::Stale { .. } } if read.path() == &rel("b.rs"))
        );
        assert_eq!(drift[2], Drift::Missing(rel("c.rs")));
        assert_eq!(drift[2].path(), &rel("c.rs"));

        // an edited file that is gone is missing too, before the reads
        let gone = texts(&[("b.rs", B)]);
        assert_eq!(
            basis.check(&view(&gone)),
            vec![Drift::Missing(rel("a.rs")), Drift::Missing(rel("c.rs"))]
        );
    }

    #[test]
    fn of_chain_takes_every_base_from_the_loaded_text() {
        // The parent edits a.rs; the child, made against the parent's state, edits it again and
        // reads b.rs.
        let parent_state = "let b = 1;";
        let loaded = texts(&[("a.rs", "let a = 1;"), ("b.rs", B), ("s.rs", "start")]);
        let parent = blueprint(
            1,
            vec![MutationSpec::replace(rel("a.rs"), tr(4, 5), "b").unwrap()],
            vec![ReadRange::capture(rel("a.rs"), "let a = 1;", tr(0, 10)).unwrap()],
            vec![BaseFp::capture(rel("a.rs"), "let a = 1;")],
        );
        let child = blueprint(
            2,
            vec![MutationSpec::replace(rel("a.rs"), tr(4, 5), "c").unwrap()],
            vec![
                ReadRange::capture(rel("a.rs"), parent_state, tr(0, 10)).unwrap(),
                ReadRange::capture(rel("b.rs"), B, tr(0, 3)).unwrap(),
            ],
            vec![BaseFp::capture(rel("a.rs"), parent_state)],
        );
        let edits = vec![
            Edit::write(rel("a.rs"), "let c = 1;"),
            Edit::write(rel("s.rs"), "started"),
        ];
        let basis = Basis::of_chain(&[&parent, &child], &edits, &view(&loaded));
        assert_eq!(
            basis.files(),
            &[
                BaseFp::capture(rel("a.rs"), "let a = 1;"),
                BaseFp::capture(rel("s.rs"), "start"),
            ]
        );
        assert_eq!(
            basis.reads(),
            &[ReadRange::capture(rel("b.rs"), B, tr(0, 3)).unwrap()]
        );
        assert!(basis.check(&view(&loaded)).is_empty());
    }

    #[test]
    fn of_chain_leaves_out_a_file_the_base_lacks() {
        let loaded = texts(&[("a.rs", A)]);
        let bp = blueprint(
            1,
            vec![MutationSpec::replace(rel("a.rs"), tr(4, 5), "b").unwrap()],
            vec![ReadRange::capture(rel("a.rs"), A, tr(0, 10)).unwrap()],
            vec![BaseFp::capture(rel("a.rs"), A)],
        );
        let edits = vec![
            Edit::write(rel("new.rs"), "fresh"),
            Edit::delete(rel("gone.rs")),
        ];
        let basis = Basis::of_chain(&[&bp], &edits, &view(&loaded));
        assert_eq!(basis.files(), &[BaseFp::capture(rel("a.rs"), A)]);
        assert!(basis.reads().is_empty());
    }
}
