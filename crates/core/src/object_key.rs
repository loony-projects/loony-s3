//! Object key validation (architecture.md §9/§51). Keys are arbitrary logical
//! identifiers, never trusted as filesystem paths — that guarantee is structural
//! (physical paths are always `ShardId`-derived, see `storage`), but keys are still
//! validated here so obviously-malformed input is rejected early with a clear error
//! rather than surfacing as a confusing failure three layers down.

use serde::{Deserialize, Serialize};

const MAX_KEY_LEN: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ObjectKey(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid object key: {reason}")]
pub struct InvalidObjectKey {
    pub reason: &'static str,
}

impl ObjectKey {
    pub fn parse(key: impl Into<String>) -> Result<Self, InvalidObjectKey> {
        let key = key.into();
        if key.is_empty() {
            return Err(InvalidObjectKey {
                reason: "must not be empty",
            });
        }
        if key.len() > MAX_KEY_LEN {
            return Err(InvalidObjectKey {
                reason: "must be at most 1024 bytes",
            });
        }
        if key.contains('\0') {
            return Err(InvalidObjectKey {
                reason: "must not contain a null byte",
            });
        }
        if key.chars().any(|c| c.is_control()) {
            return Err(InvalidObjectKey {
                reason: "must not contain control characters",
            });
        }
        Ok(Self(key))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ObjectKey {
    type Error = InvalidObjectKey;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<ObjectKey> for String {
    fn from(value: ObjectKey) -> Self {
        value.0
    }
}

impl std::fmt::Display for ObjectKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_typical_keys() {
        for key in ["users/123/avatar.jpg", "videos/2026/movie.mp4", "a"] {
            assert!(ObjectKey::parse(key).is_ok(), "{key} should be valid");
        }
    }

    #[test]
    fn rejects_empty_and_oversized() {
        assert!(ObjectKey::parse("").is_err());
        assert!(ObjectKey::parse("a".repeat(1025)).is_err());
        assert!(ObjectKey::parse("a".repeat(1024)).is_ok());
    }

    #[test]
    fn rejects_null_bytes_and_control_chars() {
        assert!(ObjectKey::parse("a\0b").is_err());
        assert!(ObjectKey::parse("a\nb").is_err());
        assert!(ObjectKey::parse("a\tb").is_err());
    }

    #[test]
    fn does_not_special_case_path_traversal_syntax() {
        // `../` is a perfectly valid *logical key* -- it is never used to build a
        // physical path (that's the whole point of shard-id-addressed storage), so
        // there is nothing to reject here. This test documents that intentionally.
        assert!(ObjectKey::parse("../../etc/passwd").is_ok());
    }
}
