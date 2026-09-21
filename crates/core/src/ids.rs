//! Persistent, UUIDv7-backed identity newtypes shared across the workspace.
//!
//! UUIDv7 (not v4) is used everywhere so identifiers sort roughly by creation time,
//! which is convenient in logs, directory listings, and manifests without needing a
//! separate sequence counter from anything.

use std::fmt;
use std::str::FromStr;

use uuid::Uuid;

/// A UUID failed to parse into one of this crate's identity newtypes.
#[derive(Debug, thiserror::Error)]
#[error("invalid {type_name}: {source}")]
pub struct IdParseError {
    type_name: &'static str,
    #[source]
    source: uuid::Error,
}

macro_rules! define_uuid_id {
    ($name:ident, $doc:expr) => {
        #[doc = $doc]
        #[derive(
            Debug,
            Clone,
            Copy,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            serde::Serialize,
            serde::Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Mint a new, time-ordered identifier.
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        // A stable, all-zero sentinel -- *not* `Self::new()`. Callers that want a fresh
        // identity must say so explicitly; `Default` exists only so this type can sit
        // inside other `Default`-deriving structs and libraries (e.g. `openraft`, which
        // compares a stored value against a freshly-constructed `Default` as its "no
        // value yet" sentinel — two independently-minted random UUIDs would almost
        // never compare equal, silently breaking that check).
        impl Default for $name {
            fn default() -> Self {
                Self(Uuid::nil())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(s).map(Self).map_err(|source| IdParseError {
                    type_name: stringify!($name),
                    source,
                })
            }
        }
    };
}

define_uuid_id!(
    NodeId,
    "Persistent identity of a storage/metadata node. Never derived from a network \
     address (architecture.md §34) — minted once and persisted for the lifetime of the \
     node's data directory."
);
define_uuid_id!(
    VolumeId,
    "Persistent identity of a single disk/volume on a node. Never derived solely from \
     a mount path (architecture.md §48)."
);
define_uuid_id!(
    ShardId,
    "Opaque physical identifier of one erasure/replication shard on disk, minted by the \
     shard store at write time. Never derived from the object key (architecture.md §51)."
);
define_uuid_id!(
    BucketId,
    "Stable identity of a bucket, independent of its (unique) name."
);
define_uuid_id!(
    ObjectId,
    "Stable identity of an object, independent of its key or version."
);
define_uuid_id!(
    VersionId,
    "Time-ordered identity of one version of an object (architecture.md §16) — minted \
     by the coordinator at manifest-build time, not by the metadata store, so the \
     expensive streaming/encoding work never waits on a metadata round-trip."
);
define_uuid_id!(
    UploadId,
    "Identity of one in-progress multipart upload (architecture.md §27)."
);
define_uuid_id!(
    OwnerId,
    "Identity of the account/principal that owns buckets and credentials."
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_through_display_and_from_str() {
        let id = NodeId::new();
        let parsed: NodeId = id.to_string().parse().unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn roundtrips_through_serde_json() {
        let id = VolumeId::new();
        let json = serde_json::to_string(&id).unwrap();
        let parsed: VolumeId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn distinct_types_with_equal_uuids_do_not_confuse_at_compile_time() {
        // NodeId and VolumeId are structurally identical but nominally distinct types;
        // this test exists mainly to document that `define_uuid_id!` intentionally does
        // not derive any cross-type conversion.
        let uuid = Uuid::now_v7();
        let node = NodeId(uuid);
        let volume = VolumeId(uuid);
        assert_eq!(node.as_uuid(), volume.as_uuid());
    }

    #[test]
    fn rejects_garbage_input() {
        let err = "not-a-uuid".parse::<ShardId>().unwrap_err();
        assert!(err.to_string().contains("invalid ShardId"));
    }

    #[test]
    fn two_new_ids_are_never_equal() {
        assert_ne!(NodeId::new(), NodeId::new());
    }
}
