//! A Blueprint: one proposed change, with what it read and why.
//!
//! A Blueprint is never trusted as written. It cannot be built with a spec outside what it
//! read, with a spec in a file that has no [`BaseFp`], or with two specs on the same bytes; the
//! intent check, freshness and the analyzer run later, in `app::apply`.

use super::{
    BaseFp, BlueprintId, DomainError, IntentCall, MutationSpec, NonEmpty, ReadRange, RelPath,
    TextRange,
};
use std::collections::{BTreeMap, BTreeSet};

/// A reference `<doc>#<item>` to one item of a specification document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecRef {
    doc: String,
    item: String,
}

impl SpecRef {
    /// Parses `<doc>#<item>`.
    ///
    /// # Contract
    /// - C1: `doc` is one or more of `[A-Za-z0-9_./-]` and `item` one or more of
    ///   `[A-Za-z0-9_.:-]`, separated by exactly one `#`; anything else (empty, whitespace, no
    ///   `#`, two `#`) is refused with [`DomainError::SpecRef`].
    ///   tested: domain::blueprint::tests::spec_ref_is_doc_hash_item
    pub fn parse(s: &str) -> Result<Self, DomainError> {
        let bad = || DomainError::SpecRef(s.to_owned());
        let (doc, item) = s.split_once('#').ok_or_else(bad)?;
        let doc_ok = !doc.is_empty()
            && doc
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '-'));
        let item_ok = !item.is_empty()
            && item
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'));
        if doc_ok && item_ok {
            Ok(Self {
                doc: doc.to_owned(),
                item: item.to_owned(),
            })
        } else {
            Err(bad())
        }
    }

    /// The document.
    pub fn doc(&self) -> &str {
        &self.doc
    }

    /// The item of the document.
    pub fn item(&self) -> &str {
        &self.item
    }
}

impl std::fmt::Display for SpecRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}#{}", self.doc, self.item)
    }
}

/// Who proposed a Blueprint and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionRecord {
    proposer: String,
    rationale: String,
}

impl DecisionRecord {
    /// A record of the proposer and the rationale.
    ///
    /// # Contract
    /// - C1: refuses a proposer or rationale that is empty after trimming, with
    ///   [`DomainError::Empty`].
    ///   tested: domain::blueprint::tests::records_and_refs_refuse_empty
    pub fn new(proposer: &str, rationale: &str) -> Result<Self, DomainError> {
        if proposer.trim().is_empty() {
            return Err(DomainError::Empty("proposer".to_owned()));
        }
        if rationale.trim().is_empty() {
            return Err(DomainError::Empty("rationale".to_owned()));
        }
        Ok(Self {
            proposer: proposer.to_owned(),
            rationale: rationale.to_owned(),
        })
    }

    /// Who proposed it.
    pub fn proposer(&self) -> &str {
        &self.proposer
    }

    /// Why.
    pub fn rationale(&self) -> &str {
        &self.rationale
    }
}

/// Why a Blueprint or a set of them cannot be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlueprintError {
    /// A Blueprint without a mutation.
    #[error("a blueprint needs at least one mutation")]
    NoSpecs,
    /// Spec number `spec` lies outside every range the Blueprint read.
    #[error("spec {spec} lies outside everything the blueprint read")]
    Uncovered {
        /// Index of the spec.
        spec: usize,
    },
    /// Specs `a` and `b` touch the same bytes.
    #[error("specs {a} and {b} overlap")]
    Overlap {
        /// Lower index.
        a: usize,
        /// Higher index.
        b: usize,
    },
    /// Spec number `spec` is in a file without a base fingerprint.
    #[error("spec {spec} is in a file without a base fingerprint")]
    NoBase {
        /// Index of the spec.
        spec: usize,
    },
    /// Two base fingerprints name one path.
    #[error("two base fingerprints name {0}")]
    DuplicateBase(RelPath),
    /// Two reads have the same path and range.
    #[error("two reads have the same path and range")]
    DuplicateRead,
    /// A set without a Blueprint.
    #[error("a blueprint set needs at least one blueprint")]
    EmptySet,
    /// Two Blueprints of a set share an id.
    #[error("blueprint id {0} appears twice")]
    DuplicateId(BlueprintId),
}

