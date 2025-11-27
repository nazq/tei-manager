//! Model metadata parsing
//!
//! Parses model configuration from HuggingFace's config.json files
//! to extract embedding dimension, model type, etc.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Model metadata extracted from HuggingFace config.json
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HfModelMetadata {
    /// Model architecture type (e.g., "bert", "mpnet", "xlm-roberta")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_type: Option<String>,

    /// Hidden size / embedding dimension
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden_size: Option<u32>,

    /// Maximum sequence length
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_position_embeddings: Option<u32>,

    /// Vocabulary size
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vocab_size: Option<u32>,

    /// Number of hidden layers
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_hidden_layers: Option<u32>,

    /// Number of attention heads
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_attention_heads: Option<u32>,
}

/// Raw config.json structure (partial)
#[derive(Debug, Deserialize)]
struct RawConfig {
    model_type: Option<String>,
    hidden_size: Option<u32>,
    max_position_embeddings: Option<u32>,
    vocab_size: Option<u32>,
    num_hidden_layers: Option<u32>,
    num_attention_heads: Option<u32>,
    // Some models use different names
    d_model: Option<u32>,
    n_positions: Option<u32>,
}

/// Parse model metadata from a cached model's config.json
///
/// # Arguments
/// * `cache_path` - Path to the model's snapshot directory (containing config.json)
///
/// # Returns
/// * `Some(HfModelMetadata)` if config.json exists and is valid
/// * `None` if config.json doesn't exist or can't be parsed
pub fn parse_model_config(cache_path: &Path) -> Option<HfModelMetadata> {
    let config_path = cache_path.join("config.json");

    if !config_path.exists() {
        return None;
    }

    let content = std::fs::read_to_string(&config_path).ok()?;
    let raw: RawConfig = serde_json::from_str(&content).ok()?;

    Some(HfModelMetadata {
        model_type: raw.model_type,
        hidden_size: raw.hidden_size.or(raw.d_model),
        max_position_embeddings: raw.max_position_embeddings.or(raw.n_positions),
        vocab_size: raw.vocab_size,
        num_hidden_layers: raw.num_hidden_layers,
        num_attention_heads: raw.num_attention_heads,
    })
}

/// Estimate number of parameters from model metadata
///
/// This is a rough estimate based on transformer architecture
pub fn estimate_parameters(metadata: &HfModelMetadata) -> Option<u64> {
    let hidden = metadata.hidden_size? as u64;
    let layers = metadata.num_hidden_layers? as u64;
    let vocab = metadata.vocab_size? as u64;
    let heads = metadata.num_attention_heads.unwrap_or(12) as u64;

    // Rough estimate for transformer:
    // - Embedding: vocab * hidden
    // - Per layer: 4 * hidden^2 (attention) + 8 * hidden^2 (FFN) = 12 * hidden^2
    // - Output: hidden * vocab (if not tied)
    let embedding_params = vocab * hidden;
    let layer_params = layers * 12 * hidden * hidden;
    let _head_dim = hidden / heads; // Not used in simple estimate

    Some(embedding_params + layer_params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn create_test_config(dir: &TempDir, content: &str) -> std::path::PathBuf {
        let config_path = dir.path().join("config.json");
        let mut file = std::fs::File::create(&config_path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
        dir.path().to_path_buf()
    }

    #[test]
    fn test_parse_bert_config() {
        let dir = TempDir::new().unwrap();
        let content = r#"{
            "model_type": "bert",
            "hidden_size": 384,
            "max_position_embeddings": 512,
            "vocab_size": 30522,
            "num_hidden_layers": 6,
            "num_attention_heads": 12
        }"#;

        let path = create_test_config(&dir, content);
        let metadata = parse_model_config(&path).unwrap();

        assert_eq!(metadata.model_type, Some("bert".to_string()));
        assert_eq!(metadata.hidden_size, Some(384));
        assert_eq!(metadata.max_position_embeddings, Some(512));
        assert_eq!(metadata.vocab_size, Some(30522));
        assert_eq!(metadata.num_hidden_layers, Some(6));
        assert_eq!(metadata.num_attention_heads, Some(12));
    }

    #[test]
    fn test_parse_config_with_alternative_names() {
        let dir = TempDir::new().unwrap();
        let content = r#"{
            "model_type": "gpt2",
            "d_model": 768,
            "n_positions": 1024,
            "vocab_size": 50257
        }"#;

        let path = create_test_config(&dir, content);
        let metadata = parse_model_config(&path).unwrap();

        assert_eq!(metadata.model_type, Some("gpt2".to_string()));
        assert_eq!(metadata.hidden_size, Some(768));
        assert_eq!(metadata.max_position_embeddings, Some(1024));
    }

    #[test]
    fn test_parse_missing_config() {
        let dir = TempDir::new().unwrap();
        let result = parse_model_config(dir.path());
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_invalid_json() {
        let dir = TempDir::new().unwrap();
        let path = create_test_config(&dir, "not valid json");
        let result = parse_model_config(&path);
        assert!(result.is_none());
    }

    #[test]
    fn test_estimate_parameters() {
        let metadata = HfModelMetadata {
            model_type: Some("bert".to_string()),
            hidden_size: Some(384),
            max_position_embeddings: Some(512),
            vocab_size: Some(30522),
            num_hidden_layers: Some(6),
            num_attention_heads: Some(12),
        };

        let params = estimate_parameters(&metadata).unwrap();
        // Should be roughly 22M for MiniLM-L6
        assert!(params > 10_000_000);
        assert!(params < 50_000_000);
    }
}
