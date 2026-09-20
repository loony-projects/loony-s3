//! Structured tracing setup (architecture.md §66/prompt §66-67). Never log secrets,
//! object bodies, or signatures — that rule is enforced at the call sites that will
//! exist once `auth`/`api` land (Phase 3/4), not here; this module only wires the
//! subscriber up.

use std::error::Error;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;

#[derive(Debug, Clone)]
pub struct LoggingConfig {
    /// Emit newline-delimited JSON instead of the human-readable format. Production
    /// deployments want this so logs are directly ingestible; local development
    /// usually doesn't.
    pub json: bool,
    /// A `tracing_subscriber::EnvFilter` directive string, e.g. `"info"` or
    /// `"server=debug,info"`.
    pub env_filter: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            json: false,
            env_filter: "info".to_string(),
        }
    }
}

/// Install the global tracing subscriber. Must be called exactly once, as early as
/// possible in `main`. Returns an error instead of panicking if a subscriber is already
/// installed, so callers (including tests that exercise multiple entry points in one
/// process) can decide whether that's fatal.
pub fn init_tracing(config: LoggingConfig) -> Result<(), Box<dyn Error + Send + Sync>> {
    let filter = EnvFilter::try_new(&config.env_filter).unwrap_or_else(|_| EnvFilter::new("info"));

    if config.json {
        fmt().json().with_env_filter(filter).try_init()
    } else {
        fmt().with_env_filter(filter).try_init()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_human_readable_at_info_level() {
        let config = LoggingConfig::default();
        assert!(!config.json);
        assert_eq!(config.env_filter, "info");
    }

    #[test]
    fn first_init_in_this_process_succeeds() {
        // Only one test in this crate may install the global subscriber, since it's
        // process-global; this is that one test.
        init_tracing(LoggingConfig::default()).unwrap();
        // A second call must fail cleanly rather than panicking.
        assert!(init_tracing(LoggingConfig::default()).is_err());
    }
}
