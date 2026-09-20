//! `server` binary: config, `--mode standalone|cluster` dispatch, and wiring of the
//! concrete trait implementations from every other crate. See ../../docs/architecture.md.
//!
//! Phase 7 status: standalone mode serves the real S3 API behind SigV4 auth. Cluster
//! mode now bootstraps or joins a real cluster (node registry + heartbeat-driven
//! failure detection over the Phase 6 RPC transport) and serves the S3 API alongside
//! it — but each node's bucket/object metadata is still local to itself; unifying that
//! is Phase 8 (Raft replication) + Phase 9 (distributed PUT/GET). See
//! `s3_cluster::service`'s module docs for exactly what "cluster" means before then.

mod config;

use std::sync::Arc;

use clap::Parser;
use config::{Cli, ClusterConfig, Config, ProcessEnv};
use s3_api::AppState;
use s3_cluster::{ClusterIdentity, ClusterMembershipService, NodeIdentity};
use s3_core::{ClusterId, NodeId};
use s3_metadata::{Credential, MetadataStore, RedbMetadataStore};
use s3_object::{BucketService, ObjectService};
use s3_observability::{LoggingConfig, init_tracing, install_recorder};
use s3_storage::LocalVolumeManager;
use sha2::{Digest, Sha256};

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    dotenvy::dotenv().ok(); // best-effort; absence of a .env file is not an error

    let config = match Config::load(cli, &ProcessEnv) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("server: invalid configuration: {err}");
            return std::process::ExitCode::FAILURE;
        }
    };

    if let Err(err) = init_tracing(LoggingConfig::default()) {
        eprintln!("server: failed to install tracing subscriber: {err}");
        return std::process::ExitCode::FAILURE;
    }

    if let Err(err) = install_recorder() {
        tracing::error!(%err, "failed to install metrics recorder");
        return std::process::ExitCode::FAILURE;
    }

    let identity = match NodeIdentity::load_or_create(&config.data_dir).await {
        Ok(identity) => identity,
        Err(err) => {
            tracing::error!(%err, "failed to load or create node identity");
            return std::process::ExitCode::FAILURE;
        }
    };

    if let Some(expected) = config.node_id_override
        && expected != identity.node_id
    {
        tracing::error!(
            expected = %expected,
            actual = %identity.node_id,
            "--node-id / S3_NODE_ID does not match this data directory's persisted NODE_ID"
        );
        return std::process::ExitCode::FAILURE;
    }

    let volumes =
        match LocalVolumeManager::open(identity.node_id, config.volume_paths.clone()).await {
            Ok(volumes) => volumes,
            Err(err) => {
                tracing::error!(%err, "failed to open local volumes");
                return std::process::ExitCode::FAILURE;
            }
        };
    let volume_ids: Vec<_> = volumes.volume_ids().collect();
    let volume_count = volume_ids.len();
    let volumes: Arc<dyn s3_storage::ShardStore> = Arc::new(volumes);

    tracing::info!(
        mode = ?config.mode,
        node_id = %identity.node_id,
        data_dir = %config.data_dir.display(),
        region = %config.region,
        bind_addr = %config.bind_addr,
        admin_addr = %config.admin_addr,
        volume_count,
        "server foundation initialized"
    );

    let metadata_path = config.data_dir.join("meta.redb");
    let metadata = match RedbMetadataStore::open(metadata_path).await {
        Ok(store) => Arc::new(store),
        Err(err) => {
            tracing::error!(%err, "failed to open metadata store");
            return std::process::ExitCode::FAILURE;
        }
    };

    if let Err(err) = seed_root_credential(&metadata, identity.node_id).await {
        tracing::error!(%err, "failed to seed root credential");
        return std::process::ExitCode::FAILURE;
    }

    if let Some(cluster_config) = &config.cluster
        && let Err(code) = start_cluster_mode(
            cluster_config,
            &identity,
            &config.data_dir,
            metadata.clone(),
            volumes.clone(),
        )
        .await
    {
        return code;
    }

    let state = AppState {
        buckets: Arc::new(BucketService::new(metadata.clone())),
        objects: Arc::new(ObjectService::new(
            metadata.clone(),
            volumes,
            identity.node_id,
            volume_ids,
        )),
        credentials: metadata,
        region: config.region.clone(),
    };
    let router = s3_api::build_router(state);

    let listener = match tokio::net::TcpListener::bind(config.bind_addr).await {
        Ok(listener) => listener,
        Err(err) => {
            tracing::error!(%err, addr = %config.bind_addr, "failed to bind S3 API address");
            return std::process::ExitCode::FAILURE;
        }
    };
    tracing::info!(addr = %config.bind_addr, "S3 API listening");
    eprintln!("server: S3 API listening on http://{}", config.bind_addr);

    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("shutdown signal received, draining");
    };
    if let Err(err) = axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
    {
        tracing::error!(%err, "server exited with an error");
        return std::process::ExitCode::FAILURE;
    }

    std::process::ExitCode::SUCCESS
}