/// The parts of a Blueprint before they are checked against each other.
#[derive(Debug, Clone)]
pub struct BlueprintParts {
    /// The id.
    pub id: BlueprintId,
    /// The intent and its arguments.
    pub intent: IntentCall,
    /// The mutations.
    pub specs: Vec<MutationSpec>,
    /// What was read.
    pub reads: Vec<ReadRange>,
    /// The fingerprint of the whole base text of each file.
    pub bases: Vec<BaseFp>,
    /// The specifications answered to.
    pub spec_refs: Vec<SpecRef>,
    /// Who proposed it and why.
    pub record: DecisionRecord,
}

/// One proposed change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blueprint {
    id: BlueprintId,
    intent: IntentCall,
    specs: NonEmpty<MutationSpec>,
    reads: Vec<ReadRange>,
    bases: Vec<BaseFp>,
    spec_refs: Vec<SpecRef>,
    record: DecisionRecord,
}

impl Blueprint {
    /// Builds a Blueprint, checking the parts against each other.
    ///
    /// # Contract
    /// - C1: no specs gives `NoSpecs`; then two reads of one path and range give
    ///   `DuplicateRead`; then two base fingerprints of one path give `DuplicateBase`; then a
    ///   spec in a file without a base fingerprint gives `NoBase` with the lowest such index;
    ///   then a spec not `contains`-ed by a read of its path gives `Uncovered` (an insert at
    ///   `p` needs a read with `start <= p <= end`); then specs of one file that overlap give
    ///   `Overlap` with the lowest indices.
    ///   tested: domain::blueprint::tests::uncovered_spec_rejected
    ///   tested: domain::blueprint::tests::edited_file_needs_one_base
    /// - C2: overlapping specs of one file are refused; specs of different files never
    ///   overlap.
    ///   tested: domain::blueprint::tests::overlapping_specs_rejected
    /// - C3: every file with a spec has exactly one base fingerprint; a base fingerprint of a
    ///   file without a spec is allowed, and the bases are kept in path order.
    ///   tested: domain::blueprint::tests::edited_file_needs_one_base
    pub fn new(parts: BlueprintParts) -> Result<Self, BlueprintError> {
        let BlueprintParts {
            id,
            intent,
            specs,
            reads,
            mut bases,
            spec_refs,
            record,
        } = parts;
        let specs = NonEmpty::new(specs).ok_or(BlueprintError::NoSpecs)?;
        let mut seen: BTreeSet<(&RelPath, TextRange)> = BTreeSet::new();
        for r in &reads {
            if !seen.insert((r.path(), r.range())) {
                return Err(BlueprintError::DuplicateRead);
            }
        }
        let mut base_paths: BTreeSet<&RelPath> = BTreeSet::new();
        for b in &bases {
            if !base_paths.insert(b.path()) {
                return Err(BlueprintError::DuplicateBase(b.path().clone()));
            }
        }
        for (i, s) in specs.iter().enumerate() {
            if !base_paths.contains(s.path()) {
                return Err(BlueprintError::NoBase { spec: i });
            }
        }
        for (i, s) in specs.iter().enumerate() {
            let covered = reads
                .iter()
                .any(|r| r.path() == s.path() && r.range().contains(&s.range()));
            if !covered {
                return Err(BlueprintError::Uncovered { spec: i });
            }
        }
        let all = specs.as_slice();
        for (a, sa) in all.iter().enumerate() {
            for (b, sb) in all.iter().enumerate().skip(a + 1) {
                if sa.path() == sb.path() && sa.range().overlaps(&sb.range()) {
                    return Err(BlueprintError::Overlap { a, b });
                }
            }
        }
        bases.sort_by(|x, y| x.path().cmp(y.path()));
        Ok(Self {
            id,
            intent,
            specs,
            reads,
            bases,
            spec_refs,
            record,
        })
    }

