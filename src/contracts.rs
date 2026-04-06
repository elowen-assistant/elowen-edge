//! Wire contracts published and consumed by the edge runtime.
//!
//! Keep these in sync with the equivalent DTOs in `elowen-api`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Requested execution intent for a dispatched job.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExecutionIntent {
    WorkspaceChange,
    ReadOnly,
}

impl ExecutionIntent {
    /// Returns the stable wire label used in prompts, summaries, and reports.
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::WorkspaceChange => "workspace_change",
            Self::ReadOnly => "read_only",
        }
    }
}

/// Device registration payload sent to the orchestrator API.
#[derive(Debug, Serialize)]
pub(crate) struct RegisterDeviceRequest {
    pub(crate) name: String,
    pub(crate) primary_flag: bool,
    pub(crate) allowed_repos: Vec<String>,
    pub(crate) allowed_repo_roots: Vec<String>,
    pub(crate) discovered_repos: Vec<String>,
    pub(crate) capabilities: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) trust: Option<DeviceRegistrationTrustProof>,
}

/// Orchestrator-signed challenge used before trusted registration.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct RegistrationChallengeResponse {
    pub(crate) challenge_id: String,
    pub(crate) challenge: String,
    pub(crate) issued_at: DateTime<Utc>,
    pub(crate) orchestrator_public_key: String,
    pub(crate) signature: String,
}

/// Edge-signed proof attached to trusted registration requests.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct DeviceRegistrationTrustProof {
    pub(crate) orchestrator_challenge_id: String,
    pub(crate) orchestrator_challenge: String,
    pub(crate) orchestrator_challenge_issued_at: DateTime<Utc>,
    pub(crate) orchestrator_signature: String,
    pub(crate) edge_public_key: String,
    pub(crate) edge_signature: String,
}

/// Dispatched execution request sent from the orchestrator to the edge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct JobDispatchMessage {
    pub(crate) job_id: String,
    pub(crate) short_id: String,
    pub(crate) correlation_id: String,
    pub(crate) thread_id: String,
    pub(crate) title: String,
    pub(crate) device_id: String,
    pub(crate) repo_name: String,
    pub(crate) base_branch: String,
    pub(crate) branch_name: String,
    pub(crate) request_text: String,
    pub(crate) execution_intent: ExecutionIntent,
    pub(crate) dispatched_at: DateTime<Utc>,
}

/// Lifecycle event emitted by the edge back to the orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct JobLifecycleEvent {
    pub(crate) job_id: String,
    pub(crate) correlation_id: String,
    pub(crate) device_id: String,
    pub(crate) event_type: String,
    pub(crate) status: Option<String>,
    pub(crate) result: Option<String>,
    pub(crate) failure_class: Option<String>,
    pub(crate) worktree_path: Option<String>,
    pub(crate) detail: Option<String>,
    pub(crate) payload_json: Option<Value>,
    pub(crate) created_at: DateTime<Utc>,
}

/// Approval command sent to the edge after a user approves a push.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct JobApprovalCommand {
    pub(crate) approval_id: String,
    pub(crate) job_id: String,
    pub(crate) short_id: String,
    pub(crate) correlation_id: String,
    pub(crate) device_id: String,
    pub(crate) repo_name: String,
    pub(crate) branch_name: String,
    pub(crate) action_type: String,
    pub(crate) approved_at: DateTime<Utc>,
}

/// Availability probe request sent via NATS request/reply.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct AvailabilityProbeMessage {
    pub(crate) probe_id: String,
    pub(crate) job_id: Option<String>,
    pub(crate) device_id: String,
    pub(crate) sent_at: DateTime<Utc>,
}

/// Availability response returned by the edge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AvailabilitySnapshot {
    pub(crate) probe_id: String,
    pub(crate) job_id: Option<String>,
    pub(crate) device_id: String,
    pub(crate) available: bool,
    pub(crate) reason: String,
    pub(crate) responded_at: DateTime<Utc>,
}
