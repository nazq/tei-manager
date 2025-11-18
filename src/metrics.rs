//! Prometheus metrics

use anyhow::Result;
use metrics_exporter_prometheus::PrometheusBuilder;

/// Setup Prometheus metrics exporter
/// Returns a handle that can be used to retrieve metrics
pub fn setup_metrics() -> Result<metrics_exporter_prometheus::PrometheusHandle> {
    let handle = PrometheusBuilder::new()
        .install_recorder()
        .map_err(|e| anyhow::anyhow!("Failed to install Prometheus exporter: {}", e))?;

    tracing::info!("Prometheus metrics exporter installed");

    Ok(handle)
}

/// Record instance creation
pub fn record_instance_created(name: &str, model_id: &str) {
    metrics::counter!("tei_manager_instances_created_total",
        "instance" => name.to_string(),
        "model" => model_id.to_string()
    )
    .increment(1);
}

/// Record instance deletion
pub fn record_instance_deleted(name: &str) {
    metrics::counter!("tei_manager_instances_deleted_total",
        "instance" => name.to_string()
    )
    .increment(1);
}

/// Record health check failure
pub fn record_health_check_failure(name: &str) {
    metrics::counter!("tei_manager_health_check_failures_total",
        "instance" => name.to_string()
    )
    .increment(1);
}

/// Record instance restart
pub fn record_instance_restart(name: &str) {
    metrics::counter!("tei_manager_instance_restarts_total",
        "instance" => name.to_string()
    )
    .increment(1);
}

/// Update total instance count gauge
pub fn update_instance_count(count: usize) {
    metrics::gauge!("tei_manager_instances_count").set(count as f64);
}
