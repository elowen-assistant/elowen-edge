//! Job execution, validation, and push-approval flows.

use anyhow::Context;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{fs, io::AsyncWriteExt, process::Command, sync::Mutex};
use tracing::info;

use crate::{
    config::EdgeConfig,
    contracts::{
        ExecutionIntent, JobApprovalCommand, JobDispatchMessage, JobLifecycleEvent, JobTargetKind,
    },
    discovery::resolve_repo_root,
    events::publish_job_event,
    sandbox::{
        SandboxPolicy, apply_sandbox_environment, classify_failure, enforce_worktree_containment,
        is_disallowed_validation_program, prepare_sandbox_policy, resolve_validation_program,
        sandbox_blocked_report, sandbox_error, sandbox_report_value,
    },
};

/// Summary of one execution or approval command path written back into job artifacts.
struct CommandOutcome {
    detail: String,
    result: String,
    failure_class: Option<String>,
    summary_markdown: String,
    execution_report: Value,
    approval_summary: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
/// Commit metadata captured after a mutating run produces a new commit.
struct CommitRecord {
    sha: String,
    short_sha: String,
    message: String,
    changed_files: Vec<String>,
}

/// Snapshot of post-run git status used in execution reports.
struct GitReport {
    status_lines: Vec<String>,
    diff_stat: Option<String>,
    changed_files: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
/// Minimal subset of repo-local assistant config read by the edge runtime.
struct AssistantConfig {
    #[serde(default)]
    validation: ValidationConfig,
}

#[derive(Debug, Deserialize, Default)]
/// Build/test commands resolved from `.assistant/config.toml`.
struct ValidationConfig {
    build: Option<Vec<String>>,
    test: Option<Vec<String>>,
    working_dir: Option<String>,
}

/// Resolved validation commands and config source for one job.
struct ValidationPlan {
    build: Option<CommandSpec>,
    test: Option<CommandSpec>,
    config_source: String,
}

/// Executable plus working directory for a spawned command.
struct CommandSpec {
    argv: Vec<String>,
    working_dir: PathBuf,
}

/// Structured result payload for build and test validation commands.
struct ValidationResults {
    build: Value,
    test: Value,
    overall_success: bool,
    config_source: String,
}

/// Guards single-job execution and emits failure lifecycle events when execution fails.
pub(crate) async fn handle_job_dispatch(
    dispatch: JobDispatchMessage,
    config: EdgeConfig,
    nats: async_nats::Client,
    active_job_id: Arc<Mutex<Option<String>>>,
) -> anyhow::Result<()> {
    let busy_job_id = {
        let mut guard = active_job_id.lock().await;
        if let Some(current_job_id) = guard.clone() {
            Some(current_job_id)
        } else {
            *guard = Some(dispatch.job_id.clone());
            None
        }
    };

    if let Some(current_job_id) = busy_job_id {
        publish_job_event(
            &nats,
            JobLifecycleEvent {
                job_id: dispatch.job_id.clone(),
                correlation_id: dispatch.correlation_id.clone(),
                device_id: config.device_id.clone(),
                event_type: "job.rejected".to_string(),
                status: Some("pending".to_string()),
                result: None,
                failure_class: None,
                worktree_path: None,
                detail: Some(format!(
                    "edge device is already running active job {current_job_id}"
                )),
                payload_json: Some(json!({ "active_job_id": current_job_id })),
                created_at: Utc::now(),
            },
        )
        .await?;
        return Ok(());
    }

    let execution_result = run_job_execution(&dispatch, &config, &nats).await;

    {
        let mut guard = active_job_id.lock().await;
        if guard.as_deref() == Some(dispatch.job_id.as_str()) {
            *guard = None;
        }
    }

    if let Err(error) = execution_result {
        publish_job_event(
            &nats,
            JobLifecycleEvent {
                job_id: dispatch.job_id.clone(),
                correlation_id: dispatch.correlation_id.clone(),
                device_id: config.device_id.clone(),
                event_type: "job.failed".to_string(),
                status: Some("failed".to_string()),
                result: Some("failure".to_string()),
                failure_class: Some("execution".to_string()),
                worktree_path: None,
                detail: Some(error.to_string()),
                payload_json: None,
                created_at: Utc::now(),
            },
        )
        .await?;
    }

    Ok(())
}

async fn run_job_execution(
    dispatch: &JobDispatchMessage,
    config: &EdgeConfig,
    nats: &async_nats::Client,
) -> anyhow::Result<()> {
    publish_job_event(
        nats,
        JobLifecycleEvent {
            job_id: dispatch.job_id.clone(),
            correlation_id: dispatch.correlation_id.clone(),
            device_id: config.device_id.clone(),
            event_type: "job.accepted".to_string(),
            status: Some("accepted".to_string()),
            result: None,
            failure_class: None,
            worktree_path: None,
            detail: Some("edge accepted dispatched job".to_string()),
            payload_json: Some(json!({
                "target_kind": dispatch.target_kind,
                "target_name": dispatch.target_name(),
                "branch_name": dispatch.branch_name,
                "base_branch": dispatch.base_branch,
            })),
            created_at: Utc::now(),
        },
    )
    .await?;

    let execution_path = create_execution_workspace(dispatch, config).await?;
    let execution_path_str = execution_path.to_string_lossy().to_string();
    let reported_path = matches!(dispatch.target_kind, JobTargetKind::Repository)
        .then_some(execution_path_str.clone());

    if matches!(dispatch.target_kind, JobTargetKind::Repository) {
        publish_job_event(
            nats,
            JobLifecycleEvent {
                job_id: dispatch.job_id.clone(),
                correlation_id: dispatch.correlation_id.clone(),
                device_id: config.device_id.clone(),
                event_type: "job.worktree_created".to_string(),
                status: Some("accepted".to_string()),
                result: None,
                failure_class: None,
                worktree_path: reported_path.clone(),
                detail: Some("git worktree created for dispatched job".to_string()),
                payload_json: Some(json!({
                    "target_kind": dispatch.target_kind,
                    "target_name": dispatch.target_name(),
                    "branch_name": dispatch.branch_name,
                    "base_branch": dispatch.base_branch,
                })),
                created_at: Utc::now(),
            },
        )
        .await?;
    }

    publish_job_event(
        nats,
        JobLifecycleEvent {
            job_id: dispatch.job_id.clone(),
            correlation_id: dispatch.correlation_id.clone(),
            device_id: config.device_id.clone(),
            event_type: "job.started".to_string(),
            status: Some("running".to_string()),
            result: None,
            failure_class: None,
            worktree_path: reported_path.clone(),
            detail: Some("job execution started".to_string()),
            payload_json: None,
            created_at: Utc::now(),
        },
    )
    .await?;

    let command_outcome = match run_codex_wrapper(dispatch, config, &execution_path).await {
        Ok(outcome) => outcome,
        Err(error) => {
            let (failure_class, detail) = classify_failure(&error);
            publish_job_event(
                nats,
                JobLifecycleEvent {
                    job_id: dispatch.job_id.clone(),
                    correlation_id: dispatch.correlation_id.clone(),
                    device_id: config.device_id.clone(),
                    event_type: "job.failed".to_string(),
                    status: Some("failed".to_string()),
                    result: Some("failure".to_string()),
                    failure_class: Some(failure_class),
                    worktree_path: reported_path.clone(),
                    detail: Some(detail),
                    payload_json: None,
                    created_at: Utc::now(),
                },
            )
            .await?;
            return Ok(());
        }
    };

    publish_job_event(
        nats,
        JobLifecycleEvent {
            job_id: dispatch.job_id.clone(),
            correlation_id: dispatch.correlation_id.clone(),
            device_id: config.device_id.clone(),
            event_type: "job.completed".to_string(),
            status: Some("completed".to_string()),
            result: Some(command_outcome.result.clone()),
            failure_class: command_outcome.failure_class.clone(),
            worktree_path: reported_path.clone(),
            detail: Some(command_outcome.detail),
            payload_json: Some(json!({
                "summary_markdown": command_outcome.summary_markdown,
                "execution_report": command_outcome.execution_report,
                "push_required": command_outcome.approval_summary.is_some(),
            })),
            created_at: Utc::now(),
        },
    )
    .await?;

    if let Some(approval_summary) = command_outcome.approval_summary {
        publish_job_event(
            nats,
            JobLifecycleEvent {
                job_id: dispatch.job_id.clone(),
                correlation_id: dispatch.correlation_id.clone(),
                device_id: config.device_id.clone(),
                event_type: "job.awaiting_approval".to_string(),
                status: Some("awaiting_approval".to_string()),
                result: Some(command_outcome.result),
                failure_class: command_outcome.failure_class,
                worktree_path: reported_path,
                detail: Some("push remains gated behind explicit approval".to_string()),
                payload_json: Some(json!({
                    "action_type": "push",
                    "summary": approval_summary,
                })),
                created_at: Utc::now(),
            },
        )
        .await?;
    }

    Ok(())
}

pub(crate) async fn handle_job_approval(
    command: JobApprovalCommand,
    config: EdgeConfig,
    nats: async_nats::Client,
    active_job_id: Arc<Mutex<Option<String>>>,
) -> anyhow::Result<()> {
    wait_for_idle_slot(&active_job_id, &command.job_id).await;
    let push_result = run_approved_push(&command, &config, &nats).await;

    {
        let mut guard = active_job_id.lock().await;
        if guard.as_deref() == Some(command.job_id.as_str()) {
            *guard = None;
        }
    }

    if let Err(error) = push_result {
        let (failure_class, detail) = classify_push_failure(&error);
        publish_job_event(
            &nats,
            JobLifecycleEvent {
                job_id: command.job_id.clone(),
                correlation_id: command.correlation_id.clone(),
                device_id: config.device_id.clone(),
                event_type: "job.failed".to_string(),
                status: Some("failed".to_string()),
                result: Some("failure".to_string()),
                failure_class: Some(failure_class),
                worktree_path: None,
                detail: Some(detail),
                payload_json: Some(json!({
                    "approval_id": command.approval_id,
                    "action_type": command.action_type,
                    "branch_name": command.branch_name,
                })),
                created_at: Utc::now(),
            },
        )
        .await?;
    }

    Ok(())
}

async fn wait_for_idle_slot(active_job_id: &Arc<Mutex<Option<String>>>, next_job_id: &str) {
    loop {
        let claimed = {
            let mut guard = active_job_id.lock().await;
            match guard.as_deref() {
                None => {
                    *guard = Some(next_job_id.to_string());
                    true
                }
                Some(current_job_id) if current_job_id == next_job_id => true,
                _ => false,
            }
        };

        if claimed {
            return;
        }

        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn run_approved_push(
    command: &JobApprovalCommand,
    config: &EdgeConfig,
    nats: &async_nats::Client,
) -> anyhow::Result<()> {
    let repo_name = command
        .target_name
        .as_deref()
        .context("approved push is missing target name")?;
    let branch_name = command
        .branch_name
        .as_deref()
        .context("approved push is missing branch name")?;
    let worktree_path = config.worktree_root.join(repo_name).join(&command.short_id);
    let worktree_path = enforce_worktree_containment(
        &config.worktree_root,
        &worktree_path,
        "approved push worktree",
    )
    .await?;

    fs::metadata(&worktree_path).await.with_context(|| {
        format!(
            "approved push worktree is missing at {}",
            worktree_path.display()
        )
    })?;

    let worktree_path_str = worktree_path.to_string_lossy().to_string();
    publish_job_event(
        nats,
        JobLifecycleEvent {
            job_id: command.job_id.clone(),
            correlation_id: command.correlation_id.clone(),
            device_id: config.device_id.clone(),
            event_type: "job.push_started".to_string(),
            status: Some("pushing".to_string()),
            result: Some("success".to_string()),
            failure_class: None,
            worktree_path: Some(worktree_path_str.clone()),
            detail: Some("approved push started on edge".to_string()),
            payload_json: Some(json!({
                "approval_id": command.approval_id,
                "action_type": command.action_type,
                "branch_name": command.branch_name,
                "remote": "origin",
            })),
            created_at: Utc::now(),
        },
    )
    .await?;

    let output = Command::new("git")
        .arg("-C")
        .arg(&worktree_path)
        .args(["push", "-u", "origin"])
        .arg(branch_name)
        .output()
        .await
        .with_context(|| format!("failed to execute approved push for branch {}", branch_name))?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if !output.status.success() {
        anyhow::bail!(
            "git push failed for `{}`: {}",
            branch_name,
            summarize_process_output(&stdout, &stderr)
        );
    }

    publish_job_event(
        nats,
        JobLifecycleEvent {
            job_id: command.job_id.clone(),
            correlation_id: command.correlation_id.clone(),
            device_id: config.device_id.clone(),
            event_type: "job.push_completed".to_string(),
            status: Some("completed".to_string()),
            result: Some("success".to_string()),
            failure_class: None,
            worktree_path: Some(worktree_path_str),
            detail: Some(format!("pushed branch `{}` to `origin`", branch_name)),
            payload_json: Some(json!({
                "approval_id": command.approval_id,
                "action_type": command.action_type,
                "branch_name": command.branch_name,
                "remote": "origin",
                "stdout": truncate_text(&stdout, 800),
                "stderr": truncate_text(&stderr, 800),
            })),
            created_at: Utc::now(),
        },
    )
    .await?;

    Ok(())
}

async fn create_worktree(
    dispatch: &JobDispatchMessage,
    config: &EdgeConfig,
) -> anyhow::Result<PathBuf> {
    let repo_name = dispatch.target_name();
    let branch_name = dispatch
        .branch_name
        .as_deref()
        .context("repository dispatch is missing branch_name")?;
    let base_branch = dispatch
        .base_branch
        .as_deref()
        .context("repository dispatch is missing base_branch")?;
    let repo_root = resolve_repo_root(config, repo_name)?;
    ensure_repo_root(&repo_root, repo_name).await?;

    let worktree_parent = config.worktree_root.join(repo_name);
    let worktree_path = worktree_parent.join(&dispatch.short_id);
    fs::create_dir_all(&worktree_parent)
        .await
        .with_context(|| {
            format!(
                "failed to create worktree parent {}",
                worktree_parent.display()
            )
        })?;

    if fs::metadata(&worktree_path).await.is_ok() {
        let _ = Command::new("git")
            .arg("-C")
            .arg(&repo_root)
            .args(["worktree", "remove", "--force"])
            .arg(&worktree_path)
            .output()
            .await;
        let _ = fs::remove_dir_all(&worktree_path).await;
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(&repo_root)
        .args(["worktree", "add", "--force", "-B"])
        .arg(branch_name)
        .arg(&worktree_path)
        .arg(base_branch)
        .output()
        .await
        .with_context(|| format!("failed to create worktree for {}", repo_name))?;

    if !output.status.success() {
        anyhow::bail!(
            "git worktree add failed: {}",
            summarize_command_output(&output.stdout, &output.stderr)
        );
    }

    write_job_request_files(dispatch, &worktree_path).await?;
    Ok(worktree_path)
}

async fn create_capability_workspace(
    dispatch: &JobDispatchMessage,
    config: &EdgeConfig,
) -> anyhow::Result<PathBuf> {
    let workspace_parent = config.worktree_root.join("_capability");
    let workspace_path = workspace_parent.join(&dispatch.short_id);
    fs::create_dir_all(&workspace_parent)
        .await
        .with_context(|| {
            format!(
                "failed to create capability workspace parent {}",
                workspace_parent.display()
            )
        })?;
    if fs::metadata(&workspace_path).await.is_ok() {
        let _ = fs::remove_dir_all(&workspace_path).await;
    }
    fs::create_dir_all(&workspace_path)
        .await
        .with_context(|| format!("failed to create {}", workspace_path.display()))?;
    write_job_request_files(dispatch, &workspace_path).await?;
    Ok(workspace_path)
}

async fn create_execution_workspace(
    dispatch: &JobDispatchMessage,
    config: &EdgeConfig,
) -> anyhow::Result<PathBuf> {
    match dispatch.target_kind {
        JobTargetKind::Repository => create_worktree(dispatch, config).await,
        JobTargetKind::Capability => create_capability_workspace(dispatch, config).await,
    }
}

async fn run_codex_wrapper(
    dispatch: &JobDispatchMessage,
    config: &EdgeConfig,
    worktree_path: &Path,
) -> anyhow::Result<CommandOutcome> {
    let sandbox = prepare_sandbox_policy(config, worktree_path).await?;
    if let Some(command) = &config.codex_command {
        return run_codex_cli(dispatch, config, &sandbox, command).await;
    }

    run_simulated_codex_wrapper(dispatch, config, &sandbox).await
}

async fn run_simulated_codex_wrapper(
    dispatch: &JobDispatchMessage,
    config: &EdgeConfig,
    sandbox: &SandboxPolicy,
) -> anyhow::Result<CommandOutcome> {
    tokio::time::sleep(Duration::from_millis(config.simulated_run_ms)).await;

    let summary_path = sandbox.worktree_path.join("elowen-job-summary.md");
    let summary_body = format!(
        "# Simulated Slice 4 Execution\n\n\
        - Job: {}\n\
        - Thread: {}\n\
        - Target kind: {}\n\
        - Target: {}\n\
        - Branch: {}\n\
        - Base branch: {}\n\
        - Runner: simulated\n\n\
        ## Request\n\n{}\n",
        dispatch.job_id,
        dispatch.thread_id,
        dispatch.target_kind.as_str(),
        dispatch.target_name(),
        dispatch.branch_name.as_deref().unwrap_or("n/a"),
        dispatch.base_branch.as_deref().unwrap_or("n/a"),
        dispatch.prompt
    );

    fs::write(&summary_path, summary_body)
        .await
        .with_context(|| format!("failed to write {}", summary_path.display()))?;

    finalize_command_outcome(
        dispatch,
        config,
        sandbox,
        "simulated",
        json!({
            "summary_path": summary_path.to_string_lossy().to_string(),
        }),
        "simulated Codex wrapper completed successfully".to_string(),
    )
    .await
}

async fn run_codex_cli(
    dispatch: &JobDispatchMessage,
    config: &EdgeConfig,
    sandbox: &SandboxPolicy,
    command: &str,
) -> anyhow::Result<CommandOutcome> {
    let prompt_path = sandbox.worktree_path.join("elowen-job-request.md");
    let prompt_body = fs::read_to_string(&prompt_path)
        .await
        .with_context(|| format!("failed to read {}", prompt_path.display()))?;
    let output_path = sandbox.worktree_path.join("elowen-runner-output.jsonl");
    let error_path = sandbox.worktree_path.join("elowen-runner-error.log");
    let last_message_path = sandbox.worktree_path.join("elowen-codex-last-message.txt");
    let args = build_codex_exec_args(config, &sandbox.worktree_path, &last_message_path)?;
    let working_dir = enforce_worktree_containment(
        &sandbox.worktree_path,
        &sandbox.worktree_path,
        "Codex working directory",
    )
    .await?;
    let mut child = Command::new(command);
    child
        .args(&args)
        .current_dir(&working_dir)
        .env("ELOWEN_JOB_ID", &dispatch.job_id)
        .env("ELOWEN_JOB_SHORT_ID", &dispatch.short_id)
        .env("ELOWEN_THREAD_ID", &dispatch.thread_id)
        .env("ELOWEN_JOB_TITLE", &dispatch.title)
        .env("ELOWEN_WORKTREE_PATH", &sandbox.worktree_path)
        .env("ELOWEN_REQUEST_TEXT", &dispatch.prompt)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_sandbox_environment(&mut child, sandbox);
    child.env("ELOWEN_JOB_TARGET_KIND", dispatch.target_kind.as_str());
    if matches!(dispatch.target_kind, JobTargetKind::Repository) {
        child.env("ELOWEN_REPO_NAME", dispatch.target_name());
    }
    if let Some(branch_name) = dispatch.branch_name.as_deref() {
        child.env("ELOWEN_BRANCH_NAME", branch_name);
    }
    if let Some(base_branch) = dispatch.base_branch.as_deref() {
        child.env("ELOWEN_BASE_BRANCH", base_branch);
    }
    if matches!(dispatch.target_kind, JobTargetKind::Capability) {
        child.env("ELOWEN_CAPABILITY_NAME", dispatch.target_name());
    }
    let mut child = child
        .spawn()
        .with_context(|| format!("failed to start Codex CLI `{command}`"))?;

    let mut stdin = child
        .stdin
        .take()
        .context("failed to open stdin for Codex CLI process")?;
    stdin
        .write_all(prompt_body.as_bytes())
        .await
        .context("failed to send prompt to Codex CLI")?;
    drop(stdin);

    let output = child
        .wait_with_output()
        .await
        .with_context(|| format!("failed while waiting for Codex CLI `{command}`"))?;

    fs::write(&output_path, &output.stdout)
        .await
        .with_context(|| format!("failed to write {}", output_path.display()))?;
    fs::write(&error_path, &output.stderr)
        .await
        .with_context(|| format!("failed to write {}", error_path.display()))?;

    let stdout = truncate_text(&String::from_utf8_lossy(&output.stdout), 4000);
    let stderr = truncate_text(&String::from_utf8_lossy(&output.stderr), 4000);
    let last_message = fs::read_to_string(&last_message_path)
        .await
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let event_messages = extract_codex_event_messages(&output.stdout);

    if !output.status.success() {
        anyhow::bail!(
            "Codex CLI failed with status {}. See {} and {}",
            output.status,
            output_path.display(),
            error_path.display()
        );
    }

    finalize_command_outcome(
        dispatch,
        config,
        sandbox,
        "codex-cli",
        json!({
            "command": command,
            "args": args,
            "prompt_path": prompt_path.to_string_lossy().to_string(),
            "output_path": output_path.to_string_lossy().to_string(),
            "error_path": error_path.to_string_lossy().to_string(),
            "last_message_path": last_message_path.to_string_lossy().to_string(),
            "last_message": last_message,
            "event_messages": event_messages,
            "stdout": stdout,
            "stderr": stderr,
        }),
        format!("Codex CLI `{command}` completed successfully"),
    )
    .await
}

async fn finalize_command_outcome(
    dispatch: &JobDispatchMessage,
    config: &EdgeConfig,
    sandbox: &SandboxPolicy,
    runner: &str,
    runner_payload: Value,
    base_detail: String,
) -> anyhow::Result<CommandOutcome> {
    let validation = run_validation_suite(config, &sandbox.worktree_path, sandbox).await?;
    let build_status = validation_status(&validation.build);
    let test_status = validation_status(&validation.test);
    let sandbox_blocked =
        matches!(build_status, "sandbox_blocked") || matches!(test_status, "sandbox_blocked");
    let result = if validation.overall_success {
        "success"
    } else {
        "failure"
    };
    let failure_class = if validation.overall_success {
        None
    } else if sandbox_blocked {
        Some("sandbox".to_string())
    } else {
        Some("validation".to_string())
    };
    let detail = if validation.overall_success {
        base_detail.clone()
    } else if sandbox_blocked {
        format!(
            "sandbox blocked post-execution validation (build: {build_status}, test: {test_status})"
        )
    } else {
        format!(
            "validation failed after Codex execution (build: {build_status}, test: {test_status})"
        )
    };
    let mut git_report = if matches!(dispatch.target_kind, JobTargetKind::Repository) {
        capture_git_report(&sandbox.worktree_path).await?
    } else {
        GitReport {
            status_lines: Vec::new(),
            diff_stat: None,
            changed_files: Vec::new(),
        }
    };
    let commit = if matches!(dispatch.target_kind, JobTargetKind::Repository)
        && validation.overall_success
        && !matches!(dispatch.execution_intent, ExecutionIntent::ReadOnly)
    {
        let commit = maybe_create_job_commit(&sandbox.worktree_path, dispatch, &git_report).await?;
        git_report = capture_git_report(&sandbox.worktree_path).await?;
        commit
    } else {
        None
    };

    let mut execution_report = serde_json::Map::new();
    execution_report.insert("runner".to_string(), json!(runner));
    if let Some(object) = runner_payload.as_object() {
        for (key, value) in object {
            execution_report.insert(key.clone(), value.clone());
        }
    }
    execution_report.insert(
        "validation_config_source".to_string(),
        json!(validation.config_source),
    );
    execution_report.insert("build".to_string(), validation.build.clone());
    execution_report.insert("test".to_string(), validation.test.clone());
    execution_report.insert("git_status".to_string(), json!(git_report.status_lines));
    execution_report.insert("diff_stat".to_string(), json!(git_report.diff_stat));
    execution_report.insert("changed_files".to_string(), json!(git_report.changed_files));
    execution_report.insert("commit".to_string(), json!(commit));
    execution_report.insert(
        "execution_intent".to_string(),
        json!(dispatch.execution_intent),
    );
    execution_report.insert("target_kind".to_string(), json!(dispatch.target_kind));
    execution_report.insert("target_name".to_string(), json!(dispatch.target_name()));
    execution_report.insert(
        "read_only_change_detected".to_string(),
        json!(
            matches!(dispatch.execution_intent, ExecutionIntent::ReadOnly)
                && !git_report.changed_files.is_empty()
        ),
    );
    execution_report.insert("sandbox".to_string(), sandbox_report_value(sandbox));
    let execution_report = Value::Object(execution_report);

    let detail = if validation.overall_success {
        if matches!(dispatch.target_kind, JobTargetKind::Capability) {
            format!("{base_detail}; capability execution produced no repository worktree changes")
        } else if matches!(dispatch.execution_intent, ExecutionIntent::ReadOnly)
            && !git_report.changed_files.is_empty()
        {
            format!(
                "{base_detail}; read-only mode left {} changed entries uncommitted in the disposable worktree",
                git_report.changed_files.len()
            )
        } else if matches!(dispatch.execution_intent, ExecutionIntent::ReadOnly) {
            format!("{base_detail}; read-only mode produced no tracked repo changes")
        } else if let Some(commit) = commit.as_ref() {
            format!(
                "{base_detail}; created commit {} for branch {}",
                commit.short_sha,
                dispatch.branch_name.as_deref().unwrap_or("unknown")
            )
        } else {
            format!("{base_detail}; no committed repo changes were detected")
        }
    } else {
        detail
    };

    let summary_markdown = format!(
        "# Job Summary\n\n\
        - Result: {result}\n\
        - Runner: {runner}\n\
        - Target kind: {}\n\
        - Target: {}\n\
        - Branch: {}\n\
        - Execution intent: {}\n\
        - Validation config: {}\n\n\
        ## Request\n\n{}\n\n\
        ## Validation\n\n\
        - Build: {build_status}\n\
        - Test: {test_status}\n\n\
        ## Commit\n\n\
        - Commit: {commit_line}\n\n\
        ## Workspace Changes\n\n\
        - Changed entries: {}\n\
        - Diff stat: {}\n",
        dispatch.target_kind.as_str(),
        dispatch.target_name(),
        dispatch.branch_name.as_deref().unwrap_or("n/a"),
        dispatch.execution_intent.as_str(),
        validation.config_source,
        dispatch.prompt,
        git_report.changed_files.len(),
        git_report
            .diff_stat
            .clone()
            .unwrap_or_else(|| "no tracked diff".to_string()),
        commit_line = commit
            .as_ref()
            .map(|commit| format!("`{}` ({})", commit.short_sha, commit.message))
            .unwrap_or_else(|| "none".to_string()),
    );
    let approval_summary = if matches!(dispatch.target_kind, JobTargetKind::Capability)
        || matches!(dispatch.execution_intent, ExecutionIntent::ReadOnly)
    {
        None
    } else {
        commit.as_ref().map(|commit| {
            format!(
                "Approve push for `{}` on branch `{}` with commit `{}` after reviewing the generated summary, validation output, and {} changed entries.",
                dispatch.target_name(),
                dispatch.branch_name.as_deref().unwrap_or("unknown"),
                commit.short_sha,
                git_report.changed_files.len(),
            )
        })
    };

    Ok(CommandOutcome {
        detail,
        result: result.to_string(),
        failure_class,
        summary_markdown,
        execution_report,
        approval_summary,
    })
}

async fn write_job_request_files(
    dispatch: &JobDispatchMessage,
    worktree_path: &Path,
) -> anyhow::Result<()> {
    let prompt_path = worktree_path.join("elowen-job-request.md");
    let metadata_path = worktree_path.join(".elowen-job.json");
    let intent_guidance = execution_intent_guidance(&dispatch.execution_intent);

    fs::write(
        &prompt_path,
        format!(
            "# Elowen Job Request\n\n\
            - Job: {}\n\
            - Thread: {}\n\
            - Target kind: {}\n\
            - Target: {}\n\
            - Branch: {}\n\
            - Base branch: {}\n\n\
            - Execution intent: {}\n\n\
            ## Requested Work\n\n{}\n",
            dispatch.job_id,
            dispatch.thread_id,
            dispatch.target_kind.as_str(),
            dispatch.target_name(),
            dispatch.branch_name.as_deref().unwrap_or("n/a"),
            dispatch.base_branch.as_deref().unwrap_or("n/a"),
            dispatch.execution_intent.as_str(),
            dispatch.prompt
        ) + &format!("\n## Execution Guidance\n\n{}\n", intent_guidance),
    )
    .await
    .with_context(|| format!("failed to write {}", prompt_path.display()))?;

    let metadata = serde_json::to_vec_pretty(&json!({
        "job_id": dispatch.job_id,
        "short_id": dispatch.short_id,
        "thread_id": dispatch.thread_id,
        "title": dispatch.title,
        "target_kind": dispatch.target_kind,
        "target_name": dispatch.target_name(),
        "target_name": dispatch.target_name(),
        "base_branch": dispatch.base_branch,
        "branch_name": dispatch.branch_name,
        "execution_intent": dispatch.execution_intent,
        "prompt": dispatch.prompt,
        "dispatched_at": dispatch.dispatched_at,
    }))
    .context("failed to serialize job metadata")?;

    let mut file = fs::File::create(&metadata_path)
        .await
        .with_context(|| format!("failed to create {}", metadata_path.display()))?;
    file.write_all(&metadata)
        .await
        .with_context(|| format!("failed to write {}", metadata_path.display()))?;

    Ok(())
}

fn execution_intent_guidance(intent: &ExecutionIntent) -> &'static str {
    match intent {
        ExecutionIntent::WorkspaceChange => {
            "Carry out the requested work, summarize what changed, and leave any execution artifacts ready for review."
        }
        ExecutionIntent::ReadOnly => {
            "Inspect and report only. Do not create durable repository changes, do not create commits, and do not ask for push approval. If execution leaves local artifacts behind, call that out in the result."
        }
    }
}

async fn capture_git_report(worktree_path: &Path) -> anyhow::Result<GitReport> {
    let status_output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["status", "--short"])
        .output()
        .await
        .context("failed to capture git status")?;
    let diff_stat_output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["diff", "--stat"])
        .output()
        .await
        .context("failed to capture git diff --stat")?;

    let status_lines = String::from_utf8_lossy(&status_output.stdout)
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .filter(|line| {
            let path = normalize_git_status_path(line);
            !is_runtime_artifact_path(path)
        })
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let changed_files = status_lines
        .iter()
        .map(|line| normalize_git_status_path(line).to_string())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let diff_stat = String::from_utf8_lossy(&diff_stat_output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");

    Ok(GitReport {
        status_lines,
        diff_stat: (!diff_stat.is_empty()).then_some(diff_stat),
        changed_files,
    })
}

async fn maybe_create_job_commit(
    worktree_path: &Path,
    dispatch: &JobDispatchMessage,
    git_report: &GitReport,
) -> anyhow::Result<Option<CommitRecord>> {
    if git_report.changed_files.is_empty() {
        return Ok(None);
    }

    let mut add = Command::new("git");
    add.arg("-C").arg(worktree_path).arg("add").arg("--");
    for path in &git_report.changed_files {
        add.arg(path);
    }
    let add_output = add
        .output()
        .await
        .context("failed to stage job changes before commit")?;
    if !add_output.status.success() {
        anyhow::bail!(
            "git add failed before job commit: {}",
            summarize_command_output(&add_output.stdout, &add_output.stderr)
        );
    }

    let staged_output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["diff", "--cached", "--name-only"])
        .output()
        .await
        .context("failed to inspect staged job changes")?;
    let staged_files = String::from_utf8_lossy(&staged_output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    if staged_files.is_empty() {
        return Ok(None);
    }

    let message = build_job_commit_message(dispatch);
    let commit_output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["commit", "-m"])
        .arg(&message)
        .output()
        .await
        .context("failed to create job commit")?;
    if !commit_output.status.success() {
        anyhow::bail!(
            "git commit failed for job `{}`: {}",
            dispatch.short_id,
            summarize_command_output(&commit_output.stdout, &commit_output.stderr)
        );
    }

    let rev_output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["rev-parse", "HEAD"])
        .output()
        .await
        .context("failed to read committed job revision")?;
    if !rev_output.status.success() {
        anyhow::bail!(
            "git rev-parse failed after job commit: {}",
            summarize_command_output(&rev_output.stdout, &rev_output.stderr)
        );
    }
    let sha = String::from_utf8_lossy(&rev_output.stdout)
        .trim()
        .to_string();
    let short_sha = sha.chars().take(8).collect::<String>();

