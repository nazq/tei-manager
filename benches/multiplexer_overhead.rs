//! Multiplexer overhead benchmarks using testcontainers
//!
//! These benchmarks measure the overhead of routing requests through the
//! multiplexer vs direct TEI connections. They use testcontainers to spin up
//! a real TEI instance.
//!
//! Run with: `cargo bench --bench multiplexer_overhead`

use arrow::array::StringArray;
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use futures::future::join_all;
use std::borrow::Cow;
use std::hint::black_box;
use std::sync::Arc;
use tei_manager::grpc::proto::tei::v1::{
    EmbedRequest, TruncationDirection, embed_client::EmbedClient,
};
use testcontainers::core::{ContainerPort, Mount, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, Image};
use tokio::runtime::Runtime;
use tokio::sync::OnceCell;

/// TEI Docker image configuration for benchmarks
#[derive(Debug, Clone)]
struct TeiImage {
    model_id: String,
}

impl TeiImage {
    fn new(model_id: &str) -> Self {
        Self {
            model_id: model_id.to_string(),
        }
    }
}

impl Image for TeiImage {
    fn name(&self) -> &str {
        "ghcr.io/huggingface/text-embeddings-inference"
    }

    fn tag(&self) -> &str {
        "cpu-1.8.3-grpc"
    }

    fn ready_conditions(&self) -> Vec<WaitFor> {
        vec![WaitFor::message_on_stdout("\"message\":\"Ready\"")]
    }

    fn env_vars(
        &self,
    ) -> impl IntoIterator<Item = (impl Into<Cow<'_, str>>, impl Into<Cow<'_, str>>)> {
        vec![("MODEL_ID".to_string(), self.model_id.clone())]
    }

    fn expose_ports(&self) -> &[ContainerPort] {
        &[ContainerPort::Tcp(80)]
    }

    fn mounts(&self) -> impl IntoIterator<Item = &Mount> {
        static MOUNTS: std::sync::OnceLock<Vec<Mount>> = std::sync::OnceLock::new();
        MOUNTS.get_or_init(|| {
            let hf_cache = std::env::var("HF_HOME")
                .or_else(|_| std::env::var("HUGGINGFACE_HUB_CACHE"))
                .unwrap_or_else(|_| {
                    std::env::var("HOME")
                        .map(|h| format!("{}/.cache/huggingface/hub", h))
                        .unwrap_or_default()
                });

            if !hf_cache.is_empty() && std::path::Path::new(&hf_cache).exists() {
                vec![Mount::bind_mount(hf_cache, "/data")]
            } else {
                vec![]
            }
        })
    }
}

/// Container handle for benchmarks
struct BenchContainer {
    #[allow(dead_code)]
    container: ContainerAsync<TeiImage>,
    endpoint: String,
}

impl BenchContainer {
    async fn start() -> Self {
        use tokio::time::{Duration, sleep};
        use tonic::transport::Channel;

        println!("Starting TEI container for benchmarks...");

        let image = TeiImage::new("BAAI/bge-small-en-v1.5");
        let container = image.start().await.expect("Failed to start container");
        let port = container
            .get_host_port_ipv4(80)
            .await
            .expect("Failed to get port");
        let endpoint = format!("http://127.0.0.1:{}", port);

        // Wait for gRPC to be ready
        for i in 0..60 {
            match Channel::from_shared(endpoint.clone())
                .unwrap()
                .connect()
                .await
            {
                Ok(_) => {
                    println!("TEI container ready on port {} ({}ms)", port, i * 100);
                    return Self {
                        container,
                        endpoint,
                    };
                }
                Err(_) => {
                    sleep(Duration::from_millis(100)).await;
                }
            }
        }

        panic!("TEI container failed to become ready");
    }
}

/// Global container instance (started once, reused across benchmarks)
static CONTAINER: OnceCell<BenchContainer> = OnceCell::const_new();

async fn get_container() -> &'static BenchContainer {
    CONTAINER
        .get_or_init(|| async { BenchContainer::start().await })
        .await
}

/// Benchmark direct TEI embed call
async fn benchmark_direct_embed(endpoint: &str, text: &str) {
    let mut client = EmbedClient::connect(endpoint.to_string())
        .await
        .expect("Failed to connect");

    let request = EmbedRequest {
        inputs: text.to_string(),
        truncate: true,
        normalize: true,
        truncation_direction: TruncationDirection::Right as i32,
        prompt_name: None,
        dimensions: None,
    };

    client.embed(request).await.expect("Embed failed");
}

