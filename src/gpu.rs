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

    /// [`Self::free_vram_mib`], falling back to a conservative fraction of
    /// system memory on unified-memory platforms (Grace / DGX Spark GB10),
    /// where nvidia-smi reports `[N/A]` for memory.total/memory.free.
    ///
    /// The fallback only applies when at least one GPU is visible but none of
    /// them reports memory — a GPU-less host still returns `None`.
    pub fn free_vram_mib_or_unified(&self, gpu_id: Option<u32>) -> Option<u64> {
        self.free_vram_mib_with_meminfo(gpu_id, read_proc_meminfo().as_deref())
    }

    /// [`Self::free_vram_mib_or_unified`] with `/proc/meminfo` content
    /// injected, for deterministic tests.
    pub fn free_vram_mib_with_meminfo(
        &self,
        gpu_id: Option<u32>,
        meminfo: Option<&str>,
    ) -> Option<u64> {
        if let Some(vram) = self.free_vram_mib(gpu_id) {
            return Some(vram);
        }
        // Unified memory is only plausible when GPUs are visible yet none of
        // them reports dedicated memory; otherwise (no GPUs, or a bad gpu_id
        // alongside GPUs that do report VRAM) stay on the default path.
        if self.devices.is_empty() || self.devices.iter().any(|d| d.memory_free_mib.is_some()) {
            return None;
        }
        let available_mib = meminfo.and_then(meminfo_available_kib)? / 1024;
        let budget_mib = available_mib / UNIFIED_MEMORY_DIVISOR;
        tracing::info!(
            gpu_id = ?gpu_id,
            mem_available_mib = available_mib,
            budget_mib = budget_mib,
            "GPUs report no dedicated VRAM (unified memory); using {}% of system MemAvailable as the free-VRAM budget",
            100 / UNIFIED_MEMORY_DIVISOR
        );
        Some(budget_mib)
    }
}

/// On unified-memory platforms, this fraction (1/N) of `MemAvailable` is
/// treated as the free-VRAM budget. Conservative on purpose: system memory is
/// shared with everything else on the box.
const UNIFIED_MEMORY_DIVISOR: u64 = 4;

/// Parse `MemAvailable` (in KiB) from `/proc/meminfo` content.
pub fn meminfo_available_kib(meminfo: &str) -> Option<u64> {
    meminfo.lines().find_map(|line| {
        line.strip_prefix("MemAvailable:")?
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()
    })
}

