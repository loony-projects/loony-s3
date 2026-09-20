//! `ClusterMembership` trait: node registry, heartbeat-based failure-detection hints,
//! and the authoritative state-transition path via `s3-metadata` Raft commands.
//! Bootstrap/join procedure. See docs/architecture.md sections 17-18, and
//! `service.rs`'s module docs for exactly what "authoritative" means before Phase 8's
//! real multi-voter Raft exists.

mod cluster_identity;
mod error;
mod identity;
mod membership;
mod service;

pub use cluster_identity::ClusterIdentity;
pub use error::ClusterError;
pub use identity::{NodeIdentity, NodeIdentityError};
pub use membership::ClusterMembership;
pub use service::{ClusterMembershipService, run_heartbeat_loop};
