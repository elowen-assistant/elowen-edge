use anyhow::Context;
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    env,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{fs, io::AsyncWriteExt, process::Command, sync::Mutex};
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
    workspace_root: PathBuf,
    worktree_root: PathBuf,
    codex_command: Option<String>,
    codex_args: Vec<String>,
    simulated_run_ms: u64,
}

#[derive(Debug, Serialize)]
struct RegisterDeviceRequest {
    name: String,
    primary_flag: bool,
    allowed_repos: Vec<String>,
    capabilities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobLifecycleEvent {
    job_id: String,
    device_id: String,
    event_type: String,
    status: Option<String>,
    result: Option<String>,
    failure_class: Option<String>,
    worktree_path: Option<String>,
    detail: Option<String>,
    payload_json: Option<Value>,
    created_at: DateTime<Utc>,
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

struct CommandOutcome {
    detail: String,
    result: String,
    failure_class: Option<String>,
    summary_markdown: String,
    execution_report: Value,
    approval_summary: Option<String>,
}

struct GitReport {
    status_lines: Vec<String>,
    diff_stat: Option<String>,
    changed_files: Vec<String>,
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
                short_id = %dispatch.short_id,
                repo_name = %dispatch.repo_name,
                branch_name = %dispatch.branch_name,
                "received job dispatch"
            );

            if let Err(error) = handle_job_dispatch(
                dispatch,
                dispatch_config.clone(),
                dispatch_nats.clone(),
                dispatch_active_job_id.clone(),
            )
            .await
            {
                warn!(error = %error, "job dispatch handler failed");
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
        let workspace_root = PathBuf::from(
            env::var("ELOWEN_EDGE_WORKSPACE_ROOT").unwrap_or_else(|_| "/workspace".to_string()),
        );
        let worktree_root = env::var("ELOWEN_EDGE_WORKTREE_ROOT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| workspace_root.join(".elowen").join("worktrees"));

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
            workspace_root,
            worktree_root,
            codex_command: env::var("ELOWEN_CODEX_COMMAND")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            codex_args: parse_json_list_env("ELOWEN_CODEX_ARGS_JSON")?,
            simulated_run_ms: env::var("ELOWEN_SIMULATED_RUN_MS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(1500),
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

async fn handle_job_dispatch(
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
            device_id: config.device_id.clone(),
            event_type: "job.accepted".to_string(),
            status: Some("accepted".to_string()),
            result: None,
            failure_class: None,
            worktree_path: None,
            detail: Some("edge accepted dispatched job".to_string()),
            payload_json: Some(json!({
                "repo_name": dispatch.repo_name,
                "branch_name": dispatch.branch_name,
                "base_branch": dispatch.base_branch,
            })),
            created_at: Utc::now(),
        },
    )
    .await?;

    let worktree_path = create_worktree(dispatch, config).await?;
    let worktree_path_str = worktree_path.to_string_lossy().to_string();

    publish_job_event(
        nats,
        JobLifecycleEvent {
            job_id: dispatch.job_id.clone(),
            device_id: config.device_id.clone(),
            event_type: "job.worktree_created".to_string(),
            status: Some("accepted".to_string()),
            result: None,
            failure_class: None,
            worktree_path: Some(worktree_path_str.clone()),
            detail: Some("git worktree created for dispatched job".to_string()),
            payload_json: Some(json!({
                "repo_name": dispatch.repo_name,
                "branch_name": dispatch.branch_name,
                "base_branch": dispatch.base_branch,
            })),
            created_at: Utc::now(),
        },
    )
    .await?;

    publish_job_event(
        nats,
        JobLifecycleEvent {
            job_id: dispatch.job_id.clone(),
            device_id: config.device_id.clone(),
            event_type: "job.started".to_string(),
            status: Some("running".to_string()),
            result: None,
            failure_class: None,
            worktree_path: Some(worktree_path_str.clone()),
            detail: Some("job execution started".to_string()),
            payload_json: None,
            created_at: Utc::now(),
        },
    )
    .await?;

    let command_outcome = match run_codex_wrapper(dispatch, config, &worktree_path).await {
        Ok(outcome) => outcome,
        Err(error) => {
            publish_job_event(
                nats,
                JobLifecycleEvent {
                    job_id: dispatch.job_id.clone(),
                    device_id: config.device_id.clone(),
                    event_type: "job.failed".to_string(),
                    status: Some("failed".to_string()),
                    result: Some("failure".to_string()),
                    failure_class: Some("execution".to_string()),
                    worktree_path: Some(worktree_path_str.clone()),
                    detail: Some(error.to_string()),
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
            device_id: config.device_id.clone(),
            event_type: "job.completed".to_string(),
            status: Some("completed".to_string()),
            result: Some(command_outcome.result.clone()),
            failure_class: command_outcome.failure_class.clone(),
            worktree_path: Some(worktree_path_str.clone()),
            detail: Some(command_outcome.detail),
            payload_json: Some(json!({
                "summary_markdown": command_outcome.summary_markdown,
                "execution_report": command_outcome.execution_report,
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
                device_id: config.device_id.clone(),
                event_type: "job.awaiting_approval".to_string(),
                status: Some("awaiting_approval".to_string()),
                result: Some(command_outcome.result),
                failure_class: command_outcome.failure_class,
                worktree_path: Some(worktree_path_str),
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

async fn create_worktree(
    dispatch: &JobDispatchMessage,
    config: &EdgeConfig,
) -> anyhow::Result<PathBuf> {
    let repo_root = config.workspace_root.join(&dispatch.repo_name);
    ensure_repo_root(&repo_root, &dispatch.repo_name).await?;

    let worktree_parent = config.worktree_root.join(&dispatch.repo_name);
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
        .arg(&dispatch.branch_name)
        .arg(&worktree_path)
        .arg(&dispatch.base_branch)
        .output()
        .await
        .with_context(|| format!("failed to create worktree for {}", dispatch.repo_name))?;

    if !output.status.success() {
        anyhow::bail!(
            "git worktree add failed: {}",
            summarize_command_output(&output.stdout, &output.stderr)
        );
    }

    write_job_request_files(dispatch, &worktree_path).await?;
    Ok(worktree_path)
}

async fn run_codex_wrapper(
    dispatch: &JobDispatchMessage,
    config: &EdgeConfig,
    worktree_path: &Path,
) -> anyhow::Result<CommandOutcome> {
    if let Some(command) = &config.codex_command {
        return run_external_codex_wrapper(dispatch, config, worktree_path, command).await;
    }

    run_simulated_codex_wrapper(dispatch, config, worktree_path).await
}

async fn run_simulated_codex_wrapper(
    dispatch: &JobDispatchMessage,
    config: &EdgeConfig,
    worktree_path: &Path,
) -> anyhow::Result<CommandOutcome> {
    tokio::time::sleep(Duration::from_millis(config.simulated_run_ms)).await;

    let summary_path = worktree_path.join("elowen-job-summary.md");
    let summary_body = format!(
        "# Simulated Slice 4 Execution\n\n\
        - Job: {}\n\
        - Thread: {}\n\
        - Repo: {}\n\
        - Branch: {}\n\
        - Base branch: {}\n\
        - Runner: simulated\n\n\
        ## Request\n\n{}\n",
        dispatch.job_id,
        dispatch.thread_id,
        dispatch.repo_name,
        dispatch.branch_name,
        dispatch.base_branch,
        dispatch.request_text
    );

    fs::write(&summary_path, summary_body)
        .await
        .with_context(|| format!("failed to write {}", summary_path.display()))?;

    let git_report = capture_git_report(worktree_path).await?;
    let execution_report = json!({
        "runner": "simulated",
        "summary_path": summary_path.to_string_lossy().to_string(),
        "build": {
            "status": "not_run",
            "reason": "no project-specific build command is configured in the Slice 5 scaffold"
        },
        "test": {
            "status": "not_run",
            "reason": "no project-specific test command is configured in the Slice 5 scaffold"
        },
        "git_status": git_report.status_lines,
        "diff_stat": git_report.diff_stat,
        "changed_files": git_report.changed_files,
    });
    let summary_markdown = format!(
        "# Job Summary\n\n\
        - Result: success\n\
        - Runner: simulated\n\
        - Repo: {}\n\
        - Branch: {}\n\n\
        ## Request\n\n{}\n\n\
        ## Validation\n\n\
        - Build: not run\n\
        - Test: not run\n\n\
        ## Workspace Changes\n\n\
        - Changed entries: {}\n\
        - Diff stat: {}\n",
        dispatch.repo_name,
        dispatch.branch_name,
        dispatch.request_text,
        git_report.changed_files.len(),
        git_report
            .diff_stat
            .clone()
            .unwrap_or_else(|| "no tracked diff".to_string()),
    );
    let approval_summary = Some(format!(
        "Approve push for `{}` on branch `{}`. Review the generated summary and {} changed entries before pushing.",
        dispatch.repo_name,
        dispatch.branch_name,
        git_report.changed_files.len(),
    ));

    Ok(CommandOutcome {
        detail: "simulated Codex wrapper completed successfully".to_string(),
        result: "success".to_string(),
        failure_class: None,
        summary_markdown,
        execution_report,
        approval_summary,
    })
}

async fn run_external_codex_wrapper(
    dispatch: &JobDispatchMessage,
    config: &EdgeConfig,
    worktree_path: &Path,
    command: &str,
) -> anyhow::Result<CommandOutcome> {
    let output = Command::new(command)
        .args(&config.codex_args)
        .current_dir(worktree_path)
        .env("ELOWEN_JOB_ID", &dispatch.job_id)
        .env("ELOWEN_JOB_SHORT_ID", &dispatch.short_id)
        .env("ELOWEN_THREAD_ID", &dispatch.thread_id)
        .env("ELOWEN_JOB_TITLE", &dispatch.title)
        .env("ELOWEN_REPO_NAME", &dispatch.repo_name)
        .env("ELOWEN_BRANCH_NAME", &dispatch.branch_name)
        .env("ELOWEN_BASE_BRANCH", &dispatch.base_branch)
        .env("ELOWEN_WORKTREE_PATH", worktree_path)
        .env("ELOWEN_REQUEST_TEXT", &dispatch.request_text)
        .output()
        .await
        .with_context(|| format!("failed to run Codex wrapper command `{command}`"))?;

    let stdout = truncate_text(&String::from_utf8_lossy(&output.stdout), 4000);
    let stderr = truncate_text(&String::from_utf8_lossy(&output.stderr), 4000);
    let log_path = worktree_path.join("elowen-runner-output.log");
    let log_body = format!("stdout:\n{}\n\nstderr:\n{}\n", stdout, stderr);
    fs::write(&log_path, log_body)
        .await
        .with_context(|| format!("failed to write {}", log_path.display()))?;

    if !output.status.success() {
        anyhow::bail!("Codex wrapper command failed with status {}", output.status);
    }

    let git_report = capture_git_report(worktree_path).await?;
    let execution_report = json!({
        "runner": "external",
        "command": command,
        "args": config.codex_args.clone(),
        "stdout": stdout,
        "stderr": stderr,
        "log_path": log_path.to_string_lossy().to_string(),
        "build": {
            "status": "not_run",
            "reason": "external runner did not emit a build report"
        },
        "test": {
            "status": "not_run",
            "reason": "external runner did not emit a test report"
        },
        "git_status": git_report.status_lines,
        "diff_stat": git_report.diff_stat,
        "changed_files": git_report.changed_files,
    });
    let summary_markdown = format!(
        "# Job Summary\n\n\
        - Result: success\n\
        - Runner: external\n\
        - Repo: {}\n\
        - Branch: {}\n\n\
        ## Request\n\n{}\n\n\
        ## Runner Output\n\n\
        - Command: `{}`\n\
        - Log: `{}`\n\n\
        ## Workspace Changes\n\n\
        - Changed entries: {}\n\
        - Diff stat: {}\n",
        dispatch.repo_name,
        dispatch.branch_name,
        dispatch.request_text,
        command,
        log_path.to_string_lossy(),
        git_report.changed_files.len(),
        git_report
            .diff_stat
            .clone()
            .unwrap_or_else(|| "no tracked diff".to_string()),
    );
    let approval_summary = Some(format!(
        "Approve push for `{}` on branch `{}` after reviewing the external runner output and {} changed entries.",
        dispatch.repo_name,
        dispatch.branch_name,
        git_report.changed_files.len(),
    ));

    Ok(CommandOutcome {
        detail: format!("external Codex wrapper `{command}` completed successfully"),
        result: "success".to_string(),
        failure_class: None,
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

    fs::write(
        &prompt_path,
        format!(
            "# Elowen Job Request\n\n\
            - Job: {}\n\
            - Thread: {}\n\
            - Repo: {}\n\
            - Branch: {}\n\
            - Base branch: {}\n\n\
            ## Requested Work\n\n{}\n",
            dispatch.job_id,
            dispatch.thread_id,
            dispatch.repo_name,
            dispatch.branch_name,
            dispatch.base_branch,
            dispatch.request_text
        ),
    )
    .await
    .with_context(|| format!("failed to write {}", prompt_path.display()))?;

    let metadata = serde_json::to_vec_pretty(&json!({
        "job_id": dispatch.job_id,
        "short_id": dispatch.short_id,
        "thread_id": dispatch.thread_id,
        "title": dispatch.title,
        "repo_name": dispatch.repo_name,
        "base_branch": dispatch.base_branch,
        "branch_name": dispatch.branch_name,
        "request_text": dispatch.request_text,
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
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let changed_files = status_lines
        .iter()
        .map(|line| line.get(3..).unwrap_or(line.as_str()).trim().to_string())
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

async fn publish_job_event(
    nats: &async_nats::Client,
    event: JobLifecycleEvent,
) -> anyhow::Result<()> {
    let payload = serde_json::to_vec(&event).context("failed to serialize job lifecycle event")?;
    nats.publish("elowen.jobs.events".to_string(), payload.into())
        .await
        .context("failed to publish job lifecycle event")?;
    Ok(())
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

fn parse_json_list_env(key: &str) -> anyhow::Result<Vec<String>> {
    let Some(value) = env::var(key).ok().filter(|value| !value.trim().is_empty()) else {
        return Ok(Vec::new());
    };

    serde_json::from_str::<Vec<String>>(&value)
        .with_context(|| format!("failed to parse {key} as a JSON string array"))
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
