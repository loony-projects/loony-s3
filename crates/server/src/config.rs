//! Startup configuration (prompt §71): environment variables, with a handful of CLI
//! flags overriding them for the cluster bootstrap/join flow shown in §35. Validated
//! eagerly in [`Config::load`] rather than lazily at first use, so a misconfigured
//! deployment fails at startup instead of mid-request.

use std::net::{AddrParseError, SocketAddr};
use std::path::PathBuf;

use clap::Parser;
use loony_core::NodeId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
    Standalone,
    Cluster,
}

#[derive(Debug, Parser)]
#[command(name = "server", about = "LS3 object storage server")]
pub struct Cli {
    /// Deployment mode. Overrides LS3_MODE if given.
    #[arg(long, value_enum)]
    pub mode: Option<Mode>,

    /// This node's persistent id override. Normally omitted: the id is auto-generated
    /// on first start and persisted in NODE_ID under the data directory. Overrides
    /// LS3_NODE_ID.
    #[arg(long)]
    pub node_id: Option<String>,

    /// Address of an existing cluster member to join (cluster mode only).
    #[arg(long)]
    pub join: Option<String>,

    /// Bootstrap a brand-new cluster (cluster mode only, requires an empty data dir).
    #[arg(long)]
    pub bootstrap: bool,

    /// Cluster id to mint when bootstrapping. Overrides LS3_CLUSTER_ID.
    #[arg(long)]
    pub cluster_id: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("missing required setting {0} (env var or CLI flag)")]
    Missing(&'static str),

    #[error("invalid value for {key}: {value:?} ({reason})")]
    Invalid {
        key: &'static str,
        value: String,
        reason: String,
    },

    #[error(
        "cluster mode requires exactly one of --join or --bootstrap (LS3_JOIN / --bootstrap), not both or neither"
    )]
    ClusterJoinXorBootstrap,

    #[error("--bootstrap requires a cluster id (--cluster-id or LS3_CLUSTER_ID)")]
    BootstrapRequiresClusterId,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub mode: Mode,
    pub node_id_override: Option<NodeId>,
    pub bind_addr: SocketAddr,
    pub admin_addr: SocketAddr,
    pub data_dir: PathBuf,
    pub region: String,
    pub volume_paths: Vec<PathBuf>,
    pub cluster: Option<ClusterConfig>,
}

#[derive(Debug, Clone)]
pub struct ClusterConfig {
    pub cluster_id: Option<String>,
    pub cluster_addr: SocketAddr,
    pub advertise_addr: String,
    pub join: Option<String>,
    pub bootstrap: bool,
}

/// Abstracts "read an environment variable" so [`Config::load`] is testable without
/// mutating the real process environment (which is shared, global, and unsafe to
/// mutate from parallel tests).
pub trait EnvSource {
    fn get(&self, key: &str) -> Option<String>;
}

pub struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

impl Config {
    pub fn load(cli: Cli, env: &impl EnvSource) -> Result<Config, ConfigError> {
        let mode = cli
            .mode
            .or_else(|| {
                env.get("LS3_MODE").and_then(|v| match v.as_str() {
                    "standalone" => Some(Mode::Standalone),
                    "cluster" => Some(Mode::Cluster),
                    _ => None,
                })
            })
            .ok_or(ConfigError::Missing("LS3_MODE / --mode"))?;

        let data_dir = env
            .get("LS3_DATA_DIR")
            .map(PathBuf::from)
            .ok_or(ConfigError::Missing("LS3_DATA_DIR"))?;

        let bind_addr = parse_addr_or_default(env, "LS3_BIND_ADDR", "0.0.0.0:9000")?;
        let admin_addr = parse_addr_or_default(env, "LS3_ADMIN_ADDR", "0.0.0.0:9001")?;
        let region = env
            .get("LS3_REGION")
            .unwrap_or_else(|| "us-east-1".to_string());

        let volume_paths = match env.get("LS3_VOLUME_PATHS") {
            Some(raw) => raw.split(',').map(|s| PathBuf::from(s.trim())).collect(),
            None => vec![data_dir.join("volumes").join("vol-0")],
        };

        let node_id_override = match cli.node_id.or_else(|| env.get("LS3_NODE_ID")) {
            Some(raw) => Some(raw.parse::<NodeId>().map_err(|e| ConfigError::Invalid {
                key: "LS3_NODE_ID",
                value: raw,
                reason: e.to_string(),
            })?),
            None => None,
        };

        let cluster = if mode == Mode::Cluster {
            let cluster_addr = parse_addr_or_default(env, "LS3_CLUSTER_ADDR", "0.0.0.0:9100")?;
            let advertise_addr = env
                .get("LS3_ADVERTISE_ADDR")
                .ok_or(ConfigError::Missing("LS3_ADVERTISE_ADDR"))?;
            let cluster_id = cli.cluster_id.or_else(|| env.get("LS3_CLUSTER_ID"));
            let join = cli.join.or_else(|| env.get("LS3_JOIN"));
            let bootstrap = cli.bootstrap || env.get("LS3_BOOTSTRAP").as_deref() == Some("true");

            if join.is_some() == bootstrap {
                return Err(ConfigError::ClusterJoinXorBootstrap);
            }
            if bootstrap && cluster_id.is_none() {
                return Err(ConfigError::BootstrapRequiresClusterId);
            }

            Some(ClusterConfig {
                cluster_id,
                cluster_addr,
                advertise_addr,
                join,
                bootstrap,
            })
        } else {
            None
        };

        Ok(Config {
            mode,
            node_id_override,
            bind_addr,
            admin_addr,
            data_dir,
            region,
            volume_paths,
            cluster,
        })
    }
}

