//! Domain types shared by every other crate: identity newtypes, bucket/object/manifest
//! domain model, shard placement/receipt types. No I/O. No dependency on any other
//! crate in this workspace. See docs/architecture.md §3-§6.

mod bucket;
mod cluster;
mod etag;
mod ids;
mod manifest;
mod object_key;
mod shard;

pub use bucket::{Bucket, BucketName, InvalidBucketName, VersioningState};
pub use cluster::{ClusterId, InvalidClusterId, NodeInfo, NodeState};
pub use etag::ETag;
pub use ids::{
    BucketId, IdParseError, NodeId, ObjectId, OwnerId, ShardId, UploadId, VersionId, VolumeId,
};
pub use manifest::{DurabilityPolicy, ObjectManifest, PartManifest, ShardLocation, Stripe};
pub use object_key::{InvalidObjectKey, ObjectKey};
pub use shard::{ShardReceipt, ShardStat, ShardTarget};