    Ok(Some(CommitRecord {
        sha,
        short_sha,
        message,
        changed_files: staged_files,
    }))
}

fn build_job_commit_message(dispatch: &JobDispatchMessage) -> String {
    let title = truncate_text(dispatch.title.trim(), 48);
    format!("Elowen job {}: {}", dispatch.short_id, title)
}

async fn run_validation_suite(
    config: &EdgeConfig,
    worktree_path: &Path,
    sandbox: &SandboxPolicy,
) -> anyhow::Result<ValidationResults> {
    let plan = load_validation_plan(worktree_path).await?;
    let build = match plan.build {
        Some(spec) => {
            execute_validation_command("build", spec, config.validation_timeout_secs, sandbox).await
        }
        None => json!({
            "status": "not_configured",
            "reason": "no build command is configured for this repository",
        }),
    };

    let test = if validation_status(&build) == "failed" {
        json!({
            "status": "skipped",
            "reason": "test command was skipped because the build command failed",
        })
    } else if validation_status(&build) == "sandbox_blocked" {
        json!({
            "status": "skipped",
            "reason": "test command was skipped because the build command was blocked by the sandbox",
        })
    } else {
        match plan.test {
            Some(spec) => {
                execute_validation_command("test", spec, config.validation_timeout_secs, sandbox)
                    .await
            }
            None => json!({
                "status": "not_configured",
                "reason": "no test command is configured for this repository",
            }),
        }
    };

    let overall_success = matches!(validation_status(&build), "passed" | "not_configured")
        && matches!(
            validation_status(&test),
            "passed" | "not_configured" | "skipped"
        );

    Ok(ValidationResults {
        build,
        test,
        overall_success,
        config_source: plan.config_source,
    })
}

async fn load_validation_plan(worktree_path: &Path) -> anyhow::Result<ValidationPlan> {
    let config_path = worktree_path.join(".assistant").join("config.toml");
    if fs::metadata(&config_path).await.is_ok() {
        let contents = fs::read_to_string(&config_path)
            .await
            .with_context(|| format!("failed to read {}", config_path.display()))?;
        let assistant_config = toml::from_str::<AssistantConfig>(&contents)
            .with_context(|| format!("failed to parse {}", config_path.display()))?;
        return build_validation_plan(
            worktree_path,
            assistant_config.validation,
            format!("repo config at {}", config_path.display()),
        );
    }

    if fs::metadata(worktree_path.join("Cargo.toml")).await.is_ok() {
        return build_validation_plan(
            worktree_path,
            ValidationConfig {
                build: Some(vec!["cargo".to_string(), "check".to_string()]),
                test: Some(vec![
                    "cargo".to_string(),
                    "test".to_string(),
                    "--quiet".to_string(),
                ]),
                working_dir: None,
            },
            "inferred from Cargo.toml".to_string(),
        );
    }

    Ok(ValidationPlan {
        build: None,
        test: None,
        config_source: "no repository validation config found".to_string(),
    })
}

fn build_validation_plan(
    worktree_path: &Path,
    config: ValidationConfig,
    config_source: String,
) -> anyhow::Result<ValidationPlan> {
    let working_dir = resolve_working_dir(worktree_path, config.working_dir.as_deref())?;
    Ok(ValidationPlan {
        build: build_command_spec(config.build, &working_dir)?,
        test: build_command_spec(config.test, &working_dir)?,
        config_source,
    })
}

fn resolve_working_dir(
    worktree_path: &Path,
    configured_dir: Option<&str>,
) -> anyhow::Result<PathBuf> {
    let Some(configured_dir) = configured_dir
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(worktree_path.to_path_buf());
    };