    /// The id.
    pub fn id(&self) -> BlueprintId {
        self.id
    }

    /// The intent and its arguments.
    pub fn intent(&self) -> &IntentCall {
        &self.intent
    }

    /// The mutations, in the order given.
    pub fn specs(&self) -> &[MutationSpec] {
        self.specs.as_slice()
    }

    /// What was read.
    pub fn reads(&self) -> &[ReadRange] {
        &self.reads
    }

    /// The base fingerprints, in path order, one per path.
    pub fn bases(&self) -> &[BaseFp] {
        &self.bases
    }

    /// The specifications answered to.
    pub fn spec_refs(&self) -> &[SpecRef] {
        &self.spec_refs
    }

    /// Who proposed it and why.
    pub fn record(&self) -> &DecisionRecord {
        &self.record
    }

    /// The specs grouped by file, files in path order, specs in the order given.
    pub fn edits_by_file(&self) -> BTreeMap<&RelPath, Vec<&MutationSpec>> {
        let mut out: BTreeMap<&RelPath, Vec<&MutationSpec>> = BTreeMap::new();
        for s in self.specs.iter() {
            out.entry(s.path()).or_default().push(s);
        }
        out
    }
}

/// The Blueprints proposed for one task: at least one, ids unique, in the order proposed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlueprintSet(NonEmpty<Blueprint>);

impl BlueprintSet {
    /// Builds a set.
    ///
    /// # Contract
    /// - C1: no Blueprint gives `EmptySet`; two with one id give `DuplicateId`.
    ///   tested: domain::blueprint::tests::duplicate_ids_rejected
    pub fn new(items: Vec<Blueprint>) -> Result<Self, BlueprintError> {
        let mut seen = BTreeSet::new();
        for b in &items {
            if !seen.insert(b.id()) {
                return Err(BlueprintError::DuplicateId(b.id()));
            }
        }
        NonEmpty::new(items)
            .map(Self)
            .ok_or(BlueprintError::EmptySet)
    }

