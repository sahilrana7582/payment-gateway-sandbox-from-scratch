//! Boot sequence: pool → migrations → state → HTTP server + job worker,
//! all sharing one cancellation token for graceful shutdown.

use std::sync::Arc;

use domain::clock::SystemClock;
use queue::worker::{Worker, WorkerConfig};
use tokio_util::sync::CancellationToken;
use tracing::info;
use webhooks::{worker::WebhookDeliveryHandler, WebhookConfig};

use crate::config::Config;

pub async fn run(config: Config) -> anyhow::Result<()> {
    config.validate()?;

    let pool = store::pool::connect(&store::pool::PoolConfig::new(&config.database_url)).await?;
    store::pool::run_migrations(&pool).await?;
    info!("migrations applied");

    let clock: Arc<dyn domain::clock::Clock> = Arc::new(SystemClock);

    let state = api::AppState::new(
        pool.clone(),
        clock.clone(),
        config.fingerprint_pepper.clone(),
        config.simulate_latency,
    );
    let app = api::router(state);

    // One token governs everything: ctrl-c cancels it, the HTTP server drains,
    // the worker loop exits.
    let shutdown = CancellationToken::new();

    // Background job worker — webhook delivery for now. The dispute and
    // risk-review handlers register here when those milestones land; until
    // then their jobs die cleanly as "no handler registered".
    let mut worker = Worker::new(pool.clone(), clock.clone(), WorkerConfig::default());
    worker.register(Arc::new(WebhookDeliveryHandler::new(
        clock.clone(),
        WebhookConfig::default(),
    )));
    let worker_task = tokio::spawn(worker.run(shutdown.clone()));

    // Ctrl-C → cancel.
    {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            info!("shutdown signal received");
            shutdown.cancel();
        });
    }

    let addr = config.bind_addr();
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!(%addr, "payment sandbox listening — TEST MODE, no real money");

    let server_shutdown = shutdown.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { server_shutdown.cancelled().await })
        .await?;

    // Server drained; make sure the worker is down too.
    shutdown.cancel();
    let _ = worker_task.await;
    info!("goodbye");

    Ok(())
}
