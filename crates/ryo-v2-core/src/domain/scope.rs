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
