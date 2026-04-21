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
    /// Optional env file loaded before runtime config parsing.
    pub(crate) env_file: Option<PathBuf>,
    /// Prints a new trust keypair and exits without starting the runtime.
    pub(crate) generate_trust_keypair: bool,
}

/// Configuration required by the edge runtime after startup parsing.
#[derive(Clone)]
pub(crate) struct EdgeConfig {
    /// Base URL for the orchestrator API.
    pub(crate) api_url: String,
    /// NATS connection string used for probes, dispatch, and events.
    pub(crate) nats_url: String,
    /// Stable device identity published to the orchestrator.
    pub(crate) device_id: String,
    /// Operator-visible device label.
    pub(crate) device_name: String,
    /// Marks whether this device should be treated as the primary edge.
    pub(crate) primary_flag: bool,
    /// Explicit repository names allowed for dispatch.
    pub(crate) allowed_repos: Vec<String>,
    /// Trusted parent directories scanned for nested repositories.
    pub(crate) allowed_repo_roots: Vec<PathBuf>,
    /// Discovered repos that should be hidden from dispatch selection.
    pub(crate) hidden_repos: Vec<String>,
    /// Nested paths under trusted roots that must not be scanned.
    pub(crate) excluded_repo_paths: Vec<PathBuf>,
    /// Capability strings advertised during registration.
    pub(crate) capabilities: Vec<String>,
    /// Workspace root used for local repo discovery defaults.
    pub(crate) workspace_root: PathBuf,
    /// Root folder under which disposable job worktrees are created.
    pub(crate) worktree_root: PathBuf,
    /// External Codex executable, if real runner mode is enabled.
    pub(crate) codex_command: Option<String>,
    /// Extra CLI arguments forwarded to the Codex command.
    pub(crate) codex_args: Vec<String>,
    /// Duration used by the simulated runner path.
    pub(crate) simulated_run_ms: u64,
    /// Timeout applied to repository validation commands.
    pub(crate) validation_timeout_secs: u64,
    /// Sandbox boundary applied around worktree execution.
    pub(crate) sandbox_mode: SandboxMode,
    /// Orchestrator keys pinned locally for trusted registration.
    pub(crate) trusted_orchestrator_keys: Vec<TrustedOrchestratorKey>,
    /// Current edge signing key used for trusted registration.
    pub(crate) edge_signing_key: Option<String>,
    /// Previous edge signing key used during trusted re-enrollment.
    pub(crate) previous_edge_signing_key: Option<String>,
}

/// Trusted orchestrator signing key accepted for challenge verification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TrustedOrchestratorKey {
    pub(crate) key_id: Option<String>,
    pub(crate) public_key: String,
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
        let allowed_repo_roots =
            parse_repo_root_env("ELOWEN_ALLOWED_REPO_ROOTS", &workspace_root, env_overlay)?;
        let excluded_repo_paths = parse_repo_policy_path_env(
            "ELOWEN_REPO_SCAN_EXCLUDE_PATHS",
            &workspace_root,
            &allowed_repo_roots,
            env_overlay,
        )?;

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
            allowed_repos: parse_allowed_repos_env(
                &[
                    "elowen-api",
                    "elowen-ui",
                    "elowen-edge",
                    "elowen-notes",
                    "elowen-platform",
                ],
                &allowed_repo_roots,
                env_overlay,
            ),
            allowed_repo_roots,
            hidden_repos: parse_list_env("ELOWEN_HIDDEN_REPOS", &[], env_overlay),
            excluded_repo_paths,
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
            trusted_orchestrator_keys: parse_trusted_orchestrator_keys(env_overlay)?,
            edge_signing_key: env_value("ELOWEN_EDGE_SIGNING_KEY", env_overlay),
            previous_edge_signing_key: env_value("ELOWEN_PREVIOUS_EDGE_SIGNING_KEY", env_overlay),
        })
    }
}

/// Parses CLI startup arguments.
pub(crate) fn parse_startup_options() -> anyhow::Result<StartupOptions> {
    let mut env_file = None;
    let mut generate_trust_keypair = false;
    let mut args = env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--generate-trust-keypair" => {
                generate_trust_keypair = true;
            }
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

    Ok(StartupOptions {
        env_file,
        generate_trust_keypair,
    })
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

/// Returns a trimmed config value, preferring the env file overlay over the
/// inherited process environment.
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
    parse_list_value(&value)
}

fn parse_allowed_repos_env(
    default: &[&str],
    allowed_repo_roots: &[PathBuf],
    env_overlay: &EnvOverlay,
) -> Vec<String> {
    if let Some(value) = env_value("ELOWEN_ALLOWED_REPOS", env_overlay) {
        return parse_list_value(&value);
    }

    if allowed_repo_roots.is_empty() {
        return default.iter().map(|value| value.to_string()).collect();
    }

    Vec::new()
}

