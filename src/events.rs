//! NATS event publication helpers.

use anyhow::Context;
use tracing::info;

use crate::contracts::JobLifecycleEvent;

pub(crate) async fn publish_job_event(
    nats: &async_nats::Client,
    event: JobLifecycleEvent,
) -> anyhow::Result<()> {
    let payload = serde_json::to_vec(&event).context("failed to serialize job lifecycle event")?;
    nats.publish("elowen.jobs.events".to_string(), payload.into())
        .await
        .context("failed to publish job lifecycle event")?;
    info!(
        job_id = %event.job_id,
        correlation_id = %event.correlation_id,
        event_type = %event.event_type,
        "published job lifecycle event"
    );
    Ok(())
}
