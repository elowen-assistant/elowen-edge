//! Workspace sandbox policy generation and enforcement.

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{fs, process::Command};

use crate::config::EdgeConfig;

pub(crate) const SANDBOX_ERROR_PREFIX: &str = "sandbox blocked: ";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SandboxMode {
    Off,
    Workspace,
}

impl SandboxMode {
    pub(crate) fn from_env(value: Option<&str>) -> anyhow::Result<Self> {
        let normalized = value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("workspace")
            .to_ascii_lowercase();

        match normalized.as_str() {
            "off" => Ok(Self::Off),
            "workspace" => Ok(Self::Workspace),
            _ => anyhow::bail!(
                "unsupported ELOWEN_SANDBOX_MODE `{normalized}`; expected `workspace` or `off`"
            ),
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Workspace => "workspace",
        }
    }
}

#[derive(Clone)]
pub(crate) struct SandboxPolicy {
    pub(crate) mode: SandboxMode,
    pub(crate) worktree_path: PathBuf,
    pub(crate) sandbox_root: PathBuf,
    pub(crate) temp_root: PathBuf,
    pub(crate) cache_root: PathBuf,
    pub(crate) policy_path: PathBuf,
}

pub(crate) async fn prepare_sandbox_policy(
    config: &EdgeConfig,
    worktree_path: &Path,
) -> anyhow::Result<SandboxPolicy> {
    let worktree_path =
        enforce_worktree_containment(&config.worktree_root, worktree_path, "job worktree").await?;
    let sandbox_root = worktree_path.join(".elowen-sandbox");
    let temp_root = sandbox_root.join("tmp");
    let cache_root = sandbox_root.join("cache");
    let policy_path = sandbox_root.join("policy.json");

    fs::create_dir_all(&temp_root)
        .await
        .with_context(|| format!("failed to create {}", temp_root.display()))?;
    fs::create_dir_all(&cache_root)
        .await
        .with_context(|| format!("failed to create {}", cache_root.display()))?;

    let policy = SandboxPolicy {
        mode: config.sandbox_mode,
        worktree_path,
        sandbox_root,
        temp_root,
        cache_root,
        policy_path,
    };
    let policy_body = serde_json::to_vec_pretty(&sandbox_report_value(&policy))
        .context("failed to serialize sandbox policy")?;
    fs::write(&policy.policy_path, policy_body)
        .await
        .with_context(|| format!("failed to write {}", policy.policy_path.display()))?;

    Ok(policy)
}

pub(crate) fn sandbox_report_value(policy: &SandboxPolicy) -> Value {
    json!({
        "mode": policy.mode.as_str(),
        "worktree_path": policy.worktree_path.to_string_lossy().to_string(),
        "sandbox_root": policy.sandbox_root.to_string_lossy().to_string(),
        "temp_root": policy.temp_root.to_string_lossy().to_string(),
        "cache_root": policy.cache_root.to_string_lossy().to_string(),
        "policy_path": policy.policy_path.to_string_lossy().to_string(),
        "working_dir_must_stay_within_worktree": true,
        "validation_shells_blocked": true,
        "cache_redirects": [
            "TMP",
            "TEMP",
            "TMPDIR",
            "CARGO_TARGET_DIR",
            "XDG_CACHE_HOME",
            "XDG_STATE_HOME",
            "XDG_CONFIG_HOME",
            "npm_config_cache",
            "PIP_CACHE_DIR",
            "UV_CACHE_DIR"
        ]
    })
}

pub(crate) fn apply_sandbox_environment(command: &mut Command, policy: &SandboxPolicy) {
    command
        .env("TMP", &policy.temp_root)
        .env("TEMP", &policy.temp_root)
        .env("TMPDIR", &policy.temp_root)
        .env("CARGO_TARGET_DIR", policy.sandbox_root.join("cargo-target"))
        .env("XDG_CACHE_HOME", &policy.cache_root)
        .env("XDG_STATE_HOME", policy.cache_root.join("state"))
        .env("XDG_CONFIG_HOME", policy.cache_root.join("config"))
        .env("npm_config_cache", policy.cache_root.join("npm"))
        .env("PIP_CACHE_DIR", policy.cache_root.join("pip"))
        .env("UV_CACHE_DIR", policy.cache_root.join("uv"))
        .env("ELOWEN_SANDBOX_MODE", policy.mode.as_str())
        .env("ELOWEN_SANDBOX_POLICY_FILE", &policy.policy_path)
        .env("ELOWEN_SANDBOX_WORKTREE", &policy.worktree_path);
}

pub(crate) async fn enforce_worktree_containment(
    worktree_root: &Path,
    candidate: &Path,
    label: &str,
) -> anyhow::Result<PathBuf> {
    let resolved_root = fs::canonicalize(worktree_root)
        .await
        .with_context(|| format!("failed to resolve sandbox root {}", worktree_root.display()))?;
    let resolved_candidate = fs::canonicalize(candidate)
        .await
        .with_context(|| format!("failed to resolve {label} {}", candidate.display()))?;
    if !resolved_candidate.starts_with(&resolved_root) {
        return Err(sandbox_error(format!(
            "{label} `{}` escapes sandbox root `{}`",
            resolved_candidate.display(),
            resolved_root.display()
        )));
    }

    Ok(resolved_candidate)
}

pub(crate) async fn resolve_validation_program(
    sandbox: &SandboxPolicy,
    working_dir: &Path,
    program: &str,
) -> anyhow::Result<PathBuf> {
    if is_disallowed_validation_program(program) {
        return Err(sandbox_error(format!(
            "validation command `{program}` is not allowed; invoke a direct executable instead of a shell"
        )));
    }

    let program_path = Path::new(program);
    if program_path.is_absolute() || program_path.components().count() > 1 {
        let candidate = if program_path.is_absolute() {
            program_path.to_path_buf()
        } else {
            working_dir.join(program_path)
        };
        return enforce_worktree_containment(
            &sandbox.worktree_path,
            &candidate,
            "validation command path",
        )
        .await;
    }

    Ok(PathBuf::from(program))
}

pub(crate) fn is_disallowed_validation_program(program: &str) -> bool {
    matches!(
        validation_program_name(program).as_str(),
        "cmd"
            | "cmd.exe"
            | "powershell"
            | "powershell.exe"
            | "pwsh"
            | "pwsh.exe"
            | "sh"
            | "bash"
            | "zsh"
    )
}

pub(crate) fn validation_program_name(program: &str) -> String {
    Path::new(program)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(program)
        .trim()
        .to_ascii_lowercase()
}

pub(crate) fn sandbox_blocked_report(
    kind: &str,
    argv: &[String],
    working_dir: &str,
    started_at: DateTime<Utc>,
    duration: Duration,
    reason: String,
) -> Value {
    json!({
        "status": "sandbox_blocked",
        "kind": kind,
        "command": argv,
        "working_dir": working_dir,
        "started_at": started_at,
        "duration_ms": duration.as_millis() as u64,
        "reason": reason,
    })
}

pub(crate) fn sandbox_error(message: impl Into<String>) -> anyhow::Error {
    anyhow::anyhow!("{SANDBOX_ERROR_PREFIX}{}", message.into())
}

pub(crate) fn classify_failure(error: &anyhow::Error) -> (String, String) {
    let detail = error.to_string();
    if let Some(stripped) = detail.strip_prefix(SANDBOX_ERROR_PREFIX) {
        ("sandbox".to_string(), stripped.to_string())
    } else {
        ("execution".to_string(), detail)
    }
}
