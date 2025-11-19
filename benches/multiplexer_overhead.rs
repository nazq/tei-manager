use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use futures::future::join_all;
use tei_manager::grpc::proto::multiplexer::v1::{
    EmbedRequest as MuxEmbedRequest, Target, target::Routing,
    tei_multiplexer_client::TeiMultiplexerClient,
};
use tei_manager::grpc::proto::tei::v1::{
    EmbedRequest, TruncationDirection, embed_client::EmbedClient,
};
use tokio::runtime::Runtime;

const INSTANCE_ENDPOINT: &str = "http://localhost:8081"; // Direct to managed instance
const MULTIPLEXER_ENDPOINT: &str = "http://localhost:9001";

/// Benchmark direct connection to managed TEI instance (bypassing multiplexer)
async fn benchmark_direct_embed(text: &str) {
    let mut client = EmbedClient::connect(INSTANCE_ENDPOINT)
        .await
        .expect("Failed to connect to instance");

    let request = EmbedRequest {
        inputs: text.to_string(),
        truncate: true,
        normalize: true,
        truncation_direction: TruncationDirection::Right as i32,
        prompt_name: None,
        dimensions: None,
    };

    client.embed(request).await.expect("Direct embed failed");
}

/// Benchmark embedding via multiplexer
async fn benchmark_multiplexer_embed(text: &str, instance_name: &str) {
    let mut client = TeiMultiplexerClient::connect(MULTIPLEXER_ENDPOINT)
        .await
        .expect("Failed to connect to multiplexer");

    let request = MuxEmbedRequest {
        target: Some(Target {
            routing: Some(Routing::InstanceName(instance_name.to_string())),
        }),
        request: Some(EmbedRequest {
            inputs: text.to_string(),
            truncate: true,
            normalize: true,
            truncation_direction: TruncationDirection::Right as i32,
            prompt_name: None,
            dimensions: None,
        }),
    };

    client
        .embed(request)
        .await
        .expect("Multiplexer embed failed");
}

/// Benchmark concurrent requests
async fn benchmark_direct_concurrent(text: &str, concurrency: usize) {
    let tasks: Vec<_> = (0..concurrency)
        .map(|_| benchmark_direct_embed(text))
        .collect();

    join_all(tasks).await;
}

async fn benchmark_multiplexer_concurrent(text: &str, instance_name: &str, concurrency: usize) {
    let tasks: Vec<_> = (0..concurrency)
        .map(|_| benchmark_multiplexer_embed(text, instance_name))
        .collect();

    join_all(tasks).await;
}

/// Benchmark direct streaming embed
async fn benchmark_direct_embed_stream(text: &str, batch_size: usize) {
    use tokio_stream::StreamExt;

    let mut client = EmbedClient::connect(INSTANCE_ENDPOINT)
        .await
        .expect("Failed to connect to instance");

    // Clone to avoid lifetime issues in the iterator closure
    let text_owned = text.to_string();

    // Create stream of requests
    let requests = tokio_stream::iter((0..batch_size).map(move |_| EmbedRequest {
        inputs: text_owned.clone(),
        truncate: true,
        normalize: true,
        truncation_direction: TruncationDirection::Right as i32,
        prompt_name: None,
        dimensions: None,
    }));

    // Send stream and collect responses
    let mut response_stream = client
        .embed_stream(requests)
        .await
        .expect("Direct embed_stream failed")
        .into_inner();

    // Consume all responses
    let mut count = 0;
    while response_stream.next().await.is_some() {
        count += 1;
    }
    assert_eq!(count, batch_size, "Expected {} responses", batch_size);
}

