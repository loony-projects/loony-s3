//! Startup configuration (prompt §71): environment variables, with a handful of CLI
//! flags overriding them for the cluster bootstrap/join flow shown in §35. Validated
//! eagerly in [`Config::load`] rather than lazily at first use, so a misconfigured
//! deployment fails at startup instead of mid-request.

use std::net::{AddrParseError, SocketAddr};
use std::path::PathBuf;

use clap::Parser;
use loony_console::{ConsoleConfig, is_secure_url, origin_of};
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

    #[error(
        "LS3_ADMIN_ADDR must be a loopback address, not {0}: the admin API is local-only (use \
         an SSH tunnel to reach it from another machine)"
    )]
    AdminAddrNotLoopback(SocketAddr),

    #[error(
        "sign-in needs LS3_OIDC_ISSUER, LS3_OIDC_CLIENT_ID, LS3_OIDC_CLIENT_SECRET and \
         LS3_OIDC_REDIRECT_URIS together: {0} is missing"
    )]
    IncompleteSignInConfig(&'static str),
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
    /// `None` leaves sign-in off, and the console API isn't served at all.
    pub console: Option<ConsoleConfig>,
    /// `None` leaves the web UI to be served some other way.
    pub ui: Option<UiConfig>,
}

#[derive(Debug, Clone)]
pub struct UiConfig {
    pub dir: PathBuf,
    pub addr: SocketAddr,
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

        let bind_addr = parse_addr_or_default(env, "LS3_BIND_ADDR", "0.0.0.0:7353")?;
        let admin_addr = parse_addr_or_default(env, "LS3_ADMIN_ADDR", "127.0.0.1:7253")?;
        if !admin_addr.ip().is_loopback() {
            return Err(ConfigError::AdminAddrNotLoopback(admin_addr));
        }
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
            console: load_console_config(env)?,
            ui: load_ui_config(env)?,
        })
    }
}

const DEFAULT_SESSION_TTL_SECS: u64 = 8 * 60 * 60;
const MAX_SESSION_TTL_SECS: u64 = 7 * 24 * 60 * 60;

fn load_console_config(env: &impl EnvSource) -> Result<Option<ConsoleConfig>, ConfigError> {
    let get = |key| {
        env.get(key)
            .filter(|value: &String| !value.trim().is_empty())
    };
    let issuer = get("LS3_OIDC_ISSUER");
    let client_id = get("LS3_OIDC_CLIENT_ID");
    let client_secret = get("LS3_OIDC_CLIENT_SECRET");
    let redirect_uris = get("LS3_OIDC_REDIRECT_URIS");

    if issuer.is_none() && client_id.is_none() && client_secret.is_none() && redirect_uris.is_none()
    {
        return Ok(None);
    }
    let issuer = issuer.ok_or(ConfigError::IncompleteSignInConfig("LS3_OIDC_ISSUER"))?;
    let client_id = client_id.ok_or(ConfigError::IncompleteSignInConfig("LS3_OIDC_CLIENT_ID"))?;
    let client_secret = client_secret.ok_or(ConfigError::IncompleteSignInConfig(
        "LS3_OIDC_CLIENT_SECRET",
    ))?;
    let redirect_uris = redirect_uris.ok_or(ConfigError::IncompleteSignInConfig(
        "LS3_OIDC_REDIRECT_URIS",
    ))?;

    let require_secure = |key: &'static str, url: &str| {
        if is_secure_url(url) {
            Ok(())
        } else {
            Err(ConfigError::Invalid {
                key,
                value: url.to_string(),
                reason: "must be https, or http to this machine".into(),
            })
        }
    };
    require_secure("LS3_OIDC_ISSUER", &issuer)?;
    let internal_url = get("LS3_OIDC_INTERNAL_URL");
    if let Some(url) = &internal_url {
        require_secure("LS3_OIDC_INTERNAL_URL", url)?;
    }

    let redirect_uris: Vec<String> = redirect_uris
        .split(',')
        .map(|uri| uri.trim().to_string())
        .filter(|uri| !uri.is_empty())
        .collect();
    if redirect_uris.is_empty() {
        return Err(ConfigError::IncompleteSignInConfig(
            "LS3_OIDC_REDIRECT_URIS",
        ));
    }
    for uri in &redirect_uris {
        require_secure("LS3_OIDC_REDIRECT_URIS", uri)?;
        if origin_of(uri).is_none() {
            return Err(ConfigError::Invalid {
                key: "LS3_OIDC_REDIRECT_URIS",
                value: uri.clone(),
                reason: "must be an absolute URL".into(),
            });
        }
    }

    let session_ttl_secs = match env.get("LS3_CONSOLE_SESSION_TTL_SECS") {
        Some(raw) => match raw.parse::<u64>() {
            Ok(secs) if (1..=MAX_SESSION_TTL_SECS).contains(&secs) => secs,
            _ => {
                return Err(ConfigError::Invalid {
                    key: "LS3_CONSOLE_SESSION_TTL_SECS",
                    value: raw,
                    reason: format!("must be from 1 to {MAX_SESSION_TTL_SECS}"),
                });
            }
        },
        None => DEFAULT_SESSION_TTL_SECS,
    };

    Ok(Some(ConsoleConfig {
        issuer,
        internal_url,
        client_id,
        client_secret,
        redirect_uris,
        session_ttl: std::time::Duration::from_secs(session_ttl_secs),
        client_ip_header: get("LS3_TRUSTED_IP_HEADER"),
    }))
}