fn parse_json_list_env(key: &str, env_overlay: &EnvOverlay) -> anyhow::Result<Vec<String>> {
    let Some(value) = env_value(key, env_overlay) else {
        return Ok(Vec::new());
    };

    serde_json::from_str::<Vec<String>>(&value)
        .with_context(|| format!("failed to parse {key} as a JSON string array"))
}

fn parse_list_value(value: &str) -> Vec<String> {
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

fn parse_repo_root_env(
    key: &str,
    workspace_root: &Path,
    env_overlay: &EnvOverlay,
) -> anyhow::Result<Vec<PathBuf>> {
    let Some(value) = env_value(key, env_overlay) else {
        return Ok(Vec::new());
    };

    let mut roots = Vec::new();

    for candidate in value.split(',') {
        let trimmed = candidate.trim();
        if trimmed.is_empty() {
            continue;
        }

        let raw_path = PathBuf::from(trimmed);
        let resolved = if raw_path.is_absolute() {
            raw_path
        } else {
            workspace_root.join(raw_path)
        };
        let canonical = stdfs::canonicalize(&resolved)
            .with_context(|| format!("failed to resolve repository root {}", resolved.display()))?;

        if !canonical.is_dir() {
            anyhow::bail!(
                "configured repository root {} is not a directory",
                canonical.display()
            );
        }

        if roots.iter().any(|existing| existing == &canonical) {
            continue;
        }

        roots.push(canonical);
    }

    Ok(roots)
}

fn parse_repo_policy_path_env(
    key: &str,
    workspace_root: &Path,
    allowed_repo_roots: &[PathBuf],
    env_overlay: &EnvOverlay,
) -> anyhow::Result<Vec<PathBuf>> {
    let Some(value) = env_value(key, env_overlay) else {
        return Ok(Vec::new());
    };

    let mut paths = Vec::new();

    for candidate in value.split(',') {
        let trimmed = candidate.trim();
        if trimmed.is_empty() {
            continue;
        }

        let raw_path = PathBuf::from(trimmed);
        let resolved = if raw_path.is_absolute() {
            raw_path
        } else {
            workspace_root.join(raw_path)
        };
        let canonical = stdfs::canonicalize(&resolved).with_context(|| {
            format!(
                "failed to resolve repository policy path {}",
                resolved.display()
            )
        })?;

        if !canonical.exists() {
            anyhow::bail!(
                "configured repository policy path {} does not exist",
                canonical.display()
            );
        }

        if !allowed_repo_roots
            .iter()
            .any(|root| canonical.starts_with(root))
        {
            anyhow::bail!(
                "configured repository policy path {} is outside the trusted repository roots",
                canonical.display()
            );
        }

        if paths.iter().any(|existing| existing == &canonical) {
            continue;
        }

        paths.push(canonical);
    }

    Ok(paths)
}

fn startup_usage() -> &'static str {
    "Usage: elowen-edge [--env-file PATH] [--generate-trust-keypair]\n\n\
Reads runtime configuration from the process environment. When --env-file is set,\n\
the file is parsed first and the current process environment still wins on conflicts.\n\
You can also set ELOWEN_EDGE_ENV_FILE instead of passing --env-file.\n\n\
Use --generate-trust-keypair to print base64url Ed25519 key material for Slice 28 trusted registration.\n"
}

fn parse_trusted_orchestrator_keys(
    env_overlay: &EnvOverlay,
) -> anyhow::Result<Vec<TrustedOrchestratorKey>> {
    let mut keys = Vec::new();

    if let Some(value) = env_value("ELOWEN_TRUSTED_ORCHESTRATOR_KEYS_JSON", env_overlay) {
        let parsed = serde_json::from_str::<Vec<TrustedOrchestratorKeyEnv>>(&value)
            .context("failed to parse ELOWEN_TRUSTED_ORCHESTRATOR_KEYS_JSON")?;

        for entry in parsed {
            let public_key = entry.public_key.trim();
            if public_key.is_empty() {
                anyhow::bail!(
                    "ELOWEN_TRUSTED_ORCHESTRATOR_KEYS_JSON cannot contain empty public keys"
                );
            }

            push_unique_orchestrator_key(
                &mut keys,
                TrustedOrchestratorKey {
                    key_id: entry
                        .key_id
                        .map(|value| value.trim().to_string())
                        .filter(|value| !value.is_empty()),
                    public_key: public_key.to_string(),
                },
            );
        }
    }

    if let Some(value) = env_value("ELOWEN_ORCHESTRATOR_PUBLIC_KEYS", env_overlay) {
        for public_key in parse_list_value(&value) {
            push_unique_orchestrator_key(
                &mut keys,
                TrustedOrchestratorKey {
                    key_id: None,
                    public_key,
                },
            );
        }
    }

    if let Some(value) = env_value("ELOWEN_ORCHESTRATOR_PUBLIC_KEY", env_overlay) {
        push_unique_orchestrator_key(
            &mut keys,
            TrustedOrchestratorKey {
                key_id: None,
                public_key: value,
            },
        );
    }

    Ok(keys)
}

