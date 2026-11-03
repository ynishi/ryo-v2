//! The one stacked procedure of the host: a parent's level on a `Stacked` fork, and per repeat a
//! variant's level written on top of it, diagnosed and popped.
//!
//! Every binary of the host that measures the stacked mode runs this function and observes it
//! through its hook, so that what one measures is exactly the procedure another times.

use crate::proc::ProcError;
use anyhow::{Context, Result};
use ryo_v2_core::app::ports::{CheckError, Stacked};
use ryo_v2_core::domain::{Edit, Finding, RelPath, Scope, ScopedFindings, Verdict};

/// A point of [`stacked_series`] at which its hook is called.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Point<'a> {
    /// The parent's level is written and diagnosed; once per series.
    ParentDiagnosed,
    /// A repeat is about to write the variant's level.
    BeforeWrite,
    /// The variant's level has been diagnosed (or its diagnosis failed).
    AfterDiagnose,
    /// The variant's level is about to be popped.
    BeforePop,
    /// The variant's level has been popped: the level's verdict, `None` when the pop did not
    /// revert to the parent's state.
    AfterPop(Option<&'a Verdict>),
}

impl Point<'_> {
    /// The point's name: the variant's own name, as the series' errors and the test logs print it.
    pub fn name(self) -> &'static str {
        match self {
            Point::ParentDiagnosed => "ParentDiagnosed",
            Point::BeforeWrite => "BeforeWrite",
            Point::AfterDiagnose => "AfterDiagnose",
            Point::BeforePop => "BeforePop",
            Point::AfterPop(_) => "AfterPop",
        }
    }
}

/// The hook of [`stacked_series`]: it observes a point and may end the series with a failed read.
pub type Hook<'h> = dyn FnMut(Point<'_>) -> Result<(), ProcError> + 'h;

/// The diagnosis of the top level at `scope`, and the files a check of the edits that take the
/// base to its state covers (`Stacked::files_of`).
fn diagnose_top<A: Stacked>(
    a: &A,
    s: &A::Session,
    f: &A::Fork,
    scope: Scope,
) -> Result<(ScopedFindings, Vec<RelPath>), CheckError> {
    let found = a.diagnose(s, f, scope)?;
    Ok((found, a.files_of(s, f, scope)?))
}

/// The repeats of one series on a fork of `a`: `parent` is level 1, written and diagnosed once,
/// and each of `repeat` repeats writes `variant` as level 2, diagnoses it and pops it. The fork is
/// released at the end, whatever happened. Returns whether every repeat and the release of the
/// parent's level reverted; the series stops at the first repeat whose pop did not revert.
///
/// A level's verdict is formed as the search forms a child's: `Verdict::of_level` of the level's
/// diagnosis on top of the findings of the parent's level (themselves formed as the search forms
/// its start state's), kept to the files a check of the composed edits covers. So it is the
/// verdict a rebuilt check gives for the same state. `name` labels what is printed to stderr.
///
/// # Contract
/// - C1: the one procedure of the stacked mode for every binary of the host: it opens the fork,
///   writes the parent as a level, diagnoses it and calls the hook at ParentDiagnosed; per repeat
///   it writes the variant as a level on the parent's level, diagnoses it, pops it, and calls the
///   hook at BeforeWrite, AfterDiagnose, BeforePop and AfterPop in that order; then it releases
///   the fork. The hook observes and never changes the session.
///   tested: stacked::tests::probe_and_bench_run_the_same_order
///   tested: stacked::tests::a_stacked_verdict_is_formed_as_the_search_forms_it
/// - C2: a hook error (a failed read of `/proc`) ends the series with that error; the fork is
///   still released, and a failed release is printed, the series' error being the one returned.
///   A failure of the repeat that the hook's error cuts short (a failed diagnosis of the
///   variant's level, a pop that did not revert) is named in that error.
///   tested: stacked::tests::a_failed_hook_read_ends_the_series
///   tested: stacked::tests::a_failed_release_keeps_the_series_error
///   tested: stacked::tests::a_hook_error_names_the_failure_it_cuts_short
#[allow(clippy::too_many_arguments)]
pub fn stacked_series<A: Stacked>(
    a: &A,
    s: &mut A::Session,
    name: &str,
    parent: &[Edit],
    variant: &[Edit],
    scope: Scope,
    repeat: u32,
    hook: &mut Hook<'_>,
) -> Result<bool> {
    let baseline = a.baseline(s);
    let mut f = a.fork(s).context("open the fork")?;
    let ran = levels(
        a, s, &mut f, parent, variant, scope, repeat, &baseline, hook,
    );
    let released = a.release(s, f);
    let reverted = match ran {
        Ok(reverted) => reverted,
        Err(e) => {
            // The series' error is the one returned; the release's is printed, not lost.
            if let Err(r) = released {
                eprintln!("{name}: release after a failed series: {r}");
            }
            return Err(e);
        }
    };
    match released {
        Ok(()) => Ok(reverted),
        // A failed revert poisoned the session, so its release fails too; the series is
        // counted as a revert failure, as a rebuilt one is, and the release failure is printed.
        Err(e) if !reverted => {
            eprintln!("{name}: release after a failed revert: {e}");
            Ok(false)
        }
        // The parent's level did not revert to the base: a revert failure of the series too.
        Err(e @ CheckError::RevertMismatch { .. }) => {
            eprintln!("{name}: REVERT-MISMATCH releasing the parent level: {e}");
            Ok(false)
        }
        Err(e) => Err(e).context("release the fork"),
    }
}

