//! TEI Manager - Main entry point

use anyhow::{Context, Result};
use clap::Parser;
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::Arc;
use tei_manager::{
    HealthMonitor, Registry, StateManager, api,
    auth::{AuthManager, MtlsProvider},
    config::ManagerConfig,
    metrics,
};
use tokio::signal;

#[derive(Parser, Debug)]
#[command(name = "tei-manager")]
#[command(about = "Dynamic TEI Instance Manager", long_about = None)]
#[command(version)]
struct Cli {
    /// Path to configuration file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Override API port
    #[arg(long)]
    port: Option<u16>,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, default_value = "info")]
    log_level: String,

    /// Log format (json or pretty)
    #[arg(long, default_value = "json")]
    log_format: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Install rustls crypto provider globally (required for rustls 0.23+)
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = Cli::parse();

    // Setup logging
    match cli.log_format.as_str() {
        "pretty" => {
            tracing_subscriber::fmt()
                .with_env_filter(&cli.log_level)
                .init();
        }
        _ => {
            tracing_subscriber::fmt()
                .with_env_filter(&cli.log_level)
                .json()
                .init();
        }
    }

    tracing::info!("Starting TEI Manager");

    // Detect available GPUs
    let gpu_info = tei_manager::gpu::init();
    if gpu_info.count() == 0 {
        tracing::warn!("No GPUs detected - TEI instances will run on CPU");
    } else {
        tracing::info!(
            gpu_count = gpu_info.count(),
            cuda_visible_devices = %gpu_info.cuda_visible_devices,
            "GPU detection complete"
        );
    }

    // Load configuration
    let mut config = ManagerConfig::load(cli.config)?;

    // CLI overrides
    if let Some(port) = cli.port {
        config.api_port = port;
    }

    config.validate()?;

    tracing::info!(
        api_port = config.api_port,
        state_file = ?config.state_file,
        max_instances = ?config.max_instances,
        "Configuration loaded"
    );

    // Setup metrics
    let prometheus_handle = metrics::setup_metrics()?;

    // Build auth manager if enabled
    let auth_manager = build_auth_manager(&config)?;

    // Initialize registry
    let registry = Arc::new(Registry::new(
        config.max_instances,
        config.tei_binary_path.clone(),
        config.instance_port_start,
        config.instance_port_end,
    ));

    // Initialize state manager
    let state_manager = Arc::new(StateManager::new(
        config.state_file.clone(),
        registry.clone(),
        config.tei_binary_path.clone(),
    ));

    // Restore instances or seed from config
    if config.auto_restore_on_restart {
        tracing::info!("Auto-restore enabled, restoring instances from state");
        state_manager.restore().await?;
    } else if !config.instances.is_empty() {
        tracing::info!(
            count = config.instances.len(),
            "Seeding instances from config"
        );
        for instance_config in &config.instances {
            match registry.add(instance_config.clone()).await {
                Ok(instance) => {
                    if let Err(e) = instance.start(&config.tei_binary_path).await {
                        tracing::error!(
                            error = %e,
                            instance = %instance_config.name,
                            "Failed to start seeded instance"
                        );
                    }
                }
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        instance = %instance_config.name,
                        "Failed to add seeded instance"
                    );
                }
            }
        }
    }

    // Start health monitor
    let health_monitor = Arc::new(HealthMonitor::new(
        registry.clone(),
        config.health_check_interval_secs,
        config.startup_timeout_secs,
        config.max_failures_before_restart,
        true, // auto_restart
        config.tei_binary_path.clone(),
    ));

    let monitor_handle = tokio::spawn({
        let monitor = health_monitor.clone();
        async move {
            monitor.run().await;
        }
    });

    // Setup API
    let app_state = api::AppState {
        registry: registry.clone(),
        state_manager: state_manager.clone(),
        prometheus_handle,
        auth_manager: auth_manager.clone(),
    };

    let app = api::create_router(app_state);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], config.api_port));

    // Build TLS configuration if mTLS is enabled
    let tls_config = build_tls_config(&config)?;

    // Start gRPC server in background if enabled
    let grpc_handle = if config.grpc_enabled {
        let grpc_addr = std::net::SocketAddr::from(([0, 0, 0, 0], config.grpc_port));
        let grpc_registry = registry.clone();
        let grpc_max_message_size_mb = config.grpc_max_message_size_mb;
        let grpc_max_parallel_streams = config.grpc_max_parallel_streams;

        // Build gRPC TLS config if mTLS is enabled
        let grpc_tls_config =
            if config.auth.enabled && config.auth.providers.contains(&"mtls".to_string()) {
                let mtls_config = config.auth.mtls.as_ref().expect("mTLS config should exist");

                // Load certificate files as strings for tonic
                let cert_pem = std::fs::read_to_string(&mtls_config.server_cert)
                    .context("Failed to read server certificate for gRPC")?;
                let key_pem = std::fs::read_to_string(&mtls_config.server_key)
                    .context("Failed to read server key for gRPC")?;
                let ca_pem = std::fs::read_to_string(&mtls_config.ca_cert)
                    .context("Failed to read CA certificate for gRPC")?;

                Some((cert_pem, key_pem, ca_pem))
            } else {
                None
            };

        Some(tokio::spawn(async move {
            tracing::info!(addr = %grpc_addr, "Starting gRPC multiplexer server");
            if let Err(e) = tei_manager::grpc::server::start_grpc_server(
                grpc_addr,
                grpc_registry,
                grpc_tls_config,
                grpc_max_message_size_mb,
                grpc_max_parallel_streams,
            )
            .await
            {
                tracing::error!(error = %e, "gRPC server error");
            }
        }))
    } else {
        tracing::info!("gRPC multiplexer disabled");
        None
    };

    // Run HTTP server with graceful shutdown
    // If gRPC is enabled, both servers run concurrently
    if let Some(tls_config) = tls_config {
        tracing::info!(addr = %addr, "Starting HTTPS API server with mTLS");
        let rustls_config =
            axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(tls_config));
        tokio::select! {
            result = axum_server::bind_rustls(addr, rustls_config)
                .serve(app.into_make_service())
                => {
                result.context("HTTPS API server error")?;
            }
            _ = async {
                if let Some(handle) = grpc_handle {
                    let _ = handle.await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                tracing::error!("gRPC server exited unexpectedly");
            }
            _ = shutdown_signal() => {
                tracing::info!("Shutdown signal received");
            }
        }
    } else {
        tracing::info!(addr = %addr, "Starting HTTP API server (no TLS)");
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .context("Failed to bind API server")?;

        tokio::select! {
            result = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()) => {
                result.context("HTTP API server error")?;
            }
            _ = async {
                if let Some(handle) = grpc_handle {
                    let _ = handle.await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                tracing::error!("gRPC server exited unexpectedly");
            }
        }
    }

    tracing::info!("Shutting down...");

    // Stop all instances
    tracing::info!("Stopping all instances");
    for instance in registry.list().await {
        if let Err(e) = instance.stop().await {
            tracing::error!(
                instance = %instance.config.name,
                error = %e,
                "Failed to stop instance during shutdown"
            );
        }
    }

    // Save final state
    tracing::info!("Saving final state");
    state_manager.save().await?;

    // Cancel health monitor
    monitor_handle.abort();

    tracing::info!("Shutdown complete");

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            tracing::info!("Received Ctrl+C signal");
        },
        _ = terminate => {
            tracing::info!("Received SIGTERM signal");
        },
    }
}

