//! agentos-server - the daemon entry point.
//!
//! Equivalent to "agentos start" but with no CLI parsing, so it can be supervised by a process
//! manager. Everything it does is bootstrap, serve, wait, drain.

use agentos_core::config::RuntimeConfig;
use agentos_core::error::Result;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("agentos-server failed [{}, retryable={}]: {}", e.code(), e.is_retryable(), e.message);
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let config = RuntimeConfig::load()?;
    agentos_core::telemetry::init_tracing(
        &config.observability.log_level,
        matches!(config.observability.log_format, agentos_core::config::LogFormat::Json),
    )?;

    let http_addr = config.api.http_addr.clone();
    let grpc_addr = config.api.grpc_addr.clone();
    let node = config.node.name.clone();

    let kernel: Arc<agentos_kernel::Kernel> = agentos_kernel::Kernel::bootstrap(config).await?;
    let shutdown = tokio_util::sync::CancellationToken::new();

    {
        let kernel = kernel.clone();
        let token = shutdown.clone();
        tokio::spawn(async move {
            if let Err(e) = agentos_api::serve(kernel, token).await {
                // A runtime without its gateway answers nothing: fail loudly instead of leaving a
                // process that looks alive under a supervisor.
                tracing::error!(error = %e, "http gateway stopped, exiting");
                std::process::exit(1);
            }
        });
    }
    {
        let kernel = kernel.clone();
        let token = shutdown.clone();
        let grpc_bind = grpc_addr.clone();
        tokio::spawn(async move {
            match agentos_kernel::transports::serve_grpc(kernel, grpc_bind, token).await {
                Ok(addr) => tracing::info!(%addr, "grpc endpoint listening"),
                Err(e) => tracing::error!(error = %e, "grpc server stopped"),
            }
        });
    }

    tracing::info!(
        node = %node,
        http = %http_addr,
        grpc = %grpc_addr,
        workspace = %kernel.config.policy.workspace_root.display(),
        data_dir = %kernel.config.storage.data_dir.display(),
        "agentos runtime ready"
    );

    wait_for_shutdown().await;
    tracing::info!("shutdown signal received, draining");
    shutdown.cancel();
    kernel.shutdown().await;
    tracing::info!("bye");
    Ok(())
}

async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
