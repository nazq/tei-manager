//! E2E tests for dense embeddings using TEI container
//!
//! Run with: `cargo test --test e2e_dense`
//!
//! Model: BAAI/bge-small-en-v1.5 (~133MB)

mod e2e;

use e2e::common::{DENSE_MODEL, TeiContainer};
use tokio::sync::OnceCell;

/// Shared TEI container for dense embedding tests
static DENSE_TEI: OnceCell<TeiContainer> = OnceCell::const_new();

async fn get_dense_tei() -> &'static TeiContainer {
    DENSE_TEI
        .get_or_init(|| async {
            TeiContainer::start_dense(DENSE_MODEL)
                .await
                .expect("Failed to start dense TEI container")
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
async fn test_embed_single_text() {
    let tei = get_dense_tei().await;
    let mut client = create_tei_client(&tei.grpc_endpoint()).await;

    let requests = vec![tei_manager::grpc::proto::tei::v1::EmbedRequest {
        inputs: "Hello, world!".to_string(),
        truncate: true,
        normalize: true,
        truncation_direction: 0,
        prompt_name: None,
        dimensions: None,
    }];

    let response_stream = client
        .embed_stream(tokio_stream::iter(requests))
        .await
        .expect("embed_stream failed");

    let mut responses: Vec<_> = vec![];
    let mut stream = response_stream.into_inner();
    while let Some(result) = tokio_stream::StreamExt::next(&mut stream).await {
        responses.push(result.expect("stream error"));
    }

    assert_eq!(responses.len(), 1);
    assert!(!responses[0].embeddings.is_empty());

    // bge-small-en-v1.5 has 384 dimensions
    assert_eq!(responses[0].embeddings.len(), 384);
}

#[tokio::test]
async fn test_embed_batch() {
    let tei = get_dense_tei().await;
    let mut client = create_tei_client(&tei.grpc_endpoint()).await;

    let texts = [
        "The quick brown fox jumps over the lazy dog.",
        "Machine learning is transforming industries.",
        "Rust is a systems programming language.",
    ];

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

    let mut responses: Vec<_> = vec![];
    let mut stream = response_stream.into_inner();
    while let Some(result) = tokio_stream::StreamExt::next(&mut stream).await {
        responses.push(result.expect("stream error"));
    }

    assert_eq!(responses.len(), 3);

    // All embeddings should have same dimension
    for response in &responses {
        assert_eq!(response.embeddings.len(), 384);
    }

    // Embeddings should be normalized (magnitude ~= 1.0)
    for response in &responses {
        let magnitude: f32 = response
            .embeddings
            .iter()
            .map(|x| x * x)
            .sum::<f32>()
            .sqrt();
        assert!(
            (magnitude - 1.0).abs() < 0.01,
            "Expected normalized embedding, got magnitude {}",
            magnitude
        );
    }
}

#[tokio::test]
async fn test_embed_similarity() {
    let tei = get_dense_tei().await;
    let mut client = create_tei_client(&tei.grpc_endpoint()).await;

    let texts = [
        "I love programming in Rust",
        "Rust programming is my favorite",
        "The weather is sunny today",
    ];

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

    // Cosine similarity (embeddings are normalized, so dot product = cosine)
    let sim_01: f32 = embeddings[0]
        .iter()
        .zip(&embeddings[1])
        .map(|(a, b)| a * b)
        .sum();
    let sim_02: f32 = embeddings[0]
        .iter()
        .zip(&embeddings[2])
        .map(|(a, b)| a * b)
        .sum();

    // Similar texts should have higher similarity
    assert!(
        sim_01 > sim_02,
        "Expected similar texts to have higher similarity: sim(0,1)={} vs sim(0,2)={}",
        sim_01,
        sim_02
    );
}