/// Bootstraps or joins the cluster, then starts the internal RPC server (shard I/O +
/// cluster protocol, Phase 6/7) and, if this node is the authority, the heartbeat loop.
/// Returns `Err(exit_code)` on any failure that should stop startup.
async fn start_cluster_mode(
    cluster_config: &ClusterConfig,
    identity: &NodeIdentity,
    data_dir: &std::path::Path,
    metadata: Arc<RedbMetadataStore>,
    shard_store: Arc<dyn s3_storage::ShardStore>,
) -> Result<(), std::process::ExitCode> {
    let (token, token_is_dev_default) = resolve_cluster_token(cluster_config.cluster_id.as_deref());
    if token_is_dev_default {
        tracing::warn!(
            "no S3_CLUSTER_TOKEN set -- using a token derived from the cluster id. Fine for \
             local development, not for anything else (see docs/architecture.md §41)."
        );
    }

    let advertise_addr = cluster_config.advertise_addr.clone();

    let membership = if cluster_config.bootstrap {
        let cluster_id = match cluster_config.cluster_id.as_deref().map(ClusterId::parse) {
            Some(Ok(id)) => id,
            Some(Err(err)) => {
                tracing::error!(%err, "invalid --cluster-id");
                return Err(std::process::ExitCode::FAILURE);
            }
            None => unreachable!("Config::load requires a cluster id when bootstrapping"),
        };
        if let Err(err) = ClusterIdentity::check_and_persist(data_dir, &cluster_id).await {
            tracing::error!(%err, "cluster identity check failed");
            return Err(std::process::ExitCode::FAILURE);
        }
        let metadata_dyn: Arc<dyn MetadataStore> = metadata.clone();
        match ClusterMembershipService::bootstrap(
            cluster_id.clone(),
            metadata_dyn,
            identity.node_id,
            advertise_addr,
            Vec::new(),
            token.clone(),
        )
        .await
        {
            Ok(service) => {
                tracing::info!(cluster_id = %cluster_id, node_id = %identity.node_id, "bootstrapped new cluster");
                eprintln!("server: bootstrapped cluster {cluster_id:?} as the first node");
                service
            }
            Err(err) => {
                tracing::error!(%err, "failed to bootstrap cluster");
                return Err(std::process::ExitCode::FAILURE);
            }
        }
    } else {
        let seed = cluster_config
            .join
            .clone()
            .expect("Config::load requires --join outside bootstrap");
        let seed_url = if seed.starts_with("http://") || seed.starts_with("https://") {
            seed
        } else {
            format!("http://{seed}")
        };
        let claimed = match ClusterIdentity::load(data_dir).await {
            Ok(id) => id,
            Err(err) => {
                tracing::error!(%err, "failed to read local cluster identity");
                return Err(std::process::ExitCode::FAILURE);
            }
        };
        match ClusterMembershipService::join(
            seed_url.clone(),
            token.clone(),
            identity.node_id,
            advertise_addr,
            Vec::new(),
            claimed,
        )
        .await
        {
            Ok((service, cluster_id)) => {
                if let Err(err) = ClusterIdentity::check_and_persist(data_dir, &cluster_id).await {
                    tracing::error!(%err, "cluster identity check failed after join");
                    return Err(std::process::ExitCode::FAILURE);
                }
                tracing::info!(cluster_id = %cluster_id, node_id = %identity.node_id, seed = %seed_url, "joined cluster");
                eprintln!("server: joined cluster {cluster_id:?} via {seed_url}");
                service
            }
            Err(err) => {
                tracing::error!(%err, seed = %seed_url, "failed to join cluster");
                return Err(std::process::ExitCode::FAILURE);
            }
        }
    };

    let membership = Arc::new(membership);
    tokio::spawn(s3_cluster::run_heartbeat_loop(
        membership.clone(),
        std::time::Duration::from_secs(5),
    ));

    let rpc_state = s3_rpc::RpcServerState {
        shard_store,
        metadata: metadata as Arc<dyn MetadataStore>,
        local_node: identity.node_id,
        token,
    };
    let rpc_router = s3_rpc::build_router(rpc_state);
    let rpc_listener = match tokio::net::TcpListener::bind(cluster_config.cluster_addr).await {
        Ok(listener) => listener,
        Err(err) => {
            tracing::error!(%err, addr = %cluster_config.cluster_addr, "failed to bind internal RPC address");
            return Err(std::process::ExitCode::FAILURE);
        }
    };
    tracing::info!(addr = %cluster_config.cluster_addr, "internal RPC listening");
    eprintln!(
        "server: internal RPC listening on http://{}",
        cluster_config.cluster_addr
    );
    tokio::spawn(async move {
        if let Err(err) = axum::serve(rpc_listener, rpc_router).await {
            tracing::error!(%err, "internal RPC server exited with an error");
        }
    });

    Ok(())
}

