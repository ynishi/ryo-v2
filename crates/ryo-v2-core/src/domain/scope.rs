//! How far a check reaches.

/// Which files a check diagnoses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scope {
    /// 編集 file を持つ member crate と、その逆依存閉包 (transitive dependents) の file だけ診断する (ryo-v2-ra の `scoped_files` が選ぶ)。
    #[default]
    Affected,
    /// workspace の local file 全部。
    Workspace,
}

impl Scope {
    /// Whether a check at `self` reaches at least as far as one at `other`.
    ///
    /// # Contract
    /// - C1: `Workspace` covers both scopes; `Affected` covers `Affected` only.
    ///   tested: domain::scope::tests::workspace_covers_both_affected_only_itself
    pub fn covers(self, other: Scope) -> bool {
        matches!(
            (self, other),
            (Scope::Workspace, _) | (Scope::Affected, Scope::Affected)
        )
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Scope::Affected => "affected",
            Scope::Workspace => "workspace",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_covers_both_affected_only_itself() {
        assert!(Scope::Workspace.covers(Scope::Workspace));
        assert!(Scope::Workspace.covers(Scope::Affected));
        assert!(Scope::Affected.covers(Scope::Affected));
        assert!(!Scope::Affected.covers(Scope::Workspace));
    }
}
