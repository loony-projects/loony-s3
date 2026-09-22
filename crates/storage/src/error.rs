use std::path::PathBuf;

use loony_core::{NodeId, ShardId, ShardTarget, VolumeId};

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("shard {0} not found")]
    NotFound(ShardId),

    #[error("volume {0} is not known to this node")]
    UnknownVolume(VolumeId),

    #[error(
        "volume metadata at {path} belongs to node {expected} but this node is {actual} \
         — a volume must never be silently adopted by the wrong node"
    )]
    NodeMismatch {
        path: PathBuf,
        expected: NodeId,
        actual: NodeId,
    },

    #[error("volume metadata at {path} is corrupt: {source}")]
    CorruptVolumeMeta {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("write target {target:?} is not local to this node ({local_node})")]
    NotLocal {
        target: ShardTarget,
        local_node: NodeId,
    },

    #[error("remote node {0} is unreachable: {1}")]
    Unreachable(NodeId, String),

    #[error("remote node {0} returned an unexpected response: {1}")]
    Remote(NodeId, String),
}