/// Benchmark concurrent requests
async fn benchmark_concurrent(endpoint: &str, text: &str, concurrency: usize) {
    let tasks: Vec<_> = (0..concurrency)
        .map(|_| benchmark_direct_embed(endpoint, text))
        .collect();

    join_all(tasks).await;
}

/// Benchmark streaming embed
async fn benchmark_embed_stream(endpoint: &str, text: &str, batch_size: usize) {
    use tokio_stream::StreamExt;

    let mut client = EmbedClient::connect(endpoint.to_string())
        .await
        .expect("Failed to connect");

    let text_owned = text.to_string();
    let requests = tokio_stream::iter((0..batch_size).map(move |_| EmbedRequest {
        inputs: text_owned.clone(),
        truncate: true,
        normalize: true,
        truncation_direction: TruncationDirection::Right as i32,
        prompt_name: None,
        dimensions: None,
    }));

    let mut response_stream = client
        .embed_stream(requests)
        .await
        .expect("embed_stream failed")
        .into_inner();

    let mut count = 0;
    while response_stream.next().await.is_some() {
        count += 1;
    }
    assert_eq!(count, batch_size, "Expected {} responses", batch_size);
}

/// Create Arrow IPC batch from texts
fn create_arrow_batch(texts: &[&str]) -> Vec<u8> {
    let schema = Arc::new(Schema::new(vec![Field::new("text", DataType::Utf8, false)]));
    let array = StringArray::from(texts.to_vec());
    let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(array)]).unwrap();

    let mut buffer = Vec::new();
    {
        let mut writer = StreamWriter::try_new(&mut buffer, &schema).unwrap();
        writer.write(&batch).unwrap();
        writer.finish().unwrap();
    }
    buffer
}

fn bench_embedding_latency(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    // Start container once
    let container = rt.block_on(get_container());
    let endpoint = container.endpoint.clone();

    let long_text = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. ".repeat(10);
    let test_cases = vec![
        ("short", "Hello world"),
        (
            "medium",
            "The quick brown fox jumps over the lazy dog. This is a test sentence for benchmarking.",
        ),
        ("long", long_text.as_str()),
    ];

    let mut group = c.benchmark_group("tei_latency");

    for (name, text) in &test_cases {
        group.bench_with_input(BenchmarkId::new("embed", name), text, |b, text| {
            b.to_async(&rt)
                .iter(|| benchmark_direct_embed(black_box(&endpoint), black_box(text)));
        });
    }

    group.finish();
}

fn bench_concurrent_requests(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let container = rt.block_on(get_container());
    let endpoint = container.endpoint.clone();
    let text = "The quick brown fox jumps over the lazy dog.";
    let concurrency_levels = vec![2, 5, 10];

    let mut group = c.benchmark_group("tei_concurrent");
    group.sample_size(10);

    for concurrency in concurrency_levels {
        group.bench_with_input(
            BenchmarkId::new("requests", concurrency),
            &concurrency,
            |b, &concurrency| {
                b.to_async(&rt).iter(|| {
                    benchmark_concurrent(black_box(&endpoint), black_box(text), concurrency)
                });
            },
        );
    }

    group.finish();
}

fn bench_streaming_requests(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let container = rt.block_on(get_container());
    let endpoint = container.endpoint.clone();
    let text = "The quick brown fox jumps over the lazy dog.";
    let batch_sizes = vec![5, 10, 20];

    let mut group = c.benchmark_group("tei_streaming");
    group.sample_size(10);

    for batch_size in batch_sizes {
        group.bench_with_input(
            BenchmarkId::new("batch", batch_size),
            &batch_size,
            |b, &batch_size| {
                b.to_async(&rt).iter(|| {
                    benchmark_embed_stream(black_box(&endpoint), black_box(text), batch_size)
                });
            },
        );
    }

    group.finish();
}

fn bench_arrow_batch(c: &mut Criterion) {
    let rt = Runtime::new().unwrap();

    let container = rt.block_on(get_container());
    let _endpoint = container.endpoint.clone();
    let text = "The quick brown fox jumps over the lazy dog.";

    let batch_sizes = vec![1, 10, 50];

    let mut group = c.benchmark_group("arrow_batch_creation");
    group.sample_size(100);

    for batch_size in batch_sizes {
        let texts: Vec<&str> = vec![text; batch_size];

        // Benchmark Arrow batch creation (no network)
        group.bench_with_input(
            BenchmarkId::new("create", batch_size),
            &texts,
            |b, texts| {
                b.iter(|| create_arrow_batch(black_box(texts)));
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_embedding_latency,
    bench_concurrent_requests,
    bench_streaming_requests,
    bench_arrow_batch
);
criterion_main!(benches);