    let resolved = worktree_path.join(configured_dir);
    if !resolved.exists() {
        anyhow::bail!(
            "configured validation working_dir does not exist: {}",
            resolved.display()
        );
    }

    Ok(resolved)
}

fn build_command_spec(
    argv: Option<Vec<String>>,
    working_dir: &Path,
) -> anyhow::Result<Option<CommandSpec>> {
    let Some(argv) = argv else {
        return Ok(None);
    };

    let argv = argv
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    if argv.is_empty() {
        anyhow::bail!("validation command entries must not be empty");
    }

    Ok(Some(CommandSpec {
        argv,
        working_dir: working_dir.to_path_buf(),
    }))
}

async fn execute_validation_command(
    kind: &str,
    spec: CommandSpec,
    timeout_secs: u64,
    sandbox: &SandboxPolicy,
) -> Value {
    let started_at = Utc::now();
    let started = Instant::now();
    let argv = spec.argv;
    let original_working_dir = spec.working_dir.to_string_lossy().to_string();
    let working_dir = match enforce_worktree_containment(
        &sandbox.worktree_path,
        &spec.working_dir,
        "validation working directory",
    )
    .await
    {
        Ok(path) => path,
        Err(error) => {
            return sandbox_blocked_report(
                kind,
                &argv,
                &original_working_dir,
                started_at,
                started.elapsed(),
                error.to_string(),
            );
        }
    };
    let program = match resolve_validation_program(sandbox, &working_dir, &argv[0]).await {
        Ok(path) => path,
        Err(error) => {
            return sandbox_blocked_report(
                kind,
                &argv,
                &original_working_dir,
                started_at,
                started.elapsed(),
                error.to_string(),
            );
        }
    };
    let mut command = Command::new(&program);
    command.args(&argv[1..]).current_dir(&working_dir);
    apply_sandbox_environment(&mut command, sandbox);

    let working_dir = working_dir.to_string_lossy().to_string();
    let resolved_program = program.to_string_lossy().to_string();

    match tokio::time::timeout(Duration::from_secs(timeout_secs), command.output()).await {
        Err(_) => json!({
            "status": "failed",
            "command": argv,
            "resolved_program": resolved_program,
            "working_dir": working_dir,
            "started_at": started_at,
            "duration_ms": started.elapsed().as_millis() as u64,
            "reason": format!("{kind} command timed out after {timeout_secs} seconds"),
        }),
        Ok(Err(error)) => json!({
            "status": "failed",
            "command": argv,
            "resolved_program": resolved_program,
            "working_dir": working_dir,
            "started_at": started_at,
            "duration_ms": started.elapsed().as_millis() as u64,
            "reason": error.to_string(),
        }),
        Ok(Ok(output)) => json!({
            "status": if output.status.success() { "passed" } else { "failed" },
            "command": argv,
            "resolved_program": resolved_program,
            "working_dir": working_dir,
            "started_at": started_at,
            "duration_ms": started.elapsed().as_millis() as u64,
            "exit_code": output.status.code(),
            "stdout": truncate_text(&String::from_utf8_lossy(&output.stdout), 4000),
            "stderr": truncate_text(&String::from_utf8_lossy(&output.stderr), 4000),
        }),
    }
}

