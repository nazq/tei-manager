use anyhow::{Context, Result};
use arrow::array::{ArrayRef, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::reader::StreamReader;
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Semaphore;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity};

use tei_manager::grpc::proto::multiplexer::v1::{
    EmbedArrowRequest, EmbedRequest, Target, tei_multiplexer_client::TeiMultiplexerClient,
};
use tei_manager::grpc::proto::tei::v1 as tei;

#[derive(Debug, Clone, ValueEnum)]
enum BenchMode {
    /// Single text per request with concurrent execution
    Standard,
    /// Batched Arrow IPC format
    Arrow,
}

#[derive(Parser, Debug)]
#[clap(
    name = "tei-bench-client",
    about = "Benchmark TEI embeddings via gRPC (standard or Arrow mode)"
)]
struct Args {
    /// gRPC endpoint (e.g., http://host:port or https://host:port)
    #[clap(short, long)]
    endpoint: String,

    /// Instance name to target
    #[clap(short, long)]
    instance: String,

    /// Benchmark mode
    #[clap(short, long, value_enum, default_value = "standard")]
    mode: BenchMode,

    /// Number of texts to embed
    #[clap(short, long, default_value = "10000")]
    num_texts: usize,

    /// Batch size (concurrent requests for standard, texts per request for arrow)
    #[clap(short, long, default_value = "100")]
    batch_size: usize,

    /// Client certificate path (for mTLS)
    #[clap(long)]
    cert: Option<PathBuf>,

    /// Client key path (for mTLS)
    #[clap(long)]
    key: Option<PathBuf>,

    /// CA certificate path (optional)
    #[clap(long)]
    ca: Option<PathBuf>,

    /// Skip TLS certificate verification (use localhost as domain)
    #[clap(long)]
    insecure: bool,

    /// Noop mode: return dummy embeddings for round-trip testing (Arrow mode only)
    #[clap(long)]
    noop: bool,

