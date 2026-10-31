//! rustc error codes.

use super::DomainError;

/// A rustc error code: `E` followed by exactly four ASCII digits.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ErrorCode(String);

impl ErrorCode {
    /// Parses `"E0599"`; rejects `"e0599"`, `"E599"`, `"E05990"` and non-E-code names.
    pub fn parse(s: &str) -> Result<Self, DomainError> {
        if s.len() == 5 && s.starts_with('E') && s[1..].bytes().all(|b| b.is_ascii_digit()) {
            Ok(Self(s.to_owned()))
        } else {
            Err(DomainError::ErrorCode(s.to_owned()))
        }
    }

    /// The code as text, e.g. `"E0599"`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ErrorCode {
    type Err = DomainError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_e_and_four_digits() {
        assert_eq!(ErrorCode::parse("E0599").unwrap().as_str(), "E0599");
        for bad in ["E599", "e0599", "E05990", "typed-hole", "", "E05a9"] {
            assert!(ErrorCode::parse(bad).is_err(), "{bad}");
        }
    }
}
