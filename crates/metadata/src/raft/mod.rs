//! Phase 8: `openraft`-backed metadata replication, wrapping the exact state machine
//! `redb_store.rs` implements directly for the pre-Raft single-voter case (see this
//! crate's top-level `lib.rs` doc and architecture.md §5).

pub(crate) mod applier;
pub(crate) mod billing_apply;
#[cfg(test)]
mod billing_tests;
mod log_store;
mod network;
pub(crate) mod projects;
mod snapshot;
mod state_machine;
mod store;
#[cfg(test)]
mod tests;
pub(crate) mod types;

pub use network::NoopNetworkFactory;
pub use store::{RaftMetadataStore, RaftStatus};
pub use types::{CommandResponse, CommandResult, MetadataCommand, Raft, TypeConfig};
