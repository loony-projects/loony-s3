//! `HttpRaftNetworkFactory`: the real, cross-process [`RaftNetworkFactory`] used once a
//! Raft group has more than one voter (Phase 8) — reuses this crate's existing
//! bearer-token-authenticated HTTP transport (`server.rs`/`client.rs`, Phase 6) rather
//! than inventing a second one. `loony_metadata::NoopNetworkFactory` covers the
//! single-voter case, which never needs to reach a peer at all.
//!
//! Unlike [`RemoteShardStore`](crate::RemoteShardStore), this doesn't need a
//! [`NodeAddressResolver`](crate::NodeAddressResolver): `openraft` already tracks each
//! peer's address as part of its replicated membership config (the `BasicNode` handed
//! to [`new_client`](openraft::network::RaftNetworkFactory::new_client)), so there's
//! nothing extra to resolve.

use openraft::error::{InstallSnapshotError, NetworkError, RPCError, RaftError, RemoteError, Unreachable};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse, VoteRequest,
    VoteResponse,
};
use openraft::BasicNode;
use loony_core::NodeId;
use loony_metadata::TypeConfig;
use serde::Serialize;
use serde::de::DeserializeOwned;

#[derive(Clone)]
pub struct HttpRaftNetworkFactory {
    http: reqwest::Client,
    token: String,
}

impl HttpRaftNetworkFactory {
    pub fn new(token: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            token,
        }
    }

    // `RPCError` is `openraft`'s own type, shaped generically over every RPC's distinct
    // error payload -- boxing it here would just push the same size cost onto every
    // caller instead of removing it, for a single non-hot-path network call.
    #[allow(clippy::result_large_err)]
    async fn send_rpc<Req, Resp, Err>(
        &self,
        target: NodeId,
        target_node: &BasicNode,
        path: &str,
        req: Req,
    ) -> Result<Resp, RPCError<NodeId, BasicNode, Err>>
    where
        Req: Serialize,
        Err: std::error::Error + DeserializeOwned,
        Resp: DeserializeOwned,
    {
        let url = format!("http://{}/internal/v1/raft/{path}", target_node.addr);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&req)
            .send()
            .await
            .map_err(|e| {
                if e.is_connect() {
                    RPCError::Unreachable(Unreachable::new(&e))
                } else {
                    RPCError::Network(NetworkError::new(&e))
                }
            })?;

        let result: Result<Resp, Err> = resp
            .json()
            .await
            .map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        result.map_err(|e| RPCError::RemoteError(RemoteError::new(target, e)))
    }
}

impl RaftNetworkFactory<TypeConfig> for HttpRaftNetworkFactory {
    type Network = HttpRaftNetwork;

    async fn new_client(&mut self, target: NodeId, node: &BasicNode) -> Self::Network {
        HttpRaftNetwork {
            factory: self.clone(),
            target,
            target_node: node.clone(),
        }
    }
}

pub struct HttpRaftNetwork {
    factory: HttpRaftNetworkFactory,
    target: NodeId,
    target_node: BasicNode,
}

impl RaftNetwork<TypeConfig> for HttpRaftNetwork {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<NodeId>, RPCError<NodeId, BasicNode, RaftError<NodeId>>> {
        self.factory
            .send_rpc(self.target, &self.target_node, "append", rpc)
            .await
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<InstallSnapshotResponse<NodeId>, RPCError<NodeId, BasicNode, RaftError<NodeId, InstallSnapshotError>>> {
        self.factory
            .send_rpc(self.target, &self.target_node, "snapshot", rpc)
            .await
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<NodeId>,
        _option: RPCOption,
    ) -> Result<VoteResponse<NodeId>, RPCError<NodeId, BasicNode, RaftError<NodeId>>> {
        self.factory.send_rpc(self.target, &self.target_node, "vote", rpc).await
    }
}
