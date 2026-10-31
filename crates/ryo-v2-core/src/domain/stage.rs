//! The stages a Blueprint passes before the analyzer check, and the path they stage.
//!
//! A [`Staged`] is the only input of a candidate: its edits, roles and basis come from the
//! stages, never from the caller.

use super::basis::View;
use super::file_role::file_role;
use super::resolution::risk_marks;
use super::search::compose;
use super::text::splice;
use super::{
    Basis, Blocking, Blueprint, DecisionLog, Drift, Edit, FileFacts, FileRole, LogEntry, NonEmpty,
    RelPath, Stage,
};
use std::collections::{BTreeMap, BTreeSet};

/// What a Blueprint is staged on: the start state of a decision, or a staged parent.
pub(crate) enum On<'a> {
    /// Whole-file edits over the loaded text (empty for an apply; a search's start state).
    Start(&'a [Edit]),
    /// A path that passed the stages; the Blueprint's specs and reads are against its result.
    Parent(&'a Staged),
}

/// A path of Blueprints that passed every stage before the analyzer check, with the edits it
/// would make, the roles of the files it edits and its basis. Only [`stage`] makes one, and it
/// is the only input [`Candidate::new`](super::Candidate) takes.
///
/// # Contract
/// - C1 [invariant]: every value comes from `stage` (outside `#[cfg(test)]`): its edits, roles
///   and basis are those of its path staged in order over the loaded text.
///   tested: domain::stage::tests::staged_on_a_parent_composes_and_keeps_the_loaded_basis
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Staged {
    /// Root first, the staged Blueprint last.
    path: NonEmpty<Blueprint>,
    /// The last Blueprint's edits, spliced against the view of `on`.
    own: Vec<Edit>,
    /// `compose` of the edits of `on` and `own`.
    edits: Vec<Edit>,
    /// The roles of `on` and of the files the last Blueprint edits.
    roles: BTreeSet<FileRole>,
    /// `Basis::of_chain` of the path and the edits over the loaded text.
    basis: Basis,
}

impl Staged {
    /// The Blueprints, the root first and the staged one last.
    pub(crate) fn path(&self) -> &NonEmpty<Blueprint> {
        &self.path
    }

    /// The edits of the last Blueprint alone, against the view of what it was staged on.
    pub(crate) fn own(&self) -> &[Edit] {
        &self.own
    }

    /// The whole-file edits of the path over the loaded text.
    pub(crate) fn edits(&self) -> &[Edit] {
        &self.edits
    }

    /// The roles of the files the path edits.
    pub(crate) fn roles(&self) -> &BTreeSet<FileRole> {
        &self.roles
    }

    /// What the path rests on in the loaded text.
    #[cfg(test)]
    pub(crate) fn basis(&self) -> &Basis {
        &self.basis
    }

    /// The path, edits, roles and basis, for the candidate built from them.
    pub(crate) fn into_parts(self) -> (NonEmpty<Blueprint>, Vec<Edit>, BTreeSet<FileRole>, Basis) {
        (self.path, self.edits, self.roles, self.basis)
    }

    /// A value built by hand, for the unit tests of what consumes it; `own` is `edits`.
    #[cfg(test)]
    pub(crate) fn for_tests(
        path: NonEmpty<Blueprint>,
        roles: BTreeSet<FileRole>,
        edits: Vec<Edit>,
        basis: Basis,
    ) -> Self {
        Self {
            path,
            own: edits.clone(),
            edits,
            roles,
            basis,
        }
    }
}

/// A stage refused the Blueprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refused {
    /// The stage that refused.
    pub(crate) stage: Stage,
    /// Why.
    pub(crate) blocking: Blocking,
}

/// The text of `path` after `edits` over `base`: the latest edit of the path, `None` for a
/// deletion, else `base`.
pub(crate) fn view_of(edits: &[Edit], base: &View<'_>, path: &RelPath) -> Option<String> {
    match edits.iter().rev().find(|ed| ed.path == *path) {
        Some(ed) => ed.new_text.clone(),
        None => base(path),
    }
}

