//! Runtime startup and subscription loops.

use anyhow::Context;
use chrono::Utc;
use futures_util::StreamExt;
use reqwest::Client as HttpClient;
use std::{sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::{
    config::{EdgeCommand, EdgeConfig, parse_command},
    contracts::{
        AvailabilityProbeMessage, AvailabilitySnapshot, JobApprovalCommand, JobDispatchMessage,
    },
    execution::{handle_job_approval, handle_job_dispatch, preflight_codex_runner},
    registration::{print_trust_keypair, register_device},
    status::{EdgeStatus, write_status},
};

/// Initializes process-wide tracing before the async runtime starts.
fn init_tracing(service_name: &'static str, config: &EdgeConfig) {
    let env_filter = config
        .rust_log
        .clone()
        .map(tracing_subscriber::EnvFilter::new)
        .or_else(|| tracing_subscriber::EnvFilter::try_from_default_env().ok())
        .unwrap_or_else(|| tracing_subscriber::EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(true);

    if config.log_format.eq_ignore_ascii_case("json") {
        builder
            .json()
            .with_current_span(false)
            .with_span_list(false)
            .flatten_event(true)
            .with_ansi(false)
            .init();
    } else {
        builder.with_ansi(true).init();
    }

    info!(service = service_name, log_format = %config.log_format, "tracing initialized");
}

/// Starts the edge runtime.
pub fn run() -> anyhow::Result<()> {
    match parse_command()? {
        EdgeCommand::Run { config_path } => {
            let config = EdgeConfig::load(&config_path)?;
            init_tracing("elowen-edge", &config);
            info!(path = %config.config_path.display(), "loaded edge TOML config");
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("failed to build tokio runtime")?;
            runtime.block_on(async_main(config))
        }
        EdgeCommand::Tui { config_path } => crate::tui::run(config_path),
        EdgeCommand::ImportEnv {
            env_file,
            config_path,
        } => crate::config::import_env_file(&env_file, &config_path),
        EdgeCommand::GenerateTrustKeypair => {
            print_trust_keypair();
            Ok(())
        }
    }
}

/// Builds the runtime dependencies, registers the device, and starts the
/// long-lived NATS subscriptions for probes, dispatch, and approval messages.
async fn async_main(config: EdgeConfig) -> anyhow::Result<()> {
    let mut status = EdgeStatus::new(&config);
    write_status(&config.status_path, &status).await?;
    info!(
        sandbox_mode = %config.sandbox_mode.as_str(),
        "edge sandbox mode configured"
    );
    preflight_codex_runner(&config).await?;
    let http = HttpClient::builder()
        .build()
        .context("failed to build HTTP client")?;
    let nats = async_nats::connect(&config.nats_url)
        .await
        .context("failed to connect to NATS")?;
    status.mark_nats("connected");
    write_status(&config.status_path, &status).await?;
    let active_job_id = Arc::new(Mutex::new(None::<String>));

    loop {
        match register_device(&http, &config).await {
            Ok(()) => break,
            Err(error) => {
                status.mark_registration_error(
                    error.to_string(),
                    crate::registration::registration_error_code(&error).map(str::to_string),
                );
                write_status(&config.status_path, &status).await?;
                warn!(error = %error, "initial device registration failed; retrying");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    status.mark_registration_success();
    write_status(&config.status_path, &status).await?;
    info!(device_id = %config.device_id, "registered edge device");

    let heartbeat_http = http.clone();
    let heartbeat_config = config.clone();
    let heartbeat_status_path = config.status_path.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(30));
        let mut heartbeat_status = EdgeStatus::new(&heartbeat_config);
        heartbeat_status.mark_nats("connected");

        loop {
            ticker.tick().await;
            match register_device(&heartbeat_http, &heartbeat_config).await {
                Ok(()) => {
                    heartbeat_status.mark_registration_success();
                    let _ = write_status(&heartbeat_status_path, &heartbeat_status).await;
                    info!(device_id = %heartbeat_config.device_id, "edge registration heartbeat")
                }
                Err(error) => {
                    heartbeat_status.mark_registration_error(
                        error.to_string(),
                        crate::registration::registration_error_code(&error).map(str::to_string),
                    );
                    let _ = write_status(&heartbeat_status_path, &heartbeat_status).await;
                    warn!(error = %error, "edge registration heartbeat failed")
                }
            }
        }
    });

    let subject = format!("elowen.devices.availability.probe.{}", config.device_id);
    let mut subscription = nats
        .subscribe(subject.clone())
        .await
        .context("failed to subscribe to availability probes")?;
    let dispatch_subject = format!("elowen.jobs.dispatch.{}", config.device_id);
    let mut dispatch_subscription = nats
        .subscribe(dispatch_subject.clone())
        .await
        .context("failed to subscribe to job dispatch")?;
    let approval_subject = format!("elowen.jobs.approvals.{}", config.device_id);
    let mut approval_subscription = nats
        .subscribe(approval_subject.clone())
        .await
        .context("failed to subscribe to approval commands")?;

    info!(subject = %subject, "awaiting availability probes");
    info!(subject = %dispatch_subject, "awaiting job dispatches");
    info!(subject = %approval_subject, "awaiting approval commands");

    let dispatch_config = config.clone();
    let dispatch_nats = nats.clone();
    let dispatch_active_job_id = active_job_id.clone();
    tokio::spawn(async move {
        while let Some(message) = dispatch_subscription.next().await {
            let dispatch: JobDispatchMessage = match serde_json::from_slice(&message.payload) {
                Ok(dispatch) => dispatch,
                Err(error) => {
                    warn!(error = %error, "failed to decode job dispatch");
                    continue;
                }
            };

            if dispatch.device_id != dispatch_config.device_id {
                warn!(
                    expected_device_id = %dispatch_config.device_id,
                    received_device_id = %dispatch.device_id,
                    "ignoring mismatched job dispatch"
                );
                continue;
            }

            info!(
                job_id = %dispatch.job_id,
                correlation_id = %dispatch.correlation_id,
                short_id = %dispatch.short_id,
                target_kind = %dispatch.target_kind.as_str(),
                target_name = %dispatch.target_name(),
                branch_name = %dispatch.branch_name.as_deref().unwrap_or("n/a"),
                "received job dispatch"
            );
            let dispatch_job_id = dispatch.job_id.clone();
            let dispatch_correlation_id = dispatch.correlation_id.clone();

            if let Err(error) = handle_job_dispatch(
                dispatch,
                dispatch_config.clone(),
                dispatch_nats.clone(),
                dispatch_active_job_id.clone(),
            )
            .await
            {
                warn!(
                    job_id = %dispatch_job_id,
                    correlation_id = %dispatch_correlation_id,
                    error = %error,
                    "job dispatch handler failed"
                );
            }
        }
    });

    let approval_config = config.clone();
    let approval_nats = nats.clone();
    let approval_active_job_id = active_job_id.clone();
    tokio::spawn(async move {
        while let Some(message) = approval_subscription.next().await {
            let command: JobApprovalCommand = match serde_json::from_slice(&message.payload) {
                Ok(command) => command,
                Err(error) => {
                    warn!(error = %error, "failed to decode approval command");
                    continue;
                }
            };

            if command.device_id != approval_config.device_id {
                warn!(
                    expected_device_id = %approval_config.device_id,
                    received_device_id = %command.device_id,
                    "ignoring mismatched approval command"
                );
                continue;
            }

            info!(
                job_id = %command.job_id,
                correlation_id = %command.correlation_id,
                approval_id = %command.approval_id,
                target_kind = %command.target_kind.as_str(),
                target_name = %command.target_name(),
                branch_name = %command.branch_name.as_deref().unwrap_or("n/a"),
                "received approval command"
            );

            if let Err(error) = handle_job_approval(
                command,
                approval_config.clone(),
                approval_nats.clone(),
                approval_active_job_id.clone(),
            )
            .await
            {
                warn!(error = %error, "job approval handler failed");
            }
        }
    });

    while let Some(message) = subscription.next().await {
        let reply_subject = match message.reply.clone() {
            Some(reply_subject) => reply_subject,
            None => {
                warn!("received probe message without reply subject");
                continue;
            }
        };

        let probe: AvailabilityProbeMessage = match serde_json::from_slice(&message.payload) {
            Ok(probe) => probe,
            Err(error) => {
                warn!(error = %error, "failed to decode availability probe");
                continue;
            }
        };

        if probe.device_id != config.device_id {
            warn!(
                expected_device_id = %config.device_id,
                received_device_id = %probe.device_id,
                "ignoring mismatched availability probe"
            );
            continue;
        }

        let current_job_id = active_job_id.lock().await.clone();
        let available = current_job_id.is_none();
        let reason = match current_job_id {
            Some(job_id) => format!("busy with active job {job_id}"),
            None => "idle".to_string(),
        };
        let response = AvailabilitySnapshot {
            probe_id: probe.probe_id,
            job_id: probe.job_id,
            device_id: config.device_id.clone(),
            available,
            reason,
            responded_at: Utc::now(),
        };

        match serde_json::to_vec(&response) {
            Ok(payload) => {
                if let Err(error) = nats.publish(reply_subject, payload.into()).await {
                    warn!(error = %error, "failed to publish availability response");
                    continue;
                }
            }
            Err(error) => {
                warn!(error = %error, "failed to encode availability response");
                continue;
            }
        }

        info!(
            device_id = %config.device_id,
            available = response.available,
            reason = %response.reason,
            "responded to availability probe"
        );
    }

    Ok(())
}
