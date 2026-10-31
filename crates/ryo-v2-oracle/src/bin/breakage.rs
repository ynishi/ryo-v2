//! Generates breakage cases for a corpus workspace, and sets their groups from
//! the expected files.
//!
//! Usage:
//! - `breakage gen <ws> [--kinds a,b|all] [--seeds N]` (defaults: all kinds, 3 seeds)
//! - `breakage regroup <ws>`
#![warn(missing_docs)]

use anyhow::{Context, Result, bail};
use ryo_v2_oracle::breakage::{Kind, generate, group_for, read_sources, sites, write_case};
use ryo_v2_oracle::case::Case;
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

fn regroup(m: &Materials, ws: &str) -> Result<()> {
    let cases = Case::load_all(&m.corpus).context("load cases")?;
    for case in cases
        .iter()
        .filter(|c| c.workspace_rel == ws && c.origin.starts_with("breakage:"))
    {
        let kind = case
            .origin
            .split(':')
            .nth(1)
            .and_then(Kind::parse)
            .with_context(|| format!("{}: origin {:?} has no kind", case.id, case.origin))?;
        let Some(expected) = case.expected().with_context(|| case.id.clone())? else {
            println!("skip {}: no expected", case.id);
            continue;
        };
        let new = group_for(kind, &expected);
        if new == case.group {
            continue;
        }
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