/// Benchmark multiplexer streaming embed
async fn benchmark_multiplexer_embed_stream(text: &str, instance_name: &str, batch_size: usize) {
    use tokio_stream::StreamExt;

    let mut client = TeiMultiplexerClient::connect(MULTIPLEXER_ENDPOINT)
        .await
        .expect("Failed to connect to multiplexer");

    // Clone values to avoid lifetime issues in the iterator closure
    let instance_name_owned = instance_name.to_string();
    let text_owned = text.to_string();

    // Create stream of requests with routing
    let requests = tokio_stream::iter((0..batch_size).map(move |_| MuxEmbedRequest {
        target: Some(Target {
            routing: Some(Routing::InstanceName(instance_name_owned.clone())),
        }),
        request: Some(EmbedRequest {
            inputs: text_owned.clone(),
            truncate: true,
            normalize: true,
            truncation_direction: TruncationDirection::Right as i32,
            prompt_name: None,
            dimensions: None,
        }),
    }));

    // Send stream and collect responses
    let mut response_stream = client
        .embed_stream(requests)
        .await
        .expect("Multiplexer embed_stream failed")
        .into_inner();

    // Consume all responses
    let mut count = 0;
    while response_stream.next().await.is_some() {
        count += 1;
    }
    assert_eq!(count, batch_size, "Expected {} responses", batch_size);
}

fn bench_embedding_overhead(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    // Test with different input sizes
    let long_text = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. ".repeat(10);
    let extra_long_text = "The quick brown fox jumps over the lazy dog. ".repeat(50); // ~2250 chars, near token limit
    let test_cases = vec![
        ("short", "Hello world"),
        (
            "medium",
            "The quick brown fox jumps over the lazy dog. This is a test sentence for benchmarking the embedding performance.",
        ),
        ("long", long_text.as_str()),
        ("extra-long", extra_long_text.as_str()),
    ];

    let mut group = c.benchmark_group("embedding_overhead");

    for (name, text) in &test_cases {
        // Benchmark direct TEI call (new connection each time)
        group.bench_with_input(BenchmarkId::new("direct", name), text, |b, text| {
            b.to_async(&rt)
                .iter(|| benchmark_direct_embed(black_box(text)));
        });

        // Benchmark via multiplexer (new connection each time)
        group.bench_with_input(BenchmarkId::new("multiplexer", name), text, |b, text| {
            b.to_async(&rt)
                .iter(|| benchmark_multiplexer_embed(black_box(text), "bench-instance"));
        });
    }

    group.finish();
}

fn bench_concurrent_requests(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let text = "The quick brown fox jumps over the lazy dog.";
    let concurrency_levels = vec![5, 10, 20];

    let mut group = c.benchmark_group("concurrent_requests");
    // Give more time for concurrent benchmarks
    group.sample_size(10);

    for concurrency in concurrency_levels {
        // Direct concurrent
        group.bench_with_input(
            BenchmarkId::new("direct", concurrency),
            &concurrency,
            |b, &concurrency| {
                b.to_async(&rt)
                    .iter(|| benchmark_direct_concurrent(black_box(text), concurrency));
            },
        );

        // Multiplexer concurrent
        group.bench_with_input(
            BenchmarkId::new("multiplexer", concurrency),
            &concurrency,
            |b, &concurrency| {
                b.to_async(&rt).iter(|| {
                    benchmark_multiplexer_concurrent(black_box(text), "bench-instance", concurrency)
                });
            },
        );
    }

    group.finish();
}

fn bench_streaming_requests(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();
    let text = "The quick brown fox jumps over the lazy dog.";
    let batch_sizes = vec![5, 10, 20];

    let mut group = c.benchmark_group("streaming_requests");
    // Fewer samples for streaming benchmarks since they're slower
    group.sample_size(10);

    for batch_size in batch_sizes {
        // Direct streaming
        group.bench_with_input(
            BenchmarkId::new("direct", batch_size),
            &batch_size,
            |b, &batch_size| {
                b.to_async(&rt)
                    .iter(|| benchmark_direct_embed_stream(black_box(text), batch_size));
            },
        );

        // Multiplexer streaming
        group.bench_with_input(
            BenchmarkId::new("multiplexer", batch_size),
            &batch_size,
            |b, &batch_size| {
                b.to_async(&rt).iter(|| {
                    benchmark_multiplexer_embed_stream(
                        black_box(text),
                        "bench-instance",
                        batch_size,
                    )
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_embedding_overhead,
    bench_concurrent_requests,
    bench_streaming_requests
);
criterion_main!(benches);
