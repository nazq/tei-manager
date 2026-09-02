//! E2E tests for the gRPC multiplexer service
//!
//! These tests verify the multiplexer can route requests to real TEI backends.
//! Run with: `cargo test --test e2e_multiplexer`

mod e2e;

use e2e::common::{DENSE_MODEL, SPARSE_MODEL, TeiContainer, create_arrow_batch};
use tonic::transport::Channel;

use tei_manager::grpc::proto::tei::v1::embed_client::EmbedClient;

// ============================================================================
// EmbedArrow E2E Tests
// ============================================================================

#[tokio::test]
async fn test_embed_arrow_with_real_backend() {
    let tei = TeiContainer::start_dense(DENSE_MODEL)
        .await
        .expect("Failed to start dense TEI container");

    // Test texts
    let texts = &["Hello world", "Testing embeddings", "Rust is great"];

    // Connect directly to TEI and use the embed service
    let channel = Channel::from_shared(tei.grpc_endpoint())
        .unwrap()
        .connect()
        .await
        .expect("Failed to connect to TEI");

    let mut client = EmbedClient::new(channel);

    // Use streaming API
    let requests: Vec<_> = texts
        .iter()
        .map(|text| tei_manager::grpc::proto::tei::v1::EmbedRequest {
            inputs: text.to_string(),
            truncate: true,
            normalize: Some(true),
            truncation_direction: 0,
            prompt_name: None,
            dimensions: None,
        })
        .collect();

    let response_stream = client
        .embed_stream(tokio_stream::iter(requests))
        .await
        .expect("embed_stream failed");

    let mut embeddings: Vec<Vec<f32>> = vec![];
    let mut stream = response_stream.into_inner();
    while let Some(result) = tokio_stream::StreamExt::next(&mut stream).await {
        embeddings.push(result.expect("stream error").embeddings);
    }

    assert_eq!(embeddings.len(), 3);

    // Verify embedding dimensions (bge-small-en-v1.5 = 384)
    for emb in &embeddings {
        assert_eq!(emb.len(), 384);
    }
}

#[tokio::test]
async fn test_embed_sparse_with_real_backend() {
    let tei = TeiContainer::start_sparse(SPARSE_MODEL)
        .await
        .expect("Failed to start sparse TEI container");

    let channel = Channel::from_shared(tei.grpc_endpoint())
        .unwrap()
        .connect()
        .await
        .expect("Failed to connect to TEI");

    let mut client = EmbedClient::new(channel);

    let texts = ["search query", "information retrieval"];

    let requests: Vec<_> = texts
        .iter()
        .map(
            |text| tei_manager::grpc::proto::tei::v1::EmbedSparseRequest {
                inputs: text.to_string(),
                truncate: true,
                truncation_direction: 0,
                prompt_name: None,
            },
        )
        .collect();

    let response_stream = client
        .embed_sparse_stream(tokio_stream::iter(requests))
        .await
        .expect("embed_sparse_stream failed");

    let mut responses: Vec<_> = vec![];
    let mut stream = response_stream.into_inner();
    while let Some(result) = tokio_stream::StreamExt::next(&mut stream).await {
        responses.push(result.expect("stream error"));
    }

    assert_eq!(responses.len(), 2);

    // Verify we got sparse embeddings
    for resp in &responses {
        assert!(!resp.sparse_embeddings.is_empty());
        // All values should be non-negative (SPLADE uses ReLU)
        for sv in &resp.sparse_embeddings {
            assert!(sv.value >= 0.0);
        }
    }
}

// ============================================================================
// Arrow IPC Format Tests
// ============================================================================

