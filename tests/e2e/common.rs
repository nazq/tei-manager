//! Common test fixtures and helpers for e2e tests using testcontainers

#![allow(dead_code)]

use std::borrow::Cow;
use std::sync::Arc;
use testcontainers::core::{ContainerPort, Mount, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, Image};

/// Small dense embedding model (~133MB, commonly cached)
pub const DENSE_MODEL: &str = "BAAI/bge-small-en-v1.5";

/// Small sparse embedding model (~420MB)
pub const SPARSE_MODEL: &str = "naver/splade-cocondenser-selfdistil";

/// Default gRPC port inside the container (TEI gRPC image uses port 80)
const GRPC_PORT: u16 = 80;

/// TEI Docker image configuration
#[derive(Debug, Clone)]
pub struct TeiImage {
    model_id: String,
    pooling: Option<String>,
    env_vars: Vec<(String, String)>,
}

impl TeiImage {
    /// Create a new TEI image for dense embeddings
    pub fn dense(model_id: &str) -> Self {
        Self {
            model_id: model_id.to_string(),
            pooling: None,
            env_vars: Vec::new(),
        }
    }

    /// Create a new TEI image for sparse embeddings (SPLADE)
    pub fn sparse(model_id: &str) -> Self {
        Self {
            model_id: model_id.to_string(),
            pooling: Some("splade".to_string()),
            env_vars: Vec::new(),
        }
    }

    /// Add an environment variable
    pub fn with_env(mut self, key: &str, value: &str) -> Self {
        self.env_vars.push((key.to_string(), value.to_string()));
        self
    }
}

impl Image for TeiImage {
    fn name(&self) -> &str {
        "ghcr.io/huggingface/text-embeddings-inference"
    }

    fn tag(&self) -> &str {
        // Use CPU gRPC image for CI compatibility
        "cpu-1.8.3-grpc"
    }

    fn ready_conditions(&self) -> Vec<WaitFor> {
        // Wait for the gRPC server to be ready
        // TEI outputs JSON logs, so we look for the "Ready" message in JSON format
        vec![WaitFor::message_on_stdout("\"message\":\"Ready\"")]
    }

    fn env_vars(
        &self,
    ) -> impl IntoIterator<Item = (impl Into<Cow<'_, str>>, impl Into<Cow<'_, str>>)> {
        let mut vars: Vec<(String, String)> = vec![("MODEL_ID".to_string(), self.model_id.clone())];

        if let Some(ref pooling) = self.pooling {
            vars.push(("POOLING".to_string(), pooling.clone()));
        }

        // Add custom env vars
        vars.extend(self.env_vars.clone());

        vars
    }

    fn expose_ports(&self) -> &[ContainerPort] {
        &[ContainerPort::Tcp(GRPC_PORT)]
    }

    fn mounts(&self) -> impl IntoIterator<Item = &Mount> {
        // Mount host HF cache to speed up model downloads
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

/// A running TEI container
///
/// The container is automatically stopped when this struct is dropped.
pub struct TeiContainer {
    container: ContainerAsync<TeiImage>,
    grpc_port: u16,
}

impl TeiContainer {
    /// Start a TEI container for dense embeddings
    pub async fn start_dense(
        model_id: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Self::start(TeiImage::dense(model_id)).await
    }

    /// Start a TEI container for sparse embeddings
    pub async fn start_sparse(
        model_id: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Self::start(TeiImage::sparse(model_id)).await
    }

    /// Start a TEI container with custom image configuration
    pub async fn start(image: TeiImage) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        use tei_manager::grpc::proto::tei::v1::embed_client::EmbedClient;
        use tokio::time::{Duration, sleep};
        use tonic::transport::Channel;

        println!("Starting TEI container with model {}...", image.model_id);

        let container = image.start().await?;
        let grpc_port = container.get_host_port_ipv4(GRPC_PORT).await?;

        // Wait for gRPC to be actually accepting connections AND model to be loaded
        // The "Ready" log message appears when the server starts, but the model
        // might still be loading. We need to send a warmup request to ensure
        // the model is fully loaded and ready to process requests.
        let endpoint = format!("http://127.0.0.1:{}", grpc_port);
        let mut connected = false;

        for i in 0..60 {
            // First, try to connect
            if !connected {
                match Channel::from_shared(endpoint.clone())
                    .unwrap()
                    .connect()
                    .await
                {
                    Ok(_) => {
                        println!(
                            "TEI container gRPC connected on port {} (after {}ms)",
                            grpc_port,
                            i * 100
                        );
                        connected = true;
                    }
                    Err(_) => {
                        sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                }
            }

            // Then, send a warmup request to ensure model is loaded
            if connected {
                match Channel::from_shared(endpoint.clone())
                    .unwrap()
                    .connect()
                    .await
                {
                    Ok(channel) => {
                        let mut client = EmbedClient::new(channel);
                        let warmup_req = tei_manager::grpc::proto::tei::v1::EmbedRequest {
                            inputs: "warmup".to_string(),
                            truncate: true,
                            normalize: true,
                            truncation_direction: 0,
                            prompt_name: None,
                            dimensions: None,
                        };

                        match client.embed(warmup_req).await {
                            Ok(_) => {
                                println!(
                                    "TEI container ready on port {} (warmup complete after {}ms)",
                                    grpc_port,
                                    i * 100
                                );
                                return Ok(Self {
                                    container,
                                    grpc_port,
                                });
                            }
                            Err(e) => {
                                // Model might still be loading
                                if i % 10 == 0 {
                                    println!("Warmup attempt {} failed: {}", i, e);
                                }
                                sleep(Duration::from_millis(100)).await;
                            }
                        }
                    }
                    Err(_) => {
                        sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        }

        if connected {
            println!(
                "TEI container on port {} connected but warmup timed out, proceeding anyway",
                grpc_port
            );
        } else {
            println!(
                "TEI container ready on port {} (connection check timed out, proceeding anyway)",
                grpc_port
            );
        }
        Ok(Self {
            container,
            grpc_port,
        })
    }

    /// Get the gRPC endpoint URL
    pub fn grpc_endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.grpc_port)
    }

    /// Get the mapped gRPC port
    pub fn grpc_port(&self) -> u16 {
        self.grpc_port
    }
}

/// Create Arrow IPC batch from texts
pub fn create_arrow_batch(texts: &[&str]) -> Vec<u8> {
    use arrow::array::{ArrayRef, StringArray};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::ipc::writer::StreamWriter;
    use arrow::record_batch::RecordBatch;

    let text_array = StringArray::from(texts.to_vec());
    let schema = Arc::new(Schema::new(vec![Field::new("text", DataType::Utf8, false)]));
    let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(text_array) as ArrayRef])
        .expect("Failed to create RecordBatch");

    let mut buffer = Vec::new();
    {
        let mut writer =
            StreamWriter::try_new(&mut buffer, &schema).expect("Failed to create writer");
        writer.write(&batch).expect("Failed to write batch");
        writer.finish().expect("Failed to finish writer");
    }

    buffer
}
