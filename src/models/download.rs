//! Model download functionality using hf-hub
//!
//! Provides async model downloading from HuggingFace Hub using the native
//! Rust hf-hub crate instead of shelling out to huggingface-cli.

use hf_hub::api::tokio::{Api, ApiBuilder};
use std::path::PathBuf;

/// Download a model from HuggingFace Hub
///
/// Uses the hf-hub crate to download all model files to the local cache.
/// This is compatible with the standard HuggingFace cache structure.
///
/// # Arguments
/// * `model_id` - The model identifier (e.g., "BAAI/bge-small-en-v1.5")
///
/// # Returns
/// * `Ok(PathBuf)` - Path to the downloaded model's snapshot directory
/// * `Err(String)` - Error message if download failed
pub async fn download_model(model_id: &str) -> Result<PathBuf, String> {
    download_model_to_cache(model_id, None).await
}

/// Download a model to a specific cache directory
///
/// # Arguments
/// * `model_id` - The model identifier (e.g., "BAAI/bge-small-en-v1.5")
/// * `cache_dir` - Optional custom cache directory. If None, uses default HF cache.
///
/// # Returns
/// * `Ok(PathBuf)` - Path to the downloaded model's snapshot directory
/// * `Err(String)` - Error message if download failed
pub async fn download_model_to_cache(
    model_id: &str,
    cache_dir: Option<PathBuf>,
) -> Result<PathBuf, String> {
    tracing::info!(model_id = %model_id, cache_dir = ?cache_dir, "Starting model download via hf-hub");

    let api = match cache_dir {
        Some(dir) => ApiBuilder::new()
            .with_cache_dir(dir)
            .build()
            .map_err(|e| format!("Failed to create HF API client: {}", e))?,
        None => Api::new().map_err(|e| format!("Failed to create HF API client: {}", e))?,
    };

    let repo = api.model(model_id.to_string());

    // Download essential embedding model files
    // These are the minimum files needed for TEI to load a model
    let essential_files = ["config.json", "tokenizer.json"];

    let mut config_path: Option<PathBuf> = None;
    for file in &essential_files {
        tracing::debug!(model_id = %model_id, file = %file, "Downloading file");
        let path = repo
            .get(file)
            .await
            .map_err(|e| format!("Failed to download {}: {}", file, e))?;

        // Save config.json path to derive snapshot dir
        if *file == "config.json" {
            config_path = Some(path);
        }
    }

    // Try to download model weights - safetensors preferred, fall back to pytorch
    let weight_files = [
        "model.safetensors",
        "pytorch_model.bin",
        "model.onnx",
        // Sharded safetensors
        "model.safetensors.index.json",
    ];

    let mut downloaded_weights = false;
    for file in &weight_files {
        match repo.get(file).await {
            Ok(_) => {
                tracing::debug!(model_id = %model_id, file = %file, "Downloaded weight file");
                downloaded_weights = true;

                // If we got an index file, download all shards
                if file.ends_with(".index.json") {
                    download_sharded_weights(&repo, model_id).await?;
                }
                break;
            }
            Err(_) => continue,
        }
    }

    if !downloaded_weights {
        tracing::warn!(model_id = %model_id, "No standard weight files found, model may use custom format");
    }

    // Download optional files that may be needed
    let optional_files = [
        "tokenizer_config.json",
        "special_tokens_map.json",
        "vocab.txt",
        "sentence_bert_config.json",
        "modules.json",
    ];

    for file in &optional_files {
        if repo.get(file).await.is_ok() {
            tracing::debug!(model_id = %model_id, file = %file, "Downloaded optional file");
        }
    }

    // Return the snapshot directory (parent of config.json)
    config_path
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .ok_or_else(|| {
            format!(
                "Model downloaded but snapshot path not found for {}",
                model_id
            )
        })
}

/// Download sharded weight files referenced in an index file
async fn download_sharded_weights(
    repo: &hf_hub::api::tokio::ApiRepo,
    model_id: &str,
) -> Result<(), String> {
    // Get the index file content
    let index_path = repo
        .get("model.safetensors.index.json")
        .await
        .map_err(|e| format!("Failed to get index file: {}", e))?;

    let index_content = tokio::fs::read_to_string(&index_path)
        .await
        .map_err(|e| format!("Failed to read index file: {}", e))?;

    // Parse to find weight_map keys (shard filenames)
    let index: serde_json::Value = serde_json::from_str(&index_content)
        .map_err(|e| format!("Failed to parse index file: {}", e))?;

    if let Some(weight_map) = index.get("weight_map").and_then(|v| v.as_object()) {
        // Collect unique shard files
        let shards: std::collections::HashSet<&str> =
            weight_map.values().filter_map(|v| v.as_str()).collect();

        tracing::info!(
            model_id = %model_id,
            shard_count = shards.len(),
            "Downloading sharded weights"
        );

        for shard in shards {
            tracing::debug!(model_id = %model_id, shard = %shard, "Downloading shard");
            repo.get(shard)
                .await
                .map_err(|e| format!("Failed to download shard {}: {}", shard, e))?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_api_creation() {
        // Just verify we can create the API client
        let api = Api::new();
        assert!(api.is_ok());
    }

    #[tokio::test]
    async fn test_api_builder_with_cache_dir() {
        let temp_dir = tempfile::tempdir().unwrap();
        let api = ApiBuilder::new()
            .with_cache_dir(temp_dir.path().to_path_buf())
            .build();
        assert!(api.is_ok());
    }

    #[tokio::test]
    #[ignore = "requires network access and downloads ~100MB"]
    async fn test_download_small_model() {
        // This test downloads a real model to a temp directory
        let temp_dir = tempfile::tempdir().unwrap();
        let result = download_model_to_cache(
            "sentence-transformers/all-MiniLM-L6-v2",
            Some(temp_dir.path().to_path_buf()),
        )
        .await;
        assert!(result.is_ok(), "Download failed: {:?}", result.err());
        let path = result.unwrap();
        assert!(path.join("config.json").exists());
        assert!(path.join("model.safetensors").exists() || path.join("pytorch_model.bin").exists());
    }
}