fn validation_status(report: &Value) -> &str {
    report
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
}

async fn ensure_repo_root(repo_root: &Path, repo_name: &str) -> anyhow::Result<()> {
    let metadata = fs::metadata(repo_root)
        .await
        .with_context(|| format!("workspace repository `{repo_name}` was not found"))?;

    if !metadata.is_dir() {
        anyhow::bail!("workspace repository `{repo_name}` is not a directory");
    }

    let git_dir = repo_root.join(".git");
    if fs::metadata(&git_dir).await.is_err() {
        anyhow::bail!("workspace repository `{repo_name}` is not a git checkout");
    }

    Ok(())
}

fn summarize_process_output(stdout: &str, stderr: &str) -> String {
    let stdout = stdout.trim();
    let stderr = stderr.trim();

    if !stderr.is_empty() {
        truncate_text(stderr, 240)
    } else if !stdout.is_empty() {
        truncate_text(stdout, 240)
    } else {
        "process exited without output".to_string()
    }
}

fn classify_push_failure(error: &anyhow::Error) -> (String, String) {
    let detail = error.to_string();
    if detail.contains("worktree")
        || detail.contains("sandbox")
        || detail.contains("missing")
        || detail.contains("containment")
    {
        ("infrastructure".to_string(), detail)
    } else {
        ("execution".to_string(), detail)
    }
}