fn parse_addr_or_default(
    env: &impl EnvSource,
    key: &'static str,
    default: &str,
) -> Result<SocketAddr, ConfigError> {
    let raw = env.get(key).unwrap_or_else(|| default.to_string());
    raw.parse()
        .map_err(|e: AddrParseError| ConfigError::Invalid {
            key,
            value: raw,
            reason: e.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct FakeEnv(HashMap<&'static str, &'static str>);

    impl EnvSource for FakeEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key).map(|s| s.to_string())
        }
    }

    fn empty_cli() -> Cli {
        Cli {
            mode: None,
            node_id: None,
            join: None,
            bootstrap: false,
            cluster_id: None,
        }
    }

    #[test]
    fn standalone_loads_with_just_mode_and_data_dir() {
        let env = FakeEnv(HashMap::from([
            ("LS3_MODE", "standalone"),
            ("LS3_DATA_DIR", "/data"),
        ]));
        let config = Config::load(empty_cli(), &env).unwrap();

        assert_eq!(config.mode, Mode::Standalone);
        assert_eq!(config.data_dir, PathBuf::from("/data"));
        assert_eq!(config.bind_addr, "0.0.0.0:9000".parse().unwrap());
        assert!(config.cluster.is_none());
        assert_eq!(
            config.volume_paths,
            vec![PathBuf::from("/data/volumes/vol-0")]
        );
    }

    #[test]
    fn cli_mode_overrides_env() {
        let env = FakeEnv(HashMap::from([
            ("LS3_MODE", "standalone"),
            ("LS3_DATA_DIR", "/data"),
        ]));
        let cli = Cli {
            mode: Some(Mode::Standalone),
            ..empty_cli()
        };
        let config = Config::load(cli, &env).unwrap();
        assert_eq!(config.mode, Mode::Standalone);
    }

    #[test]
    fn missing_mode_is_an_error() {
        let env = FakeEnv(HashMap::from([("LS3_DATA_DIR", "/data")]));
        let err = Config::load(empty_cli(), &env).unwrap_err();
        assert!(matches!(err, ConfigError::Missing("LS3_MODE / --mode")));
    }

    #[test]
    fn missing_data_dir_is_an_error() {
        let env = FakeEnv(HashMap::from([("LS3_MODE", "standalone")]));
        let err = Config::load(empty_cli(), &env).unwrap_err();
        assert!(matches!(err, ConfigError::Missing("LS3_DATA_DIR")));
    }

    #[test]
    fn invalid_bind_addr_is_rejected() {
        let env = FakeEnv(HashMap::from([
            ("LS3_MODE", "standalone"),
            ("LS3_DATA_DIR", "/data"),
            ("LS3_BIND_ADDR", "not-an-address"),
        ]));
        let err = Config::load(empty_cli(), &env).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Invalid {
                key: "LS3_BIND_ADDR",
                ..
            }
        ));
    }

    #[test]
    fn cluster_mode_requires_advertise_addr_and_join_xor_bootstrap() {
        let env = FakeEnv(HashMap::from([
            ("LS3_MODE", "cluster"),
            ("LS3_DATA_DIR", "/data"),
        ]));
        let err = Config::load(empty_cli(), &env).unwrap_err();
        assert!(matches!(err, ConfigError::Missing("LS3_ADVERTISE_ADDR")));

        let env = FakeEnv(HashMap::from([
            ("LS3_MODE", "cluster"),
            ("LS3_DATA_DIR", "/data"),
            ("LS3_ADVERTISE_ADDR", "node-01:9100"),
        ]));
        let err = Config::load(empty_cli(), &env).unwrap_err();
        assert!(matches!(err, ConfigError::ClusterJoinXorBootstrap));

        let cli = Cli {
            bootstrap: true,
            join: Some("http://node-01:9100".into()),
            ..empty_cli()
        };
        let err = Config::load(cli, &env).unwrap_err();
        assert!(matches!(err, ConfigError::ClusterJoinXorBootstrap));
    }

    #[test]
    fn bootstrap_without_cluster_id_is_rejected() {
        let env = FakeEnv(HashMap::from([
            ("LS3_MODE", "cluster"),
            ("LS3_DATA_DIR", "/data"),
            ("LS3_ADVERTISE_ADDR", "node-01:9100"),
        ]));
        let cli = Cli {
            bootstrap: true,
            ..empty_cli()
        };
        let err = Config::load(cli, &env).unwrap_err();
        assert!(matches!(err, ConfigError::BootstrapRequiresClusterId));
    }

    #[test]
    fn cluster_join_loads_successfully() {
        let env = FakeEnv(HashMap::from([
            ("LS3_MODE", "cluster"),
            ("LS3_DATA_DIR", "/data"),
            ("LS3_ADVERTISE_ADDR", "node-03:9100"),
        ]));
        let cli = Cli {
            join: Some("http://node-01:9100".into()),
            ..empty_cli()
        };
        let config = Config::load(cli, &env).unwrap();

        let cluster = config.cluster.unwrap();
        assert_eq!(cluster.join.as_deref(), Some("http://node-01:9100"));
        assert!(!cluster.bootstrap);
    }

    #[test]
    fn explicit_volume_paths_are_split_on_comma() {
        let env = FakeEnv(HashMap::from([
            ("LS3_MODE", "standalone"),
            ("LS3_DATA_DIR", "/data"),
            ("LS3_VOLUME_PATHS", "/mnt/a, /mnt/b"),
        ]));
        let config = Config::load(empty_cli(), &env).unwrap();
        assert_eq!(
            config.volume_paths,
            vec![PathBuf::from("/mnt/a"), PathBuf::from("/mnt/b")]
        );
    }
}
