//! The typed judgement of proposals before they are checked (System 1).
//!
//! A judge sees only what a proposal says it will do: the command, the splices and the static
//! metrics. It answers with a [`Prior`] per proposal and a [`Noul`] for each one it finds a flaw
//! in. Nothing verified reaches it: a [`JudgeRequest`] holds [`Proposal`]s, and a [`Proposal`]
//! is built from a [`Blueprint`] alone.

use super::search::StaticMetrics;
use super::{Blueprint, Command, DomainError, Fingerprint, RelPath, Splice, TextRange};
use std::collections::BTreeSet;

/// How likely a proposal is to be worth checking, in thousandths. At most 1000.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Prior(u16);

impl Prior {
    /// The prior of a proposal nobody judged: 500 of 1000.
    pub const NEUTRAL: Prior = Prior(500);

    /// A prior of `permille` thousandths.
    ///
    /// # Contract
    /// - C1: refuses a value above 1000 with `None`; accepts 0 to 1000.
    ///   tested: domain::judge::tests::prior_out_of_range_rejected
    pub fn new(permille: u16) -> Option<Prior> {
        (permille <= 1000).then_some(Prior(permille))
    }

    /// The prior in thousandths.
    pub fn permille(self) -> u16 {
        self.0
    }
}

/// A kind of flaw a judge looks for in a proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Criterion {
    /// The change reaches across a layer it should not.
    LayerViolation,
    /// The change exposes what the abstraction hides.
    LeakyAbstraction,
    /// A name no longer says what the thing does.
    NamingMismatch,
    /// The change alters what the program does.
    BehaviourChange,
    /// The change contradicts the spec it answers to. Not `SpecMismatch`, which is a resolution.
    SpecConflict,
}

impl Criterion {
    /// Every criterion, in the order a judge is asked about them.
    pub const ALL: [Criterion; 5] = [
        Criterion::LayerViolation,
        Criterion::LeakyAbstraction,
        Criterion::NamingMismatch,
        Criterion::BehaviourChange,
        Criterion::SpecConflict,
    ];

    /// The stable kebab-case name.
    pub fn id(self) -> &'static str {
        match self {
            Criterion::LayerViolation => "layer-violation",
            Criterion::LeakyAbstraction => "leaky-abstraction",
            Criterion::NamingMismatch => "naming-mismatch",
            Criterion::BehaviourChange => "behaviour-change",
            Criterion::SpecConflict => "spec-conflict",
        }
    }

    /// The criterion whose [`id`](Criterion::id) is `s`.
    ///
    /// # Contract
    /// - C1: `parse(c.id()) == Some(c)` for every criterion, and `None` for any other text.
    ///   tested: domain::judge::tests::criterion_ids_round_trip
    pub fn parse(s: &str) -> Option<Criterion> {
        Criterion::ALL.into_iter().find(|c| c.id() == s)
    }
}

impl std::fmt::Display for Criterion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.id())
    }
}

/// A flaw a judge found in a proposal: the criterion and where it shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Noul {
    criterion: Criterion,
    path: RelPath,
    range: TextRange,
}

impl Noul {
    /// A flaw of `criterion` at `range` of `path`.
    pub fn new(criterion: Criterion, path: RelPath, range: TextRange) -> Noul {
        Noul {
            criterion,
            path,
            range,
        }
    }

    /// The criterion violated.
    pub fn criterion(&self) -> Criterion {
        self.criterion
    }

    /// The file.
    pub fn path(&self) -> &RelPath {
        &self.path
    }

    /// Where in the file.
    pub fn range(&self) -> TextRange {
        self.range
    }
}

/// The identity of a proposal by what it does, not by the id of the Blueprint it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProposalId(Fingerprint);

impl ProposalId {
    /// The id of the proposal `bp` makes.
    ///
    /// # Contract
    /// - C1: the fingerprint of the command's text, a newline, then each splice's text and a
    ///   newline; so two Blueprints with different ids and the same command and splices get the
    ///   same id, and a different command gives a different one.
    ///   tested: domain::judge::tests::proposal_id_is_content_not_id
    pub fn of(bp: &Blueprint) -> ProposalId {
        let mut s = bp.command().to_string();
        s.push('\n');
        for splice in bp.splices() {
            s.push_str(&splice.to_string());
            s.push('\n');
        }
        ProposalId(Fingerprint::of(s.as_bytes()))
    }

    /// Parses the [`Display`](std::fmt::Display) form.
    ///
    /// # Contract
    /// - C1: accepts what `Display` prints and refuses any other text.
    ///   tested: domain::judge::tests::proposal_id_is_content_not_id
    pub fn parse(s: &str) -> Result<ProposalId, DomainError> {
        Fingerprint::parse(s).map(ProposalId)
    }

    /// The fingerprint behind the id.
    pub fn fingerprint(self) -> Fingerprint {
        self.0
    }
}

impl std::fmt::Display for ProposalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// What a judge is shown of one Blueprint: its command, its splices and its static metrics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    id: ProposalId,
    depth: u8,
    command: Command,
    splices: Vec<Splice>,
    metrics: StaticMetrics,
}

