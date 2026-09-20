//! `MetadataStore` trait and its production implementation, `RedbMetadataStore`: a
//! command-applying state machine persisted in redb, with every mutation wrapped in one
//! ACID write transaction.
//!
//! This is deliberately not wired to `openraft` yet. Architecture.md §2/§26 decided
//! standalone mode should run metadata as a single-voter Raft group and cluster mode as
//! a 3/5-voter group, sharing one implementation — but for exactly one voter, a
//! replicated log and a plain write-ahead log are the same computation: no leader
//! election is possible or needed, there are no peers to replicate to, and log entries
//! are applied the instant they're durable. So this phase implements that single-voter
//! case directly as a redb-backed state machine with the exact command shapes real Raft
//! log entries will carry (see `commands.rs`), and Phase 8 wraps it in `openraft` only
//! once a second voter makes the distinction real. See docs/architecture.md §5.

mod commands;
mod error;
mod redb_store;
mod store;

pub use commands::{
    BeginMultipart, CompleteMultipart, CreateBucket, Credential, ListObjectsPage, ListObjectsQuery,
    MultipartUploadState, ObjectSummary, PartSummary, RegisterNode,
};
pub use error::MetaError;
pub use redb_store::RedbMetadataStore;
pub use store::MetadataStore;
