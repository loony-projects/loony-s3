//! Tracing/metrics setup shared by `server`: structured tracing, a Prometheus
//! metrics registry, and request-id propagation helpers. See docs/architecture.md and
//! the prompt's sections 66-68.

mod logging;
mod metrics;
mod request_id;

pub use logging::{LoggingConfig, init_tracing};
pub use metrics::install_recorder;
pub use request_id::UuidV7RequestId;