    /// The Blueprints in order.
    pub fn iter(&self) -> std::slice::Iter<'_, Blueprint> {
        self.0.iter()
    }

    /// How many (at least 1).
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Always `false`.
    pub fn is_empty(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Ident;

    fn rel(s: &str) -> RelPath {
        RelPath::parse(s).unwrap()
    }
    fn tr(a: u32, b: u32) -> TextRange {
        TextRange::new(a, b).unwrap()
    }
    fn bid(n: u8) -> BlueprintId {
        BlueprintId::parse(&format!("01890a5d-ac96-774b-bcce-b302099a80{n:02x}")).unwrap()
    }
    fn read(path: &str, a: u32, b: u32) -> ReadRange {
        let text = "x".repeat(64);
        ReadRange::capture(rel(path), &text, tr(a, b)).unwrap()
    }
    fn call() -> IntentCall {
        IntentCall::RenameLocal {
            from: Ident::parse("a").unwrap(),
            to: Ident::parse("b").unwrap(),
        }
    }
    fn base(path: &str) -> BaseFp {
        BaseFp::capture(rel(path), &"x".repeat(64))
    }
    /// Builds a Blueprint with one base per file that has a spec.
    fn make(
        id: BlueprintId,
        specs: Vec<MutationSpec>,
        reads: Vec<ReadRange>,
    ) -> Result<Blueprint, BlueprintError> {
        let paths: BTreeSet<&RelPath> = specs.iter().map(MutationSpec::path).collect();
        let bases = paths.into_iter().map(|p| base(p.as_str())).collect();
        make_with(id, specs, reads, bases)
    }
    fn make_with(
        id: BlueprintId,
        specs: Vec<MutationSpec>,
        reads: Vec<ReadRange>,
        bases: Vec<BaseFp>,
    ) -> Result<Blueprint, BlueprintError> {
        Blueprint::new(BlueprintParts {
            id,
            intent: call(),
            specs,
            reads,
            bases,
            spec_refs: vec![SpecRef::parse("spec#1").unwrap()],
            record: DecisionRecord::new("agent", "because").unwrap(),
        })
    }
    fn rep(path: &str, a: u32, b: u32) -> MutationSpec {
        MutationSpec::replace(rel(path), tr(a, b), "b").unwrap()
    }

    #[test]
    fn records_and_refs_refuse_empty() {
        assert!(DecisionRecord::new("", "r").is_err());
        assert!(DecisionRecord::new("p", " ").is_err());
        let r = DecisionRecord::new("p", "r").unwrap();
        assert_eq!((r.proposer(), r.rationale()), ("p", "r"));
    }

    #[test]
    fn spec_ref_is_doc_hash_item() {
        let r = SpecRef::parse("docs/m3.md#4.1:a-b").unwrap();
        assert_eq!((r.doc(), r.item()), ("docs/m3.md", "4.1:a-b"));
        assert_eq!(r.to_string(), "docs/m3.md#4.1:a-b");
        let u = SpecRef::parse("my_doc-2/a_b.md#item_1:x-9").unwrap();
        assert_eq!((u.doc(), u.item()), ("my_doc-2/a_b.md", "item_1:x-9"));
        assert_eq!(SpecRef::parse(&r.to_string()), Ok(r));
        for bad in [
            "", "  ", "m3", "#4", "m3#", "m3#4#5", "m 3#4", "m3#4 ", " m3#4", "m3#4/5", "m:3#4",
            "m3##4", "é#4",
        ] {
            assert_eq!(
                SpecRef::parse(bad),
                Err(DomainError::SpecRef(bad.to_owned())),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn uncovered_spec_rejected() {
        assert_eq!(
            make(bid(1), vec![], vec![read("a.rs", 0, 10)]),
            Err(BlueprintError::NoSpecs)
        );
        assert_eq!(
            make(bid(1), vec![rep("a.rs", 2, 3)], vec![]),
            Err(BlueprintError::Uncovered { spec: 0 })
        );
        // read of another file, and a range past the read
        assert_eq!(
            make(bid(1), vec![rep("a.rs", 2, 3)], vec![read("b.rs", 0, 10)]),
            Err(BlueprintError::Uncovered { spec: 0 })
        );
        assert_eq!(
            make(
                bid(1),
                vec![rep("a.rs", 2, 3), rep("a.rs", 9, 12)],
                vec![read("a.rs", 0, 10)]
            ),
            Err(BlueprintError::Uncovered { spec: 1 })
        );
        // an insert at the end of a read is covered, one past it is not
        let at = |p| MutationSpec::insert(rel("a.rs"), p, "z").unwrap();
        assert!(make(bid(1), vec![at(10)], vec![read("a.rs", 0, 10)]).is_ok());
        assert_eq!(
            make(bid(1), vec![at(11)], vec![read("a.rs", 0, 10)]),
            Err(BlueprintError::Uncovered { spec: 0 })
        );
        assert_eq!(
            make(
                bid(1),
                vec![rep("a.rs", 2, 3)],
                vec![read("a.rs", 0, 10), read("a.rs", 0, 10)]
            ),
            Err(BlueprintError::DuplicateRead)
        );
    }

    #[test]
    fn overlapping_specs_rejected() {
        let reads = || vec![read("a.rs", 0, 20), read("b.rs", 0, 20)];
        assert_eq!(
            make(
                bid(1),
                vec![rep("a.rs", 0, 3), rep("a.rs", 5, 8), rep("a.rs", 7, 9)],
                reads()
            ),
            Err(BlueprintError::Overlap { a: 1, b: 2 })
        );
        // same range in different files is fine
        let b = make(bid(1), vec![rep("a.rs", 0, 3), rep("b.rs", 0, 3)], reads()).unwrap();
        let by_file = b.edits_by_file();
        assert_eq!(by_file.len(), 2);
        assert_eq!(by_file[&rel("a.rs")].len(), 1);
        assert_eq!(b.specs().len(), 2);
        assert_eq!(b.reads().len(), 2);
        assert_eq!(b.spec_refs().len(), 1);
        assert_eq!(b.record().proposer(), "agent");
        assert_eq!(b.id(), bid(1));
        assert_eq!(b.intent(), &call());
    }

    #[test]
    fn edited_file_needs_one_base() {
        let specs = || vec![rep("a.rs", 0, 3), rep("b.rs", 0, 3)];
        let reads = || vec![read("a.rs", 0, 10), read("b.rs", 0, 10)];
        assert_eq!(
            make_with(bid(1), specs(), reads(), vec![base("a.rs")]),
            Err(BlueprintError::NoBase { spec: 1 })
        );
        assert_eq!(
            make_with(bid(1), specs(), reads(), vec![base("b.rs")]),
            Err(BlueprintError::NoBase { spec: 0 })
        );
        // both lack a base: the lowest spec index is reported
        assert_eq!(
            make_with(bid(1), specs(), reads(), vec![]),
            Err(BlueprintError::NoBase { spec: 0 })
        );
        assert_eq!(
            make_with(bid(1), specs(), reads(), vec![base("z.rs")]),
            Err(BlueprintError::NoBase { spec: 0 })
        );
        assert_eq!(
            make_with(
                bid(1),
                specs(),
                reads(),
                vec![base("a.rs"), base("b.rs"), base("a.rs")]
            ),
            Err(BlueprintError::DuplicateBase(rel("a.rs")))
        );
        // a base of a file that is only read is allowed; bases come back in path order
        let ok = make_with(
            bid(1),
            specs(),
            reads(),
            vec![base("z.rs"), base("b.rs"), base("a.rs")],
        )
        .unwrap();
        let paths: Vec<&str> = ok.bases().iter().map(|b| b.path().as_str()).collect();
        assert_eq!(paths, vec!["a.rs", "b.rs", "z.rs"]);
        // order of the checks
        let dup_reads = || vec![read("a.rs", 0, 10), read("a.rs", 0, 10)];
        assert_eq!(
            make_with(
                bid(1),
                vec![rep("a.rs", 0, 3)],
                dup_reads(),
                vec![base("a.rs"), base("a.rs")]
            ),
            Err(BlueprintError::DuplicateRead)
        );
        assert_eq!(
            make_with(bid(1), specs(), reads(), vec![base("a.rs"), base("a.rs")]),
            Err(BlueprintError::DuplicateBase(rel("a.rs")))
        );
        assert_eq!(
            make_with(bid(1), vec![rep("a.rs", 2, 3)], vec![], vec![]),
            Err(BlueprintError::NoBase { spec: 0 })
        );
    }

    #[test]
    fn duplicate_ids_rejected() {
        let one = |n| make(bid(n), vec![rep("a.rs", 0, 3)], vec![read("a.rs", 0, 10)]).unwrap();
        assert_eq!(BlueprintSet::new(vec![]), Err(BlueprintError::EmptySet));
        assert_eq!(
            BlueprintSet::new(vec![one(1), one(2), one(1)]),
            Err(BlueprintError::DuplicateId(bid(1)))
        );
        let set = BlueprintSet::new(vec![one(2), one(1)]).unwrap();
        assert_eq!(set.len(), 2);
        assert!(!set.is_empty());
        let ids: Vec<_> = set.iter().map(Blueprint::id).collect();
        assert_eq!(ids, vec![bid(2), bid(1)]);
    }
}
