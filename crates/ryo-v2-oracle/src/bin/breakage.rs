//! Generates breakage cases for a corpus workspace, and sets their groups from
//! the expected files.
//!
//! Usage:
//! - `breakage gen <ws> [--kinds a,b|all] [--seeds N]` (defaults: all kinds, 3 seeds)
//! - `breakage regroup <ws>` (cases listed in `KEPT_GROUPS` keep their group and are reported)

use anyhow::{Context, Result, bail};
use ryo_v2_oracle::breakage::{Kind, generate, group_for, read_sources, sites, write_case};
use ryo_v2_oracle::case::Case;
use ryo_v2_oracle::groups::Group;
use ryo_v2_oracle::materials::Materials;

const USAGE: &str =
    "usage: breakage gen <ws> [--kinds a,b|all] [--seeds N] | breakage regroup <ws>";

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let cmd = args.next().context(USAGE)?;
    let ws = args.next().context(USAGE)?;
    let m = Materials::find().context("find materials")?;
    match cmd.as_str() {
        "gen" => {
            let mut kinds: Vec<Kind> = Kind::ALL.to_vec();
            let mut seeds: u32 = 3;
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--kinds" => {
                        let v = args.next().context("--kinds needs a value")?;
                        if v != "all" {
                            kinds = v
                                .split(',')
                                .map(|k| {
                                    Kind::parse(k.trim())
                                        .with_context(|| format!("unknown kind {k:?}"))
                                })
                                .collect::<Result<_>>()?;
                        }
                    }
                    "--seeds" => {
                        seeds = args
                            .next()
                            .context("--seeds needs a number")?
                            .parse()
                            .context("--seeds is a number")?;
                    }
                    other => bail!("unknown argument {other:?}"),
                }
            }
            gen_cases(&m, &ws, &kinds, seeds)
        }
        "regroup" => {
            if let Some(extra) = args.next() {
                bail!("unknown argument {extra:?}");
            }
            regroup(&m, &ws)
        }
        _ => bail!("{USAGE}"),
    }
}

fn gen_cases(m: &Materials, ws: &str, kinds: &[Kind], seeds: u32) -> Result<()> {
    let files =
        read_sources(&m.corpus.join(ws)).with_context(|| format!("read sources of {ws}"))?;
    for &kind in kinds {
        if sites(kind, &files).is_empty() {
            println!("{}: no site in {ws}", kind.id());
            continue;
        }
        for seed in 0..seeds {
            let id = format!("{}-{seed}", kind.id());
            match generate(kind, seed, &files) {
                Some(b) => {
                    write_case(&m.corpus, ws, &b).with_context(|| format!("write {id}"))?;
                    println!("{id}: {}:{} {}", b.site.path, b.site.line, b.note);
                }
                None => println!("{id}: skip"),
            }
        }
    }
    Ok(())
}

/// Breakage cases whose group stays although `group_for` would move it:
/// (workspace, case id, the CHECK_LEDGER row that decided it).
/// The decision (2026-10-31) is recorded in CHECK_LEDGER G-C-1 and Counts.
const KEPT_GROUPS: &[(&str, &str, &str)] = &[
    ("base/extract-trait-multicrate", "change-type-0", "G-C-1"),
    ("base/extract-trait-multicrate", "change-type-1", "G-C-1"),
    ("base/extract-trait-multicrate", "remove-import-1", "G-C-1"),
    ("base/extract-trait-multicrate", "remove-import-2", "G-C-1"),
    ("base/extract-trait-multicrate", "typo-type-1", "G-C-1"),
];

