//! `MetadataStore` trait, with two implementations:
//!
//! - [`RedbMetadataStore`]: a command-applying state machine persisted directly in
//!   redb, every mutation in one ACID write transaction, no consensus involved. Kept as
//!   a reference implementation and for its own unit tests; production wiring uses
//!   [`RaftMetadataStore`] instead (see below).
//! - [`RaftMetadataStore`] (Phase 8, `raft/`): the same state machine, wrapped in
//!   `openraft`. Every mutation is a Raft log entry (see `commands.rs`'s doc comment,
//!   written back in Phase 2 anticipating exactly this), committed through
//!   `Raft::client_write` before it's applied — real consensus even for a single-voter
//!   group (standalone mode), so standalone and cluster mode run the literal same code
//!   path rather than an equivalence argument about it. See docs/architecture.md §5.

mod commands;
mod error;
mod raft;
mod redb_store;
mod store;

pub use commands::{
    BeginMultipart, CompleteMultipart, CreateBucket, Credential, ListObjectsPage, ListObjectsQuery,
    MultipartUploadState, ObjectSummary, PartSummary, RegisterNode,
};
pub use error::MetaError;
pub use raft::{CommandResponse, CommandResult, MetadataCommand, NoopNetworkFactory, Raft, RaftMetadataStore, TypeConfig};
pub use redb_store::RedbMetadataStore;
pub use store::MetadataStore;
