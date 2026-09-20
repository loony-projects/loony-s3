//! The `ClusterMembership` trait (architecture.md §4): the live view of who's in the
//! cluster and how healthy they are. [`crate::ClusterMembershipService`] is the one
//! implementation.

use async_trait::async_trait;
use s3_core::{NodeId, NodeInfo, NodeState};

use crate::error::ClusterError;

#[async_trait]
pub trait ClusterMembership: Send + Sync {
    fn local_node_id(&self) -> NodeId;
    async fn members(&self) -> Result<Vec<NodeInfo>, ClusterError>;
    async fn node_state(&self, id: NodeId) -> Result<Option<NodeState>, ClusterError> {
        Ok(self
            .members()
            .await?
            .into_iter()
            .find(|n| n.node_id == id)
            .map(|n| n.state))
    }
}
