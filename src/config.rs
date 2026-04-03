//! Runtime configuration and startup parsing.

use anyhow::Context;
use std::{
    collections::HashMap,
    env, fs as stdfs,
    path::{Path, PathBuf},
};

use crate::{SandboxMode, detect_device_id, detect_device_name, parse_bool};

/// Overlay of environment variables loaded from an env file.
pub(crate) type EnvOverlay = HashMap<String, String>;

/// Startup-only command-line options.
pub(crate) struct StartupOptions {
    pub(crate) env_file: Option<PathBuf>,
}

/// Configuration required by the edge runtime after startup parsing.
#[derive(Clone)]
pub(crate) struct EdgeConfig {
    pub(crate) api_url: String,
    pub(crate) nats_url: String,
    pub(crate) device_id: String,
    pub(crate) device_name: String,
    pub(crate) primary_flag: bool,
    pub(crate) allowed_repos: Vec<String>,
    pub(crate) capabilities: Vec<String>,
    pub(crate) workspace_root: PathBuf,
    pub(crate) worktree_root: PathBuf,
    pub(crate) codex_command: Option<String>,
    pub(crate) codex_args: Vec<String>,
    pub(crate) simulated_run_ms: u64,
    pub(crate) validation_timeout_secs: u64,
    pub(crate) sandbox_mode: SandboxMode,
}

impl EdgeConfig {
    /// Loads the runtime config from the environment and env-file overlay.
    pub(crate) fn from_env(env_overlay: &EnvOverlay) -> anyhow::Result<Self> {
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

/// Parses CLI startup arguments.
pub(crate) fn parse_startup_options() -> anyhow::Result<StartupOptions> {
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

/// Loads optional env-file values while letting the process environment win.
pub(crate) fn load_env_overlay(env_file: Option<&Path>) -> anyhow::Result<EnvOverlay> {
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

pub(crate) fn env_value(key: &str, env_overlay: &EnvOverlay) -> Option<String> {
    env_overlay
        .get(key)
        .cloned()
        .or_else(|| env::var(key).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
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

fn startup_usage() -> &'static str {
    "Usage: elowen-edge [--env-file PATH]\n\n\
Reads runtime configuration from the process environment. When --env-file is set,\n\
the file is parsed first and the current process environment still wins on conflicts.\n\
You can also set ELOWEN_EDGE_ENV_FILE instead of passing --env-file.\n"
}

fn parse_env_file_value(raw_value: &str) -> String {
    if raw_value.len() >= 2 {
        let bytes = raw_value.as_bytes();
        let is_quoted = (bytes[0] == b'\"' && bytes[raw_value.len() - 1] == b'\"')
            || (bytes[0] == b'\'' && bytes[raw_value.len() - 1] == b'\'');
        if is_quoted {
            return raw_value[1..raw_value.len() - 1].to_string();
        }
    }

    raw_value.to_string()
}
