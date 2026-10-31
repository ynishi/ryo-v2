//! A Blueprint: one proposed change, with what it read and why.
//!
//! A Blueprint is never trusted as written. It cannot be built with a spec outside what it
//! read, or with two specs on the same bytes; the intent check, freshness and the analyzer run
//! later, in `app::apply`.

use super::{
    BlueprintId, DomainError, IntentCall, MutationSpec, NonEmpty, ReadRange, RelPath, TextRange,
};
use std::collections::{BTreeMap, BTreeSet};

/// A reference to the specification a Blueprint answers to (free text, never empty).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecRef(String);

impl SpecRef {
    /// Wraps a reference.
    ///
    /// # Contract
    /// - C1: refuses text that is empty after trimming, with [`DomainError::Empty`].
    ///   tested: domain::blueprint::tests::records_and_refs_refuse_empty
    pub fn new(s: &str) -> Result<Self, DomainError> {
        if s.trim().is_empty() {
            return Err(DomainError::Empty("spec reference".to_owned()));
        }
        Ok(Self(s.to_owned()))
    }

    /// The reference as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SpecRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
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

/// One proposed change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blueprint {
    id: BlueprintId,
    intent: IntentCall,
    specs: NonEmpty<MutationSpec>,
    reads: Vec<ReadRange>,
    spec_refs: Vec<SpecRef>,
    record: DecisionRecord,
}

impl Blueprint {
    /// Builds a Blueprint, checking the parts against each other.
    ///
    /// # Contract
    /// - C1: no specs gives `NoSpecs`; then two reads of one path and range give
    ///   `DuplicateRead`; then a spec not `contains`-ed by a read of its path gives
    ///   `Uncovered` (an insert at `p` needs a read with `start <= p <= end`); then specs
    ///   of one file that overlap give `Overlap` with the lowest indices.
    ///   tested: domain::blueprint::tests::uncovered_spec_rejected
    /// - C2: overlapping specs of one file are refused; specs of different files never
    ///   overlap.
    ///   tested: domain::blueprint::tests::overlapping_specs_rejected
    pub fn new(
        id: BlueprintId,
        intent: IntentCall,
        specs: Vec<MutationSpec>,
        reads: Vec<ReadRange>,
        spec_refs: Vec<SpecRef>,
        record: DecisionRecord,
    ) -> Result<Self, BlueprintError> {
        let specs = NonEmpty::new(specs).ok_or(BlueprintError::NoSpecs)?;
        let mut seen: BTreeSet<(&RelPath, TextRange)> = BTreeSet::new();
        for r in &reads {
            if !seen.insert((r.path(), r.range())) {
                return Err(BlueprintError::DuplicateRead);
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
        Ok(Self {
            id,
            intent,
            specs,
            reads,
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
    fn make(
        id: BlueprintId,
        specs: Vec<MutationSpec>,
        reads: Vec<ReadRange>,
    ) -> Result<Blueprint, BlueprintError> {
        Blueprint::new(
            id,
            call(),
            specs,
            reads,
            vec![SpecRef::new("spec#1").unwrap()],
            DecisionRecord::new("agent", "because").unwrap(),
        )
    }
    fn rep(path: &str, a: u32, b: u32) -> MutationSpec {
        MutationSpec::replace(rel(path), tr(a, b), "b").unwrap()
    }

    #[test]
    fn records_and_refs_refuse_empty() {
        assert!(SpecRef::new("").is_err());
        assert!(SpecRef::new("  ").is_err());
        assert_eq!(SpecRef::new("m3#4").unwrap().as_str(), "m3#4");
        assert!(DecisionRecord::new("", "r").is_err());
        assert!(DecisionRecord::new("p", " ").is_err());
        let r = DecisionRecord::new("p", "r").unwrap();
        assert_eq!((r.proposer(), r.rationale()), ("p", "r"));
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
