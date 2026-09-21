//! Phase 8: `openraft`-backed metadata replication, wrapping the exact state machine
//! `redb_store.rs` implements directly for the pre-Raft single-voter case (see this
//! crate's top-level `lib.rs` doc and architecture.md §5).

mod applier;
mod log_store;
mod network;
mod snapshot;
mod state_machine;
mod store;
#[cfg(test)]
mod tests;
mod types;

pub use network::NoopNetworkFactory;
pub use store::RaftMetadataStore;
pub use types::{CommandResponse, CommandResult, MetadataCommand, Raft, TypeConfig};
