//! Prometheus metrics skeleton (prompt §67). This installs the global recorder and
//! exposes a way to render it; the actual counters/gauges/histograms listed in §67
//! (`requests_total`, `shard_reads_total`, `raft_term`, ...) get recorded at their
//! call sites once those subsystems exist (Phase 3 onward) — nothing to record yet in
//! Phase 1 beyond proving the plumbing works.

use metrics_exporter_prometheus::{BuildError, PrometheusBuilder, PrometheusHandle};

/// Install the global Prometheus metrics recorder. Must be called exactly once, before
/// any `metrics::counter!`/`gauge!`/`histogram!` call site executes anywhere in the
/// process. The returned handle renders the current registry in Prometheus text format
/// for the `/metrics` endpoint `api` will expose once it exists.
pub fn install_recorder() -> Result<PrometheusHandle, BuildError> {
    PrometheusBuilder::new().install_recorder()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_recorder_renders_a_recorded_counter() {
        let handle = install_recorder().unwrap();
        metrics::counter!("test_counter_total").increment(1);
        let rendered = handle.render();
        assert!(rendered.contains("test_counter_total"));
    }
}
