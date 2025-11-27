//! HuggingFace cache detection utilities
//!
//! Detects models downloaded to the HuggingFace cache directory.
//! Cache structure:
//! ```text
//! ~/.cache/huggingface/hub/
//! ├── models--BAAI--bge-small-en-v1.5/
//! │   ├── snapshots/
//! │   │   └── {revision}/
//! │   │       ├── config.json
//! │   │       ├── model.safetensors
//! │   │       └── tokenizer.json
//! │   └── refs/
//! │       └── main
//! └── models--sentence-transformers--all-MiniLM-L6-v2/
//!     └── ...
//! ```

use std::path::PathBuf;

/// Get the HuggingFace cache directory
///
/// Checks in order:
/// 1. `$HF_HOME/hub`
/// 2. `$XDG_CACHE_HOME/huggingface/hub`
/// 3. `~/.cache/huggingface/hub`
pub fn get_cache_dir() -> PathBuf {
    // Check HF_HOME first
    if let Ok(hf_home) = std::env::var("HF_HOME") {
        return PathBuf::from(hf_home).join("hub");
    }

    // Check XDG_CACHE_HOME
    if let Ok(xdg_cache) = std::env::var("XDG_CACHE_HOME") {
        return PathBuf::from(xdg_cache).join("huggingface/hub");
    }

    // Default to ~/.cache/huggingface/hub
    dirs::home_dir()
        .map(|h| h.join(".cache/huggingface/hub"))
        .unwrap_or_else(|| PathBuf::from("/tmp/huggingface/hub"))
}

/// Convert model ID to cache directory name
///
/// HuggingFace uses `models--{org}--{name}` format
/// e.g., "BAAI/bge-small-en-v1.5" -> "models--BAAI--bge-small-en-v1.5"
fn model_id_to_cache_name(model_id: &str) -> String {
    format!("models--{}", model_id.replace('/', "--"))
}

/// Convert cache directory name back to model ID
///
/// e.g., "models--BAAI--bge-small-en-v1.5" -> "BAAI/bge-small-en-v1.5"
fn cache_name_to_model_id(cache_name: &str) -> Option<String> {
    cache_name
        .strip_prefix("models--")
        .map(|s| s.replacen("--", "/", 1))
}

/// Check if a model is cached (downloaded)
pub fn is_model_cached(model_id: &str) -> bool {
    let cache_dir = get_cache_dir();
    let model_dir = cache_dir.join(model_id_to_cache_name(model_id));

    // Check if snapshots directory exists with at least one revision
    let snapshots_dir = model_dir.join("snapshots");
    if !snapshots_dir.exists() {
        return false;
    }

    // Check if there's at least one snapshot with a config.json
    if let Ok(entries) = std::fs::read_dir(&snapshots_dir) {
        for entry in entries.flatten() {
            if entry.path().join("config.json").exists() {
                return true;
            }
        }
    }

    false
}

/// Get the cache path for a model's latest snapshot
///
/// Returns the path to the snapshot directory containing model files
pub fn get_model_cache_path(model_id: &str) -> Option<PathBuf> {
    let cache_dir = get_cache_dir();
    let model_dir = cache_dir.join(model_id_to_cache_name(model_id));

    // First try to resolve via refs/main
    let refs_main = model_dir.join("refs/main");
    if refs_main.exists()
        && let Ok(revision) = std::fs::read_to_string(&refs_main)
    {
        let revision = revision.trim();
        let snapshot_path = model_dir.join("snapshots").join(revision);
        if snapshot_path.exists() {
            return Some(snapshot_path);
        }
    }

    // Fall back to finding the first snapshot with config.json
    let snapshots_dir = model_dir.join("snapshots");
    if let Ok(entries) = std::fs::read_dir(&snapshots_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.join("config.json").exists() {
                return Some(path);
            }
        }
    }

    None
}

/// Get the total size of a cached model in bytes
pub fn get_cache_size(model_id: &str) -> Option<u64> {
    let cache_dir = get_cache_dir();
    let model_dir = cache_dir.join(model_id_to_cache_name(model_id));

    if !model_dir.exists() {
        return None;
    }

    Some(dir_size(&model_dir))
}

/// Recursively calculate directory size
fn dir_size(path: &PathBuf) -> u64 {
    let mut size = 0;

    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                size += dir_size(&path);
            } else if let Ok(metadata) = std::fs::metadata(&path) {
                size += metadata.len();
            }
        }
    }

    size
}