#[tokio::test]
async fn test_arrow_ipc_roundtrip() {
    use arrow::array::{Array, StringArray};
    use arrow::ipc::reader::StreamReader;
    use std::io::Cursor;

    let texts = &["text one", "text two", "text three"];
    let arrow_ipc = create_arrow_batch(texts);

    // Verify we can read it back
    let cursor = Cursor::new(&arrow_ipc);
    let reader = StreamReader::try_new(cursor, None).expect("Failed to create reader");

    let batches: Vec<_> = reader
        .collect::<Result<Vec<_>, _>>()
        .expect("Failed to read batches");

    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].num_rows(), 3);

    // Verify text content
    let text_col = batches[0]
        .column(0)
        .as_any()
        .downcast_ref::<StringArray>()
        .expect("Expected StringArray");

    assert_eq!(text_col.value(0), "text one");
    assert_eq!(text_col.value(1), "text two");
    assert_eq!(text_col.value(2), "text three");
}

#[tokio::test]
async fn test_arrow_batch_large() {
    // Test with larger batch to verify compression/handling
    let texts: Vec<&str> = (0..100)
        .map(|i| match i % 5 {
            0 => "The quick brown fox jumps over the lazy dog",
            1 => "Machine learning models process text efficiently",
            2 => "Rust programming language is memory safe",
            3 => "Vector embeddings capture semantic meaning",
            _ => "Natural language processing advances daily",
        })
        .collect();

    let arrow_ipc = create_arrow_batch(&texts);

    // Should be reasonably sized
    assert!(arrow_ipc.len() < 50_000, "IPC should be compressed");
    assert!(arrow_ipc.len() > 100, "IPC should have content");
}

// ============================================================================
// EmbedArrow through the multiplexer (per-row error handling)
// ============================================================================