/// The levels of [`stacked_series`] on the live fork `f`; the fork is the caller's to release.
#[allow(clippy::too_many_arguments)]
fn levels<A: Stacked>(
    a: &A,
    s: &mut A::Session,
    f: &mut A::Fork,
    parent: &[Edit],
    variant: &[Edit],
    scope: Scope,
    repeat: u32,
    baseline: &[Finding],
    hook: &mut Hook<'_>,
) -> Result<bool> {
    a.write(s, f, parent)
        .context("write the --on parent as level 1")?;
    let (found, files) = diagnose_top(a, s, f, scope).context("diagnose the --on parent")?;
    let (_, below) = Verdict::of_level(
        baseline,
        &ScopedFindings::default(),
        &found,
        &Edit::deleted_paths(parent),
        &files,
    );
    call(hook, Point::ParentDiagnosed, None)?;
    let deleted = Edit::deleted_paths(variant);
    for _ in 0..repeat {
        call(hook, Point::BeforeWrite, None)?;
        // A refused write pushes no level, so there is nothing to pop.
        a.write(s, f, variant)
            .context("write the variant as a level")?;
        let diagnosed = diagnose_top(a, s, f, scope);
        // A hook error here leaves the level on the fork; the release reverts it.
        call(hook, Point::AfterDiagnose, diagnosed.as_ref().err())?;
        call(hook, Point::BeforePop, diagnosed.as_ref().err())?;
        let popped = a.pop(s, f);
        let (verdict, mismatch) = match (diagnosed, popped) {
            (_, Err(e @ CheckError::RevertMismatch { .. })) => (None, Some(e)),
            (_, Err(e)) => return Err(e).context("pop the variant's level"),
            (Err(e), Ok(_)) => return Err(e).context("diagnose the variant's level"),
            (Ok((found, files)), Ok(_)) => (
                Some(Verdict::of_level(baseline, &below, &found, &deleted, &files).0),
                None,
            ),
        };
        call(hook, Point::AfterPop(verdict.as_ref()), mismatch.as_ref())?;
        if verdict.is_none() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Calls `hook` at `point`. Its error ends the series; `pending`, a failure the series met before
/// that point and had not yet reported (a failed diagnosis, a pop that did not revert), is named
/// in that error, so that ending the series loses neither.
fn call(hook: &mut Hook<'_>, point: Point<'_>, pending: Option<&CheckError>) -> Result<()> {
    let at = point.name();
    hook(point).map_err(|e| {
        let e = anyhow::Error::from(e);
        match pending {
            Some(p) => e.context(format!("the hook at {at}, after the series met: {p}")),
            None => e.context(format!("the hook at {at}")),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ryo_v2_core::app::ports::Verifier;
    use std::cell::RefCell;
    use std::path::PathBuf;

    /// A fork whose diagnosis is `parent` with one level and `top` with more, whose `files_of` is
    /// `files`, and which logs every `fork`, `write`, `diagnose`, `pop` and `release`.
    #[derive(Default)]
    struct FakeStack {
        levels: RefCell<Vec<Vec<Edit>>>,
        parent: ScopedFindings,
        top: ScopedFindings,
        files: Vec<RelPath>,
        /// The port calls, and whatever a test's hook adds.
        log: RefCell<Vec<String>>,
        /// Whether `release` fails, as it does on a poisoned session.
        fail_release: bool,
        /// Whether a diagnosis above the parent's level is cut off.
        fail_top_diagnose: bool,
        /// Whether `pop` does not revert to the level below.
        mismatch_pop: bool,
    }
    impl FakeStack {
        fn note(&self, entry: impl Into<String>) {
            self.log.borrow_mut().push(entry.into());
        }
    }
    impl Verifier for FakeStack {
        type Session = ();
        fn text(&self, _: &(), _: &RelPath) -> Option<String> {
            None
        }
        fn facts(&self, _: &(), _: &RelPath) -> ryo_v2_core::domain::FileFacts {
            ryo_v2_core::domain::FileFacts::default()
        }
        fn baseline(&self, _: &()) -> Vec<Finding> {
            Vec::new()
        }
        fn provenance(&self, _: &()) -> ryo_v2_core::domain::Provenance {
            ryo_v2_core::domain::Provenance::new(
                "fake",
                "0",
                Default::default(),
                Default::default(),
            )
            .unwrap()
        }
        fn blind_classes(&self) -> std::collections::BTreeSet<ryo_v2_core::domain::UnseenClass> {
            Default::default()
        }
        fn check(&self, _: &mut (), _: &[Edit], _: Scope) -> Result<ScopedFindings, CheckError> {
            Ok(ScopedFindings::default())
        }
    }
    impl Stacked for FakeStack {
        type Fork = ();
        fn fork(&self, _: &mut ()) -> Result<(), CheckError> {
            self.note("fork");
            Ok(())
        }
        fn write(&self, _: &mut (), _: &mut (), edits: &[Edit]) -> Result<(), CheckError> {
            let paths: Vec<String> = edits.iter().map(|e| e.path.to_string()).collect();
            self.note(format!("write({})", paths.join(",")));
            self.levels.borrow_mut().push(edits.to_vec());
            Ok(())
        }
        fn levels(&self, _: &(), _: &()) -> Result<usize, CheckError> {
            Ok(self.levels.borrow().len())
        }
        fn fork_text(&self, _: &(), _: &(), _: &RelPath) -> Result<Option<String>, CheckError> {
            Ok(None)
        }
        fn diagnose(&self, _: &(), _: &(), _: Scope) -> Result<ScopedFindings, CheckError> {
            self.note("diagnose");
            if self.levels.borrow().len() == 1 {
                Ok(self.parent.clone())
            } else if self.fail_top_diagnose {
                Err(CheckError::TimedOut { limit_ms: 7 })
            } else {
                Ok(self.top.clone())
            }
        }
        fn files_of(&self, _: &(), _: &(), _: Scope) -> Result<Vec<RelPath>, CheckError> {
            Ok(self.files.clone())
        }
        fn pop(&self, _: &mut (), _: &mut ()) -> Result<bool, CheckError> {
            self.note("pop");
            let popped = self.levels.borrow_mut().pop().is_some();
            if self.mismatch_pop {
                return Err(CheckError::RevertMismatch {
                    paths: vec![rel("a.rs")],
                });
            }
            Ok(popped)
        }
        fn release(&self, _: &mut (), _: ()) -> Result<(), CheckError> {
            self.note("release");
            self.levels.borrow_mut().clear();
            if self.fail_release {
                return Err(CheckError::Poisoned("fake".to_owned()));
            }
            Ok(())
        }
    }

    /// The point's name, as the reports and the logs of these tests print it.
    fn name(point: Point<'_>) -> &'static str {
        match point {
            Point::ParentDiagnosed => "ParentDiagnosed",
            Point::BeforeWrite => "BeforeWrite",
            Point::AfterDiagnose => "AfterDiagnose",
            Point::BeforePop => "BeforePop",
            Point::AfterPop(_) => "AfterPop",
        }
    }

    fn rel(p: &str) -> RelPath {
        RelPath::parse(p).unwrap()
    }

    /// A failed read of `/proc`, as `proc.rs` reports one.
    fn failed_read() -> ProcError {
        ProcError::Read {
            file: PathBuf::from("/proc/self/status"),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        }
    }

    #[test]
    fn a_stacked_verdict_is_formed_as_the_search_forms_it() {
        use ryo_v2_core::domain::UnseenClass;
        use std::collections::{BTreeMap, BTreeSet};
        // The parent's level observed a class on b; the variant's level diagnoses a only.
        let below = ScopedFindings {
            files: vec![rel("a.rs"), rel("b.rs")],
            unseen_at: BTreeMap::from([(
                UnseenClass::UnresolvedPath,
                BTreeSet::from([rel("b.rs")]),
            )]),
            ..ScopedFindings::default()
        };
        let a = FakeStack {
            parent: below.clone(),
            top: ScopedFindings {
                files: vec![rel("a.rs")],
                ..ScopedFindings::default()
            },
            files: vec![rel("a.rs"), rel("b.rs")],
            ..FakeStack::default()
        };
        let parent = [Edit::write(rel("b.rs"), "fn b() {}")];
        let variant = [Edit::write(rel("a.rs"), "fn a() {}")];
        let mut seen: Vec<Option<Verdict>> = Vec::new();
        let reverted = stacked_series(
            &a,
            &mut (),
            "fake",
            &parent,
            &variant,
            Scope::Affected,
            1,
            &mut |point| {
                if let Point::AfterPop(v) = point {
                    seen.push(v.cloned());
                }
                Ok(())
            },
        )
        .unwrap();
        assert!(reverted);
        let verdict = seen.pop().flatten();
        let label = crate::bench::label_of(verdict.as_ref());
        let want = Verdict::of_level(&[], &below, &a.top, &[], &a.files).0;
        assert_eq!(verdict, Some(want));
        // The class kept from below is named: the top level's diagnosis alone would not name it.
        let named = verdict.map(|v| v.unseen).unwrap_or_default();
        assert_eq!(named, BTreeSet::from([UnseenClass::UnresolvedPath]));
        assert!(Verdict::from_baseline(&[], &a.top).unseen.is_empty());
        assert_eq!(label, "PASS");
        assert!(a.levels.borrow().is_empty(), "the level is popped");
    }

    /// The log of one series run with the bench's hook or the probe's: the fake's port calls with
    /// the hook's points between them.
    fn run_logged(observe: &mut dyn FnMut(Point<'_>) -> Result<(), ProcError>) -> Vec<String> {
        let a = FakeStack {
            files: vec![rel("a.rs"), rel("p.rs")],
            ..FakeStack::default()
        };
        let parent = [Edit::write(rel("p.rs"), "fn p() {}")];
        let variant = [Edit::write(rel("a.rs"), "fn a() {}")];
        let reverted = stacked_series(
            &a,
            &mut (),
            "fake",
            &parent,
            &variant,
            Scope::Affected,
            2,
            &mut |point| {
                a.note(name(point));
                observe(point)
            },
        )
        .unwrap();
        assert!(reverted);
        a.log.take()
    }

    #[test]
    fn probe_and_bench_run_the_same_order() {
        // The bench's hook: the strict VmRSS read at ParentDiagnosed and AfterPop.
        let mut rss = Vec::new();
        let bench = run_logged(&mut |point| {
            if matches!(point, Point::ParentDiagnosed | Point::AfterPop(_)) {
                rss.push(crate::proc::vm_rss_kb()?);
            }
            Ok(())
        });
        // The probe's hook: a reading at every point.
        let mut readings = Vec::new();
        let probe = run_logged(&mut |point| {
            readings.push((name(point), crate::proc::vm_rss_kb()?));
            Ok(())
        });
        let mut want: Vec<String> = ["fork", "write(p.rs)", "diagnose", "ParentDiagnosed"]
            .map(String::from)
            .to_vec();
        for _ in 0..2 {
            want.extend(
                [
                    "BeforeWrite",
                    "write(a.rs)",
                    "diagnose",
                    "AfterDiagnose",
                    "BeforePop",
                    "pop",
                    "AfterPop",
                ]
                .map(String::from),
            );
        }
        want.push("release".to_owned());
        assert_eq!(bench, want);
        assert_eq!(probe, bench);
        assert_eq!(rss.len(), 3);
        assert_eq!(readings.len(), 1 + 4 * 2);
        // The bench's own hook, the one `bench::run` passes, through the same series: the same
        // order, one recorded run per repeat, each with its VmRSS, and the parent's VmRSS.
        let (real, runs, rss_runs, rss_parent) = crate::bench::with_bench_hook(2, run_logged);
        assert_eq!(real, want);
        assert_eq!((runs, rss_runs), (2, 2));
        assert!(rss_parent.is_some());
    }

    #[test]
    fn a_failed_hook_read_ends_the_series() {
        let a = FakeStack::default();
        let parent = [Edit::write(rel("p.rs"), "fn p() {}")];
        let variant = [Edit::write(rel("a.rs"), "fn a() {}")];
        let result = stacked_series(
            &a,
            &mut (),
            "fake",
            &parent,
            &variant,
            Scope::Affected,
            3,
            &mut |point| match point {
                Point::AfterPop(_) => Err(failed_read()),
                _ => Ok(()),
            },
        );
        let e = result.unwrap_err();
        assert!(
            matches!(e.downcast_ref(), Some(ProcError::Read { .. })),
            "{e:#}"
        );
        let log = a.log.take();
        assert_eq!(log.last().map(String::as_str), Some("release"));
        // The first repeat's pop was the last level written: no second repeat ran.
        assert_eq!(log.iter().filter(|e| *e == "write(a.rs)").count(), 1);
    }

    #[test]
    fn a_hook_error_names_the_failure_it_cuts_short() {
        let parent = [Edit::write(rel("p.rs"), "fn p() {}")];
        let variant = [Edit::write(rel("a.rs"), "fn a() {}")];
        // A cut-off diagnosis met by a hook failing at AfterDiagnose, and a pop that did not
        // revert met by a hook failing at AfterPop.
        let cases = [
            (
                FakeStack {
                    fail_top_diagnose: true,
                    ..FakeStack::default()
                },
                "AfterDiagnose",
                "cut off after 7 ms",
            ),
            (
                FakeStack {
                    mismatch_pop: true,
                    ..FakeStack::default()
                },
                "AfterPop",
                "revert did not restore",
            ),
        ];
        for (a, at, met) in cases {
            let result = stacked_series(
                &a,
                &mut (),
                "fake",
                &parent,
                &variant,
                Scope::Affected,
                2,
                &mut |point| {
                    if point.name() == at {
                        Err(failed_read())
                    } else {
                        Ok(())
                    }
                },
            );
            let e = result.unwrap_err();
            assert!(
                matches!(e.downcast_ref(), Some(ProcError::Read { .. })),
                "{e:#}"
            );
            let shown = format!("{e:#}");
            assert!(shown.contains(at), "{shown}");
            assert!(shown.contains(met), "{shown}");
            assert_eq!(a.log.take().last().map(String::as_str), Some("release"));
        }
    }

    #[test]
    fn a_failed_release_keeps_the_series_error() {
        let a = FakeStack {
            fail_release: true,
            ..FakeStack::default()
        };
        let parent = [Edit::write(rel("p.rs"), "fn p() {}")];
        let variant = [Edit::write(rel("a.rs"), "fn a() {}")];
        let result = stacked_series(
            &a,
            &mut (),
            "fake",
            &parent,
            &variant,
            Scope::Affected,
            1,
            &mut |point| match point {
                Point::ParentDiagnosed => Err(failed_read()),
                _ => Ok(()),
            },
        );
        let e = result.unwrap_err();
        assert!(
            matches!(e.downcast_ref(), Some(ProcError::Read { .. })),
            "{e:#}"
        );
        assert_eq!(
            a.log.take(),
            ["fork", "write(p.rs)", "diagnose", "release"].map(String::from)
        );
    }
}
