//! Prometheus metrics with dependency injection for testability

use anyhow::Result;
use metrics_exporter_prometheus::PrometheusBuilder;
use std::sync::{Arc, OnceLock};

// ============================================================================
// Trait Definitions
// ============================================================================

/// Trait for recording metrics
pub trait MetricsRecorder: Send + Sync {
    /// Record a counter increment
    fn record_counter(&self, name: &'static str, labels: &[(&'static str, &str)], value: u64);

    /// Record a gauge value
    fn record_gauge(&self, name: &'static str, value: f64);

    /// Record a histogram value
    fn record_histogram(&self, name: &'static str, labels: &[(&'static str, &str)], value: f64);
}

// ============================================================================
// Production Implementation
// ============================================================================

/// Production Prometheus metrics recorder
pub struct PrometheusRecorder;

impl MetricsRecorder for PrometheusRecorder {
    fn record_counter(&self, name: &'static str, labels: &[(&'static str, &str)], value: u64) {
        match labels.len() {
            0 => metrics::counter!(name).increment(value),
            1 => metrics::counter!(name, labels[0].0 => labels[0].1.to_string()).increment(value),
            _ => {
                // For 2+ labels, use first 2
                metrics::counter!(name, labels[0].0 => labels[0].1.to_string(), labels[1].0 => labels[1].1.to_string()).increment(value)
            }
        }
    }

    fn record_gauge(&self, name: &'static str, value: f64) {
        metrics::gauge!(name).set(value);
    }

    fn record_histogram(&self, name: &'static str, labels: &[(&'static str, &str)], value: f64) {
        match labels.len() {
            0 => metrics::histogram!(name).record(value),
            1 => metrics::histogram!(name, labels[0].0 => labels[0].1.to_string()).record(value),
            _ => {
                // For 2+ labels, use first 2
                metrics::histogram!(name, labels[0].0 => labels[0].1.to_string(), labels[1].0 => labels[1].1.to_string()).record(value)
            }
        }
    }
}

// ============================================================================
// Metrics Service
// ============================================================================

/// Metrics service with dependency injection
pub struct MetricsService {
    recorder: Arc<dyn MetricsRecorder>,
}

impl MetricsService {
    /// Create a new metrics service with the given recorder
    pub fn new(recorder: Arc<dyn MetricsRecorder>) -> Self {
        Self { recorder }
    }

    /// Record instance creation
    pub fn record_instance_created(&self, name: &str, model_id: &str) {
        self.recorder.record_counter(
            "tei_manager_instances_created_total",
            &[("instance", name), ("model", model_id)],
            1,
        );
    }

    /// Record instance deletion
    pub fn record_instance_deleted(&self, name: &str) {
        self.recorder.record_counter(
            "tei_manager_instances_deleted_total",
            &[("instance", name)],
            1,
        );
    }

    /// Record health check failure
    pub fn record_health_check_failure(&self, name: &str) {
        self.recorder.record_counter(
            "tei_manager_health_check_failures_total",
            &[("instance", name)],
            1,
        );
    }

    /// Record instance restart
    pub fn record_instance_restart(&self, name: &str) {
        self.recorder.record_counter(
            "tei_manager_instance_restarts_total",
            &[("instance", name)],
            1,
        );
    }

    /// Update total instance count gauge
    pub fn update_instance_count(&self, count: usize) {
        self.recorder
            .record_gauge("tei_manager_instances_count", count as f64);
    }
}

// ============================================================================
// Global Instance (Backward Compatibility)
// ============================================================================

static METRICS_SERVICE: OnceLock<MetricsService> = OnceLock::new();

/// Initialize the global metrics service (should be called once at startup)
pub fn init_service(service: MetricsService) {
    METRICS_SERVICE.get_or_init(|| service);
}

/// Setup Prometheus metrics exporter
/// Returns a handle that can be used to retrieve metrics
pub fn setup_metrics() -> Result<metrics_exporter_prometheus::PrometheusHandle> {
    let handle = PrometheusBuilder::new()
        .install_recorder()
        .map_err(|e| anyhow::anyhow!("Failed to install Prometheus exporter: {}", e))?;

    tracing::info!("Prometheus metrics exporter installed");

    // Initialize global service with production recorder
    init_service(MetricsService::new(Arc::new(PrometheusRecorder)));

    Ok(handle)
}

/// Record instance creation (global function for backward compatibility)
pub fn record_instance_created(name: &str, model_id: &str) {
    if let Some(service) = METRICS_SERVICE.get() {
        service.record_instance_created(name, model_id);
    }
}

/// Record instance deletion (global function for backward compatibility)
pub fn record_instance_deleted(name: &str) {
    if let Some(service) = METRICS_SERVICE.get() {
        service.record_instance_deleted(name);
    }
}

/// Record health check failure (global function for backward compatibility)
pub fn record_health_check_failure(name: &str) {
    if let Some(service) = METRICS_SERVICE.get() {
        service.record_health_check_failure(name);
    }
}

