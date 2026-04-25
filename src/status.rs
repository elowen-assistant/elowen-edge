//! Local edge status reporting for the TUI and service diagnostics.

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tokio::fs;

use crate::config::EdgeConfig;

/// JSON status snapshot written by the long-running edge process.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct EdgeStatus {
    pub(crate) config_path: PathBuf,
    pub(crate) process_started_at: DateTime<Utc>,
    pub(crate) updated_at: DateTime<Utc>,
    pub(crate) device_id: String,
    pub(crate) runner_mode: String,
    pub(crate) nats_status: String,
    pub(crate) last_registration_at: Option<DateTime<Utc>>,
    pub(crate) last_registration_error: Option<String>,
    #[serde(default)]
    pub(crate) last_registration_error_code: Option<String>,
    pub(crate) service_status: Option<String>,
}

impl EdgeStatus {
    /// Builds the initial status snapshot from loaded runtime config.
    pub(crate) fn new(config: &EdgeConfig) -> Self {
        let now = Utc::now();
        Self {
            config_path: config.config_path.clone(),
            process_started_at: now,
            updated_at: now,
            device_id: config.device_id.clone(),
            runner_mode: if config.codex_command.is_some() {
                "codex-cli".to_string()
            } else {
                "simulated".to_string()
            },
            nats_status: "starting".to_string(),
            last_registration_at: None,
            last_registration_error: None,
            last_registration_error_code: None,
            service_status: None,
        }
    }

    pub(crate) fn mark_nats(&mut self, status: impl Into<String>) {
        self.nats_status = status.into();
        self.updated_at = Utc::now();
    }

    pub(crate) fn mark_registration_success(&mut self) {
        self.last_registration_at = Some(Utc::now());
        self.last_registration_error = None;
        self.last_registration_error_code = None;
        self.updated_at = Utc::now();
    }

    pub(crate) fn mark_registration_error(
        &mut self,
        error: impl Into<String>,
        code: Option<String>,
    ) {
        self.last_registration_error = Some(error.into());
        self.last_registration_error_code = code;
        self.updated_at = Utc::now();
    }
}

/// Writes the current status atomically enough for local TUI polling.
pub(crate) async fn write_status(path: &Path, status: &EdgeStatus) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create status directory {}", parent.display()))?;
    }
    let body = serde_json::to_vec_pretty(status).context("failed to serialize edge status")?;
    fs::write(path, body)
        .await
        .with_context(|| format!("failed to write status {}", path.display()))
}

/// Reads a status file if the runtime has written one.
pub(crate) fn read_status(path: &Path) -> anyhow::Result<Option<EdgeStatus>> {
    if !path.exists() {
        return Ok(None);
    }
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read status {}", path.display()))?;
    let status = serde_json::from_str(&body)
        .with_context(|| format!("failed to parse status {}", path.display()))?;
    Ok(Some(status))
}

#[cfg(test)]
mod tests {
    use super::EdgeStatus;
    use crate::config::{EdgeConfig, EdgeConfigFile, OrchestratorSection, RepositorySection};
    use std::fs;

    #[test]
    fn status_reports_simulated_runner_when_codex_is_absent() {
        let root = std::env::temp_dir().join(format!("elowen-edge-status-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let config = EdgeConfig::from_file(
            &root.join("edge.toml"),
            EdgeConfigFile {
                orchestrator: OrchestratorSection {
                    nats_url: Some("nats://127.0.0.1:4222".to_string()),
                    ..Default::default()
                },
                repositories: RepositorySection {
                    workspace_root: Some(root.clone()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();

        let status = EdgeStatus::new(&config);

        assert_eq!(status.runner_mode, "simulated");
        let _ = fs::remove_dir_all(root);
    }
}
