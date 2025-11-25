//! GPU detection and management
//!
//! Detects available GPUs via nvidia-smi and provides virtual-to-physical mapping.
//! This handles multi-tenant environments (Vast.ai, RunPod) where the container
//! may see device files for all host GPUs but only has access to a subset.

use std::process::Command;
use std::sync::OnceLock;

/// Cached GPU information detected at startup
static GPU_INFO: OnceLock<GpuInfo> = OnceLock::new();

/// Information about available GPUs
#[derive(Debug, Clone, Default)]
pub struct GpuInfo {
    /// List of GPU indices visible to this process (from nvidia-smi)
    /// These are the "virtual" indices that CUDA sees
    pub indices: Vec<u32>,
    /// Comma-separated string for CUDA_VISIBLE_DEVICES
    pub cuda_visible_devices: String,
}

impl GpuInfo {
    /// Get the number of available GPUs
    pub fn count(&self) -> usize {
        self.indices.len()
    }

    /// Check if a user-provided gpu_id is valid
    pub fn is_valid_gpu_id(&self, gpu_id: u32) -> bool {
        (gpu_id as usize) < self.indices.len()
    }

    /// Get the CUDA_VISIBLE_DEVICES value for a specific gpu_id
    /// User provides virtual index (0, 1, 2...), we return the actual index
    pub fn get_cuda_device(&self, gpu_id: u32) -> Option<String> {
        self.indices.get(gpu_id as usize).map(|idx| idx.to_string())
    }
}

/// Detect available GPUs using nvidia-smi
///
/// Returns indices of GPUs visible to this process. In multi-tenant environments,
/// this correctly returns only the GPUs allocated to this container, not all
/// GPUs on the host.
pub fn detect_gpus() -> GpuInfo {
    let output = Command::new("nvidia-smi")
        .args(["--query-gpu=index", "--format=csv,noheader"])
        .output();

    match output {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let indices: Vec<u32> = stdout
                .lines()
                .filter_map(|line| line.trim().parse::<u32>().ok())
                .collect();

            let cuda_visible_devices = indices
                .iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(",");

            tracing::info!(
                gpu_count = indices.len(),
                indices = ?indices,
                cuda_visible_devices = %cuda_visible_devices,
                "Detected available GPUs"
            );

            GpuInfo {
                indices,
                cuda_visible_devices,
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(
                stderr = %stderr,
                "nvidia-smi failed, assuming no GPUs available"
            );
            GpuInfo::default()
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Failed to run nvidia-smi, assuming no GPUs available"
            );
            GpuInfo::default()
        }
    }
}

/// Initialize GPU detection (call once at startup)
pub fn init() -> &'static GpuInfo {
    GPU_INFO.get_or_init(detect_gpus)
}

/// Get cached GPU info (panics if init() wasn't called)
pub fn get() -> &'static GpuInfo {
    GPU_INFO
        .get()
        .expect("GPU detection not initialized - call gpu::init() first")
}

/// Get cached GPU info, or detect if not initialized
pub fn get_or_init() -> &'static GpuInfo {
    GPU_INFO.get_or_init(detect_gpus)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gpu_info_validation() {
        let info = GpuInfo {
            indices: vec![0, 1],
            cuda_visible_devices: "0,1".to_string(),
        };

        assert_eq!(info.count(), 2);
        assert!(info.is_valid_gpu_id(0));
        assert!(info.is_valid_gpu_id(1));
        assert!(!info.is_valid_gpu_id(2));
        assert!(!info.is_valid_gpu_id(99));
    }

    #[test]
    fn test_get_cuda_device() {
        let info = GpuInfo {
            indices: vec![0, 1],
            cuda_visible_devices: "0,1".to_string(),
        };

        assert_eq!(info.get_cuda_device(0), Some("0".to_string()));
        assert_eq!(info.get_cuda_device(1), Some("1".to_string()));
        assert_eq!(info.get_cuda_device(2), None);
    }

    #[test]
    fn test_empty_gpu_info() {
        let info = GpuInfo::default();

        assert_eq!(info.count(), 0);
        assert!(!info.is_valid_gpu_id(0));
        assert_eq!(info.get_cuda_device(0), None);
    }
}
