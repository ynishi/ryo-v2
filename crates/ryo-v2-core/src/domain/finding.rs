//! Error diagnostics as the analyzer reports them, before the baseline difference.

use super::{ErrorCode, RelPath, UnseenClass};
use std::collections::{BTreeMap, BTreeSet};

/// The code of a [`Finding`]: a rustc E-code, or the analyzer's own name for an
/// Error that has none ("unresolved-macro-call", "syntax-error", ...).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FindingCode {
    /// A rustc error code.
    Rustc(ErrorCode),
    /// The analyzer's own code name.
    Analyzer(String),
}

/// One Error-severity diagnostic as the analyzer reported it, message included:
/// the input of the baseline difference. ([`super::Diagnostic`] is the output, without message.)
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Finding {
    /// What kind of error.
    pub code: FindingCode,
    /// The analyzer's message.
    pub message: String,
    /// Workspace-relative file.
    pub path: RelPath,
    /// 1-based line.
    pub line: u32,
}

/// What one diagnostics run looked at and what it found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopedFindings {
    /// The files diagnosed: created files included, deleted files excluded. Sorted, unique.
    pub files: Vec<RelPath>,
    /// Every Error in those files, sorted (Ord), duplicates kept.
    pub findings: Vec<Finding>,
    /// The classes of error this run did not see, though the checker would have looked, each
    /// with the files it was observed on. The one record of what the run observed; `unseen()`
    /// derives the set of classes from it. A diagnosis of a `Stacked` names at least one file
    /// for every class (`Stacked::diagnose` C8); a value made otherwise may hold a class with no
    /// file. `within` keeps such a class, because it keeps a diagnosis to the files of its own
    /// check, where the class was observed; `on_top_of` drops one from below, because a level
    /// written on top may have changed the file it was observed on.
    pub unseen_at: BTreeMap<UnseenClass, BTreeSet<RelPath>>,
}

impl ScopedFindings {
    /// The classes of `unseen_at`.
    ///
    /// # Contract
    /// - C1: the keys of `unseen_at`, each once, whether or not a file is named for it.
    ///   tested: domain::finding::tests::unseen_is_the_classes_of_unseen_at
    pub fn unseen(&self) -> BTreeSet<UnseenClass> {
        self.unseen_at.keys().copied().collect()
    }

    /// The findings of the state of a level: its diagnosis (`self`) put on top of the findings of
    /// the state below (`below`), with `deleted` the paths the level deletes. A file the level did
    /// not diagnose has, at its state, the findings it has below (`Stacked::diagnose` C3).
    ///
    /// # Contract
    /// - C1: the files are those of `self` and those of `below` that are neither in `self` nor in
    ///   `deleted`, sorted and unique.
    ///   tested: domain::finding::tests::on_top_of_keeps_the_files_below_outside_the_top
    ///   tested: domain::finding::tests::on_top_of_drops_deleted_files
    /// - C2: a finding of `below` stays unless its path is a file of `self` or in `deleted`; a
    ///   finding of `self` stays unless its path is in `deleted`, or its path is not a file of
    ///   `self` and a finding of `below` that stays equals it. A finding whose path is in no file
    ///   of either stays; a diagnosis has none (`Stacked::diagnose` C9). The findings are sorted.
    ///   tested: domain::finding::tests::on_top_of_takes_each_file_from_its_latest_diagnosis
    ///   tested: domain::finding::tests::on_top_of_drops_deleted_files
    /// - C3: `unseen_at` holds every class of `self` with its files, and every class of `below`
    ///   with those of its files the result keeps from `below`; a class of `below` left with no
    ///   file is dropped.
    ///   tested: domain::finding::tests::on_top_of_drops_unseen_of_files_diagnosed_again
    /// - C4: on top of `ScopedFindings::default()` with nothing deleted, the result is `self`.
    ///   tested: domain::finding::tests::on_top_of_nothing_is_the_top
    pub fn on_top_of(&self, below: &ScopedFindings, deleted: &[RelPath]) -> ScopedFindings {
        let top: BTreeSet<&RelPath> = self.files.iter().collect();
        let gone: BTreeSet<&RelPath> = deleted.iter().collect();
        let kept_below: BTreeSet<&RelPath> = below
            .files
            .iter()
            .filter(|p| !top.contains(p) && !gone.contains(p))
            .collect();

        let files: BTreeSet<RelPath> = top
            .iter()
            .chain(&kept_below)
            .map(|p| (*p).clone())
            .collect();

        let from_below: Vec<&Finding> = below
            .findings
            .iter()
            .filter(|f| !top.contains(&f.path) && !gone.contains(&f.path))
            .collect();
        let below_kept: BTreeSet<&Finding> = from_below.iter().copied().collect();
        let from_top = self.findings.iter().filter(|f| {
            !gone.contains(&f.path) && (top.contains(&f.path) || !below_kept.contains(f))
        });
        let mut findings: Vec<Finding> = from_below
            .iter()
            .copied()
            .chain(from_top)
            .cloned()
            .collect();
        findings.sort();

        let mut unseen_at: BTreeMap<UnseenClass, BTreeSet<RelPath>> = BTreeMap::new();
        for (class, on) in &below.unseen_at {
            let kept: BTreeSet<RelPath> = on
                .iter()
                .filter(|p| kept_below.contains(p))
                .cloned()
                .collect();
            if !kept.is_empty() {
                unseen_at.insert(*class, kept);
            }
        }
        for (class, on) in &self.unseen_at {
            unseen_at
                .entry(*class)
                .or_default()
                .extend(on.iter().cloned());
        }

        ScopedFindings {
            files: files.into_iter().collect(),
            findings,
            unseen_at,
        }
    }