fn push_unique_orchestrator_key(
    keys: &mut Vec<TrustedOrchestratorKey>,
    candidate: TrustedOrchestratorKey,
) {
    let duplicate = keys.iter().any(|existing| {
        existing.public_key == candidate.public_key && existing.key_id == candidate.key_id
    });
    if !duplicate {
        keys.push(candidate);
    }
}

#[derive(serde::Deserialize)]
struct TrustedOrchestratorKeyEnv {
    #[serde(default)]
    key_id: Option<String>,
    public_key: String,
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

#[cfg(test)]
mod tests {
    use super::{
        EnvOverlay, TrustedOrchestratorKey, parse_allowed_repos_env, parse_repo_policy_path_env,
        parse_repo_root_env, parse_trusted_orchestrator_keys,
    };
    use std::{fs, path::PathBuf};

    fn unique_temp_dir(label: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("elowen-edge-config-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn repo_roots_are_resolved_relative_to_workspace() {
        let workspace_root = unique_temp_dir("workspace");
        let child = workspace_root.join("repos");
        fs::create_dir_all(&child).unwrap();

        let mut overlay = EnvOverlay::new();
        overlay.insert("ELOWEN_ALLOWED_REPO_ROOTS".to_string(), "repos".to_string());

        let roots =
            parse_repo_root_env("ELOWEN_ALLOWED_REPO_ROOTS", &workspace_root, &overlay).unwrap();

        assert_eq!(roots, vec![fs::canonicalize(child).unwrap()]);

        let _ = fs::remove_dir_all(workspace_root);
    }

    #[test]
    fn explicit_allowlist_defaults_to_empty_when_roots_are_configured() {
        let allowed_repos = parse_allowed_repos_env(
            &["elowen-api"],
            &[PathBuf::from("D:\\Projects")],
            &EnvOverlay::new(),
        );

        assert!(allowed_repos.is_empty());
    }

    #[test]
    fn excluded_repo_paths_resolve_under_trusted_roots() {
        let workspace_root = unique_temp_dir("policy-workspace");
        let repos_root = workspace_root.join("repos");
        let excluded = repos_root.join("private");
        fs::create_dir_all(&excluded).unwrap();

        let mut overlay = EnvOverlay::new();
        overlay.insert(
            "ELOWEN_REPO_SCAN_EXCLUDE_PATHS".to_string(),
            "repos/private".to_string(),
        );

        let paths = parse_repo_policy_path_env(
            "ELOWEN_REPO_SCAN_EXCLUDE_PATHS",
            &workspace_root,
            &[fs::canonicalize(&repos_root).unwrap()],
            &overlay,
        )
        .unwrap();

        assert_eq!(paths, vec![fs::canonicalize(excluded).unwrap()]);

        let _ = fs::remove_dir_all(workspace_root);
    }

    #[test]
    fn trusted_orchestrator_keys_support_json_and_legacy_envs() {
        let mut overlay = EnvOverlay::new();
        overlay.insert(
            "ELOWEN_TRUSTED_ORCHESTRATOR_KEYS_JSON".to_string(),
            r#"[{"key_id":"current","public_key":"key-a"},{"key_id":"next","public_key":"key-b"}]"#
                .to_string(),
        );
        overlay.insert(
            "ELOWEN_ORCHESTRATOR_PUBLIC_KEY".to_string(),
            "key-b".to_string(),
        );
        overlay.insert(
            "ELOWEN_ORCHESTRATOR_PUBLIC_KEYS".to_string(),
            "key-c,key-a".to_string(),
        );

        let keys = parse_trusted_orchestrator_keys(&overlay).unwrap();

        assert_eq!(
            keys,
            vec![
                TrustedOrchestratorKey {
                    key_id: Some("current".to_string()),
                    public_key: "key-a".to_string(),
                },
                TrustedOrchestratorKey {
                    key_id: Some("next".to_string()),
                    public_key: "key-b".to_string(),
                },
                TrustedOrchestratorKey {
                    key_id: None,
                    public_key: "key-c".to_string(),
                },
                TrustedOrchestratorKey {
                    key_id: None,
                    public_key: "key-a".to_string(),
                },
                TrustedOrchestratorKey {
                    key_id: None,
                    public_key: "key-b".to_string(),
                },
            ]
        );
    }
}
