//! Identifiers of Blueprints and Rules.
//!
//! Both are UUID version 7. The core never generates one: an id arrives as a value
//! (`arch-lint.toml` denies the generator functions in `domain` and `app`).

use super::DomainError;

macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(uuid::Uuid);

        impl $name {
            /// Parses the textual UUID form; only version 7 is accepted.
            ///
            /// # Contract
            /// - C1: a version 7 UUID is accepted; any other version, and text that is not a
            ///   UUID, is refused with [`DomainError::Uuid`].
            ///   tested: domain::ids::tests::v4_uuid_rejected
            pub fn parse(s: &str) -> Result<Self, DomainError> {
                let u = uuid::Uuid::parse_str(s).map_err(|_| DomainError::Uuid(s.to_owned()))?;
                Self::from_uuid(u)
            }

            /// Wraps a UUID; only version 7 is accepted.
            ///
            /// # Contract
            /// - C1: refuses every version except 7 with [`DomainError::Uuid`].
            ///   tested: domain::ids::tests::v4_uuid_rejected
            pub fn from_uuid(u: uuid::Uuid) -> Result<Self, DomainError> {
                if u.get_version_num() == 7 {
                    Ok(Self(u))
                } else {
                    Err(DomainError::Uuid(u.to_string()))
                }
            }

            /// The wrapped UUID.
            pub fn as_uuid(&self) -> uuid::Uuid {
                self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0.hyphenated())
            }
        }

        impl std::str::FromStr for $name {
            type Err = DomainError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::parse(s)
            }
        }
    };
}

uuid_id! {
    /// Identifies one Blueprint (UUID version 7).
    BlueprintId
}
uuid_id! {
    /// Identifies one Rule (UUID version 7).
    RuleId
}

#[cfg(test)]
mod tests {
    use super::*;

    const V7: &str = "01890a5d-ac96-774b-bcce-b302099a8057";
    const V4: &str = "936da01f-9abd-4d9d-80c7-02af85c822a8";

    #[test]
    fn v4_uuid_rejected() {
        let id = BlueprintId::parse(V7).unwrap();
        assert_eq!(id.to_string(), V7);
        assert_eq!(RuleId::parse(V7).unwrap().to_string(), V7);
        assert_eq!(BlueprintId::from_uuid(id.as_uuid()).unwrap(), id);
        assert_eq!(
            BlueprintId::parse(V4),
            Err(DomainError::Uuid(V4.to_owned()))
        );
        assert!(RuleId::parse(V4).is_err());
        assert!(RuleId::from_uuid(uuid::Uuid::nil()).is_err());
        assert!(BlueprintId::parse("not a uuid").is_err());
    }
}