    /// These findings kept to the files a check of the same state covers.
    ///
    /// # Contract
    /// - C1: the files are those of `self` that are in `files`; a finding stays unless its path is
    ///   a file of `self` that is not in `files`, so a finding whose path is in no file of `self`
    ///   stays; a diagnosis has none (`Stacked::diagnose` C9, `Verifier::check` C1).
    ///   tested: domain::finding::tests::within_keeps_the_files_of_the_check
    /// - C2: a class keeps those of its files that are in `files`; a class left with no file is
    ///   dropped, and one that had no file stays.
    ///   tested: domain::finding::tests::within_keeps_the_files_of_the_check
    pub fn within(&self, files: &[RelPath]) -> ScopedFindings {
        let keep: BTreeSet<&RelPath> = files.iter().collect();
        let own: BTreeSet<&RelPath> = self.files.iter().collect();
        let mut unseen_at: BTreeMap<UnseenClass, BTreeSet<RelPath>> = BTreeMap::new();
        for (class, on) in &self.unseen_at {
            let kept: BTreeSet<RelPath> = on.iter().filter(|p| keep.contains(p)).cloned().collect();
            if on.is_empty() || !kept.is_empty() {
                unseen_at.insert(*class, kept);
            }
        }
        ScopedFindings {
            files: self
                .files
                .iter()
                .filter(|p| keep.contains(p))
                .cloned()
                .collect(),
            findings: self
                .findings
                .iter()
                .filter(|f| !own.contains(&f.path) || keep.contains(&f.path))
                .cloned()
                .collect(),
            unseen_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(s: &str) -> RelPath {
        RelPath::parse(s).unwrap()
    }

    fn err(path: &str, line: u32, message: &str) -> Finding {
        Finding {
            code: FindingCode::Rustc(ErrorCode::parse("E0599").unwrap()),
            message: message.to_owned(),
            path: rel(path),
            line,
        }
    }

    fn files(paths: &[&str]) -> Vec<RelPath> {
        paths.iter().map(|p| rel(p)).collect()
    }

    fn on(paths: &[&str]) -> BTreeSet<RelPath> {
        paths.iter().map(|p| rel(p)).collect()
    }

    #[test]
    fn unseen_is_the_classes_of_unseen_at() {
        let none = ScopedFindings::default();
        assert!(none.unseen().is_empty());
        let some = ScopedFindings {
            unseen_at: BTreeMap::from([
                (UnseenClass::UnexpandedMacro, on(&["a.rs", "b.rs"])),
                (UnseenClass::UnresolvedPath, BTreeSet::new()),
            ]),
            ..Default::default()
        };
        assert_eq!(
            some.unseen(),
            BTreeSet::from([UnseenClass::UnexpandedMacro, UnseenClass::UnresolvedPath])
        );
    }

    #[test]
    fn on_top_of_keeps_the_files_below_outside_the_top() {
        let below = ScopedFindings {
            files: files(&["a.rs", "b.rs"]),
            ..Default::default()
        };
        let top = ScopedFindings {
            files: files(&["b.rs", "c.rs"]),
            ..Default::default()
        };
        assert_eq!(
            top.on_top_of(&below, &[]).files,
            files(&["a.rs", "b.rs", "c.rs"])
        );
    }

    #[test]
    fn on_top_of_takes_each_file_from_its_latest_diagnosis() {
        let below = ScopedFindings {
            files: files(&["a.rs", "b.rs"]),
            findings: vec![err("a.rs", 1, "kept"), err("b.rs", 2, "stale")],
            ..Default::default()
        };
        let top = ScopedFindings {
            // b has no error at the top; the top's findings on a (outside its files) are one
            // equal to below's and one that differs; one finding is on a file of neither.
            files: files(&["b.rs"]),
            findings: vec![
                err("a.rs", 1, "kept"),
                err("a.rs", 3, "other"),
                err("z.rs", 4, "nowhere"),
            ],
            ..Default::default()
        };
        let got = top.on_top_of(&below, &[]);
        assert_eq!(got.files, files(&["a.rs", "b.rs"]));
        let mut want = vec![
            err("a.rs", 1, "kept"),
            err("a.rs", 3, "other"),
            err("z.rs", 4, "nowhere"),
        ];
        want.sort();
        assert_eq!(got.findings, want);
    }

    #[test]
    fn on_top_of_drops_deleted_files() {
        let below = ScopedFindings {
            files: files(&["a.rs", "b.rs"]),
            findings: vec![err("b.rs", 1, "on b")],
            ..Default::default()
        };
        let top = ScopedFindings {
            files: files(&["a.rs"]),
            findings: vec![err("b.rs", 2, "foreign on b")],
            ..Default::default()
        };
        let got = top.on_top_of(&below, &files(&["b.rs"]));
        assert_eq!(got.files, files(&["a.rs"]));
        assert!(got.findings.is_empty(), "{:?}", got.findings);
    }

    #[test]
    fn on_top_of_drops_unseen_of_files_diagnosed_again() {
        let below = ScopedFindings {
            files: files(&["a.rs", "b.rs"]),
            unseen_at: BTreeMap::from([
                (UnseenClass::UnexpandedMacro, on(&["b.rs"])),
                (UnseenClass::UnresolvedPath, on(&["a.rs"])),
                (UnseenClass::DuplicateName, BTreeSet::new()),
            ]),
            ..Default::default()
        };
        let top = ScopedFindings {
            files: files(&["b.rs"]),
            unseen_at: BTreeMap::from([(UnseenClass::FieldBounds, BTreeSet::new())]),
            ..Default::default()
        };
        let got = top.on_top_of(&below, &[]);
        assert_eq!(
            got.unseen_at,
            BTreeMap::from([
                (UnseenClass::UnresolvedPath, on(&["a.rs"])),
                (UnseenClass::FieldBounds, BTreeSet::new()),
            ])
        );
        // A class both name keeps the files of both.
        let top = ScopedFindings {
            files: files(&["b.rs"]),
            unseen_at: BTreeMap::from([(UnseenClass::UnresolvedPath, on(&["b.rs"]))]),
            ..Default::default()
        };
        assert_eq!(
            top.on_top_of(&below, &[]).unseen_at,
            BTreeMap::from([(UnseenClass::UnresolvedPath, on(&["a.rs", "b.rs"]))])
        );
        // A deleted file takes its classes with it.
        let got = ScopedFindings::default().on_top_of(&below, &files(&["a.rs"]));
        assert_eq!(
            got.unseen_at,
            BTreeMap::from([(UnseenClass::UnexpandedMacro, on(&["b.rs"]))])
        );
    }

    #[test]
    fn on_top_of_nothing_is_the_top() {
        let mut x = ScopedFindings {
            files: files(&["a.rs", "b.rs"]),
            findings: vec![
                err("a.rs", 1, "one"),
                err("a.rs", 1, "one"),
                err("b.rs", 2, "two"),
                err("z.rs", 3, "foreign"),
            ],
            unseen_at: BTreeMap::from([
                (UnseenClass::UnexpandedMacro, on(&["a.rs"])),
                (UnseenClass::UnresolvedPath, BTreeSet::new()),
            ]),
        };
        x.findings.sort();
        assert_eq!(x.on_top_of(&ScopedFindings::default(), &[]), x);
    }

    #[test]
    fn within_keeps_the_files_of_the_check() {
        let x = ScopedFindings {
            files: files(&["a.rs", "b.rs", "c.rs"]),
            findings: vec![
                err("a.rs", 1, "on a"),
                err("c.rs", 2, "on c"),
                err("z.rs", 3, "on no file"),
            ],
            unseen_at: BTreeMap::from([
                (UnseenClass::UnexpandedMacro, on(&["b.rs", "c.rs"])),
                (UnseenClass::UnresolvedPath, on(&["c.rs"])),
                (UnseenClass::DuplicateName, BTreeSet::new()),
            ]),
        };
        let got = x.within(&files(&["a.rs", "b.rs"]));
        assert_eq!(got.files, files(&["a.rs", "b.rs"]));
        assert_eq!(
            got.findings,
            vec![err("a.rs", 1, "on a"), err("z.rs", 3, "on no file")]
        );
        assert_eq!(
            got.unseen_at,
            BTreeMap::from([
                (UnseenClass::UnexpandedMacro, on(&["b.rs"])),
                (UnseenClass::DuplicateName, BTreeSet::new()),
            ])
        );
        // Kept to its own files, a value is unchanged.
        assert_eq!(x.within(&x.files), x);
    }
}
