//! Blueprint JSON to the domain `BlueprintSet`. The only place that generates ids.

use anyhow::{Context, Result, anyhow};
use ryo_v2_core::domain::{
    BaseFp, Blueprint, BlueprintId, BlueprintParts, BlueprintSet, Command, DecisionRecord,
    DeriveName, Fingerprint, Ident, NonEmpty, ReadRange, RelPath, SpecRef, Splice, TextRange,
};
use serde::Deserialize;

/// A parsed input: the set, and the ids the host filled in.
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
    command: CommandJson,
    splices: Vec<SpliceJson>,
    reads: Vec<ReadJson>,
    bases: Vec<BaseJson>,
    #[serde(default)]
    spec_refs: Vec<String>,
    record: RecordJson,
    #[serde(default)]
    builds_on: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "kind")]
enum CommandJson {
    #[serde(rename = "rename-local")]
    RenameLocal(RenameLocalJson),
    #[serde(rename = "extract-variable")]
    ExtractVariable(ExtractVariableJson),
    #[serde(rename = "add-derive")]
    AddDerive(AddDeriveJson),
    #[serde(rename = "extract-function")]
    ExtractFunction(ExtractFunctionJson),
    #[serde(rename = "move-item")]
    MoveItem(MoveItemJson),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtractFunctionJson {
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MoveItemJson {
    item: String,
    to: String,
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
enum SpliceJson {
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
struct BaseJson {
    path: String,
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

fn command(index: usize, j: CommandJson) -> Result<Command> {
    Ok(match j {
        CommandJson::RenameLocal(r) => Command::RenameLocal {
            from: field(index, "command.from", Ident::parse(&r.from))?,
            to: field(index, "command.to", Ident::parse(&r.to))?,
        },
        CommandJson::ExtractVariable(e) => Command::ExtractVariable {
            name: field(index, "command.name", Ident::parse(&e.name))?,
        },
        CommandJson::AddDerive(a) => {
            let names = a
                .derives
                .iter()
                .map(|d| field(index, "command.derives", DeriveName::parse(d)))
                .collect::<Result<Vec<_>>>()?;
            let derives = NonEmpty::new(names)
                .ok_or_else(|| anyhow!("blueprint {index}: command.derives: must not be empty"))?;
            Command::AddDerive { derives }
        }
        CommandJson::ExtractFunction(e) => Command::ExtractFunction {
            name: field(index, "command.name", Ident::parse(&e.name))?,
        },
        CommandJson::MoveItem(m) => Command::MoveItem {
            item: field(index, "command.item", Ident::parse(&m.item))?,
            to: rel(index, "command.to", &m.to)?,
        },
    })
}

fn splice(index: usize, j: SpliceJson) -> Result<Splice> {
    match j {
        SpliceJson::Replace(r) => {
            let path = rel(index, "splices.path", &r.path)?;
            let range = field(index, "splices.range", TextRange::new(r.start, r.end))?;
            field(index, "splices", Splice::replace(path, range, r.text))
        }
        SpliceJson::Insert(i) => {
            let path = rel(index, "splices.path", &i.path)?;
            field(index, "splices", Splice::insert(path, i.at, i.text))
        }
        SpliceJson::Delete(d) => {
            let path = rel(index, "splices.path", &d.path)?;
            let range = field(index, "splices.range", TextRange::new(d.start, d.end))?;
            field(index, "splices", Splice::delete(path, range))
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

fn base(index: usize, j: BaseJson) -> Result<BaseFp> {
    Ok(BaseFp::new(
        rel(index, "bases.path", &j.path)?,
        field(
            index,
            "bases.fingerprint",
            Fingerprint::parse(&j.fingerprint),
        )?,
    ))
}

/// Parses the blueprint input JSON. A missing `id` is filled with a fresh UUID v7 (reported in
/// `generated`); no other part of the program generates ids.
///
/// # Contract
/// - C1: an unknown `command.kind` (or any unknown field) is an error, and the JSON is not
///   guessed at.
///   tested: blueprint_json::tests::unknown_command_kind_rejected
/// - C2: a blueprint without `id` gets a UUID v7 and is listed in `generated`.
///   tested: blueprint_json::tests::missing_id_is_filled_with_v7
/// - C3: a domain constructor error (a splice no read covers) is an error naming the blueprint
///   index; nothing is loaded for it.
///   tested: blueprint_json::tests::uncovered_splice_is_a_parse_error
/// - C4: `bases` is required, and a blueprint whose splice is in a file without a base, or with
///   two bases of one path, is an error naming the blueprint index (and the splice).
///   tested: blueprint_json::tests::missing_base_is_a_parse_error
/// - C5: `builds_on` is optional; a parent not listed before the blueprint is an error naming
///   both ids.
///   tested: blueprint_json::tests::builds_on_must_name_an_earlier_blueprint
/// - C6: the keys `command` and `splices` are the blueprint's; the former keys `intent` and
///   `specs` are unknown fields and refused.
///   tested: blueprint_json::tests::old_blueprint_keys_are_refused
pub(crate) fn parse_blueprints(json: &str) -> Result<ParsedBlueprints> {
    let input: Input = serde_json::from_str(json).context("blueprint JSON")?;
    build(input.blueprints)
}

/// Parses blueprint objects already read out of a larger document (the `roots` of a search
/// request) with the rules of [`parse_blueprints`]; a list must not be empty.
pub(crate) fn parse_blueprint_values(values: Vec<serde_json::Value>) -> Result<ParsedBlueprints> {
    let list = values
        .into_iter()
        .enumerate()
        .map(|(index, v)| {
            serde_json::from_value::<BlueprintJson>(v).with_context(|| format!("blueprint {index}"))
        })
        .collect::<Result<Vec<_>>>()?;
    build(list)
}

fn build(list: Vec<BlueprintJson>) -> Result<ParsedBlueprints> {
    let mut generated = Vec::new();
    let mut items = Vec::new();
    for (index, bp) in list.into_iter().enumerate() {
        let id = match bp.id {
            Some(s) => field(index, "id", BlueprintId::parse(&s))?,
            None => {
                let id = field(index, "id", BlueprintId::from_uuid(uuid::Uuid::now_v7()))?;
                generated.push(id);
                id
            }
        };
        let splices = bp
            .splices
            .into_iter()
            .map(|s| splice(index, s))
            .collect::<Result<Vec<_>>>()?;
        let reads = bp
            .reads
            .into_iter()
            .map(|r| read(index, r))
            .collect::<Result<Vec<_>>>()?;
        let bases = bp
            .bases
            .into_iter()
            .map(|b| base(index, b))
            .collect::<Result<Vec<_>>>()?;
        let spec_refs = bp
            .spec_refs
            .iter()
            .map(|s| field(index, "spec_refs", SpecRef::parse(s)))
            .collect::<Result<Vec<_>>>()?;
        let record = field(
            index,
            "record",
            DecisionRecord::new(&bp.record.proposer, &bp.record.rationale),
        )?;
        let builds_on = bp
            .builds_on
            .map(|s| field(index, "builds_on", BlueprintId::parse(&s)))
            .transpose()?;
        let blueprint = field(
            index,
            "blueprint",
            Blueprint::new(BlueprintParts {
                id,
                command: command(index, bp.command)?,
                splices,
                reads,
                bases,
                spec_refs,
                record,
                builds_on,
            }),
        )?;
        items.push(blueprint);
    }
    let set = BlueprintSet::new(items).map_err(|e| anyhow!("blueprints: {e}"))?;
    Ok(ParsedBlueprints { set, generated })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_json() -> String {
        let fp = Fingerprint::of(b"a = 1");
        format!("{{\"path\":\"src/lib.rs\",\"fingerprint\":\"{fp}\"}}")
    }

    fn json(command: &str, with_id: bool, with_read: bool) -> String {
        json_with(command, with_id, with_read, Some(&base_json()))
    }

    /// The input document; `bases` is the content of the `bases` array, or the key is left out.
    fn json_with(command: &str, with_id: bool, with_read: bool, bases: Option<&str>) -> String {
        let fp = Fingerprint::of(b"a = 1");
        let bases = bases.map_or(String::new(), |b| format!("\"bases\":[{b}],"));
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
            "{{\"blueprints\":[{{{id}\"command\":{command},\"splices\":[{{\"op\":\"replace\",\"path\":\"src/lib.rs\",\"start\":0,\"end\":1,\"text\":\"b\"}}],\"reads\":[{reads}],{bases}\"record\":{{\"proposer\":\"p\",\"rationale\":\"r\"}}}}]}}"
        )
    }

    const RENAME: &str = "{\"kind\":\"rename-local\",\"from\":\"a\",\"to\":\"b\"}";

    #[test]
    fn builds_on_must_name_an_earlier_blueprint() {
        let (a, b) = (uuid::Uuid::now_v7(), uuid::Uuid::now_v7());
        let fp = Fingerprint::of(b"a = 1");
        let one = |id: uuid::Uuid, builds_on: Option<&str>| {
            let builds_on = builds_on.map_or(String::new(), |p| format!("\"builds_on\":\"{p}\","));
            format!(
                "{{\"id\":\"{id}\",{builds_on}\"command\":{RENAME},\"splices\":[{{\"op\":\"replace\",\"path\":\"src/lib.rs\",\"start\":0,\"end\":1,\"text\":\"b\"}}],\"reads\":[{{\"path\":\"src/lib.rs\",\"start\":0,\"end\":5,\"fingerprint\":\"{fp}\"}}],\"bases\":[{}],\"record\":{{\"proposer\":\"p\",\"rationale\":\"r\"}}}}",
                base_json()
            )
        };
        let doc = |items: &[String]| format!("{{\"blueprints\":[{}]}}", items.join(","));
        // The parent is listed earlier.
        let parsed = parse_blueprints(&doc(&[one(a, None), one(b, Some(&a.to_string()))]))
            .expect("parent first");
        let parent = parsed.set.iter().next().expect("first").id();
        assert_eq!(
            parsed.set.iter().nth(1).expect("second").builds_on(),
            Some(parent)
        );
        assert_eq!(parsed.set.iter().next().expect("first").builds_on(), None);
        // The parent is listed later: both ids are named.
        let err = parse_blueprints(&doc(&[one(b, Some(&a.to_string())), one(a, None)]))
            .expect_err("parent later");
        let text = format!("{err:#}");
        assert!(
            text.contains(&a.to_string()) && text.contains(&b.to_string()),
            "{text}"
        );
        // A malformed id names the field.
        let err = parse_blueprints(&doc(&[one(a, Some("not-a-uuid"))])).expect_err("malformed");
        assert!(format!("{err:#}").contains("builds_on"), "{err:#}");
    }

    #[test]
    fn unknown_command_kind_rejected() {
        let err = parse_blueprints(&json("{\"kind\":\"inline-all\"}", true, true))
            .expect_err("unknown kind");
        assert!(format!("{err:#}").contains("inline-all"), "{err:#}");
        let extra = RENAME.replace('}', ",\"extra\":1}");
        assert!(parse_blueprints(&json(&extra, true, true)).is_err());
    }

    #[test]
    fn old_blueprint_keys_are_refused() {
        let good = json(RENAME, true, true);
        parse_blueprints(&good).expect("the current keys parse");
        // The former keys are unknown fields, in a document and in a search request's roots.
        let old_command = good.replace("\"command\":", "\"intent\":");
        let err = parse_blueprints(&old_command).expect_err("old command key");
        assert!(format!("{err:#}").contains("intent"), "{err:#}");
        let old_splices = good.replace("\"splices\":", "\"specs\":");
        let err = parse_blueprints(&old_splices).expect_err("old splices key");
        assert!(format!("{err:#}").contains("specs"), "{err:#}");
        for old in [old_command, old_splices] {
            let doc: serde_json::Value = serde_json::from_str(&old).expect("json");
            let list = doc["blueprints"].as_array().cloned().expect("array");
            assert!(parse_blueprint_values(list).is_err(), "{old}");
        }
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
    fn new_command_kinds_parse() {
        let extract = "{\"kind\":\"extract-function\",\"name\":\"helper\"}";
        let parsed = parse_blueprints(&json(extract, true, true)).expect("extract-function");
        let bp = parsed.set.iter().next().expect("one");
        assert_eq!(bp.command().to_string(), "extract-function helper");
        let mv = "{\"kind\":\"move-item\",\"item\":\"Shape\",\"to\":\"geo/src/shapes.rs\"}";
        let parsed = parse_blueprints(&json(mv, true, true)).expect("move-item");
        let bp = parsed.set.iter().next().expect("one");
        assert_eq!(
            bp.command().to_string(),
            "move-item Shape -> geo/src/shapes.rs"
        );
        // An unknown field and a bad path are refused.
        let extra = extract.replace('}', ",\"extra\":1}");
        assert!(parse_blueprints(&json(&extra, true, true)).is_err());
        let bad = mv.replace("geo/src/shapes.rs", "../x.rs");
        assert!(parse_blueprints(&json(&bad, true, true)).is_err());
        // A list of blueprint objects is parsed like the `blueprints` array.
        let doc: serde_json::Value =
            serde_json::from_str(&json(extract, true, true)).expect("json");
        let list = doc["blueprints"].as_array().cloned().expect("array");
        assert_eq!(parse_blueprint_values(list).expect("list").set.len(), 1);
        assert!(parse_blueprint_values(Vec::new()).is_err());
    }

    #[test]
    fn uncovered_splice_is_a_parse_error() {
        let err =
            parse_blueprints(&json(RENAME, true, false)).expect_err("no read covers the splice");
        assert!(format!("{err:#}").contains("blueprint 0"), "{err:#}");
    }

    #[test]
    fn missing_base_is_a_parse_error() {
        // (a) no `bases` key
        let err = parse_blueprints(&json_with(RENAME, true, true, None)).expect_err("required");
        assert!(format!("{err:#}").contains("bases"), "{err:#}");
        // (b) an empty list with a splice
        let err = parse_blueprints(&json_with(RENAME, true, true, Some(""))).expect_err("no base");
        let text = format!("{err:#}");
        assert!(
            text.contains("blueprint 0") && text.contains("splice 0"),
            "{text}"
        );
        // (c) a base of the splice's file
        let parsed = parse_blueprints(&json(RENAME, true, true)).expect("parse");
        let bp = parsed.set.iter().next().expect("one");
        assert_eq!(bp.bases().len(), 1);
        assert_eq!(bp.bases()[0].path().as_str(), "src/lib.rs");
        assert_eq!(bp.bases()[0].fingerprint(), Fingerprint::of(b"a = 1"));
        // (d) two bases of one path
        let twice = format!("{0},{0}", base_json());
        let err = parse_blueprints(&json_with(RENAME, true, true, Some(&twice)))
            .expect_err("duplicate base");
        assert!(format!("{err:#}").contains("blueprint 0"), "{err:#}");
        // a bad base fingerprint names the field
        let bad = base_json().replace("fnv1a64:", "x:");
        let err = parse_blueprints(&json_with(RENAME, true, true, Some(&bad))).expect_err("bad");
        assert!(format!("{err:#}").contains("bases.fingerprint"), "{err:#}");
    }
}