/// Read `/proc/meminfo`, if it exists (Linux only).
pub fn read_proc_meminfo() -> Option<String> {
    std::fs::read_to_string("/proc/meminfo").ok()
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

/// Check every visible GPU against the compiled compute capability, and the
/// host driver against the CUDA version this image's userspace requires.
/// Returns one message per problem (empty = all good or nothing to check).
pub fn preflight(
    info: &GpuInfo,
    variant: Option<&str>,
    host: &HostDriver,
    required_cuda: Option<(u32, u32)>,
) -> Vec<String> {
    let mut msgs: Vec<String> = match variant.and_then(compiled_compute_cap) {
        Some(compiled) => info
            .devices
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
            .collect(),
        None => Vec::new(),
    };
    msgs.extend(cuda_runtime_preflight(host, required_cuda));
    msgs
}

// ============================================================================
// Driver / CUDA runtime-compat preflight
// ============================================================================

/// Environment variable baked into CUDA base images describing what the
/// image's CUDA userspace requires from the host driver, e.g.
/// `cuda>=12.9 brand=unknown,driver>=535,driver<536 ...`
pub const NVIDIA_REQUIRE_CUDA_ENV: &str = "NVIDIA_REQUIRE_CUDA";

/// Host NVIDIA driver info as reported by the `nvidia-smi` banner
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostDriver {
    /// Driver version string, e.g. "570.195.03"
    pub driver_version: Option<String>,
    /// Highest CUDA version the driver supports, e.g. (12, 8)
    pub cuda_version: Option<(u32, u32)>,
}

/// Parse a `major.minor` version like "12.8"
fn parse_version(s: &str) -> Option<(u32, u32)> {
    let (major, minor) = s.trim().split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// Extract the `cuda>=X.Y` term from an `NVIDIA_REQUIRE_CUDA` value
pub fn parse_required_cuda(require: &str) -> Option<(u32, u32)> {
    require
        .split([' ', ','])
        .find_map(|term| parse_version(term.strip_prefix("cuda>=")?))
}

/// Value following a `label` in nvidia-smi output: handles both the banner
/// (`Driver Version: 570.195.03`) and `nvidia-smi -q` (`Driver Version : ...`)
fn labeled_value(output: &str, label: &str) -> Option<String> {
    let rest = &output[output.find(label)? + label.len()..];
    let value: String = rest
        .trim_start_matches([' ', ':'])
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != '|')
        .collect();
    (!value.is_empty()).then_some(value)
}

/// Parse the driver version and its supported CUDA version out of plain
/// `nvidia-smi` (or `nvidia-smi -q`) output. Missing or `[N/A]` fields
/// become `None`; `--query-gpu` does not expose the CUDA version, which is
/// why the banner is used.
pub fn parse_host_driver(output: &str) -> HostDriver {
    HostDriver {
        driver_version: labeled_value(output, "Driver Version"),
        cuda_version: labeled_value(output, "CUDA Version")
            .as_deref()
            .and_then(parse_version),
    }
}

/// Query the host driver by running plain `nvidia-smi` (its banner carries
/// both the driver version and the CUDA version the driver supports)
pub fn detect_host_driver() -> HostDriver {
    match Command::new("nvidia-smi").output() {
        Ok(output) if output.status.success() => {
            let host = parse_host_driver(&String::from_utf8_lossy(&output.stdout));
            tracing::info!(
                driver_version = ?host.driver_version,
                cuda_version = ?host.cuda_version,
                "Detected host driver"
            );
            host
        }
        Ok(output) => {
            tracing::warn!(
                stderr = %String::from_utf8_lossy(&output.stderr),
                "nvidia-smi failed, host driver CUDA version unknown"
            );
            HostDriver::default()
        }
        Err(e) => {
            tracing::warn!(error = %e, "Failed to run nvidia-smi, host driver CUDA version unknown");
            HostDriver::default()
        }
    }
}

/// Whether the image's CUDA userspace can run on the host driver.
///
/// GeForce cards have no forward compatibility: when the host driver supports
/// an older CUDA than the image's userspace requires, TEI gets
/// `CUDA_ERROR_COMPAT_NOT_SUPPORTED_ON_DEVICE` and silently falls back to
/// CPU. Returns the preflight message when incompatible; `None` when
/// compatible or when either side is unknown (no nvidia-smi, `[N/A]`, or no
/// `NVIDIA_REQUIRE_CUDA` in the environment).
pub fn cuda_runtime_preflight(host: &HostDriver, required: Option<(u32, u32)>) -> Option<String> {
    let host_cuda = host.cuda_version?;
    let (req_major, req_minor) = required?;
    (host_cuda < (req_major, req_minor)).then(|| {
        let driver = host
            .driver_version
            .as_deref()
            .unwrap_or("(unknown version)");
        format!(
            "host driver {driver} supports CUDA {}.{} but this image's CUDA userspace requires \
             {req_major}.{req_minor}; GeForce GPUs have no forward compatibility — TEI will \
             silently fall back to CPU. Use a host with driver CUDA >= {req_major}.{req_minor}",
            host_cuda.0, host_cuda.1
        )
    })
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

    /// nvidia-smi output on a Grace unified-memory box (DGX Spark GB10)
    const SMI_UNIFIED: &str = "0, NVIDIA GB10, 12.1, [N/A], [N/A]\n";
    /// 40 GiB MemAvailable
    const MEMINFO: &str = "MemTotal:       131072000 kB\nMemFree:        10485760 kB\nMemAvailable:   41943040 kB\nBuffers:          123456 kB\n";

    #[test]
    fn test_meminfo_available_kib() {
        assert_eq!(meminfo_available_kib(MEMINFO), Some(41_943_040));
        // Missing MemAvailable
        assert_eq!(
            meminfo_available_kib("MemTotal:       131072000 kB\nMemFree: 1 kB\n"),
            None
        );
        // Garbage value / garbage content / empty
        assert_eq!(meminfo_available_kib("MemAvailable:   lots kB\n"), None);
        assert_eq!(meminfo_available_kib("complete garbage\n\x00\x01"), None);
        assert_eq!(meminfo_available_kib(""), None);
    }

    #[test]
    fn test_free_vram_mib_or_unified() {
        // Unified-memory box: no GPU reports VRAM → 25% of MemAvailable
        // (41943040 KiB = 40960 MiB → 10240 MiB)
        let unified = parse_nvidia_smi(SMI_UNIFIED);
        assert_eq!(
            unified.free_vram_mib_with_meminfo(None, Some(MEMINFO)),
            Some(10240)
        );
        assert_eq!(
            unified.free_vram_mib_with_meminfo(Some(0), Some(MEMINFO)),
            Some(10240)
        );
        // No meminfo, or meminfo without MemAvailable → None
        assert_eq!(unified.free_vram_mib_with_meminfo(None, None), None);
        assert_eq!(
            unified.free_vram_mib_with_meminfo(None, Some("MemFree: 1 kB\n")),
            None
        );

        // GPU-less host: never consults meminfo
        assert_eq!(
            GpuInfo::default().free_vram_mib_with_meminfo(None, Some(MEMINFO)),
            None
        );

        // GPUs with real VRAM: unchanged behavior, meminfo ignored
        let discrete = parse_nvidia_smi(SMI);
        assert_eq!(
            discrete.free_vram_mib_with_meminfo(None, Some(MEMINFO)),
            Some(27662)
        );
        assert_eq!(
            discrete.free_vram_mib_with_meminfo(Some(1), Some(MEMINFO)),
            Some(80000)
        );

        // Mixed: some GPU reports VRAM, so a device without memory does not
        // trigger the unified path
        let mixed = parse_nvidia_smi("0, Weird, 12.1, [N/A], [N/A]\n1, A100, 8.0, 81920, 80000\n");
        assert_eq!(
            mixed.free_vram_mib_with_meminfo(Some(0), Some(MEMINFO)),
            None
        );
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
        let no_host = HostDriver::default();
        let info = parse_nvidia_smi(SMI);
        let msgs = preflight(&info, Some(""), &no_host, None);
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        assert!(msgs[0].contains("GPU 0"));
        assert!(msgs[0].contains("12.0"));
        assert!(msgs[0].contains("sm_80"));
        assert!(msgs[0].contains("-blackwell"));

        let msgs = preflight(&info, Some("120-"), &no_host, None);
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].contains("GPU 1"));

        assert!(preflight(&info, Some("cpu-"), &no_host, None).is_empty());
        assert!(preflight(&info, None, &no_host, None).is_empty());
        assert!(preflight(&GpuInfo::default(), Some(""), &no_host, None).is_empty());
    }

    const BANNER: &str = "\
+-----------------------------------------------------------------------------------------+
| NVIDIA-SMI 570.195.03             Driver Version: 570.195.03     CUDA Version: 12.8     |
|-----------------------------------------+------------------------+----------------------+
";

    #[test]
    fn test_parse_host_driver_banner() {
        let host = parse_host_driver(BANNER);
        assert_eq!(host.driver_version.as_deref(), Some("570.195.03"));
        assert_eq!(host.cuda_version, Some((12, 8)));
    }

    #[test]
    fn test_parse_host_driver_query_format() {
        // `nvidia-smi -q` uses `label : value` lines
        let host = parse_host_driver(
            "Driver Version                            : 570.195.03\n\
             CUDA Version                              : 12.8\n",
        );
        assert_eq!(host.driver_version.as_deref(), Some("570.195.03"));
        assert_eq!(host.cuda_version, Some((12, 8)));
    }

    #[test]
    fn test_parse_host_driver_missing_or_na() {
        let host = parse_host_driver("");
        assert_eq!(host, HostDriver::default());

        let host = parse_host_driver("| Driver Version: 570.195.03  CUDA Version: [N/A]  |");
        assert_eq!(host.driver_version.as_deref(), Some("570.195.03"));
        assert_eq!(host.cuda_version, None);

        assert_eq!(parse_host_driver("total garbage"), HostDriver::default());
    }

    #[test]
    fn test_parse_required_cuda() {
        // Real-world multi-clause value from a CUDA base image
        let require = "cuda>=12.9 brand=unknown,driver>=470,driver<471 \
                       brand=tesla,driver>=535,driver<536 brand=nvidia,driver>=550,driver<551";
        assert_eq!(parse_required_cuda(require), Some((12, 9)));
        // The cuda term does not have to come first
        assert_eq!(
            parse_required_cuda("brand=tesla,driver>=535,cuda>=13.0"),
            Some((13, 0))
        );
        assert_eq!(parse_required_cuda(""), None);
        assert_eq!(parse_required_cuda("brand=unknown,driver>=535"), None);
        assert_eq!(parse_required_cuda("cuda>=garbage"), None);
    }

    #[test]
    fn test_cuda_version_ordering() {
        // Tuple ordering matches CUDA version semantics
        assert!((12, 8) < (12, 9));
        assert!((12, 9) == (12, 9));
        assert!((13, 0) > (12, 9));
        assert!((12, 10) < (13, 0));
    }

    #[test]
    fn test_cuda_runtime_preflight_messages() {
        let host = parse_host_driver(BANNER);

        // The real incident: driver 570 (CUDA 12.8) under a CUDA 12.9 image
        let msg = cuda_runtime_preflight(&host, Some((12, 9))).expect("must flag mismatch");
        assert!(msg.contains("570.195.03"), "{msg}");
        assert!(msg.contains("supports CUDA 12.8"), "{msg}");
        assert!(msg.contains("requires 12.9"), "{msg}");
        assert!(msg.contains("fall back to CPU"), "{msg}");
        assert!(msg.contains("driver CUDA >= 12.9"), "{msg}");

        // Exact match and newer host are fine
        assert_eq!(cuda_runtime_preflight(&host, Some((12, 8))), None);
        assert_eq!(cuda_runtime_preflight(&host, Some((12, 7))), None);

        // Unknown on either side → no check
        assert_eq!(cuda_runtime_preflight(&host, None), None);
        assert_eq!(
            cuda_runtime_preflight(&HostDriver::default(), Some((12, 9))),
            None
        );

        // Driver version unknown but CUDA version known: still flagged
        let cuda_only = HostDriver {
            driver_version: None,
            cuda_version: Some((12, 8)),
        };
        let msg = cuda_runtime_preflight(&cuda_only, Some((12, 9))).unwrap();
        assert!(msg.contains("(unknown version)"), "{msg}");

        // Wired into preflight(): compute-cap and CUDA problems both reported
        let info = parse_nvidia_smi(SMI);
        let msgs = preflight(&info, Some(""), &host, Some((12, 9)));
        assert_eq!(msgs.len(), 2, "{msgs:?}");
        assert!(msgs[1].contains("fall back to CPU"));
    }
}
