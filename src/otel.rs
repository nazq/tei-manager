//! OpenTelemetry tracing: OTLP export, W3C trace-context propagation and
//! log/trace correlation.
//!
//! Every existing `#[instrument]` span becomes an OTel span through the
//! `tracing-opentelemetry` bridge, so a client that sends `traceparent` in
//! gRPC metadata (or HTTP headers) sees tei-manager's work as children of
//! its own span, and the hop to text-embeddings-router carries the same
//! context onward.
//!
//! [`init`] installs the global `tracing` subscriber exactly once:
//! `EnvFilter` + fmt layer (JSON or pretty) + the OTel layer when an
//! endpoint is configured. It returns an [`OtelGuard`] that flushes pending
//! spans on drop; keep it alive for the life of `main`.

use anyhow::{Context, Result};
use opentelemetry::propagation::{Extractor, Injector};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::{KeyValue, global};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
use serde::{Deserialize, Serialize};
use tonic::metadata::{MetadataKey, MetadataMap, MetadataValue};
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

/// `[otel]` block of the manager config
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OtelConfig {
    /// OTLP/gRPC collector endpoint, e.g. "http://otel-collector:4317".
    /// Empty (default) disables export; spans still exist in-process so
    /// trace ids propagate and appear in logs.
    /// Override via: TEI_MANAGER_OTEL_ENDPOINT
    pub endpoint: String,
    /// `service.name` resource attribute (default: "tei-manager")
    pub service_name: String,
    /// Head-based sampling ratio in [0.0, 1.0] (default: 1.0)
    pub sample_ratio: f64,
    /// `deployment.environment` resource attribute; empty = unset
    pub deployment_environment: String,
}

impl Default for OtelConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            service_name: "tei-manager".to_string(),
            sample_ratio: 1.0,
            deployment_environment: String::new(),
        }
    }
}

impl OtelConfig {
    /// True when spans should be exported
    pub fn enabled(&self) -> bool {
        !self.endpoint.trim().is_empty()
    }
}

/// Flushes the span exporter on drop
pub struct OtelGuard {
    provider: Option<SdkTracerProvider>,
}

impl Drop for OtelGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.provider.take()
            && let Err(e) = provider.shutdown()
        {
            eprintln!("otel: failed to flush spans on shutdown: {e}");
        }
    }
}

/// Log output format
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Json,
    Pretty,
}

impl LogFormat {
    pub fn parse(s: &str) -> Self {
        match s {
            "pretty" => LogFormat::Pretty,
            _ => LogFormat::Json,
        }
    }
}

/// Install the global subscriber (and OTLP exporter when configured).
///
/// The W3C `TraceContextPropagator` is installed as the global propagator
/// regardless of export, so `traceparent` is honoured and forwarded even
/// when nothing is collecting.
pub fn init(cfg: &OtelConfig, log_format: LogFormat, filter: &str) -> Result<OtelGuard> {
    global::set_text_map_propagator(TraceContextPropagator::new());

    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter));

    // The bridge captures its tracer at layer construction, so the provider
    // must exist before the layer is built. Without an endpoint we still
    // install a non-exporting provider: spans then carry real trace/span
    // ids for propagation and log correlation.
    let provider = build_tracer_provider(cfg)?;
    global::set_tracer_provider(provider.clone());
    let tracer = provider.tracer(cfg.service_name.clone());

    // `with_tracer` infers its subscriber type from the call site, so the
    // OTel layer is constructed inline in each branch.
    match log_format {
        LogFormat::Json => {
            let fmt_layer = tracing_subscriber::fmt::layer()
                .json()
                .with_current_span(true)
                .with_span_list(true)
                .with_target(true);
            tracing_subscriber::registry()
                .with(env_filter)
                .with(fmt_layer)
                .with(tracing_opentelemetry::layer().with_tracer(tracer))
                .try_init()
                .context("failed to initialize tracing subscriber (json)")?;
        }
        LogFormat::Pretty => {
            let fmt_layer = tracing_subscriber::fmt::layer();
            tracing_subscriber::registry()
                .with(env_filter)
                .with(fmt_layer)
                .with(tracing_opentelemetry::layer().with_tracer(tracer))
                .try_init()
                .context("failed to initialize tracing subscriber (pretty)")?;
        }
    }

    if cfg.enabled() {
        tracing::info!(
            endpoint = %cfg.endpoint,
            service = %cfg.service_name,
            sample_ratio = cfg.sample_ratio,
            environment = %cfg.deployment_environment,
            "OpenTelemetry export enabled"
        );
    } else {
        tracing::info!(
            "OpenTelemetry export disabled (no otel.endpoint); trace context is still propagated"
        );
    }

    Ok(OtelGuard {
        provider: Some(provider),
    })
}