/// Record instance restart (global function for backward compatibility)
pub fn record_instance_restart(name: &str) {
    if let Some(service) = METRICS_SERVICE.get() {
        service.record_instance_restart(name);
    }
}

/// Update total instance count gauge (global function for backward compatibility)
pub fn update_instance_count(count: usize) {
    if let Some(service) = METRICS_SERVICE.get() {
        service.update_instance_count(count);
    }
}

// ============================================================================
// Mock Implementation for Testing
// ============================================================================

#[cfg(test)]
pub mod mocks {
    use super::*;
    use std::collections::HashMap;
    use std::sync::RwLock;

    // Type aliases to reduce complexity
    type LabelVec = Vec<(String, String)>;
    type CounterLabels = HashMap<String, LabelVec>;
    type HistogramEntry = (String, f64, LabelVec);

    /// Mock metrics recorder for testing
    pub struct MockMetricsRecorder {
        counters: Arc<RwLock<HashMap<String, u64>>>,
        counter_labels: Arc<RwLock<CounterLabels>>,
        gauges: Arc<RwLock<HashMap<String, f64>>>,
        histograms: Arc<RwLock<Vec<HistogramEntry>>>,
    }

    impl Default for MockMetricsRecorder {
        fn default() -> Self {
            Self::new()
        }
    }

    impl MockMetricsRecorder {
        pub fn new() -> Self {
            Self {
                counters: Arc::new(RwLock::new(HashMap::new())),
                counter_labels: Arc::new(RwLock::new(HashMap::new())),
                gauges: Arc::new(RwLock::new(HashMap::new())),
                histograms: Arc::new(RwLock::new(Vec::new())),
            }
        }

        /// Get the value of a counter
        pub fn get_counter(&self, name: &str) -> u64 {
            *self.counters.read().unwrap().get(name).unwrap_or(&0)
        }

        /// Get the value of a gauge
        pub fn get_gauge(&self, name: &str) -> f64 {
            *self.gauges.read().unwrap().get(name).unwrap_or(&0.0)
        }

        /// Check if a counter exists
        pub fn has_counter(&self, name: &str) -> bool {
            self.counters.read().unwrap().contains_key(name)
        }

        /// Check if a gauge exists
        pub fn has_gauge(&self, name: &str) -> bool {
            self.gauges.read().unwrap().contains_key(name)
        }

        /// Check if a counter has a specific label
        pub fn counter_has_label(&self, name: &str, key: &str, value: &str) -> bool {
            if let Some(labels) = self.counter_labels.read().unwrap().get(name) {
                labels.iter().any(|(k, v)| k == key && v == value)
            } else {
                false
            }
        }

        /// Get all histogram recordings
        pub fn get_histograms(&self) -> Vec<HistogramEntry> {
            self.histograms.read().unwrap().clone()
        }

        /// Clear all recorded metrics
        pub fn clear(&self) {
            self.counters.write().unwrap().clear();
            self.counter_labels.write().unwrap().clear();
            self.gauges.write().unwrap().clear();
            self.histograms.write().unwrap().clear();
        }
    }

