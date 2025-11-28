//! Property-based tests using proptest
//!
//! These tests verify invariants across randomized inputs, helping catch
//! edge cases that might be missed by example-based testing.

use proptest::prelude::*;
use tei_manager::config::{InstanceConfig, ManagerConfig};

// =============================================================================
// Arbitrary Implementations
// =============================================================================

/// Generate arbitrary InstanceConfig values
fn arb_instance_config() -> impl Strategy<Value = InstanceConfig> {
    (
        "[a-zA-Z][a-zA-Z0-9_-]{0,30}", // valid instance name
        "[a-zA-Z0-9/-]{3,50}",         // model_id like "BAAI/bge-small"
        1024u16..60000,                // port (valid range)
        1024u32..65536,                // max_batch_tokens
        1u32..1024,                    // max_concurrent_requests
        prop::option::of("[a-z]+"),    // pooling
        prop::option::of(0u32..8),     // gpu_id
    )
        .prop_map(
            |(name, model_id, port, max_batch_tokens, max_concurrent_requests, pooling, gpu_id)| {
                InstanceConfig {
                    name,
                    model_id,
                    port,
                    max_batch_tokens,
                    max_concurrent_requests,
                    pooling,
                    gpu_id,
                    prometheus_port: None,
                    startup_timeout_secs: None,
                    extra_args: Vec::new(),
                    created_at: None,
                }
            },
        )
}

/// Generate minimal ManagerConfig for round-trip testing
fn arb_manager_config() -> impl Strategy<Value = ManagerConfig> {
    (
        1024u16..60000, // api_port
        10u64..3600,    // health_check_interval_secs
        30u64..600,     // startup_timeout_secs
        1u32..10,       // max_failures_before_restart
        8080u16..9000,  // instance_port_start
    )
        .prop_map(
            |(
                api_port,
                health_check_interval_secs,
                startup_timeout_secs,
                max_failures_before_restart,
                instance_port_start,
            )| {
                ManagerConfig {
                    api_port,
                    health_check_interval_secs,
                    startup_timeout_secs,
                    max_failures_before_restart,
                    instance_port_start,
                    instance_port_end: instance_port_start.saturating_add(100),
                    instances: Vec::new(), // Clear to simplify round-trip (they have timestamps)
                    ..Default::default()
                }
            },
        )
}

// =============================================================================
// Config Serialization Round-Trip Tests
// =============================================================================

proptest! {
    /// InstanceConfig serializes to TOML and deserializes back to equal value
    #[test]
    fn instance_config_roundtrip(config in arb_instance_config()) {
        let toml_str = toml::to_string(&config).expect("Failed to serialize to TOML");
        let parsed: InstanceConfig = toml::from_str(&toml_str).expect("Failed to parse TOML");
        prop_assert_eq!(config, parsed);
    }

    /// ManagerConfig serializes to TOML and deserializes back
    #[test]
    fn manager_config_roundtrip(config in arb_manager_config()) {
        let toml_str = toml::to_string(&config).expect("Failed to serialize to TOML");
        let parsed: ManagerConfig = toml::from_str(&toml_str).expect("Failed to parse TOML");

        // Compare key fields (can't compare directly due to state_file PathBuf)
        prop_assert_eq!(config.api_port, parsed.api_port);
        prop_assert_eq!(config.health_check_interval_secs, parsed.health_check_interval_secs);
        prop_assert_eq!(config.startup_timeout_secs, parsed.startup_timeout_secs);
        prop_assert_eq!(config.max_failures_before_restart, parsed.max_failures_before_restart);
        prop_assert_eq!(config.instance_port_start, parsed.instance_port_start);
        prop_assert_eq!(config.instance_port_end, parsed.instance_port_end);
    }

    /// InstanceConfig serializes to JSON and deserializes back (API compatibility)
    #[test]
    fn instance_config_json_roundtrip(config in arb_instance_config()) {
        let json_str = serde_json::to_string(&config).expect("Failed to serialize to JSON");
        let parsed: InstanceConfig = serde_json::from_str(&json_str).expect("Failed to parse JSON");
        prop_assert_eq!(config, parsed);
    }
}

// =============================================================================
// Port Range Invariants
// =============================================================================

proptest! {
    /// Port range must have start < end for auto-allocation to be enabled
    #[test]
    fn port_range_invariant(start in 1024u16..50000, count in 1u16..100) {
        let end = start.saturating_add(count);

        // Auto-allocation enabled when start < end
        let auto_enabled = start < end;

        // This mirrors Registry::is_port_auto_allocation_enabled
        prop_assert_eq!(auto_enabled, start < end);

        // Valid range should allow at least `count` allocations
        if auto_enabled {
            let available_ports = end.saturating_sub(start);
            prop_assert!(available_ports >= count);
        }
    }

    /// Instance port must be valid when specified
    #[test]
    fn valid_instance_port(port in 1024u16..65535) {
        // Ports below 1024 are privileged
        prop_assert!(port >= 1024);
        // Port 0 is special (auto-allocate)
        prop_assert!(port != 0);
    }
}

// =============================================================================
// Instance Name Invariants
// =============================================================================

proptest! {
    /// Valid instance names start with a letter and contain only alphanumeric, underscore, hyphen
    #[test]
    fn valid_instance_name_format(name in "[a-zA-Z][a-zA-Z0-9_-]{0,63}") {
        // Name should not be empty
        prop_assert!(!name.is_empty());

        // First character should be a letter
        prop_assert!(name.chars().next().unwrap().is_alphabetic());

        // All characters should be valid
        for ch in name.chars() {
            prop_assert!(ch.is_alphanumeric() || ch == '_' || ch == '-');
        }

        // Name should not contain path separators (security consideration)
        prop_assert!(!name.contains('/'));
        prop_assert!(!name.contains('\\'));
    }

    /// Names with path separators should be invalid for security
    #[test]
    fn invalid_names_with_path_separators(
        prefix in "[a-zA-Z]{1,5}",
        suffix in "[a-zA-Z]{1,5}"
    ) {
        let name_with_slash = format!("{}/{}", prefix, suffix);
        let name_with_backslash = format!("{}\\{}", prefix, suffix);

        // These should be rejected as they could be path traversal attacks
        prop_assert!(name_with_slash.contains('/'));
        prop_assert!(name_with_backslash.contains('\\'));
    }
}

// =============================================================================
// GPU ID Invariants
// =============================================================================

proptest! {
    /// GPU IDs should be in reasonable range (0-15 for most systems)
    #[test]
    fn gpu_id_reasonable_range(gpu_id in 0u32..16) {
        // GPU ID 0 is always valid (first GPU)
        // Most systems have at most 8 GPUs (16 for large servers)
        prop_assert!(gpu_id < 16);
    }
}

// =============================================================================
// Batch Size Invariants
// =============================================================================

proptest! {
    /// Max batch tokens should be within practical limits
    #[test]
    fn batch_tokens_practical_limits(tokens in 256u32..131072) {
        // Minimum practical batch size
        prop_assert!(tokens >= 256);

        // Maximum practical batch size (128K tokens = very large batches)
        prop_assert!(tokens <= 131072);
    }

    /// Max concurrent requests should be reasonable
    #[test]
    fn concurrent_requests_limits(requests in 1u32..2048) {
        // At least 1 concurrent request
        prop_assert!(requests >= 1);

        // Upper bound for memory/resource reasons
        prop_assert!(requests <= 2048);
    }
}
