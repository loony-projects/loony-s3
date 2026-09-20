//! Bucket domain model (architecture.md §8 / prompt §8).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::ids::{BucketId, OwnerId};

/// A validated S3-style bucket name. Validation happens once, at construction, so every
/// other part of the system can treat a `BucketName` as already-safe.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BucketName(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid bucket name {name:?}: {reason}")]
pub struct InvalidBucketName {
    pub name: String,
    pub reason: &'static str,
}

impl BucketName {
    pub fn parse(name: impl Into<String>) -> Result<Self, InvalidBucketName> {
        let name = name.into();
        let invalid = |reason| InvalidBucketName {
            name: name.clone(),
            reason,
        };

        if !(3..=63).contains(&name.len()) {
            return Err(invalid("must be between 3 and 63 characters"));
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
        {
            return Err(invalid(
                "must contain only lowercase letters, digits, dots, and hyphens",
            ));
        }
        let first = name.chars().next().unwrap();
        let last = name.chars().next_back().unwrap();
        if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
            return Err(invalid("must start with a lowercase letter or digit"));
        }
        if !(last.is_ascii_lowercase() || last.is_ascii_digit()) {
            return Err(invalid("must end with a lowercase letter or digit"));
        }
        if name.contains("..") {
            return Err(invalid("must not contain consecutive dots"));
        }
        if name.contains(".-") || name.contains("-.") {
            return Err(invalid("must not have a dot adjacent to a hyphen"));
        }
        if name.split('.').all(|octet| octet.parse::<u8>().is_ok()) && name.split('.').count() == 4
        {
            return Err(invalid("must not be formatted as an IP address"));
        }

        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for BucketName {
    type Error = InvalidBucketName;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<BucketName> for String {
    fn from(value: BucketName) -> Self {
        value.0
    }
}

impl std::fmt::Display for BucketName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum VersioningState {
    #[default]
    Disabled,
    Enabled,
    Suspended,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bucket {
    pub bucket_id: BucketId,
    pub name: BucketName,
    pub owner_id: OwnerId,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    pub region: String,
    pub versioning_state: VersioningState,
    pub quota_bytes: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_reasonable_names() {
        for name in ["abc", "my-bucket-01", "my.bucket.name", "a1b2c3"] {
            assert!(BucketName::parse(name).is_ok(), "{name} should be valid");
        }
    }

    #[test]
    fn rejects_too_short_or_too_long() {
        assert!(BucketName::parse("ab").is_err());
        assert!(BucketName::parse("a".repeat(64)).is_err());
        assert!(BucketName::parse("a".repeat(63)).is_ok());
    }

    #[test]
    fn rejects_uppercase_and_invalid_chars() {
        assert!(BucketName::parse("MyBucket").is_err());
        assert!(BucketName::parse("my_bucket").is_err());
        assert!(BucketName::parse("my bucket").is_err());
    }

    #[test]
    fn rejects_bad_start_end_and_adjacency() {
        assert!(BucketName::parse("-bucket").is_err());
        assert!(BucketName::parse("bucket-").is_err());
        assert!(BucketName::parse(".bucket").is_err());
        assert!(BucketName::parse("bu..cket").is_err());
        assert!(BucketName::parse("bu.-cket").is_err());
    }

    #[test]
    fn rejects_ip_address_shaped_names() {
        assert!(BucketName::parse("192.168.1.1").is_err());
    }

    #[test]
    fn serde_roundtrips_through_string() {
        let name = BucketName::parse("my-bucket").unwrap();
        let json = serde_json::to_string(&name).unwrap();
        assert_eq!(json, "\"my-bucket\"");
        let parsed: BucketName = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, name);
    }

    #[test]
    fn serde_rejects_invalid_names_on_deserialize() {
        let err = serde_json::from_str::<BucketName>("\"MyBucket\"");
        assert!(err.is_err());
    }
}
