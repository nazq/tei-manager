//! GPU detection and management
//!
//! Detects available GPUs via nvidia-smi and provides virtual-to-physical mapping.
//! This handles multi-tenant environments (Vast.ai, RunPod) where the container
//! may see device files for all host GPUs but only has access to a subset.

use std::process::Command;
use std::sync::OnceLock;

/// Cached GPU information detected at startup
static GPU_INFO: OnceLock<GpuInfo> = OnceLock::new();

/// One GPU as reported by nvidia-smi
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuDevice {
    /// Index as CUDA sees it
    pub index: u32,
    pub name: String,
    /// Compute capability (major, minor), if nvidia-smi reported it
    pub compute_cap: Option<(u32, u32)>,
    pub memory_total_mib: Option<u64>,
    pub memory_free_mib: Option<u64>,
}

/// Information about available GPUs
#[derive(Debug, Clone, Default)]
pub struct GpuInfo {
    /// List of GPU indices visible to this process (from nvidia-smi)
    /// These are the "virtual" indices that CUDA sees
    pub indices: Vec<u32>,
    /// Comma-separated string for CUDA_VISIBLE_DEVICES
    pub cuda_visible_devices: String,
    /// Per-device details, same order as `indices`
    pub devices: Vec<GpuDevice>,
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

    /// Device details for a virtual gpu_id
    pub fn device(&self, gpu_id: u32) -> Option<&GpuDevice> {
        self.devices.get(gpu_id as usize)
    }

    /// Free VRAM in MiB for the GPU an instance will run on: the given
    /// `gpu_id`, else the smallest free VRAM across all visible GPUs (an
    /// unpinned instance may land on any of them).
    pub fn free_vram_mib(&self, gpu_id: Option<u32>) -> Option<u64> {
        match gpu_id {
            Some(id) => self.device(id).and_then(|d| d.memory_free_mib),
            None => self.devices.iter().filter_map(|d| d.memory_free_mib).min(),
        }
    }
}

/// Parse `nvidia-smi --query-gpu=index,name,compute_cap,memory.total,memory.free --format=csv,noheader,nounits`
pub fn parse_nvidia_smi(stdout: &str) -> GpuInfo {
    let mut devices = Vec::new();
    for line in stdout.lines() {
        let cols: Vec<&str> = line.split(',').map(str::trim).collect();
        let Some(index) = cols.first().and_then(|c| c.parse::<u32>().ok()) else {
            continue;
        };
        let name = cols.get(1).unwrap_or(&"").to_string();
        let compute_cap = cols.get(2).and_then(|c| {
            let (maj, min) = c.split_once('.')?;
            Some((maj.parse().ok()?, min.parse().ok()?))
        });
        let mib = |i: usize| cols.get(i).and_then(|c| c.parse::<u64>().ok());
        devices.push(GpuDevice {
            index,
            name,
            compute_cap,
            memory_total_mib: mib(3),
            memory_free_mib: mib(4),
        });
    }
    let indices: Vec<u32> = devices.iter().map(|d| d.index).collect();
    let cuda_visible_devices = indices
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    GpuInfo {
        indices,
        cuda_visible_devices,
        devices,
    }
}

