//! Cluster membership domain model (architecture.md §33-34/§36).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::ids::{NodeId, VolumeId};

/// A validated cluster identifier — a human-chosen name (e.g. `cluster-production`,
/// prompt §71), not a UUID, since operators name clusters and pass the name around in
/// config/join commands.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ClusterId(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid cluster id {id:?}: {reason}")]
pub struct InvalidClusterId {
    pub id: String,
    pub reason: &'static str,
}

impl ClusterId {
    pub fn parse(id: impl Into<String>) -> Result<Self, InvalidClusterId> {
        let id = id.into();
        let invalid = |reason| InvalidClusterId {
            id: id.clone(),
            reason,
        };
        if !(1..=63).contains(&id.len()) {
            return Err(invalid("must be between 1 and 63 characters"));
        }
        if !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(invalid(
                "must contain only ASCII letters, digits, hyphens, and underscores",
            ));
        }
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ClusterId {
    type Error = InvalidClusterId;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<ClusterId> for String {
    fn from(value: ClusterId) -> Self {
        value.0
    }
}

impl std::fmt::Display for ClusterId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Node lifecycle states (architecture.md §33). Authoritative transitions only ever
/// happen via a `MetadataStore` command — never inferred locally and written straight
/// into a response, so every node that reads the registry agrees on the same value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeState {
    Joining,
    Healthy,
    Suspect,
    Offline,
    Draining,
    Removed,
}

/// One entry in the cluster's node registry (architecture.md §34).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeInfo {
    pub node_id: NodeId,
    /// How *other* nodes should reach this one (`host:port` of its internal RPC
    /// listener) — not necessarily the address it locally binds to, which may differ
    /// behind NAT/containers.
    pub advertised_address: String,
    pub state: NodeState,
    /// Bumped every time this `node_id` (re)registers, so a rejoin after a long
    /// outage is distinguishable from continuous membership (architecture.md §34).
    pub generation: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub last_seen: OffsetDateTime,
    /// Topology hints (`["rack:a", "zone:us-east-1a"]`-style) for failure-domain-aware
    /// placement (architecture.md §19) — the placement engine's node-diversity
    /// preference (§10) is a simplified, node-only version of this; the full
    /// hierarchy-aware constraint is later-scoped work.
    pub failure_domain: Vec<String>,
    /// This node's local shard-storage volumes, as of its last registration/heartbeat —
    /// the placement engine's candidate pool (architecture.md §10) is built from every
    /// `HEALTHY` node's `volumes` list, not just the local node's own.
    pub volumes: Vec<VolumeId>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_reasonable_ids() {
        for id in ["cluster-production", "c1", "my_cluster"] {
            assert!(ClusterId::parse(id).is_ok(), "{id} should be valid");
        }
    }

    #[test]
    fn rejects_empty_and_invalid_chars() {
        assert!(ClusterId::parse("").is_err());
        assert!(ClusterId::parse("has space").is_err());
        assert!(ClusterId::parse("has/slash").is_err());
    }

    #[test]
    fn serde_roundtrips_through_string() {
        let id = ClusterId::parse("prod").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"prod\"");
        assert_eq!(serde_json::from_str::<ClusterId>(&json).unwrap(), id);
    }
}