    /// Max message size in MB (default: 100, Arrow mode only)
    #[clap(long, default_value = "100")]
    max_message_size_mb: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct BenchmarkResult {
    mode: String,
    instance_name: String,
    num_texts: usize,
    batch_size: usize,
    num_requests: usize,
    total_duration_secs: f64,
    throughput_per_sec: f64,
    successful: usize,
    failed: usize,
}

async fn build_channel(args: &Args) -> Result<Channel> {
    let use_tls = args.endpoint.starts_with("https://");

    let mut channel_builder =
        Channel::from_shared(args.endpoint.clone()).context("Invalid endpoint")?;

    if use_tls {
        let mut tls_config = ClientTlsConfig::new();

        // Configure mTLS if both cert and key are provided
        match (&args.cert, &args.key) {
            (Some(cert_path), Some(key_path)) => {
                let cert = tokio::fs::read(cert_path)
                    .await
                    .with_context(|| format!("Failed to read cert: {:?}", cert_path))?;
                let key = tokio::fs::read(key_path)
                    .await
                    .with_context(|| format!("Failed to read key: {:?}", key_path))?;
                let identity = Identity::from_pem(cert, key);
                tls_config = tls_config.identity(identity);
                eprintln!("Using mTLS with client certificate");
            }
            (Some(_), None) | (None, Some(_)) => {
                anyhow::bail!("Both --cert and --key must be provided for mTLS");
            }
            (None, None) => {
                eprintln!("Using server-side TLS only (no client certificate)");
            }
        }

        // Add CA certificate if provided
        if let Some(ca_path) = &args.ca {
            let ca = tokio::fs::read(ca_path)
                .await
                .with_context(|| format!("Failed to read CA: {:?}", ca_path))?;
            tls_config = tls_config.ca_certificate(Certificate::from_pem(ca));
        }

        if args.insecure {
            tls_config = tls_config.domain_name("localhost");
        }

        channel_builder = channel_builder
            .tls_config(tls_config)
            .context("Failed to configure TLS")?;
    } else {
        eprintln!("Using plain gRPC (no TLS)");
    }

    channel_builder
        .connect()
        .await
        .context("Failed to connect to endpoint")
}

fn generate_test_texts(count: usize) -> Vec<String> {
    let templates = vec![
        "The quick brown fox jumps over the lazy dog",
        "Machine learning models can process natural language efficiently",
        "Embedding vectors capture semantic meaning in high-dimensional space",
        "Text retrieval systems rely on similarity search algorithms",
        "Neural networks transform input data through multiple layers",
        "Vector databases enable fast approximate nearest neighbor search",
        "Semantic search improves information retrieval accuracy",
        "Language models understand context and generate coherent text",
        "Transformers revolutionized natural language processing",
        "Attention mechanisms help models focus on relevant information",
    ];

    (0..count)
        .map(|i| {
            let base = &templates[i % templates.len()];
            let mut text = format!("{} - sample {}", base, i + 1);
            if i % 3 == 0 {
                text.push_str(" with additional context for testing variable length inputs");
            }
            text
        })
        .collect()
}

// =============================================================================
// Standard Mode: Single text per request with concurrent execution
// =============================================================================

async fn benchmark_standard(
    client: TeiMultiplexerClient<Channel>,
    instance_name: String,
    texts: Vec<String>,
    concurrency: usize,
) -> Result<BenchmarkResult> {
    let total_texts = texts.len();
    let start = Instant::now();

    let semaphore = Arc::new(Semaphore::new(concurrency));
    let client = Arc::new(client);

    let mut tasks = Vec::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel(total_texts);

    for text in texts {
        let permit = semaphore.clone().acquire_owned().await?;
        let mut client = (*client).clone();
        let instance_name = instance_name.clone();
        let tx = tx.clone();

        let task = tokio::spawn(async move {
            let result = embed_text_standard(&mut client, instance_name, text).await;
            let _ = tx.send(result).await;
            drop(permit);
        });

        tasks.push(task);
    }

    drop(tx);

    let mut successful = 0;
    let mut failed = 0;

    while let Some(result) = rx.recv().await {
        match result {
            Ok(_) => successful += 1,
            Err(e) => {
                if failed == 0 {
                    eprintln!("First error: {}", e);
                }
                failed += 1;
            }
        }

        let total = successful + failed;
        if total % 1000 == 0 {
            eprintln!("Progress: {}/{}", total, total_texts);
        }
    }

    for task in tasks {
        task.await?;
    }

    let duration = start.elapsed();
    let duration_secs = duration.as_secs_f64();
    let throughput = successful as f64 / duration_secs;

    Ok(BenchmarkResult {
        mode: "standard".to_string(),
        instance_name,
        num_texts: total_texts,
        batch_size: concurrency,
        num_requests: total_texts,
        total_duration_secs: duration_secs,
        throughput_per_sec: throughput,
        successful,
        failed,
    })
}

async fn embed_text_standard(
    client: &mut TeiMultiplexerClient<Channel>,
    instance_name: String,
    text: String,
) -> Result<Vec<f32>> {
    let request = EmbedRequest {
        target: Some(Target {
            routing: Some(
                tei_manager::grpc::proto::multiplexer::v1::target::Routing::InstanceName(
                    instance_name,
                ),
            ),
        }),
        request: Some(tei::EmbedRequest {
            inputs: text,
            truncate: true,
            normalize: true,
            truncation_direction: 0,
            prompt_name: None,
            dimensions: None,
        }),
    };

    let response = client.embed(request).await?.into_inner();
    Ok(response.embeddings)
}

// =============================================================================
// Arrow Mode: Batched Arrow IPC format
// =============================================================================

async fn benchmark_arrow(
    mut client: TeiMultiplexerClient<Channel>,
    instance_name: String,
    texts: Vec<String>,
    batch_size: usize,
    noop: bool,
) -> Result<BenchmarkResult> {
    let total_texts = texts.len();
    let start = Instant::now();

    let mut successful = 0;
    let mut failed = 0;
    let mut num_requests = 0;

    for (batch_idx, chunk) in texts.chunks(batch_size).enumerate() {
        num_requests += 1;

        // Create Arrow RecordBatch with text column
        let text_array = StringArray::from(chunk.to_vec());
        let schema = Arc::new(Schema::new(vec![Field::new("text", DataType::Utf8, false)]));
        let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(text_array) as ArrayRef])?;

        // Serialize to Arrow IPC with LZ4 compression
        let mut arrow_ipc = Vec::new();
        {
            use arrow::ipc::CompressionType;
            use arrow::ipc::writer::IpcWriteOptions;

            let write_options = IpcWriteOptions::default()
                .try_with_compression(Some(CompressionType::LZ4_FRAME))?;

            let mut writer =
                StreamWriter::try_new_with_options(&mut arrow_ipc, &schema, write_options)?;
            writer.write(&batch)?;
            writer.finish()?;
        }