fn normalize_git_status_path(line: &str) -> &str {
    let path = line.get(3..).unwrap_or(line).trim();
    path.rsplit_once("->")
        .map(|(_, new_path)| new_path.trim())
        .unwrap_or(path)
}

fn is_runtime_artifact_path(path: &str) -> bool {
    matches!(
        path,
        ".elowen-job.json"
            | "elowen-codex-last-message.txt"
            | "elowen-job-request.md"
            | "elowen-job-summary.md"
            | "elowen-runner-error.log"
            | "elowen-runner-output.jsonl"
    ) || path.starts_with(".elowen-sandbox")
}

pub(crate) async fn preflight_codex_runner(config: &EdgeConfig) -> anyhow::Result<()> {
    let Some(command) = config.codex_command.as_deref() else {
        return Ok(());
    };

    validate_codex_args(&config.codex_args)?;
    if is_disallowed_validation_program(command) {
        return Err(sandbox_error(format!(
            "configured Codex command `{command}` is not allowed; point ELOWEN_CODEX_COMMAND at the Codex binary directly"
        )));
    }

    let output = Command::new(command)
        .arg("--version")
        .output()
        .await
        .with_context(|| format!("failed to start configured Codex CLI `{command}`"))?;

    if !output.status.success() {
        anyhow::bail!(
            "configured Codex CLI `{command}` failed preflight with status {}",
            output.status
        );
    }

    let version = truncate_text(&String::from_utf8_lossy(&output.stdout), 200);
    info!(command = %command, version = %version, "Codex CLI preflight succeeded");
    Ok(())
}