fn load_ui_config(env: &impl EnvSource) -> Result<Option<UiConfig>, ConfigError> {
    let Some(raw) = env.get("LS3_UI_DIR").filter(|dir| !dir.trim().is_empty()) else {
        return Ok(None);
    };
    let dir = PathBuf::from(&raw);
    // A site that can't serve anything should stop the server now, not show up as a blank
    // page for whoever visits next.
    if !dir.join("index.html").is_file() {
        return Err(ConfigError::Invalid {
            key: "LS3_UI_DIR",
            value: raw,
            reason: "has no index.html: build the web UI first".into(),
        });
    }
    Ok(Some(UiConfig {
        dir,
        addr: parse_addr_or_default(env, "LS3_UI_ADDR", "127.0.0.1:7354")?,
    }))
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
        assert_eq!(config.bind_addr, "0.0.0.0:7353".parse().unwrap());
        assert!(config.cluster.is_none());
        assert_eq!(
            config.volume_paths,
            vec![PathBuf::from("/data/volumes/vol-0")]
        );
    }

    #[test]
    fn admin_addr_defaults_to_loopback_and_refuses_anything_else() {
        let base = [("LS3_MODE", "standalone"), ("LS3_DATA_DIR", "/data")];
        let config = Config::load(empty_cli(), &FakeEnv(HashMap::from(base))).unwrap();
        assert_eq!(config.admin_addr, "127.0.0.1:7253".parse().unwrap());

        let mut env = HashMap::from(base);
        env.insert("LS3_ADMIN_ADDR", "0.0.0.0:7253");
        assert!(matches!(
            Config::load(empty_cli(), &FakeEnv(env)),
            Err(ConfigError::AdminAddrNotLoopback(_))
        ));
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

    fn standalone_with(settings: &[(&'static str, &'static str)]) -> Result<Config, ConfigError> {
        let mut env = HashMap::from([("LS3_MODE", "standalone"), ("LS3_DATA_DIR", "/tmp/data")]);
        env.extend(settings.iter().copied());
        Config::load(empty_cli(), &FakeEnv(env))
    }

    const SIGN_IN: [(&str, &str); 4] = [
        ("LS3_OIDC_ISSUER", "https://auth.example.com"),
        ("LS3_OIDC_CLIENT_ID", "client"),
        ("LS3_OIDC_CLIENT_SECRET", "secret"),
        (
            "LS3_OIDC_REDIRECT_URIS",
            "https://console.example.com/callback, http://localhost:5173/callback",
        ),
    ];

    #[test]
    fn sign_in_stays_off_unless_configured() {
        assert!(standalone_with(&[]).unwrap().console.is_none());
    }

    #[test]
    fn sign_in_settings_are_read() {
        let console = standalone_with(&SIGN_IN).unwrap().console.unwrap();
        assert_eq!(console.issuer, "https://auth.example.com");
        assert_eq!(console.internal_url, None);
        assert_eq!(
            console.redirect_uris,
            [
                "https://console.example.com/callback",
                "http://localhost:5173/callback"
            ]
        );
        assert_eq!(console.session_ttl.as_secs(), 8 * 60 * 60);

        let mut settings = SIGN_IN.to_vec();
        settings.push(("LS3_OIDC_INTERNAL_URL", "http://127.0.0.1:8450"));
        settings.push(("LS3_CONSOLE_SESSION_TTL_SECS", "60"));
        settings.push(("LS3_TRUSTED_IP_HEADER", "CF-Connecting-IP"));
        let console = standalone_with(&settings).unwrap().console.unwrap();
        assert_eq!(
            console.internal_url.as_deref(),
            Some("http://127.0.0.1:8450")
        );
        assert_eq!(console.session_ttl.as_secs(), 60);
        assert_eq!(
            console.client_ip_header.as_deref(),
            Some("CF-Connecting-IP")
        );
    }

    #[test]
    fn part_of_the_sign_in_settings_is_an_error() {
        for missing in SIGN_IN.map(|(key, _)| key) {
            let partial: Vec<_> = SIGN_IN
                .iter()
                .copied()
                .filter(|(key, _)| *key != missing)
                .collect();
            assert!(
                matches!(
                    standalone_with(&partial),
                    Err(ConfigError::IncompleteSignInConfig(key)) if key == missing
                ),
                "without {missing}"
            );
        }
    }

    #[test]
    fn insecure_sign_in_addresses_are_refused() {
        for (key, value) in [
            ("LS3_OIDC_ISSUER", "http://auth.example.com"),
            (
                "LS3_OIDC_REDIRECT_URIS",
                "http://console.example.com/callback",
            ),
            ("LS3_OIDC_REDIRECT_URIS", "/callback"),
            ("LS3_OIDC_INTERNAL_URL", "http://auth.internal"),
        ] {
            let mut settings: Vec<_> = SIGN_IN
                .iter()
                .copied()
                .filter(|(existing, _)| *existing != key)
                .collect();
            settings.push((key, value));
            assert!(
                matches!(standalone_with(&settings), Err(ConfigError::Invalid { .. })),
                "{key}={value}"
            );
        }
    }

    #[test]
    fn an_unreasonable_session_lifetime_is_refused() {
        for value in ["0", "-1", "soon", "999999999"] {
            let mut settings = SIGN_IN.to_vec();
            settings.push(("LS3_CONSOLE_SESSION_TTL_SECS", value));
            assert!(
                matches!(standalone_with(&settings), Err(ConfigError::Invalid { .. })),
                "{value}"
            );
        }
    }

    #[test]
    fn the_web_ui_is_only_served_when_asked_and_when_it_exists() {
        assert!(standalone_with(&[]).unwrap().ui.is_none());

        assert!(matches!(
            standalone_with(&[("LS3_UI_DIR", "/nonexistent/loony-ui")]),
            Err(ConfigError::Invalid {
                key: "LS3_UI_DIR",
                ..
            })
        ));

        let site = tempfile::tempdir().unwrap();
        std::fs::write(site.path().join("index.html"), "app").unwrap();
        let path: &'static str =
            Box::leak(site.path().to_str().unwrap().to_string().into_boxed_str());
        let ui = standalone_with(&[("LS3_UI_DIR", path)])
            .unwrap()
            .ui
            .unwrap();
        assert_eq!(ui.addr.to_string(), "127.0.0.1:7354");
        let ui = standalone_with(&[("LS3_UI_DIR", path), ("LS3_UI_ADDR", "127.0.0.1:9999")])
            .unwrap()
            .ui
            .unwrap();
        assert_eq!(ui.addr.port(), 9999);
    }
}
