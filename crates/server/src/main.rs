//! `server` binary: config, `--mode standalone|cluster` dispatch, and wiring of the
//! concrete trait implementations from every other crate. See ../../docs/architecture.md.
//!
//! Phase 4 status: standalone mode serves the real S3 API (CreateBucket, PutObject,
//! GetObject, HeadObject, DeleteObject, ListObjectsV2, bucket operations) over HTTP,
//! behind SigV4 authentication (header + presigned) and ownership-based authorization.
//! Cluster mode isn't wired yet (Raft/RPC/membership land in Phases 6-9).

mod config;

use std::sync::Arc;

use clap::Parser;
use config::{Cli, Config, Mode, ProcessEnv};
use s3_api::AppState;
use s3_cluster::NodeIdentity;
use s3_core::NodeId;
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
    if let Some(cluster) = &config.cluster {
        tracing::info!(
            cluster_id = cluster.cluster_id.as_deref().unwrap_or("(unset; learned on join)"),
            cluster_addr = %cluster.cluster_addr,
            advertise_addr = %cluster.advertise_addr,
            bootstrap = cluster.bootstrap,
            join = cluster.join.as_deref().unwrap_or("(none)"),
            "cluster mode configuration"
        );
    }

    if config.mode == Mode::Cluster {
        eprintln!(
            "server: cluster mode is configured but not implemented yet (Raft/RPC/membership \
             land in later phases). Run with --mode standalone for now."
        );
        return std::process::ExitCode::FAILURE;
    }

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

    let state = AppState {
        buckets: Arc::new(BucketService::new(metadata.clone())),
        objects: Arc::new(ObjectService::new(
            metadata.clone(),
            Arc::new(volumes),
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