fn build_codex_exec_args(
    config: &EdgeConfig,
    worktree_path: &Path,
    last_message_path: &Path,
) -> anyhow::Result<Vec<String>> {
    validate_codex_args(&config.codex_args)?;

    let mut args = vec![
        "exec".to_string(),
        "--json".to_string(),
        "--ephemeral".to_string(),
        "-C".to_string(),
        worktree_path.to_string_lossy().to_string(),
        "-o".to_string(),
        last_message_path.to_string_lossy().to_string(),
    ];
    args.extend(
        config
            .codex_args
            .iter()
            .filter(|arg| !is_redundant_codex_arg(arg))
            .cloned(),
    );
    args.push("-".to_string());
    Ok(args)
}

fn validate_codex_args(args: &[String]) -> anyhow::Result<()> {
    for arg in args {
        let normalized = arg.trim();
        if normalized.is_empty() {
            continue;
        }

        if matches!(normalized, "exec" | "e" | "-" | "review" | "resume") {
            anyhow::bail!(
                "ELOWEN_CODEX_ARGS_JSON should contain extra Codex exec flags only; remove `{normalized}`"
            );
        }

        if matches!(normalized, "-C" | "--cd" | "-o" | "--output-last-message") {
            anyhow::bail!(
                "ELOWEN_CODEX_ARGS_JSON must not include `{normalized}` because elowen-edge manages the working directory and output paths"
            );
        }

        if normalized.starts_with("--cd=") || normalized.starts_with("--output-last-message=") {
            anyhow::bail!(
                "ELOWEN_CODEX_ARGS_JSON must not override Codex working directory or output file paths"
            );
        }
    }

    Ok(())
}