/// Build AuthManager from configuration
fn build_auth_manager(config: &ManagerConfig) -> Result<Option<Arc<AuthManager>>> {
    if !config.auth.enabled {
        tracing::info!("Authentication disabled");
        return Ok(None);
    }

    tracing::info!(
        providers = ?config.auth.providers,
        "Building authentication manager"
    );

    let mut providers: Vec<Arc<dyn tei_manager::auth::AuthProvider>> = Vec::new();

    // Build mTLS provider if configured
    if config.auth.providers.contains(&"mtls".to_string()) {
        let mtls_config = config
            .auth
            .mtls
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("mTLS provider enabled but no mTLS config found"))?;

        tracing::info!(
            ca_cert = ?mtls_config.ca_cert,
            server_cert = ?mtls_config.server_cert,
            "Initializing mTLS provider"
        );

        let mtls_provider =
            MtlsProvider::new(mtls_config.clone()).context("Failed to create mTLS provider")?;

        providers.push(Arc::new(mtls_provider));
    }

    if providers.is_empty() {
        anyhow::bail!("Auth enabled but no providers configured");
    }

    tracing::info!(provider_count = providers.len(), "Auth manager initialized");

    Ok(Some(Arc::new(AuthManager::new(providers))))
}

/// Build TLS configuration for native mTLS
fn build_tls_config(config: &ManagerConfig) -> Result<Option<rustls::ServerConfig>> {
    // Only build TLS config if auth is enabled with mTLS
    if !config.auth.enabled || !config.auth.providers.contains(&"mtls".to_string()) {
        return Ok(None);
    }

    let mtls_config = config
        .auth
        .mtls
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("mTLS provider enabled but no mTLS config found"))?;

    tracing::info!("Building native TLS configuration for mTLS");

    // Load server certificate and key
    let cert_file =
        File::open(&mtls_config.server_cert).context("Failed to open server certificate")?;
    let key_file = File::open(&mtls_config.server_key).context("Failed to open server key")?;

    let mut cert_reader = BufReader::new(cert_file);
    let mut key_reader = BufReader::new(key_file);

    let certs = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .context("Failed to parse server certificate")?;

    let key = rustls_pemfile::private_key(&mut key_reader)
        .context("Failed to read private key")?
        .ok_or_else(|| anyhow::anyhow!("No private key found"))?;

    // Load CA certificate for client verification
    let ca_file = File::open(&mtls_config.ca_cert).context("Failed to open CA certificate")?;
    let mut ca_reader = BufReader::new(ca_file);

    let ca_certs = rustls_pemfile::certs(&mut ca_reader)
        .collect::<Result<Vec<_>, _>>()
        .context("Failed to parse CA certificate")?;

    // Build client certificate verifier
    let mut root_store = rustls::RootCertStore::empty();
    for cert in ca_certs {
        root_store
            .add(cert)
            .context("Failed to add CA cert to root store")?;
    }

    let client_verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(root_store))
        .build()
        .context("Failed to build client verifier")?;

    // Build TLS config with client authentication
    let tls_config = rustls::ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(certs, key)
        .context("Failed to build TLS config")?;

    tracing::info!("Native TLS configuration built successfully");

    Ok(Some(tls_config))
}