/// List all cached models
///
/// Returns model IDs for all models found in the cache
pub fn list_cached_models() -> Vec<String> {
    let cache_dir = get_cache_dir();

    if !cache_dir.exists() {
        return Vec::new();
    }

    let mut models = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&cache_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();

            // Only look at model directories
            if !name.starts_with("models--") {
                continue;
            }

            // Convert back to model ID
            if let Some(model_id) = cache_name_to_model_id(&name) {
                // Verify it's actually a valid cached model
                if is_model_cached(&model_id) {
                    models.push(model_id);
                }
            }
        }
    }

    models.sort();
    models
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_model_id_to_cache_name() {
        assert_eq!(
            model_id_to_cache_name("BAAI/bge-small-en-v1.5"),
            "models--BAAI--bge-small-en-v1.5"
        );
        assert_eq!(
            model_id_to_cache_name("sentence-transformers/all-MiniLM-L6-v2"),
            "models--sentence-transformers--all-MiniLM-L6-v2"
        );
    }

    #[test]
    fn test_cache_name_to_model_id() {
        assert_eq!(
            cache_name_to_model_id("models--BAAI--bge-small-en-v1.5"),
            Some("BAAI/bge-small-en-v1.5".to_string())
        );
        assert_eq!(
            cache_name_to_model_id("models--sentence-transformers--all-MiniLM-L6-v2"),
            Some("sentence-transformers/all-MiniLM-L6-v2".to_string())
        );
        assert_eq!(cache_name_to_model_id("not-a-model"), None);
    }

    #[test]
    fn test_roundtrip() {
        let model_id = "BAAI/bge-small-en-v1.5";
        let cache_name = model_id_to_cache_name(model_id);
        let recovered = cache_name_to_model_id(&cache_name);
        assert_eq!(recovered, Some(model_id.to_string()));
    }

    #[test]
    fn test_get_cache_dir_default() {
        // Clear env vars for test
        unsafe {
            std::env::remove_var("HF_HOME");
            std::env::remove_var("XDG_CACHE_HOME");
        }

        let cache_dir = get_cache_dir();
        assert!(cache_dir.to_string_lossy().contains("huggingface/hub"));
    }

    #[test]
    fn test_is_model_cached_not_cached() {
        // A random model ID that won't exist
        assert!(!is_model_cached("nonexistent-org/nonexistent-model-12345"));
    }

    #[test]
    fn test_get_model_cache_path_not_cached() {
        // A random model ID that won't exist
        assert!(get_model_cache_path("nonexistent-org/nonexistent-model-12345").is_none());
    }

    #[test]
    fn test_get_cache_size_not_cached() {
        // A random model ID that won't exist
        assert!(get_cache_size("nonexistent-org/nonexistent-model-12345").is_none());
    }

    #[test]
    fn test_list_cached_models_returns_vec() {
        // Just verify it returns a valid vec and doesn't panic
        let models = list_cached_models();
        // Can't assert much without knowing cache state, but verify it's a valid Vec
        let _ = models.len();
    }

    #[test]
    fn test_dir_size_empty_dir() {
        let temp_dir = tempfile::tempdir().unwrap();
        let size = dir_size(&temp_dir.path().to_path_buf());
        assert_eq!(size, 0);
    }

    #[test]
    fn test_dir_size_with_files() {
        let temp_dir = tempfile::tempdir().unwrap();

        // Create a file with known content
        let file_path = temp_dir.path().join("test.txt");
        std::fs::write(&file_path, "hello world").unwrap();

        let size = dir_size(&temp_dir.path().to_path_buf());
        assert_eq!(size, 11); // "hello world" is 11 bytes
    }

    #[test]
    fn test_dir_size_nested_dirs() {
        let temp_dir = tempfile::tempdir().unwrap();

        // Create nested structure
        let subdir = temp_dir.path().join("subdir");
        std::fs::create_dir(&subdir).unwrap();
        std::fs::write(subdir.join("file1.txt"), "abc").unwrap();
        std::fs::write(temp_dir.path().join("file2.txt"), "defgh").unwrap();

        let size = dir_size(&temp_dir.path().to_path_buf());
        assert_eq!(size, 8); // 3 + 5 bytes
    }

    // Note: Tests that modify HF_HOME env var are removed because they race with
    // parallel tests. The cache functions are tested via integration tests in
    // tests/model_registry.rs which use real cached models.
}
