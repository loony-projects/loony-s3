//! ETag (architecture.md §29 / prompt §29). For a single-part object this is the hex
//! MD5 of the object body, matching what most standard clients/tools assume even though it's
//! not part of the spec; a multipart object's ETag is `hex(MD5(concat(part MD5s)))-N`
//! per the upstream protocol's own (documented, non-cryptographic) convention. `object` computes the
//! actual digest; this type is just the validated wrapper other layers pass around.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ETag(String);

impl ETag {
    /// Build an ETag from a single-part MD5 digest.
    pub fn from_md5(digest: [u8; 16]) -> Self {
        Self(hex(&digest))
    }

    /// Build a multipart ETag: `hex(MD5(concat(part digests)))-{part_count}`, the same
    /// (non-cryptographic, documented-as-such) convention the upstream protocol uses.
    pub fn from_multipart_digests(combined_digest: [u8; 16], part_count: usize) -> Self {
        Self(format!("{}-{part_count}", hex(&combined_digest)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ETag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_part_etag_is_plain_hex() {
        let etag = ETag::from_md5([0u8; 16]);
        assert_eq!(etag.as_str(), "00000000000000000000000000000000");
    }

    #[test]
    fn multipart_etag_has_dash_suffix() {
        let etag = ETag::from_multipart_digests([0xab; 16], 7);
        assert!(etag.as_str().ends_with("-7"));
        assert_eq!(etag.as_str().len(), 32 + 2); // 32 hex chars + "-7"
    }
}
