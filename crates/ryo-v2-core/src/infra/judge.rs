//! [`Judge`] adapters that need no model: one reads the static metrics, one replays a table.

use crate::app::ports::{Judge, JudgeError};
use crate::domain::{JudgeRequest, Judgement, Noul, Prior, ProposalId, ProviderTag};
use std::collections::BTreeMap;

/// Judges by the static metrics alone: a change that cleans more, opens more room for extension
/// and writes fewer bytes is the likelier worth a check. It finds no flaw.
#[derive(Debug, Clone, Copy, Default)]
pub struct MetricJudge;

/// The prior of `clean`, `extensibility` and `edit_bytes`.
fn metric_prior(clean: i32, extensibility: i32, edit_bytes: u32) -> Prior {
    let raw =
        500 + 25 * i64::from(clean) + 25 * i64::from(extensibility) - i64::from(edit_bytes / 100);
    let permille = u16::try_from(raw.clamp(0, 1000)).unwrap_or(500);
    Prior::new(permille).unwrap_or(Prior::NEUTRAL)
}

impl Judge for MetricJudge {
    /// Gives each proposal a prior from its metrics.
    ///
    /// # Contract
    /// - C1: the prior is `500 + 25 * clean + 25 * extensibility - edit_bytes / 100` clamped to
    ///   0 and 1000, so it never falls when `clean` rises, and no flaw is reported; the provider
    ///   tag is `metric`.
    ///   tested: infra::judge::tests::metric_judge_prior_is_monotone_in_clean
    fn judge(&self, req: &JudgeRequest) -> Result<Judgement, JudgeError> {
        let provider =
            ProviderTag::new("metric").map_err(|e| JudgeError::Provider(e.to_string()))?;
        let priors = req
            .proposals()
            .iter()
            .map(|p| {
                let m = p.metrics();
                (
                    p.id(),
                    metric_prior(m.clean.0, m.extensibility.0, m.edit_bytes),
                )
            })
            .collect();
        Ok(Judgement::new(req, priors, vec![], provider))
    }
}

/// What a replay gives for one proposal.
#[derive(Debug, Clone)]
pub struct ReplayEntry {
    /// The prior to give; `None` gives the neutral one.
    pub prior: Option<Prior>,
    /// The flaw to report, if any.
    pub noul: Option<Noul>,
}

/// Answers from a table recorded earlier, so a run can be repeated without a model.
#[derive(Debug, Clone)]
pub struct ReplayJudge {
    provider: ProviderTag,
    table: BTreeMap<ProposalId, ReplayEntry>,
}

impl ReplayJudge {
    /// A judge that answers as `provider` from `table`.
    pub fn new(provider: ProviderTag, table: BTreeMap<ProposalId, ReplayEntry>) -> ReplayJudge {
        ReplayJudge { provider, table }
    }
}