        // Send gRPC request
        let request = EmbedArrowRequest {
            target: Some(Target {
                routing: Some(
                    tei_manager::grpc::proto::multiplexer::v1::target::Routing::InstanceName(
                        instance_name.clone(),
                    ),
                ),
            }),
            arrow_ipc,
            truncate: true,
            normalize: true,
            noop,
        };

        match client.embed_arrow(request).await {
            Ok(response) => {
                let response_ipc = response.into_inner().arrow_ipc;

                // Verify response
                let cursor = Cursor::new(response_ipc);
                let mut reader = StreamReader::try_new(cursor, None)?;

                if let Some(result_batch) = reader.next() {
                    let result_batch = result_batch?;
                    successful += result_batch.num_rows();
                } else {
                    failed += chunk.len();
                }
            }
            Err(e) => {
                eprintln!("Batch {} failed: {}", batch_idx, e);
                failed += chunk.len();
            }
        }

        // Progress indicator
        if (batch_idx + 1) % 10 == 0 {
            eprintln!(
                "Progress: {} batches, {} texts processed",
                batch_idx + 1,
                successful + failed
            );
        }
    }

    let duration = start.elapsed();
    let duration_secs = duration.as_secs_f64();
    let throughput = successful as f64 / duration_secs;

    Ok(BenchmarkResult {
        mode: "arrow".to_string(),
        instance_name,
        num_texts: total_texts,
        batch_size,
        num_requests,
        total_duration_secs: duration_secs,
        throughput_per_sec: throughput,
        successful,
        failed,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Build channel with optional TLS
    let channel = build_channel(&args).await?;

    // Generate test texts
    eprintln!("Generating {} test texts...", args.num_texts);
    let texts = generate_test_texts(args.num_texts);

    // Run benchmark based on mode
    let result = match args.mode {
        BenchMode::Standard => {
            eprintln!(
                "Benchmarking instance '{}' in STANDARD mode with {} texts (concurrency: {})...",
                args.instance, args.num_texts, args.batch_size
            );

            let client = TeiMultiplexerClient::new(channel);

            benchmark_standard(client, args.instance.clone(), texts, args.batch_size).await?
        }
        BenchMode::Arrow => {
            eprintln!(
                "Benchmarking instance '{}' in ARROW mode with {} texts (batch size: {})...",
                args.instance, args.num_texts, args.batch_size
            );

            let max_message_size = args.max_message_size_mb * 1024 * 1024;
            let client = TeiMultiplexerClient::new(channel)
                .max_decoding_message_size(max_message_size)
                .max_encoding_message_size(max_message_size);

            benchmark_arrow(
                client,
                args.instance.clone(),
                texts,
                args.batch_size,
                args.noop,
            )
            .await?
        }
    };

    // Output JSON result
    println!("{}", serde_json::to_string_pretty(&result)?);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::ipc::reader::StreamReader;
    use arrow::ipc::writer::StreamWriter;
    use std::io::Cursor;
    use std::sync::Arc;

    /// Helper function to create Arrow IPC bytes for testing
    fn create_arrow_ipc(texts: &[String]) -> Result<Vec<u8>> {
        let text_array = StringArray::from(texts.to_vec());
        let schema = Arc::new(Schema::new(vec![Field::new("text", DataType::Utf8, false)]));
        let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(text_array)])?;

        let mut buffer = Vec::new();
        {
            let mut writer = StreamWriter::try_new_with_options(
                &mut buffer,
                &schema,
                arrow::ipc::writer::IpcWriteOptions::default()
                    .try_with_compression(Some(arrow::ipc::CompressionType::LZ4_FRAME))?,
            )?;
            writer.write(&batch)?;
            writer.finish()?;
        }
        Ok(buffer)
    }

    #[test]
    fn test_generate_test_texts_count() {
        let texts = generate_test_texts(100);
        assert_eq!(texts.len(), 100);
    }

    #[test]
    fn test_generate_test_texts_empty() {
        let texts = generate_test_texts(0);
        assert!(texts.is_empty());
    }

    #[test]
    fn test_generate_test_texts_content_variety() {
        let texts = generate_test_texts(30);

        // Check that texts contain expected patterns
        assert!(texts.iter().any(|t| t.contains("quick brown fox")));
        assert!(texts.iter().any(|t| t.contains("Machine learning")));
        assert!(texts.iter().any(|t| t.contains("Embedding vectors")));

        // Check that some texts have additional context (every 3rd)
        let with_context = texts
            .iter()
            .filter(|t| t.contains("additional context"))
            .count();
        assert_eq!(with_context, 10); // 0, 3, 6, 9, 12, 15, 18, 21, 24, 27
    }

    #[test]
    fn test_generate_test_texts_sample_numbers() {
        let texts = generate_test_texts(5);

        assert!(texts[0].contains("sample 1"));
        assert!(texts[1].contains("sample 2"));
        assert!(texts[2].contains("sample 3"));
        assert!(texts[3].contains("sample 4"));
        assert!(texts[4].contains("sample 5"));
    }

    #[test]
    fn test_benchmark_result_serialization() {
        let result = BenchmarkResult {
            mode: "standard".to_string(),
            instance_name: "test-instance".to_string(),
            num_texts: 1000,
            batch_size: 100,
            num_requests: 1000,
            total_duration_secs: 10.5,
            throughput_per_sec: 95.238,
            successful: 950,
            failed: 50,
        };

        let json = serde_json::to_string(&result).expect("Should serialize");
        assert!(json.contains("\"mode\":\"standard\""));
        assert!(json.contains("\"instance_name\":\"test-instance\""));
        assert!(json.contains("\"num_texts\":1000"));
        assert!(json.contains("\"throughput_per_sec\":95.238"));
    }

    #[test]
    fn test_benchmark_result_deserialization() {
        let json = r#"{
            "mode": "arrow",
            "instance_name": "my-instance",
            "num_texts": 500,
            "batch_size": 50,
            "num_requests": 10,
            "total_duration_secs": 5.0,
            "throughput_per_sec": 100.0,
            "successful": 500,
            "failed": 0
        }"#;

        let result: BenchmarkResult = serde_json::from_str(json).expect("Should deserialize");
        assert_eq!(result.mode, "arrow");
        assert_eq!(result.instance_name, "my-instance");
        assert_eq!(result.num_texts, 500);
        assert_eq!(result.batch_size, 50);
        assert_eq!(result.num_requests, 10);
        assert!((result.total_duration_secs - 5.0).abs() < f64::EPSILON);
        assert!((result.throughput_per_sec - 100.0).abs() < f64::EPSILON);
        assert_eq!(result.successful, 500);
        assert_eq!(result.failed, 0);
    }

    #[test]
    fn test_create_arrow_ipc_single_text() {
        let texts = vec!["Hello, world!".to_string()];
        let ipc_bytes = create_arrow_ipc(&texts).expect("Should create Arrow IPC");

        // Verify we can read it back
        let cursor = Cursor::new(ipc_bytes);
        let mut reader = StreamReader::try_new(cursor, None).expect("Should create reader");

        let batch = reader
            .next()
            .expect("Should have batch")
            .expect("Batch should be valid");
        assert_eq!(batch.num_rows(), 1);
        assert_eq!(batch.num_columns(), 1);

        let text_col = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("Should be StringArray");
        assert_eq!(text_col.value(0), "Hello, world!");
    }

    #[test]
    fn test_create_arrow_ipc_multiple_texts() {
        let texts = vec![
            "First text".to_string(),
            "Second text".to_string(),
            "Third text".to_string(),
        ];
        let ipc_bytes = create_arrow_ipc(&texts).expect("Should create Arrow IPC");

        let cursor = Cursor::new(ipc_bytes);
        let mut reader = StreamReader::try_new(cursor, None).expect("Should create reader");

        let batch = reader
            .next()
            .expect("Should have batch")
            .expect("Batch should be valid");
        assert_eq!(batch.num_rows(), 3);

        let text_col = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("Should be StringArray");
        assert_eq!(text_col.value(0), "First text");
        assert_eq!(text_col.value(1), "Second text");
        assert_eq!(text_col.value(2), "Third text");
    }

    #[test]
    fn test_create_arrow_ipc_empty() {
        let texts: Vec<String> = vec![];
        let ipc_bytes = create_arrow_ipc(&texts).expect("Should create Arrow IPC");

        let cursor = Cursor::new(ipc_bytes);
        let mut reader = StreamReader::try_new(cursor, None).expect("Should create reader");

        let batch = reader
            .next()
            .expect("Should have batch")
            .expect("Batch should be valid");
        assert_eq!(batch.num_rows(), 0);
    }

    #[test]
    fn test_create_arrow_ipc_large_batch() {
        let texts: Vec<String> = (0..1000).map(|i| format!("Text number {}", i)).collect();
        let ipc_bytes = create_arrow_ipc(&texts).expect("Should create Arrow IPC");

        let cursor = Cursor::new(ipc_bytes);
        let mut reader = StreamReader::try_new(cursor, None).expect("Should create reader");

        let batch = reader
            .next()
            .expect("Should have batch")
            .expect("Batch should be valid");
        assert_eq!(batch.num_rows(), 1000);

        let text_col = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("Should be StringArray");
        assert_eq!(text_col.value(0), "Text number 0");
        assert_eq!(text_col.value(999), "Text number 999");
    }

    #[test]
    fn test_create_arrow_ipc_unicode() {
        let texts = vec![
            "Hello 世界".to_string(),
            "Привет мир".to_string(),
            "🚀 Rocket".to_string(),
        ];
        let ipc_bytes = create_arrow_ipc(&texts).expect("Should create Arrow IPC");

        let cursor = Cursor::new(ipc_bytes);
        let mut reader = StreamReader::try_new(cursor, None).expect("Should create reader");

        let batch = reader
            .next()
            .expect("Should have batch")
            .expect("Batch should be valid");
        assert_eq!(batch.num_rows(), 3);

        let text_col = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("Should be StringArray");
        assert_eq!(text_col.value(0), "Hello 世界");
        assert_eq!(text_col.value(1), "Привет мир");
        assert_eq!(text_col.value(2), "🚀 Rocket");
    }

    #[test]
    fn test_create_arrow_ipc_compression() {
        // Create two batches and verify compression is working by checking size
        let texts: Vec<String> = (0..100)
            .map(|_| "This is a repeated text that should compress well".to_string())
            .collect();
        let ipc_bytes = create_arrow_ipc(&texts).expect("Should create Arrow IPC");

        // With LZ4 compression, repeated data should compress significantly
        // Raw size would be ~4900 bytes (49 chars * 100), compressed should be much smaller
        let raw_size: usize = texts.iter().map(|t| t.len()).sum();
        assert!(
            ipc_bytes.len() < raw_size,
            "Compressed size {} should be less than raw size {}",
            ipc_bytes.len(),
            raw_size
        );
    }

    #[test]
    fn test_bench_mode_enum() {
        // Test that BenchMode can be parsed from strings
        use clap::ValueEnum;

        let standard = BenchMode::from_str("standard", false).expect("Should parse standard");
        assert!(matches!(standard, BenchMode::Standard));

        let arrow = BenchMode::from_str("arrow", false).expect("Should parse arrow");
        assert!(matches!(arrow, BenchMode::Arrow));
    }

    #[test]
    fn test_args_parsing_defaults() {
        use clap::Parser;

        let args = Args::try_parse_from([
            "tei-bench-client",
            "--endpoint",
            "http://localhost:50051",
            "--instance",
            "test",
        ])
        .expect("Should parse");

        assert_eq!(args.endpoint, "http://localhost:50051");
        assert_eq!(args.instance, "test");
        assert!(matches!(args.mode, BenchMode::Standard));
        assert_eq!(args.num_texts, 10000);
        assert_eq!(args.batch_size, 100);
        assert!(args.cert.is_none());
        assert!(args.key.is_none());
        assert!(args.ca.is_none());
        assert!(!args.insecure);
        assert!(!args.noop);
        assert_eq!(args.max_message_size_mb, 100);
    }

    #[test]
    fn test_args_parsing_full() {
        use clap::Parser;

        let args = Args::try_parse_from([
            "tei-bench-client",
            "--endpoint",
            "https://localhost:50051",
            "--instance",
            "my-instance",
            "--mode",
            "arrow",
            "--num-texts",
            "5000",
            "--batch-size",
            "50",
            "--cert",
            "/path/to/cert.pem",
            "--key",
            "/path/to/key.pem",
            "--ca",
            "/path/to/ca.pem",
            "--insecure",
            "--noop",
            "--max-message-size-mb",
            "200",
        ])
        .expect("Should parse");

        assert_eq!(args.endpoint, "https://localhost:50051");
        assert_eq!(args.instance, "my-instance");
        assert!(matches!(args.mode, BenchMode::Arrow));
        assert_eq!(args.num_texts, 5000);
        assert_eq!(args.batch_size, 50);
        assert_eq!(args.cert, Some(PathBuf::from("/path/to/cert.pem")));
        assert_eq!(args.key, Some(PathBuf::from("/path/to/key.pem")));
        assert_eq!(args.ca, Some(PathBuf::from("/path/to/ca.pem")));
        assert!(args.insecure);
        assert!(args.noop);
        assert_eq!(args.max_message_size_mb, 200);
    }
}
