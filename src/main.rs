use anyhow::Context;
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use std::{env, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tracing::{info, warn};

#[derive(Clone)]
struct EdgeConfig {
    api_url: String,
    nats_url: String,
    device_id: String,
    device_name: String,
    primary_flag: bool,
    allowed_repos: Vec<String>,
    capabilities: Vec<String>,
}

#[derive(Debug, Serialize)]
struct RegisterDeviceRequest {
    name: String,
    primary_flag: bool,
    allowed_repos: Vec<String>,
    capabilities: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct JobDispatchMessage {
    job_id: String,
    short_id: String,
    thread_id: String,
    title: String,
    device_id: String,
    repo_name: String,
    base_branch: String,
    branch_name: String,
    request_text: String,
    dispatched_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
struct AvailabilityProbeMessage {
    probe_id: String,
    job_id: Option<String>,
    device_id: String,
    sent_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AvailabilitySnapshot {
    probe_id: String,
    job_id: Option<String>,
    device_id: String,
    available: bool,
    reason: String,
    responded_at: DateTime<Utc>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let config = EdgeConfig::from_env()?;
    let http = HttpClient::builder()
        .build()
        .context("failed to build HTTP client")?;
    let nats = async_nats::connect(&config.nats_url)
        .await
        .context("failed to connect to NATS")?;
    let active_job_id = Arc::new(Mutex::new(None::<String>));

    wait_for_registration(&http, &config).await;
    info!(device_id = %config.device_id, "registered edge device");

    let heartbeat_http = http.clone();
    let heartbeat_config = config.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(30));

        loop {
            ticker.tick().await;
            match register_device(&heartbeat_http, &heartbeat_config).await {
                Ok(()) => {
                    info!(device_id = %heartbeat_config.device_id, "edge registration heartbeat")
                }
                Err(error) => warn!(error = %error, "edge registration heartbeat failed"),
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

    info!(subject = %subject, "awaiting availability probes");
    info!(subject = %dispatch_subject, "awaiting job dispatches");

    let dispatch_device_id = config.device_id.clone();
    tokio::spawn(async move {
        while let Some(message) = dispatch_subscription.next().await {
            let dispatch: JobDispatchMessage = match serde_json::from_slice(&message.payload) {
                Ok(dispatch) => dispatch,
                Err(error) => {
                    warn!(error = %error, "failed to decode job dispatch");
                    continue;
                }
            };

            if dispatch.device_id != dispatch_device_id {
                warn!(
                    expected_device_id = %dispatch_device_id,
                    received_device_id = %dispatch.device_id,
                    "ignoring mismatched job dispatch"
                );
                continue;
            }

            info!(
                job_id = %dispatch.job_id,
                short_id = %dispatch.short_id,
                repo_name = %dispatch.repo_name,
                branch_name = %dispatch.branch_name,
                "received job dispatch"
            );
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

impl EdgeConfig {
    fn from_env() -> anyhow::Result<Self> {
        let device_id = env::var("ELOWEN_DEVICE_ID")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(detect_device_id);
        let device_name = env::var("ELOWEN_DEVICE_NAME")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| detect_device_name(&device_id));

        Ok(Self {
            api_url: env::var("ELOWEN_API_URL")
                .unwrap_or_else(|_| "http://elowen-api:8080".to_string())
                .trim_end_matches('/')
                .to_string(),
            nats_url: env::var("ELOWEN_NATS_URL").context("missing ELOWEN_NATS_URL")?,
            device_id,
            device_name,
            primary_flag: env::var("ELOWEN_DEVICE_PRIMARY")
                .ok()
                .map(|value| parse_bool(&value))
                .unwrap_or(true),
            allowed_repos: parse_list_env(
                "ELOWEN_ALLOWED_REPOS",
                &[
                    "elowen-api",
                    "elowen-ui",
                    "elowen-edge",
                    "elowen-notes",
                    "elowen-platform",
                ],
            ),
            capabilities: parse_list_env(
                "ELOWEN_DEVICE_CAPABILITIES",
                &["codex", "git", "build", "test"],
            ),
        })
    }
}

async fn register_device(http: &HttpClient, config: &EdgeConfig) -> anyhow::Result<()> {
    let response = http
        .put(format!(
            "{}/api/v1/devices/{}",
            config.api_url, config.device_id
        ))
        .json(&RegisterDeviceRequest {
            name: config.device_name.clone(),
            primary_flag: config.primary_flag,
            allowed_repos: config.allowed_repos.clone(),
            capabilities: config.capabilities.clone(),
        })
        .send()
        .await
        .context("failed to send device registration")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("device registration failed with status {status}: {body}");
    }

    Ok(())
}

async fn wait_for_registration(http: &HttpClient, config: &EdgeConfig) {
    loop {
        match register_device(http, config).await {
            Ok(()) => return,
            Err(error) => {
                warn!(error = %error, "initial device registration failed; retrying");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

fn detect_device_id() -> String {
    env::var("COMPUTERNAME")
        .or_else(|_| env::var("HOSTNAME"))
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().to_ascii_lowercase().replace(' ', "-"))
        .unwrap_or_else(|| "elowen-edge".to_string())
}

fn detect_device_name(device_id: &str) -> String {
    env::var("COMPUTERNAME")
        .or_else(|_| env::var("HOSTNAME"))
        .unwrap_or_else(|_| device_id.to_string())
}

fn parse_bool(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn parse_list_env(key: &str, default: &[&str]) -> Vec<String> {
    let value = env::var(key).unwrap_or_else(|_| default.join(","));
    let mut items = Vec::new();

    for candidate in value.split(',') {
        let trimmed = candidate.trim();
        if trimmed.is_empty() || items.iter().any(|item| item == trimmed) {
            continue;
        }

        items.push(trimmed.to_string());
    }

    items
}