    impl MetricsRecorder for MockMetricsRecorder {
        fn record_counter(&self, name: &'static str, labels: &[(&'static str, &str)], value: u64) {
            let mut counters = self.counters.write().unwrap();
            *counters.entry(name.to_string()).or_insert(0) += value;

            // Store labels
            let mut counter_labels = self.counter_labels.write().unwrap();
            let label_vec = counter_labels.entry(name.to_string()).or_default();
            for (key, val) in labels {
                label_vec.push(((*key).to_string(), (*val).to_string()));
            }
        }

        fn record_gauge(&self, name: &'static str, value: f64) {
            let mut gauges = self.gauges.write().unwrap();
            gauges.insert(name.to_string(), value);
        }

        fn record_histogram(
            &self,
            name: &'static str,
            labels: &[(&'static str, &str)],
            value: f64,
        ) {
            let mut histograms = self.histograms.write().unwrap();
            let owned_labels: Vec<(String, String)> = labels
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect();
            histograms.push((name.to_string(), value, owned_labels));
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use mocks::MockMetricsRecorder;

    #[test]
    fn test_record_instance_created() {
        let mock = Arc::new(MockMetricsRecorder::new());
        let service = MetricsService::new(mock.clone());

        service.record_instance_created("test-inst", "bert-base");

        assert_eq!(mock.get_counter("tei_manager_instances_created_total"), 1);
        assert!(mock.counter_has_label(
            "tei_manager_instances_created_total",
            "instance",
            "test-inst"
        ));
        assert!(mock.counter_has_label(
            "tei_manager_instances_created_total",
            "model",
            "bert-base"
        ));
    }

    #[test]
    fn test_record_instance_deleted() {
        let mock = Arc::new(MockMetricsRecorder::new());
        let service = MetricsService::new(mock.clone());

        service.record_instance_deleted("test-inst");

        assert_eq!(mock.get_counter("tei_manager_instances_deleted_total"), 1);
        assert!(mock.counter_has_label(
            "tei_manager_instances_deleted_total",
            "instance",
            "test-inst"
        ));
    }

    #[test]
    fn test_multiple_increments() {
        let mock = Arc::new(MockMetricsRecorder::new());
        let service = MetricsService::new(mock.clone());

        service.record_instance_created("inst1", "model1");
        service.record_instance_created("inst2", "model2");
        service.record_instance_deleted("inst1");

        assert_eq!(mock.get_counter("tei_manager_instances_created_total"), 2);
        assert_eq!(mock.get_counter("tei_manager_instances_deleted_total"), 1);
    }

    #[test]
    fn test_gauge_updates() {
        let mock = Arc::new(MockMetricsRecorder::new());
        let service = MetricsService::new(mock.clone());

        service.update_instance_count(5);
        assert_eq!(mock.get_gauge("tei_manager_instances_count"), 5.0);

        service.update_instance_count(10);
        assert_eq!(mock.get_gauge("tei_manager_instances_count"), 10.0);
    }

    #[test]
    fn test_health_check_failure() {
        let mock = Arc::new(MockMetricsRecorder::new());
        let service = MetricsService::new(mock.clone());

        service.record_health_check_failure("failing-inst");

        assert_eq!(
            mock.get_counter("tei_manager_health_check_failures_total"),
            1
        );
        assert!(mock.counter_has_label(
            "tei_manager_health_check_failures_total",
            "instance",
            "failing-inst"
        ));
    }

    #[test]
    fn test_instance_restart() {
        let mock = Arc::new(MockMetricsRecorder::new());
        let service = MetricsService::new(mock.clone());

        service.record_instance_restart("restart-inst");

        assert_eq!(mock.get_counter("tei_manager_instance_restarts_total"), 1);
        assert!(mock.counter_has_label(
            "tei_manager_instance_restarts_total",
            "instance",
            "restart-inst"
        ));
    }

    #[test]
    fn test_metric_names_consistent() {
        let mock = Arc::new(MockMetricsRecorder::new());
        let service = MetricsService::new(mock.clone());

        service.record_instance_created("test", "model");
        service.record_instance_deleted("test");
        service.record_health_check_failure("test");
        service.record_instance_restart("test");
        service.update_instance_count(1);

        // Verify all expected metrics exist
        assert!(mock.has_counter("tei_manager_instances_created_total"));
        assert!(mock.has_counter("tei_manager_instances_deleted_total"));
        assert!(mock.has_counter("tei_manager_health_check_failures_total"));
        assert!(mock.has_counter("tei_manager_instance_restarts_total"));
        assert!(mock.has_gauge("tei_manager_instances_count"));
    }

    #[test]
    fn test_mock_clear() {
        let mock = Arc::new(MockMetricsRecorder::new());
        let service = MetricsService::new(mock.clone());

        service.record_instance_created("test", "model");
        service.update_instance_count(5);

        assert_eq!(mock.get_counter("tei_manager_instances_created_total"), 1);
        assert_eq!(mock.get_gauge("tei_manager_instances_count"), 5.0);

        mock.clear();

        assert_eq!(mock.get_counter("tei_manager_instances_created_total"), 0);
        assert_eq!(mock.get_gauge("tei_manager_instances_count"), 0.0);
    }

    #[test]
    fn test_histogram_recording() {
        let mock = Arc::new(MockMetricsRecorder::new());

        mock.record_histogram(
            "test_histogram",
            &[("label1", "value1"), ("label2", "value2")],
            42.5,
        );

        let histograms = mock.get_histograms();
        assert_eq!(histograms.len(), 1);
        assert_eq!(histograms[0].0, "test_histogram");
        assert_eq!(histograms[0].1, 42.5);
        assert_eq!(histograms[0].2.len(), 2);
    }

    #[test]
    fn test_counter_accumulation() {
        let mock = Arc::new(MockMetricsRecorder::new());
        let service = MetricsService::new(mock.clone());

        // Record same metric multiple times
        service.record_instance_restart("inst1");
        service.record_instance_restart("inst1");
        service.record_instance_restart("inst1");

        assert_eq!(mock.get_counter("tei_manager_instance_restarts_total"), 3);
    }

    #[test]
    fn test_different_instances_same_metric() {
        let mock = Arc::new(MockMetricsRecorder::new());
        let service = MetricsService::new(mock.clone());

        service.record_instance_created("inst1", "model1");
        service.record_instance_created("inst2", "model2");
        service.record_instance_created("inst3", "model3");

        // Counter should accumulate all instances
        assert_eq!(mock.get_counter("tei_manager_instances_created_total"), 3);

        // Verify labels are stored
        assert!(mock.counter_has_label("tei_manager_instances_created_total", "instance", "inst1"));
        assert!(mock.counter_has_label("tei_manager_instances_created_total", "instance", "inst2"));
        assert!(mock.counter_has_label("tei_manager_instances_created_total", "instance", "inst3"));
    }
}