/// What `regroup` does with one case.
#[derive(Debug, PartialEq)]
enum Step {
    /// The group is already right.
    Same,
    /// The group differs but stays, by the named ledger row.
    Kept(&'static str),
    /// The group is rewritten.
    Move(Group),
}

fn plan(ws: &str, case: &Case, new: Group) -> Step {
    if new == case.group {
        return Step::Same;
    }
    match KEPT_GROUPS
        .iter()
        .find(|(w, id, _)| *w == ws && *id == case.id)
    {
        Some((_, _, row)) => Step::Kept(row),
        None => Step::Move(new),
    }
}

/// The group the expected file of a breakage case gives; `None` when it has no expected file.
fn regroup_target(case: &Case) -> Result<Option<Group>> {
    let kind = case
        .origin
        .split(':')
        .nth(1)
        .and_then(Kind::parse)
        .with_context(|| format!("{}: origin {:?} has no kind", case.id, case.origin))?;
    let expected = case.expected().with_context(|| case.id.clone())?;
    Ok(expected.map(|e| group_for(kind, &e)))
}

fn regroup(m: &Materials, ws: &str) -> Result<()> {
    let cases = Case::load_all(&m.corpus).context("load cases")?;
    for case in cases
        .iter()
        .filter(|c| c.workspace_rel == ws && c.origin.starts_with("breakage:"))
    {
        let Some(new) = regroup_target(case)? else {
            println!("skip {}: no expected", case.id);
            continue;
        };
        let new = match plan(ws, case, new) {
            Step::Same => continue,
            Step::Kept(row) => {
                println!(
                    "kept {}: {} (regroup would give {}, {row})",
                    case.id,
                    case.group.id(),
                    new.id()
                );
                continue;
            }
            Step::Move(new) => new,
        };
        let path = m
            .corpus
            .join(ws)
            .join("cases")
            .join(format!("{}.toml", case.id));
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let rewritten: String = text
            .split_inclusive('\n')
            .map(|l| {
                if l.starts_with("group = ") {
                    format!("group = \"{}\"\n", new.id())
                } else {
                    l.to_string()
                }
            })
            .collect();
        std::fs::write(&path, rewritten).with_context(|| format!("write {}", path.display()))?;
        println!("{}: {} -> {}", case.id, case.group.id(), new.id());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const WS: &str = "base/extract-trait-multicrate";

    fn case(id: &str, group: Group) -> Case {
        Case {
            id: id.into(),
            workspace_rel: WS.into(),
            workspace: PathBuf::new(),
            overlay: PathBuf::new(),
            group,
            origin: "breakage:change-type".into(),
            skip: None,
            expected_path: PathBuf::new(),
        }
    }

    #[test]
    fn kept_case_is_left() {
        let kept = case("change-type-0", Group::Valid);
        assert_eq!(plan(WS, &kept, Group::TypeMismatch), Step::Kept("G-C-1"));
        assert_eq!(
            plan("base/other", &kept, Group::TypeMismatch),
            Step::Move(Group::TypeMismatch)
        );
        let unlisted = case("change-type-9", Group::Valid);
        assert_eq!(
            plan(WS, &unlisted, Group::TypeMismatch),
            Step::Move(Group::TypeMismatch)
        );
        assert_eq!(plan(WS, &kept, Group::Valid), Step::Same);
    }

    #[test]
    fn corpus_regroup_moves_nothing() {
        let m = Materials::find().expect("find materials");
        let cases = Case::load_all(&m.corpus).expect("load cases");
        let mut moves = Vec::new();
        let mut targets = std::collections::BTreeMap::new();
        for case in cases.iter().filter(|c| c.origin.starts_with("breakage:")) {
            let Some(new) = regroup_target(case).expect("target") else {
                continue;
            };
            targets.insert(
                (case.workspace_rel.clone(), case.id.clone()),
                (case.group, new),
            );
            if let Step::Move(to) = plan(&case.workspace_rel, case, new) {
                moves.push(format!(
                    "{} {}: {} -> {}",
                    case.workspace_rel,
                    case.id,
                    case.group.id(),
                    to.id()
                ));
            }
        }
        assert!(
            moves.is_empty(),
            "regroup would move:\n{}",
            moves.join("\n")
        );
        for (ws, id, row) in KEPT_GROUPS {
            match targets.get(&(ws.to_string(), id.to_string())) {
                Some((old, new)) if old != new => {}
                _ => panic!("stale KEPT_GROUPS entry {ws} {id} ({row})"),
            }
        }
    }
}
