//! Serves optimization, health, and reflection; drains native workers on shutdown.

use grpc_optimizer::{
    config::Config,
    engine::Engine,
    grpc::Service,
    pb::{DESCRIPTOR, optimizer_server::OptimizerServer},
};
use tonic::transport::Server;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("grpc_optimizer=info")),
        )
        .init();
    let config = Config::from_env()?;
    let engine = Engine::new(config.clone())?;
    let reflection = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(DESCRIPTOR)
        .build_v1()?;
    let (reporter, health) = tonic_health::server::health_reporter();
    reporter.set_serving::<OptimizerServer<Service>>().await;
    let service = Service::new(engine.clone()).into_server();
    let shutdown_engine = engine.clone();
    tracing::info!(address = %config.listen_addr, ?config, "optimizer listening");
    let result = Server::builder()
        .add_service(health)
        .add_service(reflection)
        .add_service(service)
        .serve_with_shutdown(config.listen_addr, async move {
            shutdown_signal().await;
            reporter.set_not_serving::<OptimizerServer<Service>>().await;
            shutdown_engine.shutdown();
            tracing::info!("stopped admissions; draining native work");
        })
        .await;
    engine.shutdown();
    engine.drain().await;
    result?;
    Ok(())
}

/// Waits for Ctrl-C or, on Unix, SIGTERM before starting graceful shutdown.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
