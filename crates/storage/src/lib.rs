//! `ShardStore` trait: local disk engine (atomic write protocol, hash-prefixed shard
//! layout) and, from Phase 6 onward, the remote implementation that dispatches over
//! `rpc` to other nodes. See docs/architecture.md sections 2 and 7.

mod atomic;
mod error;
mod local;
mod store;
mod volume;

pub use error::StorageError;
pub use local::{LocalVolume, LocalVolumeManager};
pub use store::{ShardBytesIn, ShardBytesOut, ShardStore};
pub use volume::{VOLUME_META_FORMAT_VERSION, VolumeMeta};
