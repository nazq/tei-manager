//! TEI Manager - Main entry point

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tei_manager::{HealthMonitor, Registry, StateManager, api, config::ManagerConfig, metrics};
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

    // Initialize registry
    let registry = Arc::new(Registry::new(config.max_instances));

    // Initialize state manager
    let state_manager = Arc::new(StateManager::new(
        config.state_file.clone(),
        registry.clone(),
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
        for instance_config in config.instances {
            match registry.add(instance_config.clone()).await {
                Ok(instance) => {
                    if let Err(e) = instance.start().await {
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
        config.health_check_initial_delay_secs,
        config.max_failures_before_restart,
        true, // auto_restart
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
    };

    let app = api::create_router(app_state);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], config.api_port));
    tracing::info!(addr = %addr, "Starting API server");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .context("Failed to bind API server")?;

    // Graceful shutdown
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("API server error")?;

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
