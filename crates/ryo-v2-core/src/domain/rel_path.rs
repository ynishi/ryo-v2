//! Workspace-relative paths.

use super::DomainError;

/// A workspace-relative path with `/` separators.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelPath(String);

impl RelPath {
    /// Rejects empty, absolute, `\`-containing, empty-component and `.` / `..` component paths.
    pub fn parse(s: &str) -> Result<Self, DomainError> {
        let ok = !s.is_empty()
            && !s.starts_with('/')
            && !s.contains('\\')
            && s.split('/').all(|c| !c.is_empty() && c != "." && c != "..");
        if ok {
            Ok(Self(s.to_owned()))
        } else {
            Err(DomainError::RelPath(s.to_owned()))
        }
    }

    /// The path as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RelPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for RelPath {
    type Err = DomainError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rejects_absolute_dotdot_and_backslash() {
        assert_eq!(
            RelPath::parse("lib/src/lib.rs").unwrap().as_str(),
            "lib/src/lib.rs"
        );
        for bad in [
            "", "/a", "a\\b", "a//b", "a/", "./a", "a/./b", "../a", "a/..", ".",
        ] {
            assert!(RelPath::parse(bad).is_err(), "{bad}");
        }
    }
}
