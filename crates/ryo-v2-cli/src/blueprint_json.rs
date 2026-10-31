//! Blueprint JSON to the domain `BlueprintSet`. The only place that generates ids.

use anyhow::{Context, Result, anyhow};
use ryo_v2_core::domain::{
    Blueprint, BlueprintId, BlueprintSet, DecisionRecord, DeriveName, Fingerprint, Ident,
    IntentCall, MutationSpec, NonEmpty, ReadRange, RelPath, SpecRef, TextRange,
};
use serde::Deserialize;

/// A parsed input: the set, and the ids the cli filled in.
#[derive(Debug)]
pub(crate) struct ParsedBlueprints {
    pub(crate) set: BlueprintSet,
    pub(crate) generated: Vec<BlueprintId>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    blueprints: Vec<BlueprintJson>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BlueprintJson {
    id: Option<String>,
    intent: IntentJson,
    specs: Vec<SpecJson>,
    reads: Vec<ReadJson>,
    #[serde(default)]
    spec_refs: Vec<String>,
    record: RecordJson,
}

#[derive(Deserialize)]
#[serde(tag = "kind")]
enum IntentJson {
    #[serde(rename = "rename-local")]
    RenameLocal(RenameLocalJson),
    #[serde(rename = "extract-variable")]
    ExtractVariable(ExtractVariableJson),
    #[serde(rename = "add-derive")]
    AddDerive(AddDeriveJson),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameLocalJson {
    from: String,
    to: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtractVariableJson {
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddDeriveJson {
    derives: Vec<String>,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum SpecJson {
    Replace(ReplaceJson),
    Insert(InsertJson),
    Delete(DeleteJson),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceJson {
    path: String,
    start: u32,
    end: u32,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InsertJson {
    path: String,
    at: u32,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteJson {
    path: String,
    start: u32,
    end: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadJson {
    path: String,
    start: u32,
    end: u32,
    fingerprint: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordJson {
    proposer: String,
    rationale: String,
}

fn field<T, E: std::fmt::Display>(
    index: usize,
    name: &str,
    r: std::result::Result<T, E>,
) -> Result<T> {
    r.map_err(|e| anyhow!("blueprint {index}: {name}: {e}"))
}

fn rel(index: usize, name: &str, path: &str) -> Result<RelPath> {
    field(index, name, RelPath::parse(path))
}

fn intent(index: usize, j: IntentJson) -> Result<IntentCall> {
    Ok(match j {
        IntentJson::RenameLocal(r) => IntentCall::RenameLocal {
            from: field(index, "intent.from", Ident::parse(&r.from))?,
            to: field(index, "intent.to", Ident::parse(&r.to))?,
        },
        IntentJson::ExtractVariable(e) => IntentCall::ExtractVariable {
            name: field(index, "intent.name", Ident::parse(&e.name))?,
        },
        IntentJson::AddDerive(a) => {
            let names = a
                .derives
                .iter()
                .map(|d| field(index, "intent.derives", DeriveName::parse(d)))
                .collect::<Result<Vec<_>>>()?;
            let derives = NonEmpty::new(names)
                .ok_or_else(|| anyhow!("blueprint {index}: intent.derives: must not be empty"))?;
            IntentCall::AddDerive { derives }
        }
    })
}

fn spec(index: usize, j: SpecJson) -> Result<MutationSpec> {
    match j {
        SpecJson::Replace(r) => {
            let path = rel(index, "specs.path", &r.path)?;
            let range = field(index, "specs.range", TextRange::new(r.start, r.end))?;
            field(index, "specs", MutationSpec::replace(path, range, r.text))
        }
        SpecJson::Insert(i) => {
            let path = rel(index, "specs.path", &i.path)?;
            field(index, "specs", MutationSpec::insert(path, i.at, i.text))
        }
        SpecJson::Delete(d) => {
            let path = rel(index, "specs.path", &d.path)?;
            let range = field(index, "specs.range", TextRange::new(d.start, d.end))?;
            field(index, "specs", MutationSpec::delete(path, range))
        }
    }
}

fn read(index: usize, j: ReadJson) -> Result<ReadRange> {
    Ok(ReadRange::new(
        rel(index, "reads.path", &j.path)?,
        field(index, "reads.range", TextRange::new(j.start, j.end))?,
        field(
            index,
            "reads.fingerprint",
            Fingerprint::parse(&j.fingerprint),
        )?,
    ))
}

/// Parses the blueprint input JSON. A missing `id` is filled with a fresh UUID v7 (reported in
/// `generated`); no other part of the program generates ids.
///
/// # Contract
/// - C1: an unknown `intent.kind` (or any unknown field) is an error, and the JSON is not
///   guessed at.
///   tested: blueprint_json::tests::unknown_intent_kind_rejected
/// - C2: a blueprint without `id` gets a UUID v7 and is listed in `generated`.
///   tested: blueprint_json::tests::missing_id_is_filled_with_v7
/// - C3: a domain constructor error (a spec no read covers) is an error naming the blueprint
///   index; nothing is loaded for it.
///   tested: blueprint_json::tests::uncovered_spec_is_a_parse_error
pub(crate) fn parse_blueprints(json: &str) -> Result<ParsedBlueprints> {
    let input: Input = serde_json::from_str(json).context("blueprint JSON")?;
    let mut generated = Vec::new();
    let mut items = Vec::new();
    for (index, bp) in input.blueprints.into_iter().enumerate() {
        let id = match bp.id {
            Some(s) => field(index, "id", BlueprintId::parse(&s))?,
            None => {
                let id = field(index, "id", BlueprintId::from_uuid(uuid::Uuid::now_v7()))?;
                generated.push(id);
                id
            }
        };
        let specs = bp
            .specs
            .into_iter()
            .map(|s| spec(index, s))
            .collect::<Result<Vec<_>>>()?;
        let reads = bp
            .reads
            .into_iter()
            .map(|r| read(index, r))
            .collect::<Result<Vec<_>>>()?;
        let spec_refs = bp
            .spec_refs
            .iter()
            .map(|s| field(index, "spec_refs", SpecRef::new(s)))
            .collect::<Result<Vec<_>>>()?;
        let record = field(
            index,
            "record",
            DecisionRecord::new(&bp.record.proposer, &bp.record.rationale),
        )?;
        let blueprint = field(
            index,
            "blueprint",
            Blueprint::new(
                id,
                intent(index, bp.intent)?,
                specs,
                reads,
                spec_refs,
                record,
            ),
        )?;
        items.push(blueprint);
    }
    let set = BlueprintSet::new(items).map_err(|e| anyhow!("blueprints: {e}"))?;
    Ok(ParsedBlueprints { set, generated })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(intent: &str, with_id: bool, with_read: bool) -> String {
        let fp = Fingerprint::of(b"a = 1");
        let id = if with_id {
            format!("\"id\": \"{}\",", uuid::Uuid::now_v7())
        } else {
            String::new()
        };
        let reads = if with_read {
            format!("{{\"path\":\"src/lib.rs\",\"start\":0,\"end\":5,\"fingerprint\":\"{fp}\"}}")
        } else {
            String::new()
        };
        format!(
            "{{\"blueprints\":[{{{id}\"intent\":{intent},\"specs\":[{{\"op\":\"replace\",\"path\":\"src/lib.rs\",\"start\":0,\"end\":1,\"text\":\"b\"}}],\"reads\":[{reads}],\"record\":{{\"proposer\":\"p\",\"rationale\":\"r\"}}}}]}}"
        )
    }

    const RENAME: &str = "{\"kind\":\"rename-local\",\"from\":\"a\",\"to\":\"b\"}";

    #[test]
    fn unknown_intent_kind_rejected() {
        let err = parse_blueprints(&json("{\"kind\":\"inline-all\"}", true, true))
            .expect_err("unknown kind");
        assert!(format!("{err:#}").contains("inline-all"), "{err:#}");
        let extra = RENAME.replace('}', ",\"extra\":1}");
        assert!(parse_blueprints(&json(&extra, true, true)).is_err());
    }

    #[test]
    fn missing_id_is_filled_with_v7() {
        let parsed = parse_blueprints(&json(RENAME, false, true)).expect("parse");
        assert_eq!(parsed.generated.len(), 1);
        assert_eq!(parsed.generated[0].as_uuid().get_version_num(), 7);
        assert_eq!(
            parsed.set.iter().next().expect("one").id(),
            parsed.generated[0]
        );
    }

    #[test]
    fn uncovered_spec_is_a_parse_error() {
        let err =
            parse_blueprints(&json(RENAME, true, false)).expect_err("no read covers the spec");
        assert!(format!("{err:#}").contains("blueprint 0"), "{err:#}");
    }
}