fn is_redundant_codex_arg(arg: &str) -> bool {
    matches!(arg.trim(), "--json" | "--ephemeral")
}

fn extract_codex_event_messages(stdout: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|event| {
            let item = event.get("item")?;
            if item.get("type")?.as_str()? != "agent_message" {
                return None;
            }

            item.get("text")?
                .as_str()
                .map(|text| truncate_text(text, 1000))
        })
        .collect()
}

fn summarize_command_output(stdout: &[u8], stderr: &[u8]) -> String {
    let stdout = truncate_text(&String::from_utf8_lossy(stdout), 1000);
    let stderr = truncate_text(&String::from_utf8_lossy(stderr), 1000);
    format!("stdout: {stdout}; stderr: {stderr}")
}

fn truncate_text(value: &str, max_chars: usize) -> String {
    let mut truncated = value.trim().chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        truncated.push_str("...");
    }
    truncated
}

#[cfg(test)]
mod tests {
    use super::execution_intent_guidance;
    use crate::{
        contracts::ExecutionIntent,
        sandbox::{SandboxMode, is_disallowed_validation_program, validation_program_name},
    };

    #[test]
    fn sandbox_mode_defaults_to_workspace() {
        assert_eq!(SandboxMode::from_env(None).unwrap(), SandboxMode::Workspace);
    }

    #[test]
    fn shell_validation_commands_are_blocked() {
        assert!(is_disallowed_validation_program("powershell"));
        assert!(is_disallowed_validation_program("bash"));
        assert!(!is_disallowed_validation_program("cargo"));
    }

    #[test]
    fn validation_program_name_uses_file_name() {
        assert_eq!(validation_program_name(r"C:\tools\cargo.exe"), "cargo.exe");
    }

    #[test]
    fn read_only_guidance_mentions_no_commit_or_push() {
        let guidance = execution_intent_guidance(&ExecutionIntent::ReadOnly);
        assert!(guidance.contains("do not create commits"));
        assert!(guidance.contains("do not ask for push approval"));
    }
}