impl Judge for ReplayJudge {
    /// Looks each proposal up in the table.
    ///
    /// # Contract
    /// - C1: a proposal in the table gets its prior, or the neutral one when the entry has
    ///   none, and its flaw; a proposal not in the table gets the neutral prior and no flaw.
    ///   The provider is the one the judge was built with.
    ///   tested: infra::judge::tests::replay_judge_replays_and_defaults_to_neutral
    fn judge(&self, req: &JudgeRequest) -> Result<Judgement, JudgeError> {
        let mut priors = Vec::new();
        let mut nouls = Vec::new();
        for p in req.proposals() {
            let Some(entry) = self.table.get(&p.id()) else {
                continue;
            };
            priors.push((p.id(), entry.prior.unwrap_or(Prior::NEUTRAL)));
            if let Some(noul) = &entry.noul {
                nouls.push((p.id(), noul.clone()));
            }
        }
        Ok(Judgement::new(req, priors, nouls, self.provider.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        Blueprint, BlueprintId, CleanDelta, Criterion, DecisionRecord, ExtDelta, Ident, IntentCall,
        MutationSpec, Proposal, ReadRange, RelPath, SpecRef, StaticMetrics, TextRange,
    };

    const TEXT: &str = "let a = 1;";

    fn rel() -> RelPath {
        RelPath::parse("src/lib.rs").unwrap()
    }
    fn tr(a: u32, b: u32) -> TextRange {
        TextRange::new(a, b).unwrap()
    }
    /// A rename of `a` to `to`; different `to` give different proposals.
    fn rename(n: u16, to: &str) -> Blueprint {
        Blueprint::new(
            BlueprintId::parse(&format!("01890a5d-ac96-774b-bcce-b302099a{n:04x}")).unwrap(),
            IntentCall::RenameLocal {
                from: Ident::parse("a").unwrap(),
                to: Ident::parse(to).unwrap(),
            },
            vec![MutationSpec::replace(rel(), tr(4, 5), to).unwrap()],
            vec![ReadRange::capture(rel(), TEXT, tr(0, 10)).unwrap()],
            vec![SpecRef::new("t").unwrap()],
            DecisionRecord::new("agent", "why").unwrap(),
        )
        .unwrap()
    }
    fn proposal(n: u16, to: &str, m: StaticMetrics) -> Proposal {
        Proposal::new(&rename(n, to), 1, m)
    }
    fn metrics(clean: i32, ext: i32, bytes: u32) -> StaticMetrics {
        StaticMetrics {
            clean: CleanDelta(clean),
            extensibility: ExtDelta(ext),
            edit_bytes: bytes,
            ..StaticMetrics::default()
        }
    }

    #[test]
    fn metric_judge_prior_is_monotone_in_clean() {
        let cleans = [-100, -30, -1, 0, 1, 2, 10, 30, 100];
        let names: Vec<String> = (0..cleans.len()).map(|i| format!("n{i}")).collect();
        let proposals: Vec<Proposal> = cleans
            .iter()
            .zip(&names)
            .enumerate()
            .map(|(i, (c, n))| proposal(u16::try_from(i).unwrap(), n, metrics(*c, 0, 0)))
            .collect();
        let ids: Vec<ProposalId> = proposals.iter().map(Proposal::id).collect();
        let req = JudgeRequest::new(proposals);
        let j = MetricJudge.judge(&req).unwrap();

        let priors: Vec<u16> = ids.iter().map(|id| j.prior_of(*id).permille()).collect();
        assert!(priors.windows(2).all(|p| p[0] <= p[1]), "{priors:?}");
        assert_eq!(priors[0], 0);
        assert_eq!(priors[3], 500);
        assert_eq!(priors[4], 525);
        assert_eq!(priors[8], 1000);
        assert!(j.nouls().is_empty());
        assert_eq!(j.provider().as_str(), "metric");

        // Extensibility adds, bytes subtract.
        let req = JudgeRequest::new(vec![
            proposal(1, "x", metrics(0, 2, 0)),
            proposal(2, "y", metrics(0, 0, 1_000)),
        ]);
        let j = MetricJudge.judge(&req).unwrap();
        let p = |i: usize| j.prior_of(req.proposals()[i].id()).permille();
        assert_eq!((p(0), p(1)), (550, 490));
    }

    #[test]
    fn replay_judge_replays_and_defaults_to_neutral() {
        let hit = proposal(1, "b", StaticMetrics::default());
        let bare = proposal(2, "c", StaticMetrics::default());
        let miss = proposal(3, "d", StaticMetrics::default());
        let noul = Noul::new(Criterion::NamingMismatch, rel(), tr(4, 5));
        let table = BTreeMap::from([
            (
                hit.id(),
                ReplayEntry {
                    prior: Prior::new(900),
                    noul: Some(noul.clone()),
                },
            ),
            (
                bare.id(),
                ReplayEntry {
                    prior: None,
                    noul: None,
                },
            ),
        ]);
        let judge = ReplayJudge::new(ProviderTag::new("replay").unwrap(), table);
        let req = JudgeRequest::new(vec![hit.clone(), bare.clone(), miss.clone()]);
        let j = judge.judge(&req).unwrap();

        assert_eq!(j.prior_of(hit.id()).permille(), 900);
        assert_eq!(j.noul_of(hit.id()), Some(&noul));
        assert_eq!(j.prior_of(bare.id()), Prior::NEUTRAL);
        assert_eq!(j.noul_of(bare.id()), None);
        assert_eq!(j.prior_of(miss.id()), Prior::NEUTRAL);
        assert_eq!(j.noul_of(miss.id()), None);
        assert_eq!(j.provider().as_str(), "replay");
    }
}
