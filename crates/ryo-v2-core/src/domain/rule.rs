//! Rules: a reviewed permission to treat one kind of intent as routine.
//!
//! A Rule does not apply anything. It is the answer to "has a human already decided that this
//! kind of change is safe enough?", read by `resolution::disposition`.

use super::{IntentKind, RiskMark, RuleId, Scope};
use std::collections::BTreeMap;

/// Who reviewed a Rule, when, and what they said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRecord {
    /// The reviewer.
    pub reviewer: String,
    /// Milliseconds since the Unix epoch, supplied by the caller.
    pub reviewed_at_ms: u64,
    /// Free-form note.
    pub note: String,
}

/// A registered Rule for one intent kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    id: RuleId,
    intent: IntentKind,
    review: ReviewRecord,
    min_scope: Scope,
}

/// Why a Rule or a catalog of them cannot be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RuleError {
    /// The intent can change behaviour: a review of the kind cannot cover every use.
    #[error("{0} can change behaviour and cannot be registered as a rule")]
    BehaviourChanging(IntentKind),
    /// The conformance check of the intent is a text match only (CHECK_LEDGER M-1): no
    /// review of the kind can cover every use until the engine generates its specs.
    #[error("{0} is checked by text match only and cannot be registered as a rule")]
    TextMatchOnly(IntentKind),
    /// A second rule for one intent kind.
    #[error("a rule for {0} is already registered")]
    Duplicate(IntentKind),
}

impl Rule {
    /// Registers a Rule.
    ///
    /// # Contract
    /// - C1: refuses an intent whose catalog risks contain `Behaviour`, so such an intent is
    ///   never auto-applicable.
    ///   tested: domain::rule::tests::behaviour_intent_cannot_be_registered
    ///   tested: domain::rule::tests::extract_function_cannot_be_registered
    /// - C2: refuses an intent whose `text_match_only()` is true with `TextMatchOnly`, so
    ///   rename-local and add-derive are never covered by a Rule and a passing candidate of
    ///   either always ends as `NeedsReview`; `Behaviour` is checked first, then
    ///   `TextMatchOnly`.
    ///   tested: domain::rule::tests::text_match_intents_cannot_be_registered
    ///   tested: app::apply::tests::text_match_intents_need_review_under_every_rule
    pub fn register(
        id: RuleId,
        intent: IntentKind,
        review: ReviewRecord,
        min_scope: Scope,
    ) -> Result<Rule, RuleError> {
        if intent.spec().risks.contains(&RiskMark::Behaviour) {
            return Err(RuleError::BehaviourChanging(intent));
        }
        if intent.text_match_only() {
            return Err(RuleError::TextMatchOnly(intent));
        }
        Ok(Self::build(id, intent, review, min_scope))
    }

    fn build(id: RuleId, intent: IntentKind, review: ReviewRecord, min_scope: Scope) -> Rule {
        Rule {
            id,
            intent,
            review,
            min_scope,
        }
    }

    /// A Rule without the registration checks, for the path-rule tests of `app::search`
    /// only (they need two kinds with a Rule; production can register only move-item).
    #[cfg(test)]
    pub(crate) fn unchecked_for_tests(
        id: RuleId,
        intent: IntentKind,
        review: ReviewRecord,
        min_scope: Scope,
    ) -> Rule {
        Self::build(id, intent, review, min_scope)
    }

    /// The id.
    pub fn id(&self) -> RuleId {
        self.id
    }

    /// The intent kind covered.
    pub fn intent(&self) -> IntentKind {
        self.intent
    }

    /// The review behind it.
    pub fn review(&self) -> &ReviewRecord {
        &self.review
    }

    /// The narrowest check scope at which the review holds.
    pub fn min_scope(&self) -> Scope {
        self.min_scope
    }
}

/// The Rules in force: at most one per intent kind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleCatalog(BTreeMap<IntentKind, Rule>);

impl RuleCatalog {
    /// Builds a catalog.
    ///
    /// # Contract
    /// - C1: a second rule of one kind gives `Duplicate`.
    ///   tested: domain::rule::tests::one_rule_per_intent
    pub fn new(rules: Vec<Rule>) -> Result<Self, RuleError> {
        let mut map = BTreeMap::new();
        for r in rules {
            let kind = r.intent();
            if map.insert(kind, r).is_some() {
                return Err(RuleError::Duplicate(kind));
            }
        }
        Ok(Self(map))
    }

    /// The rule for `k`, if one is registered.
    pub fn for_intent(&self, k: IntentKind) -> Option<&Rule> {
        self.0.get(&k)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review() -> ReviewRecord {
        ReviewRecord {
            reviewer: "me".to_owned(),
            reviewed_at_ms: 1,
            note: "ok".to_owned(),
        }
    }
    fn rid(n: u8) -> RuleId {
        RuleId::parse(&format!("01890a5d-ac96-774b-bcce-b302099a80{n:02x}")).unwrap()
    }

    #[test]
    fn behaviour_intent_cannot_be_registered() {
        assert_eq!(
            Rule::register(
                rid(1),
                IntentKind::ExtractVariable,
                review(),
                Scope::Workspace
            ),
            Err(RuleError::BehaviourChanging(IntentKind::ExtractVariable))
        );
        let r = Rule::register(rid(1), IntentKind::MoveItem, review(), Scope::Workspace).unwrap();
        assert_eq!(r.id(), rid(1));
        assert_eq!(r.intent(), IntentKind::MoveItem);
        assert_eq!(r.min_scope(), Scope::Workspace);
        assert_eq!(r.review().reviewer, "me");
    }

    #[test]
    fn text_match_intents_cannot_be_registered() {
        for k in [IntentKind::RenameLocal, IntentKind::AddDerive] {
            assert!(Rule::register(rid(1), k, review(), Scope::Workspace).is_err());
            assert_eq!(
                Rule::register(rid(1), k, review(), Scope::Workspace),
                Err(RuleError::TextMatchOnly(k))
            );
        }
        assert!(Rule::register(rid(1), IntentKind::MoveItem, review(), Scope::Workspace).is_ok());
        assert_eq!(
            Rule::register(
                rid(1),
                IntentKind::ExtractVariable,
                review(),
                Scope::Workspace
            ),
            Err(RuleError::BehaviourChanging(IntentKind::ExtractVariable))
        );
    }

    #[test]
    fn extract_function_cannot_be_registered() {
        assert_eq!(
            Rule::register(
                rid(1),
                IntentKind::ExtractFunction,
                review(),
                Scope::Workspace
            ),
            Err(RuleError::BehaviourChanging(IntentKind::ExtractFunction))
        );
        let r = Rule::register(rid(2), IntentKind::MoveItem, review(), Scope::Workspace).unwrap();
        assert_eq!(r.intent(), IntentKind::MoveItem);
    }

    #[test]
    fn one_rule_per_intent() {
        let mk = |n, k| Rule::register(rid(n), k, review(), Scope::Affected).unwrap();
        assert_eq!(
            RuleCatalog::new(vec![
                mk(1, IntentKind::MoveItem),
                mk(2, IntentKind::MoveItem)
            ]),
            Err(RuleError::Duplicate(IntentKind::MoveItem))
        );
        let cat = RuleCatalog::new(vec![mk(1, IntentKind::MoveItem)]).unwrap();
        assert!(cat.for_intent(IntentKind::MoveItem).is_some());
        assert!(cat.for_intent(IntentKind::AddDerive).is_none());
        assert!(
            RuleCatalog::default()
                .for_intent(IntentKind::MoveItem)
                .is_none()
        );
    }
}