fn build_tracer_provider(cfg: &OtelConfig) -> Result<SdkTracerProvider> {
    let sampler = if cfg.sample_ratio >= 1.0 {
        Sampler::AlwaysOn
    } else if cfg.sample_ratio <= 0.0 {
        Sampler::AlwaysOff
    } else {
        Sampler::TraceIdRatioBased(cfg.sample_ratio)
    };

    let builder = SdkTracerProvider::builder()
        .with_sampler(sampler)
        .with_resource(build_resource(cfg));

    let builder = if cfg.enabled() {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(cfg.endpoint.trim())
            .with_timeout(std::time::Duration::from_secs(10))
            .build()
            .with_context(|| format!("failed to build OTLP span exporter for {}", cfg.endpoint))?;
        builder.with_batch_exporter(exporter)
    } else {
        builder
    };

    Ok(builder.build())
}

/// `builder_empty()` skips OTEL_* env autodetection so the configured
/// `service.name` cannot be overridden from the environment.
fn build_resource(cfg: &OtelConfig) -> Resource {
    let mut kv = vec![KeyValue::new("service.name", cfg.service_name.clone())];
    if !cfg.deployment_environment.is_empty() {
        kv.push(KeyValue::new(
            "deployment.environment",
            cfg.deployment_environment.clone(),
        ));
    }
    Resource::builder_empty().with_attributes(kv).build()
}

// ============================================================================
// Propagation
// ============================================================================

/// Reads W3C trace context from tonic metadata
pub struct MetadataExtractor<'a>(pub &'a MetadataMap);

impl Extractor for MetadataExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0
            .keys()
            .map(|k| match k {
                tonic::metadata::KeyRef::Ascii(k) => k.as_str(),
                tonic::metadata::KeyRef::Binary(k) => k.as_str(),
            })
            .collect()
    }
}

/// Writes W3C trace context into tonic metadata
pub struct MetadataInjector<'a>(pub &'a mut MetadataMap);

impl Injector for MetadataInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let Ok(key) = MetadataKey::from_bytes(key.as_bytes())
            && let Ok(value) = MetadataValue::try_from(&value)
        {
            self.0.insert(key, value);
        }
    }
}

/// Reads W3C trace context from HTTP headers
pub struct HeaderExtractor<'a>(pub &'a http::HeaderMap);

impl Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

/// Parent context carried by an incoming request, if any
pub fn extract_context(extractor: &dyn Extractor) -> opentelemetry::Context {
    global::get_text_map_propagator(|p| p.extract(extractor))
}

/// Record the span's own OTel trace/span ids into its `trace_id` /
/// `span_id` fields (declared `Empty` at creation) so JSON log lines
/// emitted inside it carry them.
pub fn record_trace_ids(span: &Span) {
    let cx = span.context();
    let sc = opentelemetry::trace::TraceContextExt::span(&cx);
    let sc = sc.span_context();
    if sc.is_valid() {
        span.record("trace_id", sc.trace_id().to_string());
        span.record("span_id", sc.span_id().to_string());
    }
}

/// Split a gRPC path like `/tei_multiplexer.v1.TeiMultiplexer/EmbedArrow`
/// into (service, method)
pub fn split_grpc_path(path: &str) -> (String, String) {
    let mut parts = path.trim_start_matches('/').splitn(2, '/');
    let service = parts.next().unwrap_or("").to_string();
    let method = parts.next().unwrap_or("").to_string();
    (service, method)
}

/// Server span for one inbound gRPC request, parented on the caller's
/// `traceparent` when present. Follows OTel RPC semantic conventions.
pub fn grpc_server_span<B>(req: &http::Request<B>) -> Span {
    let (service, method) = split_grpc_path(req.uri().path());
    let span = tracing::info_span!(
        "grpc.request",
        otel.name = %format!("{service}/{method}"),
        otel.kind = "server",
        rpc.system = "grpc",
        rpc.service = %service,
        rpc.method = %method,
        trace_id = tracing::field::Empty,
        span_id = tracing::field::Empty,
    );
    let parent = extract_context(&HeaderExtractor(req.headers()));
    // Only fails if the OTel layer isn't installed; the span still works
    let _ = span.set_parent(parent);
    record_trace_ids(&span);
    span
}

/// tower layer for the tonic server: one span per RPC (see [`grpc_server_span`])
pub fn grpc_trace_layer<B>() -> tower_http::trace::TraceLayer<
    tower_http::classify::SharedClassifier<tower_http::classify::GrpcErrorsAsFailures>,
    fn(&http::Request<B>) -> Span,
> {
    tower_http::trace::TraceLayer::new_for_grpc()
        .make_span_with(grpc_server_span::<B> as fn(&http::Request<B>) -> Span)
}

/// tonic client interceptor: injects the current span's trace context so
/// the backend (text-embeddings-router with `--otlp-endpoint`) joins the trace
#[derive(Clone, Copy, Debug, Default)]
pub struct TraceContextInterceptor;

impl tonic::service::Interceptor for TraceContextInterceptor {
    fn call(&mut self, mut req: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        let cx = Span::current().context();
        global::get_text_map_propagator(|p| {
            p.inject_context(&cx, &mut MetadataInjector(req.metadata_mut()))
        });
        Ok(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::TraceContextExt;
    use std::sync::Once;

    const TRACEPARENT: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

    /// A subscriber with the OTel bridge but no exporter, so spans get real ids
    fn ensure_subscriber() {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            global::set_text_map_propagator(TraceContextPropagator::new());
            let provider = SdkTracerProvider::builder().build();
            let tracer = provider.tracer("test");
            let _ = tracing_subscriber::registry()
                .with(tracing_opentelemetry::layer().with_tracer(tracer))
                .try_init();
        });
    }

