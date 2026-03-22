use std::time::Duration;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    info!("starting elowen-edge scaffold");
    let mut ticker = tokio::time::interval(Duration::from_secs(60));

    loop {
        ticker.tick().await;
        info!("elowen-edge idle heartbeat");
    }
}
