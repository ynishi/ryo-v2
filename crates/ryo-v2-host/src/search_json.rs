//! The search request JSON to domain types, the replay judge fixture, and the `SearchReport` as
//! the JSON the `search` command prints.

use crate::blueprint_json::parse_blueprint_values;
use crate::resolution_json::resolution_to_json;
use anyhow::{Context, Result, anyhow, bail};
use ryo_v2_core::app::search::{DEFAULT_SEARCH_BUDGET, SearchReport};
use ryo_v2_core::domain::{
    Blueprint, BlueprintId, ChangeProbe, Criterion, Ident, Noul, Place, Prior, ProposalId,
    ProviderTag, RelPath, SearchBudget, TextRange,
};
use ryo_v2_core::infra::judge::{ReplayEntry, ReplayJudge};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::num::NonZeroU16;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestJson {
    #[serde(default)]
    places: Vec<PlaceJson>,
    #[serde(default)]
    probes: Vec<ProbeJson>,
    #[serde(default)]
    roots: Vec<Value>,
    #[serde(default)]
    budget: Option<BudgetJson>,
    #[serde(default)]
    judge: Option<JudgeJson>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlaceJson {
    path: String,
    #[serde(default)]
    start: Option<u32>,
    #[serde(default)]
    end: Option<u32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProbeJson {
    kind: String,
    #[serde(rename = "type")]
    ty: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BudgetJson {
    beam: Option<u16>,
    max_expand: Option<u16>,
    beam_parents: Option<u8>,
    max_depth: Option<u8>,
    per_candidate_ms: Option<u64>,
    soft_ms: Option<u64>,
    hard_ms: Option<u64>,
    audit_every: Option<NonZeroU16>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JudgeJson {
    kind: String,
    #[serde(default)]
    file: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplayJson {
    provider: String,
    entries: Vec<EntryJson>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EntryJson {
    proposal: String,
    #[serde(default)]
    prior: Option<u16>,
    #[serde(default)]
    noul: Option<NoulJson>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoulJson {
    criterion: String,
    path: String,
    start: u32,
    end: u32,
}

/// Which judge answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JudgeSpec {
    /// The metric judge: priors from the static metrics, no flaws.
    Metric,
    /// A recorded fixture, read from this file.
    Replay(String),
}

/// A parsed search request.
#[derive(Debug)]
pub(crate) struct Request {
    pub(crate) places: Vec<Place>,
    pub(crate) probes: Vec<ChangeProbe>,
    pub(crate) roots: Vec<Blueprint>,
    /// Ids the host filled in for roots without one.
    pub(crate) generated: Vec<BlueprintId>,
    pub(crate) budget: SearchBudget,
    pub(crate) judge: JudgeSpec,
}

fn probe(index: usize, p: ProbeJson) -> Result<ChangeProbe> {
    let ty = Ident::parse(&p.ty).map_err(|e| anyhow!("probe {index}: type: {e}"))?;
    match p.kind.as_str() {
        "add-variant" => Ok(ChangeProbe::AddVariant { ty }),
        "add-field" => Ok(ChangeProbe::AddField { ty }),
        "add-method" => Ok(ChangeProbe::AddMethod { ty }),
        other => {
            bail!("probe {index}: unknown kind {other:?} (add-variant, add-field, add-method)")
        }
    }
}

fn place(index: usize, t: PlaceJson) -> Result<Place> {
    let path = RelPath::parse(&t.path).map_err(|e| anyhow!("place {index}: path: {e}"))?;
    let range = match (t.start, t.end) {
        (Some(s), Some(e)) => {
            Some(TextRange::new(s, e).map_err(|e| anyhow!("place {index}: range: {e}"))?)
        }
        (None, None) => None,
        _ => bail!("place {index}: start and end go together"),
    };
    Ok(Place::new(path, range))
}

/// Parses a search request. Unknown fields are errors; the budget starts from
/// `DEFAULT_SEARCH_BUDGET` and each given field replaces its number; a request with neither
/// places nor roots is an error (the expander would have nowhere to look).
pub(crate) fn parse_request(json: &str) -> Result<Request> {
    let req: RequestJson = serde_json::from_str(json).context("search request JSON")?;
    if req.places.is_empty() && req.roots.is_empty() {
        bail!("search request: no places and no roots; give at least one");
    }
    let places = req
        .places
        .into_iter()
        .enumerate()
        .map(|(i, t)| place(i, t))
        .collect::<Result<Vec<_>>>()?;
    let probes = req
        .probes
        .into_iter()
        .enumerate()
        .map(|(i, p)| probe(i, p))
        .collect::<Result<Vec<_>>>()?;
    let (roots, generated) = if req.roots.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        let parsed = parse_blueprint_values(req.roots).context("roots")?;
        (parsed.set.iter().cloned().collect(), parsed.generated)
    };
    let mut budget = DEFAULT_SEARCH_BUDGET;
    if let Some(b) = req.budget {
        budget.beam = b.beam.unwrap_or(budget.beam);
        budget.max_expand = b.max_expand.unwrap_or(budget.max_expand);
        budget.beam_parents = b.beam_parents.unwrap_or(budget.beam_parents);
        budget.max_depth = b.max_depth.unwrap_or(budget.max_depth);
        budget.per_candidate_ms = b.per_candidate_ms.unwrap_or(budget.per_candidate_ms);
        budget.soft_ms = b.soft_ms.unwrap_or(budget.soft_ms);
        budget.hard_ms = b.hard_ms.unwrap_or(budget.hard_ms);
        budget.audit_every = b.audit_every.unwrap_or(budget.audit_every);
    }
    let judge = match req.judge {
        None => JudgeSpec::Metric,
        Some(j) => match (j.kind.as_str(), j.file) {
            ("metric", None) => JudgeSpec::Metric,
            ("metric", Some(_)) => bail!("judge: kind metric takes no file"),
            ("replay", Some(file)) => JudgeSpec::Replay(file),
            ("replay", None) => bail!("judge: kind replay needs a file"),
            (other, _) => bail!("judge: unknown kind {other:?} (metric, replay)"),
        },
    };
    Ok(Request {
        places,
        probes,
        roots,
        generated,
        budget,
        judge,
    })
}

/// Parses a replay fixture: `{provider, entries: [{proposal, prior?, noul?}]}`.
pub(crate) fn parse_replay(json: &str) -> Result<ReplayJudge> {
    let file: ReplayJson = serde_json::from_str(json).context("replay fixture JSON")?;
    let provider =
        ProviderTag::new(&file.provider).map_err(|e| anyhow!("replay fixture: provider: {e}"))?;
    let mut table = BTreeMap::new();
    for (i, e) in file.entries.into_iter().enumerate() {
        let id = ProposalId::parse(&e.proposal)
            .map_err(|err| anyhow!("replay entry {i}: proposal: {err}"))?;
        let prior = match e.prior {
            Some(p) => Some(Prior::new(p).ok_or_else(|| {
                anyhow!("replay entry {i}: prior {p} is out of range (0 to 1000)")
            })?),
            None => None,
        };
        let noul = match e.noul {
            Some(n) => {
                let criterion = Criterion::parse(&n.criterion).ok_or_else(|| {
                    anyhow!("replay entry {i}: unknown criterion {:?}", n.criterion)
                })?;
                let path = RelPath::parse(&n.path)
                    .map_err(|err| anyhow!("replay entry {i}: noul path: {err}"))?;
                let range = TextRange::new(n.start, n.end)
                    .map_err(|err| anyhow!("replay entry {i}: noul range: {err}"))?;
                Some(Noul::new(criterion, path, range))
            }
            None => None,
        };
        if table.insert(id, ReplayEntry { prior, noul }).is_some() {
            bail!("replay entry {i}: proposal {id} appears twice");
        }
    }
    Ok(ReplayJudge::new(provider, table))
}

/// The JSON document `search` prints for a `SearchReport`. `rank` counts from 1.
pub(crate) fn report_to_json(r: &SearchReport) -> Value {
    let ranking: Vec<Value> = r
        .ranking
        .iter()
        .enumerate()
        .map(|(i, n)| {
            json!({
                "rank": i + 1,
                "node": n.node.to_string(),
                "depth": n.depth,
                "path": n.path.iter().map(ToString::to_string).collect::<Vec<_>>(),
                "reward": {
                    "passed": n.reward.passed,
                    "clean": n.reward.clean,
                    "extensibility": n.reward.extensibility,
                    "edit_bytes": n.reward.edit_bytes,
                },
                "reward_score": n.reward_score,
                "prior": n.prior.permille(),
            })
        })
        .collect();
    let depths: Vec<Value> = r
        .depths
        .iter()
        .map(|d| {
            json!({
                "depth": d.depth,
                "parents": d.parents,
                "generated": d.generated,
                "after_stages": d.after_stages,
                "kept": d.kept,
                "audited": d.audited,
                "checked": d.checked,
                "passed": d.passed,
                "timed_out": d.timed_out,
                "expand_ms": d.expand_ms,
                "assess_ms": d.assess_ms,
                "judge_ms": d.judge_ms,
                "check_ms": d.check_ms,
                "writes": d.writes,
            })
        })
        .collect();
    json!({
        "resolution": resolution_to_json(&r.resolution),
        "ranking": ranking,
        "depths": depths,
        "judge": {
            "providers": r.judge_providers.iter().map(|p| p.as_str().to_owned()).collect::<Vec<_>>(),
            "errors": r.judge_errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
        },
        "expand_errors": r.expand_errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "budget_exhausted": r.budget_exhausted,
        "soft_stopped": r.soft_stopped,
        "memory_stopped": r.memory_stopped,
        "elapsed_ms": r.elapsed_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryo_v2_core::app::search::{DepthStats, RankedNode};
    use ryo_v2_core::domain::{Fingerprint, Infeasible, Resolution, Reward, SearchTree};

    #[test]
    fn request_rejects_unknown_fields() {
        let ok = "{\"places\":[{\"path\":\"src/lib.rs\"}]}";
        let req = parse_request(ok).expect("minimal request");
        assert_eq!(req.places.len(), 1);
        assert_eq!(req.budget, DEFAULT_SEARCH_BUDGET);
        assert_eq!(req.judge, JudgeSpec::Metric);
        for bad in [
            "{\"places\":[{\"path\":\"src/lib.rs\"}],\"extra\":1}",
            "{\"places\":[{\"path\":\"src/lib.rs\",\"line\":3}]}",
            "{\"places\":[{\"path\":\"src/lib.rs\"}],\"budget\":{\"depth\":1}}",
            "{\"places\":[{\"path\":\"src/lib.rs\"}],\"probes\":[{\"kind\":\"add-field\",\"type\":\"Rect\",\"x\":1}]}",
        ] {
            assert!(parse_request(bad).is_err(), "{bad}");
        }
        // Neither places nor roots, a half range, an unknown probe or judge kind.
        assert!(parse_request("{}").is_err());
        assert!(parse_request("{\"places\":[{\"path\":\"a.rs\",\"start\":1}]}").is_err());
        assert!(parse_request("{\"places\":[{\"path\":\"a.rs\"}],\"probes\":[{\"kind\":\"add-x\",\"type\":\"T\"}]}").is_err());
        assert!(
            parse_request("{\"places\":[{\"path\":\"a.rs\"}],\"judge\":{\"kind\":\"llm\"}}")
                .is_err()
        );
        // Overrides replace only the fields given.
        let req = parse_request(
            "{\"places\":[{\"path\":\"a.rs\",\"start\":0,\"end\":5}],\"budget\":{\"beam\":5,\"max_depth\":1},\"probes\":[{\"kind\":\"add-variant\",\"type\":\"Shape\"}],\"judge\":{\"kind\":\"replay\",\"file\":\"r.json\"}}",
        )
        .expect("overrides");
        assert_eq!((req.budget.beam, req.budget.max_depth), (5, 1));
        assert_eq!(req.budget.max_expand, DEFAULT_SEARCH_BUDGET.max_expand);
        assert_eq!(req.probes.len(), 1);
        assert_eq!(req.judge, JudgeSpec::Replay("r.json".into()));
    }

    #[test]
    fn replay_fixture_parses() {
        let id = Fingerprint::of(b"p").to_string();
        let ok = format!(
            "{{\"provider\":\"recorded\",\"entries\":[{{\"proposal\":\"{id}\",\"prior\":900,\"noul\":{{\"criterion\":\"naming-mismatch\",\"path\":\"src/lib.rs\",\"start\":1,\"end\":4}}}}]}}"
        );
        assert!(parse_replay(&ok).is_ok(), "{:?}", parse_replay(&ok).err());
        // A prior above 1000, an unknown criterion, a repeated proposal and an unknown field.
        assert!(parse_replay(&ok.replace("900", "1001")).is_err());
        assert!(parse_replay(&ok.replace("naming-mismatch", "vibes")).is_err());
        let twice = ok.replace("]}", &format!(",{{\"proposal\":\"{id}\"}}]}}"));
        assert!(parse_replay(&twice).is_err(), "{twice}");
        assert!(parse_replay(&ok.replace("recorded", "")).is_err());
        assert!(parse_replay(&ok.replace("\"prior\"", "\"weight\"")).is_err());
    }

    #[test]
    fn report_lists_ranking_in_order() {
        let root = SearchTree::new(Vec::new()).root();
        let node = |score: i64| RankedNode {
            node: root,
            path: Vec::new(),
            depth: 1,
            reward: Reward {
                passed: true,
                clean: 1,
                extensibility: 2,
                edit_bytes: 30,
            },
            reward_score: score,
            prior: Prior::NEUTRAL,
        };
        let report = SearchReport {
            resolution: Resolution::Failed(Infeasible::new(vec![], vec![], true).expect("budget")),
            ranking: vec![node(50), node(30)],
            trace: Vec::new(),
            depths: vec![DepthStats {
                depth: 1,
                generated: 7,
                kept: 5,
                writes: 9,
                ..DepthStats::default()
            }],
            judge_providers: vec![ProviderTag::new("metric").expect("tag")],
            judge_errors: Vec::new(),
            expand_errors: Vec::new(),
            budget_exhausted: false,
            soft_stopped: true,
            memory_stopped: false,
            elapsed_ms: 12,
        };
        let v = report_to_json(&report);
        assert_eq!(v["ranking"][0]["rank"], 1);
        assert_eq!(v["ranking"][1]["rank"], 2);
        assert_eq!(v["ranking"][0]["reward_score"], 50);
        assert_eq!(v["ranking"][1]["reward_score"], 30);
        assert_eq!(v["ranking"][0]["node"], "n0");
        assert_eq!(v["ranking"][0]["reward"]["edit_bytes"], 30);
        assert_eq!(v["ranking"][0]["prior"], 500);
        assert_eq!(v["depths"][0]["generated"], 7);
        assert_eq!(v["depths"][0]["writes"], 9);
        assert_eq!(v["judge"]["providers"][0], "metric");
        assert_eq!(v["soft_stopped"], true);
        assert_eq!(v["memory_stopped"], false);
        assert_eq!(v["resolution"]["resolution"], "failed");
    }
}