/// Dev-mode cluster token derivation (architecture.md §41's "simpler credentials"
/// allowance, same pattern as `derive_dev_credential` below): `S3_CLUSTER_TOKEN` is the
/// real production path; without it, every node derives the same token from the
/// cluster id, which every node must be told anyway (bootstrap always sets it; join
/// accepts `--cluster-id`/`S3_CLUSTER_ID` for exactly this purpose, separate from the
/// cluster-id *validation* that happens after a successful join).
fn resolve_cluster_token(cluster_id_hint: Option<&str>) -> (String, bool) {
    if let Ok(token) = std::env::var("S3_CLUSTER_TOKEN") {
        return (token, false);
    }
    let seed = cluster_id_hint.unwrap_or("unspecified-cluster");
    let digest = Sha256::digest(format!("{seed}:s3-dev-cluster-token").as_bytes());
    let token = digest
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    (token, true)
}

/// Ensures a usable SigV4 credential exists before the server starts accepting
/// requests. Prefers `S3_ROOT_ACCESS_KEY`/`S3_ROOT_SECRET_KEY` from the environment
/// (the real production path); when neither is set, falls back to a credential
/// deterministically derived from this node's persistent `NodeId` — stable across
/// restarts (so a dev server you keep restarting doesn't invalidate itself), but
/// insecure by construction (anyone who can compute a `NodeId`'s hash can compute the
/// secret), so it's logged loudly and is not a substitute for setting the env vars in
/// any deployment that matters.
async fn seed_root_credential(
    metadata: &Arc<RedbMetadataStore>,
    node_id: NodeId,
) -> Result<(), s3_metadata::MetaError> {
    let env_creds = (
        std::env::var("S3_ROOT_ACCESS_KEY").ok(),
        std::env::var("S3_ROOT_SECRET_KEY").ok(),
    );
    let (access_key, secret_key, is_dev_default) = match env_creds {
        (Some(access_key), Some(secret_key)) => (access_key, secret_key, false),
        _ => {
            let (access_key, secret_key) = derive_dev_credential(node_id);
            (access_key, secret_key, true)
        }
    };

    // Reuse the existing owner if this credential was already seeded on a previous
    // run, so restarts don't orphan the buckets/objects a prior root credential owned.
    let owner_id = match metadata.get_credential(&access_key).await? {
        Some(existing) => existing.owner_id,
        None => s3_core::OwnerId::new(),
    };

    metadata
        .put_credential(Credential {
            access_key: access_key.clone(),
            secret_key: secret_key.clone(),
            owner_id,
            enabled: true,
            created_at: time::OffsetDateTime::now_utc(),
        })
        .await?;

    if is_dev_default {
        tracing::warn!(
            access_key = %access_key,
            "no S3_ROOT_ACCESS_KEY/S3_ROOT_SECRET_KEY set -- using a credential derived from this \
             node's identity. Fine for local development, not for anything else."
        );
        eprintln!(
            "server: using a dev-default root credential (set S3_ROOT_ACCESS_KEY / \
             S3_ROOT_SECRET_KEY for anything beyond local testing):\n  \
             access key: {access_key}\n  secret key: {secret_key}"
        );
    } else {
        tracing::info!(access_key = %access_key, "root credential loaded from environment");
    }

    Ok(())
}

fn derive_dev_credential(node_id: NodeId) -> (String, String) {
    let seed = node_id.to_string();
    let access_key = format!(
        "AKIADEV{}",
        seed.replace('-', "")
            .to_uppercase()
            .get(..16)
            .unwrap_or(&seed)
    );
    let secret_key = {
        let digest = Sha256::digest(format!("{seed}:s3-dev-root-secret").as_bytes());
        digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    (access_key, secret_key)
}
