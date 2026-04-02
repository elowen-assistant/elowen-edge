use anyhow::Context;
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    env, fs as stdfs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{fs, io::AsyncWriteExt, process::Command, sync::Mutex};
use tracing::{info, warn};

type EnvOverlay = HashMap<String, String>;

struct StartupOptions {
    env_file: Option<PathBuf>,
}

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
    validation_timeout_secs: u64,
    sandbox_mode: SandboxMode,
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
    correlation_id: String,
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
    correlation_id: String,
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

#[derive(Debug, Deserialize, Default)]
struct AssistantConfig {
    #[serde(default)]
    validation: ValidationConfig,
}

#[derive(Debug, Deserialize, Default)]
struct ValidationConfig {
    build: Option<Vec<String>>,
    test: Option<Vec<String>>,
    working_dir: Option<String>,
}

struct ValidationPlan {
    build: Option<CommandSpec>,
    test: Option<CommandSpec>,
    config_source: String,
}

struct CommandSpec {
    argv: Vec<String>,
    working_dir: PathBuf,
}

struct ValidationResults {
    build: Value,
    test: Value,
    overall_success: bool,
    config_source: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SandboxMode {
    Off,
    Workspace,
}

#[derive(Clone)]
struct SandboxPolicy {
    mode: SandboxMode,
    worktree_path: PathBuf,
    sandbox_root: PathBuf,
    temp_root: PathBuf,
    cache_root: PathBuf,
    policy_path: PathBuf,
}

const SANDBOX_ERROR_PREFIX: &str = "sandbox blocked: ";

fn init_tracing(service_name: &'static str, env_overlay: &EnvOverlay) {
    let env_filter = env_value("RUST_LOG", env_overlay)
        .map(tracing_subscriber::EnvFilter::new)
        .or_else(|| tracing_subscriber::EnvFilter::try_from_default_env().ok())
        .unwrap_or_else(|| tracing_subscriber::EnvFilter::new("info"));
    let log_format =
        env_value("ELOWEN_LOG_FORMAT", env_overlay).unwrap_or_else(|| "plain".to_string());
    let builder = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(true);

    if log_format.eq_ignore_ascii_case("json") {
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

    info!(service = service_name, log_format = %log_format, "tracing initialized");
}

fn main() -> anyhow::Result<()> {
    let startup = parse_startup_options()?;
    let env_overlay = load_env_overlay(startup.env_file.as_deref())?;
    init_tracing("elowen-edge", &env_overlay);

    if let Some(env_file) = startup.env_file.as_ref() {
        info!(path = %env_file.display(), "loaded edge env file");
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?;

    runtime.block_on(async_main(env_overlay))
}

async fn async_main(env_overlay: EnvOverlay) -> anyhow::Result<()> {
    let config = EdgeConfig::from_env(&env_overlay)?;
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
                correlation_id = %dispatch.correlation_id,
                short_id = %dispatch.short_id,
                repo_name = %dispatch.repo_name,
                branch_name = %dispatch.branch_name,
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
    fn from_env(env_overlay: &EnvOverlay) -> anyhow::Result<Self> {
        let device_id = env_value("ELOWEN_DEVICE_ID", env_overlay).unwrap_or_else(detect_device_id);
        let device_name = env_value("ELOWEN_DEVICE_NAME", env_overlay)
            .unwrap_or_else(|| detect_device_name(&device_id));
        let workspace_root = PathBuf::from(
            env_value("ELOWEN_EDGE_WORKSPACE_ROOT", env_overlay)
                .unwrap_or_else(|| "/workspace".to_string()),
        );
        let worktree_root = env_value("ELOWEN_EDGE_WORKTREE_ROOT", env_overlay)
            .map(PathBuf::from)
            .unwrap_or_else(|| workspace_root.join(".elowen").join("worktrees"));

        Ok(Self {
            api_url: env_value("ELOWEN_API_URL", env_overlay)
                .unwrap_or_else(|| "http://elowen-api:8080".to_string())
                .trim_end_matches('/')
                .to_string(),
            nats_url: env_value("ELOWEN_NATS_URL", env_overlay)
                .context("missing ELOWEN_NATS_URL")?,
            device_id,
            device_name,
            primary_flag: env_value("ELOWEN_DEVICE_PRIMARY", env_overlay)
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
                env_overlay,
            ),
            capabilities: parse_list_env(
                "ELOWEN_DEVICE_CAPABILITIES",
                &["codex", "git", "build", "test"],
                env_overlay,
            ),
            workspace_root,
            worktree_root,
            codex_command: env_value("ELOWEN_CODEX_COMMAND", env_overlay),
            codex_args: parse_json_list_env("ELOWEN_CODEX_ARGS_JSON", env_overlay)?,
            simulated_run_ms: env_value("ELOWEN_SIMULATED_RUN_MS", env_overlay)
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(1500),
            validation_timeout_secs: env_value("ELOWEN_VALIDATION_TIMEOUT_SECS", env_overlay)
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(600),
            sandbox_mode: SandboxMode::from_env(
                env_value("ELOWEN_SANDBOX_MODE", env_overlay).as_deref(),
            )?,
        })
    }
}

impl SandboxMode {
    fn from_env(value: Option<&str>) -> anyhow::Result<Self> {
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

    fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Workspace => "workspace",
        }
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
            correlation_id: dispatch.correlation_id.clone(),
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
            correlation_id: dispatch.correlation_id.clone(),
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
                    worktree_path: Some(worktree_path_str.clone()),
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
                correlation_id: dispatch.correlation_id.clone(),
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
        .env("ELOWEN_REPO_NAME", &dispatch.repo_name)
        .env("ELOWEN_BRANCH_NAME", &dispatch.branch_name)
        .env("ELOWEN_BASE_BRANCH", &dispatch.base_branch)
        .env("ELOWEN_WORKTREE_PATH", &sandbox.worktree_path)
        .env("ELOWEN_REQUEST_TEXT", &dispatch.request_text)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_sandbox_environment(&mut child, sandbox);
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
    let git_report = capture_git_report(&sandbox.worktree_path).await?;
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
        base_detail
    } else if sandbox_blocked {
        format!(
            "sandbox blocked post-execution validation (build: {build_status}, test: {test_status})"
        )
    } else {
        format!(
            "validation failed after Codex execution (build: {build_status}, test: {test_status})"
        )
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
    execution_report.insert("sandbox".to_string(), sandbox_report_value(sandbox));
    let execution_report = Value::Object(execution_report);

    let summary_markdown = format!(
        "# Job Summary\n\n\
        - Result: {result}\n\
        - Runner: {runner}\n\
        - Repo: {}\n\
        - Branch: {}\n\
        - Validation config: {}\n\n\
        ## Request\n\n{}\n\n\
        ## Validation\n\n\
        - Build: {build_status}\n\
        - Test: {test_status}\n\n\
        ## Workspace Changes\n\n\
        - Changed entries: {}\n\
        - Diff stat: {}\n",
        dispatch.repo_name,
        dispatch.branch_name,
        validation.config_source,
        dispatch.request_text,
        git_report.changed_files.len(),
        git_report
            .diff_stat
            .clone()
            .unwrap_or_else(|| "no tracked diff".to_string()),
    );
    let approval_summary = validation.overall_success.then(|| {
        format!(
            "Approve push for `{}` on branch `{}` after reviewing the generated summary, validation output, and {} changed entries.",
            dispatch.repo_name,
            dispatch.branch_name,
            git_report.changed_files.len(),
        )
    });

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

async fn prepare_sandbox_policy(
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

fn sandbox_report_value(policy: &SandboxPolicy) -> Value {
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

fn apply_sandbox_environment(command: &mut Command, policy: &SandboxPolicy) {
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

async fn enforce_worktree_containment(
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

async fn resolve_validation_program(
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

fn is_disallowed_validation_program(program: &str) -> bool {
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

fn validation_program_name(program: &str) -> String {
    Path::new(program)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(program)
        .trim()
        .to_ascii_lowercase()
}

fn sandbox_blocked_report(
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

fn sandbox_error(message: impl Into<String>) -> anyhow::Error {
    anyhow::anyhow!("{SANDBOX_ERROR_PREFIX}{}", message.into())
}

fn classify_failure(error: &anyhow::Error) -> (String, String) {
    let detail = error.to_string();
    if let Some(stripped) = detail.strip_prefix(SANDBOX_ERROR_PREFIX) {
        ("sandbox".to_string(), stripped.to_string())
    } else {
        ("execution".to_string(), detail)
    }
}

async fn publish_job_event(
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

fn parse_list_env(key: &str, default: &[&str], env_overlay: &EnvOverlay) -> Vec<String> {
    let value = env_value(key, env_overlay).unwrap_or_else(|| default.join(","));
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

fn parse_json_list_env(key: &str, env_overlay: &EnvOverlay) -> anyhow::Result<Vec<String>> {
    let Some(value) = env_value(key, env_overlay) else {
        return Ok(Vec::new());
    };

    serde_json::from_str::<Vec<String>>(&value)
        .with_context(|| format!("failed to parse {key} as a JSON string array"))
}

fn parse_startup_options() -> anyhow::Result<StartupOptions> {
    let mut env_file = None;
    let mut args = env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--env-file" => {
                let value = args.next().context("missing value after --env-file")?;
                env_file = Some(PathBuf::from(value));
            }
            "--help" | "-h" => {
                print!("{}", startup_usage());
                std::process::exit(0);
            }
            _ => anyhow::bail!("unsupported argument `{arg}`\n\n{}", startup_usage()),
        }
    }

    if env_file.is_none() {
        env_file = env::var("ELOWEN_EDGE_ENV_FILE")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from);
    }

    Ok(StartupOptions { env_file })
}

fn startup_usage() -> &'static str {
    "Usage: elowen-edge [--env-file PATH]\n\n\
Reads runtime configuration from the process environment. When --env-file is set,\n\
the file is parsed first and the current process environment still wins on conflicts.\n\
You can also set ELOWEN_EDGE_ENV_FILE instead of passing --env-file.\n"
}

fn load_env_overlay(env_file: Option<&Path>) -> anyhow::Result<EnvOverlay> {
    let Some(env_file) = env_file else {
        return Ok(EnvOverlay::new());
    };

    let contents = stdfs::read_to_string(env_file)
        .with_context(|| format!("failed to read env file {}", env_file.display()))?;
    let mut env_overlay = EnvOverlay::new();

    for (index, raw_line) in contents.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let line = line.strip_prefix("export ").unwrap_or(line);
        let (key, raw_value) = line.split_once('=').with_context(|| {
            format!(
                "invalid env assignment in {} at line {}",
                env_file.display(),
                index + 1
            )
        })?;

        let key = key.trim();
        if key.is_empty() {
            anyhow::bail!(
                "invalid empty env key in {} at line {}",
                env_file.display(),
                index + 1
            );
        }

        env_overlay.insert(key.to_string(), parse_env_file_value(raw_value.trim()));
    }

    Ok(env_overlay)
}

fn parse_env_file_value(raw_value: &str) -> String {
    if raw_value.len() >= 2 {
        let bytes = raw_value.as_bytes();
        let is_quoted = (bytes[0] == b'"' && bytes[raw_value.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[raw_value.len() - 1] == b'\'');
        if is_quoted {
            return raw_value[1..raw_value.len() - 1].to_string();
        }
    }

    raw_value.to_string()
}

fn env_value(key: &str, env_overlay: &EnvOverlay) -> Option<String> {
    env_overlay
        .get(key)
        .cloned()
        .or_else(|| env::var(key).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

async fn preflight_codex_runner(config: &EdgeConfig) -> anyhow::Result<()> {
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
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
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

        if matches!(normalized, "-C" | "--cd" | "-o" | "--output-last-message") {
            let _ = iter.next();
        }
    }

    Ok(())
}

fn is_redundant_codex_arg(arg: &str) -> bool {
    matches!(arg.trim(), "--json" | "--ephemeral")
}

#[cfg(test)]
mod tests {
    use super::{SandboxMode, is_disallowed_validation_program, validation_program_name};

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
