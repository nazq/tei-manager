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