impl Proposal {
    /// The proposal of `bp` at search depth `depth`.
    pub fn new(bp: &Blueprint, depth: u8, metrics: StaticMetrics) -> Proposal {
        Proposal {
            id: ProposalId::of(bp),
            depth,
            command: bp.command().clone(),
            splices: bp.splices().to_vec(),
            metrics,
        }
    }

    /// The id.
    pub fn id(&self) -> ProposalId {
        self.id
    }

    /// The search depth it was proposed at.
    pub fn depth(&self) -> u8 {
        self.depth
    }

    /// The command and its arguments.
    pub fn command(&self) -> &Command {
        &self.command
    }

    /// The splices.
    pub fn splices(&self) -> &[Splice] {
        &self.splices
    }

    /// The static metrics of its edits.
    pub fn metrics(&self) -> &StaticMetrics {
        &self.metrics
    }
}

/// The proposals put to a judge and the criteria to judge them by.
///
/// # Contract
/// - C1 [invariant]: nothing verified reaches a judge: a request holds only proposals, and a
///   proposal is built from a Blueprint alone, so no Candidate and no verdict is in it.
///   tested: domain::judge::tests::judge_request_holds_proposals_and_all_criteria
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JudgeRequest {
    proposals: Vec<Proposal>,
}

impl JudgeRequest {
    /// A request to judge `proposals` by every criterion.
    pub fn new(proposals: Vec<Proposal>) -> JudgeRequest {
        JudgeRequest { proposals }
    }

    /// The proposals, in the order given.
    pub fn proposals(&self) -> &[Proposal] {
        &self.proposals
    }

    /// The criteria to judge by: all of them.
    pub fn criteria(&self) -> &'static [Criterion] {
        &Criterion::ALL
    }
}

/// Which judge answered: a short label, never empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderTag(String);

impl ProviderTag {
    /// A tag.
    ///
    /// # Contract
    /// - C1: refuses empty or blank text with [`DomainError::Empty`].
    ///   tested: domain::judge::tests::empty_provider_tag_rejected
    pub fn new(s: &str) -> Result<ProviderTag, DomainError> {
        if s.trim().is_empty() {
            return Err(DomainError::Empty("provider tag".to_owned()));
        }
        Ok(ProviderTag(s.to_owned()))
    }

    /// The tag as given.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A judge's answer to a request: priors and flaws, for proposals of that request only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Judgement {
    priors: Vec<(ProposalId, Prior)>,
    nouls: Vec<(ProposalId, Noul)>,
    provider: ProviderTag,
}

impl Judgement {
    /// The answer `priors` and `nouls` of `provider` to `req`.
    ///
    /// # Contract
    /// - C1: an entry whose id is not a proposal of `req` is dropped, and of a repeated id
    ///   only the first entry is kept, in priors and in flaws alike.
    ///   tested: domain::judge::tests::judgement_ignores_unknown_proposals
    pub fn new(
        req: &JudgeRequest,
        priors: Vec<(ProposalId, Prior)>,
        nouls: Vec<(ProposalId, Noul)>,
        provider: ProviderTag,
    ) -> Judgement {
        let known: BTreeSet<ProposalId> = req.proposals().iter().map(Proposal::id).collect();
        let mut seen = BTreeSet::new();
        let priors = priors
            .into_iter()
            .filter(|(id, _)| known.contains(id) && seen.insert(*id))
            .collect();
        let mut seen = BTreeSet::new();
        let nouls = nouls
            .into_iter()
            .filter(|(id, _)| known.contains(id) && seen.insert(*id))
            .collect();
        Judgement {
            priors,
            nouls,
            provider,
        }
    }

    /// The prior of `id`.
    ///
    /// # Contract
    /// - C1: the prior given for `id`, or `Prior::NEUTRAL` when the judge gave none.
    ///   tested: domain::judge::tests::judgement_ignores_unknown_proposals
    pub fn prior_of(&self, id: ProposalId) -> Prior {
        self.priors
            .iter()
            .find(|(p, _)| *p == id)
            .map_or(Prior::NEUTRAL, |(_, prior)| *prior)
    }

    /// The flaw found in `id`, if any.
    pub fn noul_of(&self, id: ProposalId) -> Option<&Noul> {
        self.nouls.iter().find(|(p, _)| *p == id).map(|(_, n)| n)
    }

    /// The priors kept, in the order given.
    pub fn priors(&self) -> &[(ProposalId, Prior)] {
        &self.priors
    }

    /// The flaws kept, in the order given.
    pub fn nouls(&self) -> &[(ProposalId, Noul)] {
        &self.nouls
    }