/// Detect available GPUs using nvidia-smi
///
/// Returns indices of GPUs visible to this process. In multi-tenant environments,
/// this correctly returns only the GPUs allocated to this container, not all
/// GPUs on the host.
pub fn detect_gpus() -> GpuInfo {
    let output = Command::new("nvidia-smi")
        .args([
            "--query-gpu=index,name,compute_cap,memory.total,memory.free",
            "--format=csv,noheader,nounits",
        ])
        .output();

    match output {
        Ok(output) if output.status.success() => {
            let info = parse_nvidia_smi(&String::from_utf8_lossy(&output.stdout));
            tracing::info!(
                gpu_count = info.count(),
                indices = ?info.indices,
                cuda_visible_devices = %info.cuda_visible_devices,
                devices = ?info.devices,
                "Detected available GPUs"
            );
            info
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

// ============================================================================
// Compute-capability preflight
// ============================================================================

/// Environment variable baked into the image with the TEI variant prefix
/// ("", "89-", "hopper-", "120-", "121-", "cpu-")
pub const TEI_VARIANT_ENV: &str = "TEI_VARIANT";

/// Compute capability the bundled text-embeddings-router was compiled for,
/// from the image's TEI variant prefix. `None` for CPU or unknown variants.
pub fn compiled_compute_cap(variant: &str) -> Option<u32> {
    match variant.trim().trim_end_matches('-') {
        "" => Some(80),
        "89" => Some(89),
        "hopper" => Some(90),
        "120" => Some(120),
        "121" => Some(121),
        "cpu" => None,
        other => other.parse().ok(),
    }
}

/// Image tag suffix to suggest for a runtime compute capability
pub fn suggested_variant(runtime: (u32, u32)) -> &'static str {
    match runtime {
        (8, _) => "default (Ampere sm_80) or -ada for sm_89",
        (9, _) => "-hopper",
        (12, 0) => "-blackwell",
        (12, 1) => "121- (DGX Spark, arm64)",
        _ => "an image built for this compute capability",
    }
}

/// Whether kernels compiled for `compiled` (e.g. 80) run on a `runtime`
/// capability: cubins are forward-compatible only within a major version.
pub fn compute_cap_compatible(compiled: u32, runtime: (u32, u32)) -> bool {
    let (major, minor) = runtime;
    major == compiled / 10 && major * 10 + minor >= compiled
}

/// Check every visible GPU against the compiled compute capability.
/// Returns one message per incompatible GPU (empty = all good or nothing to check).
pub fn preflight(info: &GpuInfo, variant: Option<&str>) -> Vec<String> {
    let Some(compiled) = variant.and_then(compiled_compute_cap) else {
        return Vec::new();
    };
    info.devices
        .iter()
        .filter_map(|d| {
            let cap = d.compute_cap?;
            (!compute_cap_compatible(compiled, cap)).then(|| {
                format!(
                    "GPU {} ({}) is compute capability {}.{} but this image's TEI is compiled for sm_{}; use the {} image",
                    d.index,
                    d.name,
                    cap.0,
                    cap.1,
                    compiled,
                    suggested_variant(cap)
                )
            })
        })
        .collect()
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
            devices: vec![],
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
            devices: vec![],
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

    const SMI: &str = "0, NVIDIA GeForce RTX 5090, 12.0, 32607, 27662\n1, NVIDIA A100-SXM4-80GB, 8.0, 81920, 80000\n";

    #[test]
    fn test_parse_nvidia_smi() {
        let info = parse_nvidia_smi(SMI);
        assert_eq!(info.indices, vec![0, 1]);
        assert_eq!(info.cuda_visible_devices, "0,1");
        assert_eq!(info.devices[0].compute_cap, Some((12, 0)));
        assert_eq!(info.devices[0].memory_free_mib, Some(27662));
        assert_eq!(info.devices[1].name, "NVIDIA A100-SXM4-80GB");
        assert_eq!(info.free_vram_mib(Some(1)), Some(80000));
        assert_eq!(info.free_vram_mib(None), Some(27662)); // smallest wins
        assert_eq!(info.free_vram_mib(Some(7)), None);
    }

    #[test]
    fn test_parse_nvidia_smi_tolerates_garbage() {
        let info = parse_nvidia_smi("garbage\n3, Weird GPU, [N/A], [N/A], [N/A]\n");
        assert_eq!(info.indices, vec![3]);
        assert_eq!(info.devices[0].compute_cap, None);
        assert_eq!(info.free_vram_mib(None), None);
        assert!(parse_nvidia_smi("").devices.is_empty());
    }

    #[test]
    fn test_compiled_compute_cap() {
        assert_eq!(compiled_compute_cap(""), Some(80));
        assert_eq!(compiled_compute_cap("89-"), Some(89));
        assert_eq!(compiled_compute_cap("hopper-"), Some(90));
        assert_eq!(compiled_compute_cap("120-"), Some(120));
        assert_eq!(compiled_compute_cap("121-"), Some(121));
        assert_eq!(compiled_compute_cap("cpu-"), None);
        assert_eq!(compiled_compute_cap("75-"), Some(75));
        assert_eq!(compiled_compute_cap("weird-"), None);
    }

    #[test]
    fn test_compute_cap_compatible() {
        assert!(compute_cap_compatible(80, (8, 0)));
        assert!(compute_cap_compatible(80, (8, 6)));
        assert!(compute_cap_compatible(80, (8, 9))); // 4090 on the default image
        assert!(!compute_cap_compatible(89, (8, 6))); // ada image on A100-class
        assert!(!compute_cap_compatible(80, (12, 0))); // the 5090 failure
        assert!(!compute_cap_compatible(120, (8, 9)));
        assert!(compute_cap_compatible(120, (12, 0)));
        assert!(compute_cap_compatible(120, (12, 1))); // same major, newer minor
        assert!(!compute_cap_compatible(80, (9, 0)));
    }

    #[test]
    fn test_preflight_messages() {
        let info = parse_nvidia_smi(SMI);
        let msgs = preflight(&info, Some(""));
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        assert!(msgs[0].contains("GPU 0"));
        assert!(msgs[0].contains("12.0"));
        assert!(msgs[0].contains("sm_80"));
        assert!(msgs[0].contains("-blackwell"));

        let msgs = preflight(&info, Some("120-"));
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].contains("GPU 1"));

        assert!(preflight(&info, Some("cpu-")).is_empty());
        assert!(preflight(&info, None).is_empty());
        assert!(preflight(&GpuInfo::default(), Some("")).is_empty());
    }
}