    #[test]
    fn config_defaults_and_enabled() {
        let cfg = OtelConfig::default();
        assert!(!cfg.enabled());
        assert_eq!(cfg.service_name, "tei-manager");
        assert_eq!(cfg.sample_ratio, 1.0);
        let cfg: OtelConfig =
            toml::from_str("endpoint = \"http://c:4317\"\nsample_ratio = 0.25").unwrap();
        assert!(cfg.enabled());
        assert_eq!(cfg.sample_ratio, 0.25);
        assert_eq!(cfg.service_name, "tei-manager");
        let blank: OtelConfig = toml::from_str("endpoint = \"   \"").unwrap();
        assert!(!blank.enabled());
    }

    #[test]
    fn resource_respects_config_not_env() {
        let cfg = OtelConfig {
            deployment_environment: "prod".into(),
            ..Default::default()
        };
        let res = build_resource(&cfg);
        let get = |k: &str| {
            res.iter()
                .find(|(key, _)| key.as_str() == k)
                .map(|(_, v)| v.to_string())
        };
        assert_eq!(get("service.name").as_deref(), Some("tei-manager"));
        assert_eq!(get("deployment.environment").as_deref(), Some("prod"));
        let res = build_resource(&OtelConfig::default());
        assert!(
            res.iter()
                .all(|(k, _)| k.as_str() != "deployment.environment")
        );
    }

    #[test]
    fn provider_builds_without_endpoint() {
        let provider = build_tracer_provider(&OtelConfig::default()).unwrap();
        provider.shutdown().unwrap();
    }

    #[test]
    fn split_grpc_path_parses() {
        assert_eq!(
            split_grpc_path("/tei_multiplexer.v1.TeiMultiplexer/EmbedArrow"),
            (
                "tei_multiplexer.v1.TeiMultiplexer".into(),
                "EmbedArrow".into()
            )
        );
        assert_eq!(split_grpc_path("/"), (String::new(), String::new()));
        assert_eq!(split_grpc_path("weird"), ("weird".into(), String::new()));
    }

    #[test]
    fn metadata_extractor_and_injector_round_trip() {
        let mut md = MetadataMap::new();
        MetadataInjector(&mut md).set("traceparent", TRACEPARENT.to_string());
        assert_eq!(MetadataExtractor(&md).get("traceparent"), Some(TRACEPARENT));
        assert!(MetadataExtractor(&md).keys().contains(&"traceparent"));
        // Invalid keys are dropped, not panicked on
        MetadataInjector(&mut md).set("Bad Key", "x".into());
        assert_eq!(md.len(), 1);
    }

    #[test]
    fn extract_context_reads_w3c_traceparent() {
        ensure_subscriber();
        let mut md = MetadataMap::new();
        md.insert("traceparent", TRACEPARENT.parse().unwrap());
        let cx = extract_context(&MetadataExtractor(&md));
        let sc = cx.span().span_context().clone();
        assert!(sc.is_valid());
        assert_eq!(
            sc.trace_id().to_string(),
            "0af7651916cd43dd8448eb211c80319c"
        );
        assert!(sc.is_remote());
    }

    #[test]
    fn grpc_server_span_is_child_of_caller() {
        ensure_subscriber();
        let req = http::Request::builder()
            .uri("/tei_multiplexer.v1.TeiMultiplexer/EmbedArrow")
            .header("traceparent", TRACEPARENT)
            .body(())
            .unwrap();
        let span = grpc_server_span(&req);
        let sc = span.context().span().span_context().clone();
        assert!(sc.is_valid());
        assert_eq!(
            sc.trace_id().to_string(),
            "0af7651916cd43dd8448eb211c80319c"
        );
        assert_ne!(
            sc.span_id().to_string(),
            "b7ad6b7169203331",
            "server span gets its own id"
        );
    }

    #[test]
    fn interceptor_injects_current_context() {
        use tonic::service::Interceptor;
        ensure_subscriber();
        let req = http::Request::builder()
            .uri("/svc/M")
            .header("traceparent", TRACEPARENT)
            .body(())
            .unwrap();
        let span = grpc_server_span(&req);
        let _guard = span.enter();
        let out = TraceContextInterceptor
            .call(tonic::Request::new(()))
            .unwrap();
        let tp = out.metadata().get("traceparent").unwrap().to_str().unwrap();
        assert!(
            tp.starts_with("00-0af7651916cd43dd8448eb211c80319c-"),
            "{tp}"
        );
        assert!(
            !tp.contains("b7ad6b7169203331"),
            "child span id, not the caller's"
        );
    }

    #[test]
    fn interceptor_without_span_injects_nothing() {
        use tonic::service::Interceptor;
        ensure_subscriber();
        let out = TraceContextInterceptor
            .call(tonic::Request::new(()))
            .unwrap();
        assert!(out.metadata().get("traceparent").is_none());
    }
}
