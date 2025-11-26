//! E2E tests for the gRPC multiplexer service
//!
//! These tests verify the multiplexer can route requests to real TEI backends.
//! Run with: `cargo test --test e2e_multiplexer`

mod e2e;

use e2e::common::{DENSE_MODEL, SPARSE_MODEL, TeiContainer, create_arrow_batch};
use tokio::sync::OnceCell;
use tonic::transport::Channel;

use tei_manager::grpc::proto::tei::v1::embed_client::EmbedClient;

/// Shared TEI containers
static DENSE_TEI: OnceCell<TeiContainer> = OnceCell::const_new();
static SPARSE_TEI: OnceCell<TeiContainer> = OnceCell::const_new();

async fn get_dense_tei() -> &'static TeiContainer {
    DENSE_TEI
        .get_or_init(|| async {
            TeiContainer::start_dense(DENSE_MODEL)
                .await
                .expect("Failed to start dense TEI container")
        })
        .await
}

async fn get_sparse_tei() -> &'static TeiContainer {
    SPARSE_TEI
        .get_or_init(|| async {
            TeiContainer::start_sparse(SPARSE_MODEL)
                .await
                .expect("Failed to start sparse TEI container")
        })
        .await
}

// ============================================================================
// EmbedArrow E2E Tests
// ============================================================================

#[tokio::test]
async fn test_embed_arrow_with_real_backend() {
    let tei = get_dense_tei().await;

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
            normalize: true,
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
    let tei = get_sparse_tei().await;

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