/// Runs the stages before the analyzer check on `bp`, against the view of `on` over `base`: file
/// role, freshness of the base fingerprints and of the reads of files without one, conformance,
/// splice and marks.
///
/// `facts` says what the files are; each stage pushes one entry to `log`; `compared` counts the
/// fingerprints compared.
///
/// # Contract
/// - C1: the stages run in the order file role, freshness (`Basis::check` of `Blueprint::basis`
///   against the view), conformance, splice, marks, against the view of `on` over `base`; each
///   logs one entry, `ok` false at the stage that refuses, and nothing runs after it.
///   tested: app::apply::tests::decision_log_stops_at_blocking_stage
///   tested: app::search::tests::child_blueprint_reads_parent_state
/// - C2: the staged path is `on`'s path followed by `bp`; its edits are `compose` of `on`'s
///   edits and its own; its roles are `on`'s and those of the files `bp` edits; its basis is
///   `Basis::of_chain` of the path and the edits over `base`, never over `on`'s state.
///   tested: app::search::tests::child_inherits_build_script_role_of_parent
///   tested: app::search::tests::search_candidate_basis_is_against_the_loaded_text
///   tested: domain::stage::tests::staged_on_a_parent_composes_and_keeps_the_loaded_basis
pub(crate) fn stage(
    bp: &Blueprint,
    on: On<'_>,
    base: &View<'_>,
    facts: &dyn Fn(&RelPath) -> FileFacts,
    log: &mut DecisionLog,
    compared: &mut usize,
) -> Result<Staged, Refused> {
    let (on_edits, on_roles, on_path): (&[Edit], BTreeSet<FileRole>, Vec<Blueprint>) = match on {
        On::Start(edits) => (edits, BTreeSet::new(), Vec::new()),
        On::Parent(p) => (
            p.edits(),
            p.roles().clone(),
            p.path().iter().cloned().collect(),
        ),
    };
    let view = |p: &RelPath| view_of(on_edits, base, p);
    let id = bp.id();
    let by_file = bp.edits_by_file();
    let mut note = |stage: Stage, ok: bool, text: String| {
        log.push(LogEntry::new(id, stage, ok, text));
    };

    // 1. file roles
    let mut own_roles = BTreeSet::new();
    for path in by_file.keys() {
        let role = file_role(path, &facts(path));
        if role == FileRole::Manifest {
            note(Stage::FileRole, false, format!("{path} is a {role}"));
            return Err(Refused {
                stage: Stage::FileRole,
                blocking: Blocking::Unverifiable {
                    blueprint: id,
                    path: (*path).clone(),
                    role,
                },
            });
        }
        own_roles.insert(role);
    }
    note(Stage::FileRole, true, format!("{} file(s)", by_file.len()));

    // 2. freshness
    let own_basis = bp.basis();
    let drift = own_basis.check(&view);
    let missing = drift
        .iter()
        .filter(|d| matches!(d, Drift::Missing(_)))
        .count();
    *compared += own_basis.files().len() + own_basis.reads().len() - missing;
    if let Some(Drift::Missing(path)) = drift.iter().find(|d| matches!(d, Drift::Missing(_))) {
        note(
            Stage::Freshness,
            false,
            format!("{path} is not in the workspace"),
        );
        return Err(Refused {
            stage: Stage::Freshness,
            blocking: Blocking::MissingFile {
                blueprint: id,
                path: path.clone(),
            },
        });
    }
    if !drift.is_empty() {
        let files = drift
            .iter()
            .filter(|d| matches!(d, Drift::File { .. }))
            .count();
        note(
            Stage::Freshness,
            false,
            format!(
                "{files} base(s) and {} read(s) are not fresh",
                drift.len() - files
            ),
        );
        return Err(Refused {
            stage: Stage::Freshness,
            blocking: Blocking::Stale {
                blueprint: id,
                drift,
            },
        });
    }
    note(
        Stage::Freshness,
        true,
        format!(
            "{} base(s) and {} read(s) fresh",
            own_basis.files().len(),
            own_basis.reads().len()
        ),
    );

    // 3. conformance
    let texts: BTreeMap<RelPath, String> = by_file
        .keys()
        .filter_map(|p| view(p).map(|t| ((*p).clone(), t)))
        .collect();
    if let Err(why) = bp.intent().conforms(bp.specs(), &texts) {
        note(Stage::Conformance, false, why.to_string());
        return Err(Refused {
            stage: Stage::Conformance,
            blocking: Blocking::Nonconforming { blueprint: id, why },
        });
    }
    note(Stage::Conformance, true, bp.intent().to_string());

    // 4. splice
    let mut own = Vec::new();
    for (path, specs) in &by_file {
        let Some(text) = texts.get(*path) else {
            // Cannot happen after the freshness stage; reported as a missing file all the same.
            note(Stage::Splice, false, format!("{path} has no text"));
            return Err(Refused {
                stage: Stage::Splice,
                blocking: Blocking::MissingFile {
                    blueprint: id,
                    path: (*path).clone(),
                },
            });
        };
        match splice(text, specs) {
            Ok(new_text) => own.push(Edit::write((*path).clone(), new_text)),
            Err(err) => {
                note(Stage::Splice, false, err.to_string());
                return Err(Refused {
                    stage: Stage::Splice,
                    blocking: Blocking::Splice { blueprint: id, err },
                });
            }
        }
    }
    note(Stage::Splice, true, format!("{} edit(s)", own.len()));

    // 5. marks
    let marks = risk_marks([bp.intent().kind()], own_roles.iter().copied());
    let names: Vec<String> = marks.iter().map(ToString::to_string).collect();
    note(Stage::Marks, true, names.join(", "));

    let edits = compose(on_edits, &own, base);
    let mut roles = on_roles;
    roles.extend(own_roles);
    let mut blueprints = on_path;
    blueprints.push(bp.clone());
    let chain: Vec<&Blueprint> = blueprints.iter().collect();
    let basis = Basis::of_chain(&chain, &edits, base);
    // `blueprints` holds `bp` at least.
    let path = NonEmpty::new(blueprints).unwrap_or_else(|| NonEmpty::one(bp.clone()));
    Ok(Staged {
        path,
        own,
        edits,
        roles,
        basis,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        BaseFp, BlueprintId, BlueprintParts, DecisionRecord, Ident, IntentCall, MutationSpec,
        ReadRange, SpecRef, TextRange,
    };

    const BASE: &str = "let a = 1;";
    const ROOT_RESULT: &str = "let b = 1;";

    fn rel(s: &str) -> RelPath {
        RelPath::parse(s).unwrap()
    }
    fn bid(n: u8) -> BlueprintId {
        BlueprintId::parse(&format!("01890a5d-ac96-774b-bcce-b302099a80{n:02x}")).unwrap()
    }
    fn tr(a: u32, b: u32) -> TextRange {
        TextRange::new(a, b).unwrap()
    }
    /// Renames `from` to `to` at bytes 4..5 of `text`, which `path` holds.
    fn rename(n: u8, path: &str, text: &str, from: &str, to: &str) -> Blueprint {
        let end = u32::try_from(text.len()).unwrap();
        Blueprint::new(BlueprintParts {
            id: bid(n),
            intent: IntentCall::RenameLocal {
                from: Ident::parse(from).unwrap(),
                to: Ident::parse(to).unwrap(),
            },
            specs: vec![MutationSpec::replace(rel(path), tr(4, 5), to).unwrap()],
            reads: vec![ReadRange::capture(rel(path), text, tr(0, end)).unwrap()],
            bases: vec![BaseFp::capture(rel(path), text)],
            spec_refs: vec![SpecRef::parse("spec#t").unwrap()],
            record: DecisionRecord::new("agent", "why").unwrap(),
        })
        .unwrap()
    }

    #[test]
    fn staged_on_a_parent_composes_and_keeps_the_loaded_basis() {
        let base = |p: &RelPath| (p.as_str() == "src/lib.rs").then(|| BASE.to_owned());
        let facts = |_: &RelPath| FileFacts::default();
        let mut log = DecisionLog::default();
        let mut compared = 0;
        let root_bp = rename(1, "src/lib.rs", BASE, "a", "b");
        let root = stage(
            &root_bp,
            On::Start(&[]),
            &base,
            &facts,
            &mut log,
            &mut compared,
        )
        .unwrap();
        let child_bp = rename(2, "src/lib.rs", ROOT_RESULT, "b", "c");
        let child = stage(
            &child_bp,
            On::Parent(&root),
            &base,
            &facts,
            &mut log,
            &mut compared,
        )
        .unwrap();
        assert_eq!(
            child.edits(),
            compose(root.edits(), child.own(), &base).as_slice()
        );
        assert_eq!(
            child.edits(),
            [Edit::write(rel("src/lib.rs"), "let c = 1;")]
        );
        let ids: Vec<BlueprintId> = child.path().iter().map(Blueprint::id).collect();
        assert_eq!(ids, vec![bid(1), bid(2)]);
        assert_eq!(
            child.basis().files(),
            [BaseFp::capture(rel("src/lib.rs"), BASE)]
        );
        assert_eq!(log.entries().len(), 10);
        assert!(log.entries().iter().all(LogEntry::ok));
        assert_eq!(compared, 2);

        // The same child on the start state is stale: its base is the root's result.
        let mut log = DecisionLog::default();
        let refused = stage(
            &child_bp,
            On::Start(&[]),
            &base,
            &facts,
            &mut log,
            &mut compared,
        )
        .unwrap_err();
        assert_eq!(refused.stage, Stage::Freshness);
        assert!(matches!(refused.blocking, Blocking::Stale { .. }));
        assert_eq!(log.entries().len(), 2);
        assert!(!log.entries()[1].ok());
    }
}
