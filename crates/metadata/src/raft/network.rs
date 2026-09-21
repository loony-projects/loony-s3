//! [`NoopNetworkFactory`]: the `RaftNetworkFactory` used for a single-voter group
//! (standalone mode, architecture.md §2's "for exactly one voter, a replicated log and
//! a plain WAL are the same computation"). A lone voter never has a peer to replicate
//! to or elect against, so every method here is unreachable in practice; wiring a real
//! HTTP transport in for a group that will never use it would just be dead weight on
//! standalone's startup path. The real transport (`s3_rpc::raft_network`) is used once
//! cluster mode adds a second voter.

use crate::raft::types::TypeConfig;

#[derive(Clone, Default)]
pub struct NoopNetworkFactory;

pub struct NoopNetwork;

impl openraft::network::RaftNetworkFactory<TypeConfig> for NoopNetworkFactory {
    type Network = NoopNetwork;

    async fn new_client(&mut self, _target: s3_core::NodeId, _node: &openraft::BasicNode) -> Self::Network {
        NoopNetwork
    }
}

impl openraft::network::RaftNetwork<TypeConfig> for NoopNetwork {
    async fn append_entries(
        &mut self,
        _rpc: openraft::raft::AppendEntriesRequest<TypeConfig>,
        _option: openraft::network::RPCOption,
    ) -> Result<
        openraft::raft::AppendEntriesResponse<s3_core::NodeId>,
        openraft::error::RPCError<s3_core::NodeId, openraft::BasicNode, openraft::error::RaftError<s3_core::NodeId>>,
    > {
        unreachable!("a single-voter Raft group never sends AppendEntries to a peer")
    }

    async fn install_snapshot(
        &mut self,
        _rpc: openraft::raft::InstallSnapshotRequest<TypeConfig>,
        _option: openraft::network::RPCOption,
    ) -> Result<
        openraft::raft::InstallSnapshotResponse<s3_core::NodeId>,
        openraft::error::RPCError<
            s3_core::NodeId,
            openraft::BasicNode,
            openraft::error::RaftError<s3_core::NodeId, openraft::error::InstallSnapshotError>,
        >,
    > {
        unreachable!("a single-voter Raft group never sends InstallSnapshot to a peer")
    }

    async fn vote(
        &mut self,
        _rpc: openraft::raft::VoteRequest<s3_core::NodeId>,
        _option: openraft::network::RPCOption,
    ) -> Result<
        openraft::raft::VoteResponse<s3_core::NodeId>,
        openraft::error::RPCError<s3_core::NodeId, openraft::BasicNode, openraft::error::RaftError<s3_core::NodeId>>,
    > {
        unreachable!("a single-voter Raft group never requests a vote from a peer")
    }
}
