//! E2E tests for sparse embeddings using TEI container
//!
//! Run with: `cargo test --test e2e_sparse`
//!
//! Model: naver/splade-cocondenser-selfdistil (~420MB)

mod e2e;

use e2e::common::{SPARSE_MODEL, TeiContainer};
use tokio::sync::OnceCell;

/// Shared TEI container for sparse embedding tests
static SPARSE_TEI: OnceCell<TeiContainer> = OnceCell::const_new();

async fn get_sparse_tei() -> &'static TeiContainer {
    SPARSE_TEI
        .get_or_init(|| async {
            TeiContainer::start_sparse(SPARSE_MODEL)
                .await
                .expect("Failed to start sparse TEI container")
        })
        .await
}

/// Connect to TEI gRPC
async fn create_tei_client(
    endpoint: &str,
) -> tei_manager::grpc::proto::tei::v1::embed_client::EmbedClient<tonic::transport::Channel> {
    let channel = tonic::transport::Channel::from_shared(endpoint.to_string())
        .expect("Invalid endpoint")
        .connect()
        .await
        .expect("Failed to connect");

    tei_manager::grpc::proto::tei::v1::embed_client::EmbedClient::new(channel)
}

#[tokio::test]
async fn test_embed_sparse_single_text() {
    let tei = get_sparse_tei().await;
    let mut client = create_tei_client(&tei.grpc_endpoint()).await;

    let requests = vec![tei_manager::grpc::proto::tei::v1::EmbedSparseRequest {
        inputs: "information retrieval search query".to_string(),
        truncate: true,
        truncation_direction: 0,
        prompt_name: None,
    }];

    let response_stream = client
        .embed_sparse_stream(tokio_stream::iter(requests))
        .await
        .expect("embed_sparse_stream failed");

    let mut responses: Vec<_> = vec![];
    let mut stream = response_stream.into_inner();
    while let Some(result) = tokio_stream::StreamExt::next(&mut stream).await {
        responses.push(result.expect("stream error"));
    }

    assert_eq!(responses.len(), 1);

    // Sparse embeddings should have non-zero values
    let sparse = &responses[0].sparse_embeddings;
    assert!(!sparse.is_empty(), "Expected non-empty sparse embeddings");

    // All values should be positive (SPLADE uses ReLU)
    for sv in sparse {
        assert!(sv.value >= 0.0, "SPLADE values should be non-negative");
    }

    // Indices should be valid vocab indices (typically < 30522 for BERT vocab)
    for sv in sparse {
        assert!(sv.index < 50000, "Index {} seems too large", sv.index);
    }
}

#[tokio::test]
async fn test_embed_sparse_batch() {
    let tei = get_sparse_tei().await;
    let mut client = create_tei_client(&tei.grpc_endpoint()).await;

    let texts = [
        "machine learning deep neural networks",
        "natural language processing transformers",
        "computer vision image recognition",
    ];

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

    assert_eq!(responses.len(), 3);

    // Each response should have sparse embeddings
    for (i, response) in responses.iter().enumerate() {
        assert!(
            !response.sparse_embeddings.is_empty(),
            "Response {} should have sparse embeddings",
            i
        );
    }
}

#[tokio::test]
async fn test_embed_sparse_term_overlap() {
    let tei = get_sparse_tei().await;
    let mut client = create_tei_client(&tei.grpc_endpoint()).await;

    // These texts share common terms, should have overlapping indices
    let texts = ["machine learning algorithms", "machine learning models"];

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

    // Extract indices from both sparse vectors
    let indices_0: std::collections::HashSet<u32> = responses[0]
        .sparse_embeddings
        .iter()
        .map(|sv| sv.index)
        .collect();
    let indices_1: std::collections::HashSet<u32> = responses[1]
        .sparse_embeddings
        .iter()
        .map(|sv| sv.index)
        .collect();

    // Should have some overlap due to shared terms
    let overlap: std::collections::HashSet<_> = indices_0.intersection(&indices_1).collect();
    assert!(
        !overlap.is_empty(),
        "Expected overlapping indices for texts with common terms"
    );
}