#[tokio::test]
async fn test_embed_arrow_via_multiplexer_reports_per_row_errors() {
    use arrow::array::{Array, ArrayRef, FixedSizeListArray, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::ipc::reader::StreamReader;
    use arrow::ipc::writer::StreamWriter;
    use arrow::record_batch::RecordBatch;
    use std::io::Cursor;
    use std::sync::Arc;
    use tei_manager::config::InstanceConfig;
    use tei_manager::grpc::multiplexer::TeiMultiplexerService;
    use tei_manager::grpc::pool::BackendPool;
    use tei_manager::grpc::proto::multiplexer::v1 as mux;
    use tei_manager::grpc::proto::multiplexer::v1::tei_multiplexer_server::TeiMultiplexer;
    use tei_manager::registry::Registry;

    let tei = TeiContainer::start_dense(DENSE_MODEL)
        .await
        .expect("Failed to start dense TEI container");

    // Registry entry pointing at the container's mapped gRPC port; the pool
    // connects to 127.0.0.1:<port> exactly like a locally spawned instance.
    let registry = Arc::new(Registry::new(
        None,
        "text-embeddings-router".to_string(),
        8080,
        8180,
    ));
    registry
        .add(InstanceConfig {
            name: "e2e".to_string(),
            model_id: DENSE_MODEL.to_string(),
            port: tei.grpc_port(),
            ..Default::default()
        })
        .await
        .expect("registry add");
    let service = TeiMultiplexerService::new(BackendPool::new(registry), 1024, 60);

    // Row 1: too long for the model with truncate=false → rejected by TEI.
    // Row 2: null → rejected by the multiplexer without touching the backend.
    let too_long = "word ".repeat(3000);
    let texts: Vec<Option<&str>> = vec![Some("Hello world"), Some(&too_long), None, Some("Rust")];
    let schema = Arc::new(Schema::new(vec![Field::new("text", DataType::Utf8, true)]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(StringArray::from(texts)) as ArrayRef],
    )
    .unwrap();
    let mut arrow_ipc = Vec::new();
    {
        let mut w = StreamWriter::try_new(&mut arrow_ipc, &schema).unwrap();
        w.write(&batch).unwrap();
        w.finish().unwrap();
    }

    let response = service
        .embed_arrow(tonic::Request::new(mux::EmbedArrowRequest {
            target: Some(mux::Target {
                routing: Some(mux::target::Routing::InstanceName("e2e".to_string())),
            }),
            arrow_ipc,
            truncate: false,
            normalize: true,
            compression: mux::ArrowCompression::Lz4 as i32,
            ..Default::default()
        }))
        .await
        .expect("embed_arrow")
        .into_inner();

    let mut reader = StreamReader::try_new(Cursor::new(response.arrow_ipc), None).unwrap();
    let out = reader.next().unwrap().unwrap();
    assert_eq!(out.num_rows(), 4, "one output row per input row");

    let emb = out
        .column(0)
        .as_any()
        .downcast_ref::<FixedSizeListArray>()
        .unwrap();
    let err = out
        .column(1)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert_eq!(emb.value_length(), 384);
    for i in 0..4 {
        println!(
            "row {i}: valid={} error={:?}",
            emb.is_valid(i),
            if err.is_null(i) {
                None
            } else {
                Some(err.value(i))
            }
        );
    }

    assert!(emb.is_valid(0) && err.is_null(0));
    assert!(emb.is_null(1), "over-long row must be null");
    assert!(
        !err.value(1).is_empty(),
        "over-long row must carry a reason"
    );
    assert!(emb.is_null(2));
    assert_eq!(err.value(2), "input text is null");
    assert!(
        emb.is_valid(3) && err.is_null(3),
        "rows after a failure still embed"
    );
}

#[tokio::test]
async fn test_embed_arrow_via_multiplexer_f16_output() {
    use arrow::array::{Array, FixedSizeListArray, Float16Array};
    use arrow::datatypes::DataType;
    use arrow::ipc::reader::StreamReader;
    use std::io::Cursor;
    use std::sync::Arc;
    use tei_manager::config::InstanceConfig;
    use tei_manager::grpc::multiplexer::TeiMultiplexerService;
    use tei_manager::grpc::pool::BackendPool;
    use tei_manager::grpc::proto::multiplexer::v1 as mux;
    use tei_manager::grpc::proto::multiplexer::v1::tei_multiplexer_server::TeiMultiplexer;
    use tei_manager::registry::Registry;

    let tei = TeiContainer::start_dense(DENSE_MODEL)
        .await
        .expect("Failed to start dense TEI container");
    let registry = Arc::new(Registry::new(
        None,
        "text-embeddings-router".to_string(),
        8080,
        8180,
    ));
    registry
        .add(InstanceConfig {
            name: "e2e-f16".to_string(),
            model_id: DENSE_MODEL.to_string(),
            port: tei.grpc_port(),
            ..Default::default()
        })
        .await
        .expect("registry add");
    let service = TeiMultiplexerService::new(BackendPool::new(registry), 1024, 60);

    let texts = ["Hello world", "Rust"];
    let embed = |dtype: mux::OutputDtype| {
        let service = service.clone();
        let ipc = create_arrow_batch(&texts);
        async move {
            let response = service
                .embed_arrow(tonic::Request::new(mux::EmbedArrowRequest {
                    target: Some(mux::Target {
                        routing: Some(mux::target::Routing::InstanceName("e2e-f16".to_string())),
                    }),
                    arrow_ipc: ipc,
                    truncate: true,
                    normalize: true,
                    output_dtype: dtype as i32,
                    ..Default::default()
                }))
                .await
                .expect("embed_arrow")
                .into_inner();
            let mut reader = StreamReader::try_new(Cursor::new(response.arrow_ipc), None).unwrap();
            reader.next().unwrap().unwrap()
        }
    };

    let f32_batch = embed(mux::OutputDtype::F32).await;
    let f16_batch = embed(mux::OutputDtype::F16).await;

    let f32_col = f32_batch
        .column(0)
        .as_any()
        .downcast_ref::<FixedSizeListArray>()
        .unwrap();
    let f16_col = f16_batch
        .column(0)
        .as_any()
        .downcast_ref::<FixedSizeListArray>()
        .unwrap();
    assert_eq!(f32_col.value_type(), DataType::Float32);
    assert_eq!(f16_col.value_type(), DataType::Float16);
    assert_eq!(f16_col.value_length(), 384);
    assert_eq!(f16_batch.num_rows(), 2);

    // f16 is a faithful rounding of the f32 result (max abs error for |x|<=1 is ~5e-4)
    let f32_row = f32_col.value(0);
    let f32_row = f32_row
        .as_any()
        .downcast_ref::<arrow::array::Float32Array>()
        .unwrap();
    let f16_row = f16_col.value(0);
    let f16_row = f16_row.as_any().downcast_ref::<Float16Array>().unwrap();
    for i in 0..384 {
        assert!((f32_row.value(i) - f16_row.value(i).to_f32()).abs() < 1e-3);
    }
}

#[tokio::test]
async fn test_embed_arrow_stream_via_multiplexer_ordered_with_per_row_errors() {
    use arrow::array::{Array, FixedSizeListArray, StringArray};
    use arrow::ipc::reader::StreamReader;
    use std::io::Cursor;
    use std::sync::Arc;
    use tei_manager::config::InstanceConfig;
    use tei_manager::grpc::multiplexer::TeiMultiplexerService;
    use tei_manager::grpc::pool::BackendPool;
    use tei_manager::grpc::proto::multiplexer::v1 as mux;
    use tei_manager::registry::Registry;

    let tei = TeiContainer::start_dense(DENSE_MODEL)
        .await
        .expect("Failed to start dense TEI container");

    let registry = Arc::new(Registry::new(
        None,
        "text-embeddings-router".to_string(),
        8080,
        8180,
    ));
    registry
        .add(InstanceConfig {
            name: "e2e-stream".to_string(),
            model_id: DENSE_MODEL.to_string(),
            port: tei.grpc_port(),
            ..Default::default()
        })
        .await
        .expect("registry add");
    let service = TeiMultiplexerService::new(BackendPool::new(registry), 1024, 60);

    // The first request fixes target and options (truncate=false). Batch 2
    // contains an over-long row that TEI must reject per-row; it also sets
    // truncate=true, which must be IGNORED — otherwise the row would embed.
    let too_long = "word ".repeat(3000);
    let first = mux::EmbedArrowRequest {
        target: Some(mux::Target {
            routing: Some(mux::target::Routing::InstanceName("e2e-stream".to_string())),
        }),
        arrow_ipc: create_arrow_batch(&["Hello world", "Rust is great"]),
        truncate: false,
        normalize: true,
        ..Default::default()
    };
    let second = mux::EmbedArrowRequest {
        arrow_ipc: create_arrow_batch(&["a fine row", &too_long]),
        truncate: true, // ignored: options come from the first request
        ..Default::default()
    };
    let third = mux::EmbedArrowRequest {
        arrow_ipc: create_arrow_batch(&["later batches still embed"]),
        ..Default::default()
    };

    let stream = service
        .embed_arrow_stream_core(tokio_stream::iter(vec![
            Ok::<_, tonic::Status>(first),
            Ok(second),
            Ok(third),
        ]))
        .await
        .expect("embed_arrow_stream");

    let mut batches = Vec::new();
    let mut stream = stream;
    while let Some(result) = tokio_stream::StreamExt::next(&mut stream).await {
        let response = result.expect("stream error");
        let mut reader = StreamReader::try_new(Cursor::new(response.arrow_ipc), None).unwrap();
        batches.push(reader.next().unwrap().unwrap());
    }

    assert_eq!(batches.len(), 3, "one response per request batch, in order");
    assert_eq!(batches[0].num_rows(), 2);
    assert_eq!(batches[1].num_rows(), 2);
    assert_eq!(batches[2].num_rows(), 1);

    for batch in &batches {
        let emb = batch
            .column(0)
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .unwrap();
        assert_eq!(emb.value_length(), 384);
    }

    // Batch 1: all rows embed
    assert_eq!(batches[0].column(0).null_count(), 0);

    // Batch 2: row 0 embeds, row 1 (over-long, truncate=false) carries a
    // per-row error without failing the batch or the stream
    let emb = batches[1]
        .column(0)
        .as_any()
        .downcast_ref::<FixedSizeListArray>()
        .unwrap();
    let err = batches[1]
        .column(1)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert!(emb.is_valid(0) && err.is_null(0));
    assert!(emb.is_null(1), "over-long row must be null");
    assert!(
        !err.value(1).is_empty(),
        "over-long row must carry a reason"
    );

    // Batch 3: later batches still embed after a per-row failure
    assert_eq!(batches[2].column(0).null_count(), 0);
}

// ============================================================================
// EmbedArrowStream concurrency and resume E2E tests
// ============================================================================

/// Registry with instances pointing at real TEI containers, all marked
/// Running so `model_id` routing sees them (in production the health monitor
/// maintains that status; it is not running in these tests).
async fn stream_registry(
    entries: &[(&str, u16)],
    model: &str,
) -> std::sync::Arc<tei_manager::registry::Registry> {
    use tei_manager::config::InstanceConfig;
    let registry = std::sync::Arc::new(tei_manager::registry::Registry::new(
        None,
        "text-embeddings-router".to_string(),
        8080,
        8180,
    ));
    for (name, port) in entries {
        registry
            .add(InstanceConfig {
                name: name.to_string(),
                model_id: model.to_string(),
                port: *port,
                ..Default::default()
            })
            .await
            .expect("registry add");
        *registry.get(name).await.unwrap().status.write().await =
            tei_manager::instance::InstanceStatus::Running;
    }
    registry
}

/// Drive a full EmbedArrowStream call and decode each Ok response.
async fn collect_stream_batches(
    service: &tei_manager::grpc::multiplexer::TeiMultiplexerService,
    requests: Vec<
        Result<tei_manager::grpc::proto::multiplexer::v1::EmbedArrowRequest, tonic::Status>,
    >,
) -> Vec<Result<arrow::record_batch::RecordBatch, tonic::Status>> {
    use arrow::ipc::reader::StreamReader;
    use std::io::Cursor;

    let mut stream = service
        .embed_arrow_stream_core(tokio_stream::iter(requests))
        .await
        .expect("embed_arrow_stream");
    let mut out = Vec::new();
    while let Some(result) = tokio_stream::StreamExt::next(&mut stream).await {
        out.push(result.map(|response| {
            let mut reader = StreamReader::try_new(Cursor::new(response.arrow_ipc), None).unwrap();
            reader.next().unwrap().unwrap()
        }));
    }
    out
}

/// Prometheus recorder for e2e assertions on per-instance metrics; installed
/// once per test process (the `metrics` crate allows a single global
/// recorder).
fn e2e_prometheus_handle() -> metrics_exporter_prometheus::PrometheusHandle {
    use metrics_exporter_prometheus::PrometheusBuilder;
    use std::sync::OnceLock;
    static HANDLE: OnceLock<metrics_exporter_prometheus::PrometheusHandle> = OnceLock::new();
    HANDLE
        .get_or_init(|| {
            PrometheusBuilder::new()
                .install_recorder()
                .expect("install recorder")
        })
        .clone()
}

/// Value of the first sample of `name` carrying all `labels` (0 if absent).
fn metric_value(rendered: &str, name: &str, labels: &[&str]) -> u64 {
    rendered
        .lines()
        .find(|line| line.starts_with(name) && labels.iter().all(|l| line.contains(l)))
        .and_then(|line| line.rsplit(' ').next())
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

#[tokio::test]
async fn test_embed_arrow_stream_k4_matches_k1_with_real_backend() {
    use arrow::array::{Array, FixedSizeListArray, Float32Array};
    use tei_manager::grpc::multiplexer::TeiMultiplexerService;
    use tei_manager::grpc::pool::BackendPool;
    use tei_manager::grpc::proto::multiplexer::v1 as mux;

    let tei = TeiContainer::start_dense(DENSE_MODEL)
        .await
        .expect("Failed to start dense TEI container");
    let registry = stream_registry(&[("e2e-k", tei.grpc_port())], DENSE_MODEL).await;
    let base = TeiMultiplexerService::new(BackendPool::new(registry), 1024, 60);
    let k1 = base.clone().with_stream_max_concurrent_batches(1);
    let k4 = base.with_stream_max_concurrent_batches(4);

    let batch_texts: Vec<Vec<&str>> = vec![
        vec!["Hello world", "Rust is great"],
        vec!["Machine learning models process text"],
        vec![
            "Vector embeddings capture meaning",
            "Quick brown fox",
            "Lazy dog",
        ],
        vec!["Streams keep order"],
        vec!["Concurrency is bounded", "Backpressure works"],
        vec!["The final batch"],
    ];
    let requests = || -> Vec<Result<mux::EmbedArrowRequest, tonic::Status>> {
        batch_texts
            .iter()
            .enumerate()
            .map(|(i, texts)| {
                Ok(mux::EmbedArrowRequest {
                    target: (i == 0).then(|| mux::Target {
                        routing: Some(mux::target::Routing::InstanceName("e2e-k".to_string())),
                    }),
                    arrow_ipc: create_arrow_batch(texts),
                    truncate: true,
                    normalize: true,
                    ..Default::default()
                })
            })
            .collect()
    };

    let k1_batches = collect_stream_batches(&k1, requests()).await;
    let k4_batches = collect_stream_batches(&k4, requests()).await;
    assert_eq!(k1_batches.len(), 6, "K=1: one response per batch");
    assert_eq!(k4_batches.len(), 6, "K=4: one response per batch");

    for (i, (a, b)) in k1_batches.iter().zip(&k4_batches).enumerate() {
        let a = a.as_ref().expect("K=1 batch ok");
        let b = b.as_ref().expect("K=4 batch ok");
        assert_eq!(a.num_rows(), batch_texts[i].len(), "batch {i} row count");
        assert_eq!(a.num_rows(), b.num_rows());
        assert_eq!(a.column(0).null_count(), 0, "batch {i}: all rows embed");
        assert_eq!(b.column(0).null_count(), 0);
        let av = a
            .column(0)
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .unwrap();
        let bv = b
            .column(0)
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .unwrap();
        assert_eq!(av.value_length(), 384);
        assert_eq!(bv.value_length(), 384);
        let af = av.values().as_any().downcast_ref::<Float32Array>().unwrap();
        let bf = bv.values().as_any().downcast_ref::<Float32Array>().unwrap();
        assert_eq!(af.len(), bf.len());
        // Same rows in the same order. Tolerance instead of bit-equality:
        // concurrent submission can change TEI's internal dynamic batching,
        // which may perturb float accumulation order.
        for j in 0..af.len() {
            assert!(
                (af.value(j) - bf.value(j)).abs() < 1e-3,
                "batch {i} value {j}: {} vs {}",
                af.value(j),
                bf.value(j)
            );
        }
    }
}

#[tokio::test]
async fn test_embed_arrow_stream_model_target_spreads_across_backends() {
    use tei_manager::grpc::multiplexer::TeiMultiplexerService;
    use tei_manager::grpc::pool::BackendPool;
    use tei_manager::grpc::proto::multiplexer::v1 as mux;

    let handle = e2e_prometheus_handle();
    let tei_a = TeiContainer::start_dense(DENSE_MODEL)
        .await
        .expect("Failed to start first TEI container");
    let tei_b = TeiContainer::start_dense(DENSE_MODEL)
        .await
        .expect("Failed to start second TEI container");
    let registry = stream_registry(
        &[
            ("spread-a", tei_a.grpc_port()),
            ("spread-b", tei_b.grpc_port()),
        ],
        DENSE_MODEL,
    )
    .await;
    let service = TeiMultiplexerService::new(BackendPool::new(registry), 1024, 60)
        .with_stream_max_concurrent_batches(4);

    // Six single-row batches; per-batch round-robin over {spread-a, spread-b}
    // must send exactly three rows to each instance.
    let requests: Vec<Result<mux::EmbedArrowRequest, tonic::Status>> = (0..6)
        .map(|i| {
            Ok(mux::EmbedArrowRequest {
                target: (i == 0).then(|| mux::Target {
                    routing: Some(mux::target::Routing::ModelId(DENSE_MODEL.to_string())),
                }),
                arrow_ipc: create_arrow_batch(&["spread me"]),
                truncate: true,
                normalize: true,
                ..Default::default()
            })
        })
        .collect();
    let batches = collect_stream_batches(&service, requests).await;
    assert_eq!(batches.len(), 6);
    for batch in &batches {
        let batch = batch.as_ref().expect("batch ok");
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(batch.column(0).null_count(), 0);
    }

    let rendered = handle.render();
    let a_rows = metric_value(
        &rendered,
        "tei_mux_rows_total",
        &[r#"instance="spread-a""#, r#"status="ok""#],
    );
    let b_rows = metric_value(
        &rendered,
        "tei_mux_rows_total",
        &[r#"instance="spread-b""#, r#"status="ok""#],
    );
    assert_eq!(a_rows, 3, "round-robin sends half the batches to spread-a");
    assert_eq!(b_rows, 3, "round-robin sends half the batches to spread-b");
}

#[tokio::test]
async fn test_embed_arrow_stream_concurrent_per_row_errors() {
    use arrow::array::{Array, FixedSizeListArray, StringArray};
    use tei_manager::grpc::multiplexer::TeiMultiplexerService;
    use tei_manager::grpc::pool::BackendPool;
    use tei_manager::grpc::proto::multiplexer::v1 as mux;

    let tei = TeiContainer::start_dense(DENSE_MODEL)
        .await
        .expect("Failed to start dense TEI container");
    let registry = stream_registry(&[("e2e-conc", tei.grpc_port())], DENSE_MODEL).await;
    let service = TeiMultiplexerService::new(BackendPool::new(registry), 1024, 60);

    // truncate=false is pinned by the first request, which also exercises the
    // caller-side K override (max_concurrent_batches = 4). Batch 2 carries an
    // over-long row that TEI must reject per-row without failing the stream.
    let too_long = "word ".repeat(3000);
    let batch_texts: Vec<Vec<&str>> = vec![
        vec!["Hello world", "Rust is great"],
        vec!["a fine batch"],
        vec!["ok row", &too_long],
        vec!["later batches"],
        vec!["still embed", "under concurrency"],
        vec!["the end"],
    ];
    let requests: Vec<Result<mux::EmbedArrowRequest, tonic::Status>> = batch_texts
        .iter()
        .enumerate()
        .map(|(i, texts)| {
            Ok(mux::EmbedArrowRequest {
                target: (i == 0).then(|| mux::Target {
                    routing: Some(mux::target::Routing::InstanceName("e2e-conc".to_string())),
                }),
                arrow_ipc: create_arrow_batch(texts),
                truncate: false,
                normalize: true,
                max_concurrent_batches: 4,
                ..Default::default()
            })
        })
        .collect();

    let batches = collect_stream_batches(&service, requests).await;
    assert_eq!(batches.len(), 6, "one response per batch, in order");
    for (i, batch) in batches.iter().enumerate() {
        let batch = batch.as_ref().expect("batch ok");
        assert_eq!(batch.num_rows(), batch_texts[i].len(), "batch {i}");
        if i != 2 {
            assert_eq!(batch.column(0).null_count(), 0, "batch {i}: all rows ok");
        }
    }
    // Batch 2: row 0 embeds, row 1 carries the per-row error.
    let batch2 = batches[2].as_ref().unwrap();
    let emb = batch2
        .column(0)
        .as_any()
        .downcast_ref::<FixedSizeListArray>()
        .unwrap();
    let err = batch2
        .column(1)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert!(emb.is_valid(0) && err.is_null(0));
    assert!(emb.is_null(1), "over-long row must be null");
    assert!(!err.value(1).is_empty(), "over-long row carries a reason");
}

#[tokio::test]
async fn test_embed_arrow_stream_resumes_after_backend_death() {
    use tei_manager::grpc::multiplexer::TeiMultiplexerService;
    use tei_manager::grpc::pool::BackendPool;
    use tei_manager::grpc::proto::multiplexer::v1 as mux;
    use tokio_stream::wrappers::ReceiverStream;

    let tei_a = TeiContainer::start_dense(DENSE_MODEL)
        .await
        .expect("Failed to start first TEI container");
    let tei_b = TeiContainer::start_dense(DENSE_MODEL)
        .await
        .expect("Failed to start second TEI container");
    let registry = stream_registry(
        &[("res-a", tei_a.grpc_port()), ("res-b", tei_b.grpc_port())],
        DENSE_MODEL,
    )
    .await;
    // Short per-batch timeout so a batch hung on the killed backend surfaces
    // as DeadlineExceeded instead of stalling the test.
    let service = TeiMultiplexerService::new(BackendPool::new(registry.clone()), 1024, 15)
        .with_stream_max_concurrent_batches(2);

    const TOTAL: usize = 10;
    let request = |i: usize, with_target: bool| -> mux::EmbedArrowRequest {
        let texts = [
            format!("resume batch {i} row 0"),
            format!("resume batch {i} row 1"),
        ];
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        mux::EmbedArrowRequest {
            target: with_target.then(|| mux::Target {
                routing: Some(mux::target::Routing::ModelId(DENSE_MODEL.to_string())),
            }),
            arrow_ipc: create_arrow_batch(&refs),
            truncate: true,
            normalize: true,
            ..Default::default()
        }
    };

    // Stream 1: warm up two batches, then kill one backend mid-stream.
    let (req_tx, req_rx) =
        tokio::sync::mpsc::channel::<Result<mux::EmbedArrowRequest, tonic::Status>>(16);
    // Send the first requests BEFORE opening the stream: the handler consumes
    // request 1 to establish target/options before returning responses.
    req_tx.send(Ok(request(0, true))).await.unwrap();
    req_tx.send(Ok(request(1, false))).await.unwrap();
    let mut responses = service
        .embed_arrow_stream_core(ReceiverStream::new(req_rx))
        .await
        .expect("open stream 1");
    let mut delivered = 0usize;
    for _ in 0..2 {
        tokio_stream::StreamExt::next(&mut responses)
            .await
            .expect("response before the kill")
            .expect("healthy batch");
        delivered += 1;
    }

    tei_b.stop_now().await.expect("stop container");

    // Send everything else; per-batch round-robin guarantees a batch hits the
    // dead backend within two admissions. Read until the failure surfaces.
    for i in 2..TOTAL {
        let _ = req_tx.send(Ok(request(i, false))).await;
    }
    drop(req_tx);
    let mut failure: Option<tonic::Status> = None;
    while let Some(result) = tokio_stream::StreamExt::next(&mut responses).await {
        match result {
            Ok(_) => {
                delivered += 1;
            }
            Err(status) => {
                failure = Some(status);
                break;
            }
        }
    }
    let failure = failure.expect("a mid-stream failure must surface");
    assert!(
        delivered < TOTAL,
        "the stream cannot finish on a dead backend"
    );
    println!(
        "stream 1 delivered {delivered} batches, then failed: {} ({})",
        failure.code(),
        failure.message()
    );
    assert!(
        tokio_stream::StreamExt::next(&mut responses)
            .await
            .is_none(),
        "the failure is terminal"
    );

    // Stand in for the health monitor (not running here): mark the dead
    // instance non-Running so model routing skips it.
    *registry.get("res-b").await.unwrap().status.write().await =
        tei_manager::instance::InstanceStatus::Failed;

    // Resume contract: `delivered` responses received => batches
    // 0..delivered-1 are durable. Reopen and resend from batch `delivered`;
    // later batches may have partially executed and were discarded — the
    // resend is safe.
    let resume_requests: Vec<Result<mux::EmbedArrowRequest, tonic::Status>> = (delivered..TOTAL)
        .map(|i| Ok(request(i, i == delivered)))
        .collect();
    let resumed = collect_stream_batches(&service, resume_requests).await;
    assert_eq!(
        resumed.len(),
        TOTAL - delivered,
        "the resumed stream completes the job"
    );
    for (offset, batch) in resumed.iter().enumerate() {
        let batch = batch
            .as_ref()
            .unwrap_or_else(|e| panic!("resumed batch {} failed: {e}", delivered + offset));
        assert_eq!(batch.num_rows(), 2);
        assert_eq!(batch.column(0).null_count(), 0);
    }
}