    /// Who answered.
    pub fn provider(&self) -> &ProviderTag {
        &self.provider
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{BaseFp, BlueprintId, BlueprintParts, DecisionRecord, Ident, ReadRange};

    fn rel(s: &str) -> RelPath {
        RelPath::parse(s).unwrap()
    }
    fn tr(a: u32, b: u32) -> TextRange {
        TextRange::new(a, b).unwrap()
    }
    fn bid(n: u8) -> BlueprintId {
        BlueprintId::parse(&format!("01890a5d-ac96-774b-bcce-b302099a80{n:02x}")).unwrap()
    }
    fn bp(n: u8, to: &str) -> Blueprint {
        let read = ReadRange::capture(rel("a.rs"), "let a = a;", tr(0, 10)).unwrap();
        Blueprint::new(BlueprintParts {
            id: bid(n),
            command: Command::RenameLocal {
                from: Ident::parse("a").unwrap(),
                to: Ident::parse(to).unwrap(),
            },
            splices: vec![Splice::replace(rel("a.rs"), tr(4, 5), to).unwrap()],
            reads: vec![read],
            bases: vec![BaseFp::capture(rel("a.rs"), "let a = a;")],
            spec_refs: vec![],
            record: DecisionRecord::new("agent", "because").unwrap(),
            builds_on: None,
        })
        .unwrap()
    }
    fn prop(n: u8, to: &str) -> Proposal {
        Proposal::new(&bp(n, to), 1, StaticMetrics::default())
    }
    fn tag() -> ProviderTag {
        ProviderTag::new("test").unwrap()
    }

    #[test]
    fn prior_out_of_range_rejected() {
        assert_eq!(Prior::new(1001), None);
        assert_eq!(Prior::new(u16::MAX), None);
        assert_eq!(Prior::new(0).map(Prior::permille), Some(0));
        assert_eq!(Prior::new(1000).map(Prior::permille), Some(1000));
        assert_eq!(Prior::NEUTRAL.permille(), 500);
        assert!(Prior::new(100) < Prior::new(200));
    }

    #[test]
    fn criterion_ids_round_trip() {
        for c in Criterion::ALL {
            assert_eq!(Criterion::parse(c.id()), Some(c));
            assert_eq!(c.to_string(), c.id());
        }
        assert_eq!(Criterion::ALL.len(), 5);
        assert_eq!(Criterion::SpecConflict.id(), "spec-conflict");
        assert_eq!(Criterion::LayerViolation.id(), "layer-violation");
        assert_eq!(Criterion::parse("SpecConflict"), None);
        assert_eq!(Criterion::parse(""), None);
    }

    #[test]
    fn empty_provider_tag_rejected() {
        assert_eq!(
            ProviderTag::new(""),
            Err(DomainError::Empty("provider tag".to_owned()))
        );
        assert!(ProviderTag::new("  ").is_err());
        assert_eq!(ProviderTag::new("metric").unwrap().as_str(), "metric");
    }

    #[test]
    fn proposal_id_is_content_not_id() {
        let a = ProposalId::of(&bp(1, "b"));
        assert_eq!(a, ProposalId::of(&bp(2, "b")));
        assert_ne!(a, ProposalId::of(&bp(1, "c")));
        assert_eq!(
            Proposal::new(&bp(1, "b"), 3, StaticMetrics::default()).id(),
            a
        );
        assert_eq!(ProposalId::parse(&a.to_string()), Ok(a));
        assert_eq!(a.fingerprint().to_string(), a.to_string());
        assert!(ProposalId::parse("nope").is_err());
    }

    #[test]
    fn judge_request_holds_proposals_and_all_criteria() {
        let p = prop(1, "b");
        assert_eq!(p.depth(), 1);
        assert_eq!(p.command().to_string(), "rename-local a -> b");
        assert_eq!(p.splices().len(), 1);
        assert_eq!(p.metrics(), &StaticMetrics::default());
        let req = JudgeRequest::new(vec![p.clone(), prop(1, "c")]);
        assert_eq!(req.proposals()[0], p);
        assert_eq!(req.proposals().len(), 2);
        assert_eq!(req.criteria(), &Criterion::ALL);
    }

    #[test]
    fn judgement_ignores_unknown_proposals() {
        let (p, q) = (prop(1, "b"), prop(1, "c"));
        let stranger = prop(1, "d").id();
        let req = JudgeRequest::new(vec![p.clone(), q.clone()]);
        let hi = Prior::new(900).unwrap();
        let lo = Prior::new(100).unwrap();
        let noul = |c| Noul::new(c, rel("a.rs"), tr(4, 5));
        let j = Judgement::new(
            &req,
            vec![(p.id(), hi), (stranger, lo), (p.id(), lo)],
            vec![
                (q.id(), noul(Criterion::NamingMismatch)),
                (stranger, noul(Criterion::LayerViolation)),
                (q.id(), noul(Criterion::SpecConflict)),
            ],
            tag(),
        );
        assert_eq!(j.priors(), &[(p.id(), hi)]);
        assert_eq!(j.prior_of(p.id()), hi);
        assert_eq!(j.prior_of(q.id()), Prior::NEUTRAL);
        assert_eq!(j.prior_of(stranger), Prior::NEUTRAL);
        assert_eq!(j.nouls().len(), 1);
        let n = j.noul_of(q.id()).unwrap();
        assert_eq!(n.criterion(), Criterion::NamingMismatch);
        assert_eq!((n.path(), n.range()), (&rel("a.rs"), tr(4, 5)));
        assert!(j.noul_of(p.id()).is_none());
        assert!(j.noul_of(stranger).is_none());
        assert_eq!(j.provider().as_str(), "test");
    }
}
