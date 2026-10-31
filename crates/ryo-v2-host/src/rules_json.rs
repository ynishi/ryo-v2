//! Rules JSON to the domain `RuleCatalog`.

use anyhow::{Context, Result, anyhow};
use ryo_v2_core::domain::{IntentKind, ReviewRecord, Rule, RuleCatalog, RuleError, RuleId, Scope};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    rules: Vec<RuleJson>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleJson {
    id: String,
    intent: String,
    review: ReviewJson,
    min_scope: ScopeJson,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewJson {
    reviewer: String,
    reviewed_at_ms: u64,
    note: String,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum ScopeJson {
    Affected,
    Workspace,
}

/// Parses the rules JSON; every id is required (the host does not invent a rule's identity).
///
/// # Contract
/// - C1: a rule for an intent that changes behaviour (extract-variable) is an error that names
///   `behaviour`, and nothing is loaded for it.
///   tested: rules_json::tests::extract_variable_rule_is_refused
///   tested: tests/apply.rs apply_extract_variable_needs_review_behaviour
/// - C2: a rule for an intent that is checked by text match only (rename-local, add-derive) is an
///   error that names `text match`, and nothing is loaded for it.
///   tested: rules_json::tests::text_match_rule_is_refused
pub(crate) fn parse_rules(json: &str) -> Result<RuleCatalog> {
    let input: Input = serde_json::from_str(json).context("rules JSON")?;
    let mut rules = Vec::new();
    for (i, r) in input.rules.into_iter().enumerate() {
        let id = RuleId::parse(&r.id).map_err(|e| anyhow!("rule {i}: id: {e}"))?;
        let kind = IntentKind::parse(&r.intent)
            .ok_or_else(|| anyhow!("rule {i}: intent: unknown intent {:?}", r.intent))?;
        let scope = match r.min_scope {
            ScopeJson::Affected => Scope::Affected,
            ScopeJson::Workspace => Scope::Workspace,
        };
        let review = ReviewRecord {
            reviewer: r.review.reviewer,
            reviewed_at_ms: r.review.reviewed_at_ms,
            note: r.review.note,
        };
        let rule = Rule::register(id, kind, review, scope).map_err(|e| match e {
            RuleError::BehaviourChanging(k) => anyhow!(
                "rule {i}: intent {} changes behaviour, so no rule may cover it",
                k.id()
            ),
            other => anyhow!("rule {i}: {other}"),
        })?;
        rules.push(rule);
    }
    RuleCatalog::new(rules).map_err(|e| anyhow!("rules: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(intent: &str) -> String {
        format!(
            "{{\"rules\":[{{\"id\":\"{}\",\"intent\":\"{intent}\",\"review\":{{\"reviewer\":\"me\",\"reviewed_at_ms\":1,\"note\":\"n\"}},\"min_scope\":\"affected\"}}]}}",
            uuid::Uuid::now_v7()
        )
    }

    #[test]
    fn extract_variable_rule_is_refused() {
        let err = parse_rules(&json("extract-variable")).expect_err("refused");
        assert!(format!("{err:#}").contains("behaviour"), "{err:#}");
        let ok = parse_rules(&json("move-item")).expect("move-item rule");
        assert!(ok.for_intent(IntentKind::MoveItem).is_some());
    }

    #[test]
    fn text_match_rule_is_refused() {
        for intent in ["rename-local", "add-derive"] {
            let err = parse_rules(&json(intent)).expect_err("refused");
            assert!(format!("{err:#}").contains("text match"), "{err:#}");
        }
    }
}
