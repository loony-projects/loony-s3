//! `ClusterMembership` trait (added in Phase 7): node registry, heartbeat-based
//! failure-detection hints, and the authoritative state-transition path via
//! `metadata` Raft commands. Bootstrap/join procedure. See docs/architecture.md
//! sections 17-18.
//!
//! Phase 1 delivers just the node-identity foundation everything else in this crate
//! builds on.

mod identity;

pub use identity::{NodeIdentity, NodeIdentityError};
