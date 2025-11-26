//! Smoke tests for the main binary and bench-client
//!
//! These tests verify the binaries can be invoked and respond correctly.
//! Run with: `cargo test --test e2e_smoke`

use std::process::Command;

/// Test that tei-manager --help works
#[test]
fn test_main_binary_help() {
    let output = Command::new("cargo")
        .args(["run", "--bin", "tei-manager", "--", "--help"])
        .output()
        .expect("Failed to run tei-manager");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Should succeed
    assert!(
        output.status.success(),
        "tei-manager --help failed: stdout={}, stderr={}",
        stdout,
        stderr
    );

    // Should show usage info
    assert!(
        stdout.contains("Usage:") || stdout.contains("tei-manager"),
        "Expected help output, got: {}",
        stdout
    );
}

/// Test that tei-manager --version works
#[test]
fn test_main_binary_version() {
    let output = Command::new("cargo")
        .args(["run", "--bin", "tei-manager", "--", "--version"])
        .output()
        .expect("Failed to run tei-manager");

    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(output.status.success(), "tei-manager --version failed");
    assert!(
        stdout.contains("tei-manager") || stdout.contains("0."),
        "Expected version output, got: {}",
        stdout
    );
}

/// Test that bench-client --help works
#[test]
fn test_bench_client_help() {
    let output = Command::new("cargo")
        .args(["run", "--bin", "bench-client", "--", "--help"])
        .output()
        .expect("Failed to run bench-client");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "bench-client --help failed: stdout={}, stderr={}",
        stdout,
        stderr
    );

    // Should show benchmark options
    assert!(
        stdout.contains("endpoint") || stdout.contains("Benchmark"),
        "Expected help output with endpoint option, got: {}",
        stdout
    );
}

/// Test that bench-client validates required arguments
#[test]
fn test_bench_client_missing_args() {
    let output = Command::new("cargo")
        .args(["run", "--bin", "bench-client"])
        .output()
        .expect("Failed to run bench-client");

    // Should fail without required args
    assert!(
        !output.status.success(),
        "bench-client should fail without arguments"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    // Should mention missing required arguments
    assert!(
        stderr.contains("required") || stderr.contains("endpoint") || stderr.contains("error"),
        "Expected error about missing args, got: {}",
        stderr
    );
}

/// Test that main binary fails gracefully with invalid config
#[test]
fn test_main_binary_invalid_config() {
    let output = Command::new("cargo")
        .args([
            "run",
            "--bin",
            "tei-manager",
            "--",
            "--config",
            "/nonexistent/config.toml",
        ])
        .output()
        .expect("Failed to run tei-manager");

    // Should fail with invalid config path
    assert!(
        !output.status.success(),
        "tei-manager should fail with invalid config"
    );
}
