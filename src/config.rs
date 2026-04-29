//! TOML runtime configuration, command-line parsing, and legacy env import.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    env, fs as stdfs,
    path::{Path, PathBuf},
};

use crate::{SandboxMode, detect_device_id, detect_device_name, parse_bool};

const DEFAULT_CAPABILITIES: &[&str] = &["codex", "git", "build", "test", "generic_jobs"];
const DEFAULT_REPOS: &[&str] = &[
    "elowen-api",
    "elowen-ui",
    "elowen-edge",
    "elowen-notes",
    "elowen-platform",
];

/// Top-level command selected at process startup.
pub(crate) enum EdgeCommand {
    Run {
        config_path: PathBuf,
    },
    Tui {
        config_path: PathBuf,
    },
    ImportEnv {
        env_file: PathBuf,
        config_path: PathBuf,
    },
    ImportTrustKey {
        from_path: PathBuf,
        to_path: PathBuf,
        provider: SecretProviderKind,
    },
    GenerateTrustKeypair,
}

/// Configuration required by the edge runtime after TOML parsing.
#[derive(Clone)]
pub(crate) struct EdgeConfig {
    /// Path to the loaded TOML config file.
    pub(crate) config_path: PathBuf,
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
    /// Path to the configured orchestrator trust bundle.
    pub(crate) orchestrator_keys_path: Option<PathBuf>,
    /// Current edge signing key used for trusted registration.
    pub(crate) edge_signing_key: Option<String>,
    /// Configured current edge signing key provider.
    pub(crate) edge_signing_key_ref: Option<SecretReference>,
    /// Previous edge signing key used during trusted re-enrollment.
    pub(crate) previous_edge_signing_key: Option<String>,
    /// Configured previous edge signing key provider.
    pub(crate) previous_edge_signing_key_ref: Option<SecretReference>,
    /// Local state directory for status and runtime artifacts.
    pub(crate) state_dir: PathBuf,
    /// Local JSON status file path.
    pub(crate) status_path: PathBuf,
    /// Platform service name used by local service-manager integrations.
    pub(crate) service_name: String,
    /// Log format used by tracing.
    pub(crate) log_format: String,
    /// Optional tracing filter.
    pub(crate) rust_log: Option<String>,
}

/// Trusted orchestrator signing key accepted for challenge verification.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TrustedOrchestratorKey {
    pub(crate) key_id: Option<String>,
    pub(crate) public_key: String,
}

/// Parsed TOML file before runtime defaults are resolved.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct EdgeConfigFile {
    pub(crate) orchestrator: OrchestratorSection,
    pub(crate) device: DeviceSection,
    pub(crate) repositories: RepositorySection,
    pub(crate) runtime: RuntimeSection,
    pub(crate) runner: RunnerSection,
    pub(crate) trust: TrustSection,
    pub(crate) service: ServiceSection,
    pub(crate) tunnel: TunnelSection,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct OrchestratorSection {
    pub(crate) api_url: Option<String>,
    pub(crate) nats_url: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct DeviceSection {
    pub(crate) id: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) primary: Option<bool>,
    pub(crate) capabilities: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct RepositorySection {
    pub(crate) workspace_root: Option<PathBuf>,
    pub(crate) worktree_root: Option<PathBuf>,
    pub(crate) allowed_roots: Vec<PathBuf>,
    pub(crate) allowed_repos: Vec<String>,
    pub(crate) hidden_repos: Vec<String>,
    pub(crate) excluded_paths: Vec<PathBuf>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct RuntimeSection {
    pub(crate) state_dir: Option<PathBuf>,
    pub(crate) log_format: Option<String>,
    pub(crate) rust_log: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct RunnerSection {
    pub(crate) codex_command: Option<String>,
    pub(crate) codex_args: Vec<String>,
    pub(crate) simulated_run_ms: Option<u64>,
    pub(crate) validation_timeout_secs: Option<u64>,
    pub(crate) sandbox_mode: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct TrustSection {
    pub(crate) orchestrator_keys_path: Option<PathBuf>,
    pub(crate) edge_signing_key_path: Option<PathBuf>,
    pub(crate) previous_edge_signing_key_path: Option<PathBuf>,
    pub(crate) edge_signing_key: Option<SecretReference>,
    pub(crate) previous_edge_signing_key: Option<SecretReference>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) struct SecretReference {
    /// Backend used to read the secret.
    pub(crate) provider: SecretProviderKind,
    /// File path used by file and DPAPI-backed-file providers.
    pub(crate) path: Option<PathBuf>,
    /// Reserved lookup name for provider backends that address secrets by key.
    pub(crate) name: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum SecretProviderKind {
    /// Plaintext file backend for Linux/container mounts and compatibility.
    File,
    /// Windows DPAPI-protected bytes stored in a local file.
    Dpapi,
    /// Reserved Windows Credential Manager backend.
    WindowsCredential,
}

impl SecretReference {
    pub(crate) fn file(path: PathBuf) -> Self {
        Self {
            provider: SecretProviderKind::File,
            path: Some(path),
            name: None,
        }
    }

    pub(crate) fn label(&self) -> &'static str {
        self.provider.label()
    }
}

impl SecretProviderKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Dpapi => "dpapi",
            Self::WindowsCredential => "windows-credential",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct ServiceSection {
    pub(crate) name: Option<String>,
    pub(crate) log_dir: Option<PathBuf>,
    pub(crate) run_as_user: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct TunnelSection {
    pub(crate) enabled: bool,
    pub(crate) user: Option<String>,
    pub(crate) host: Option<String>,
    pub(crate) local_port: Option<u16>,
    pub(crate) remote_port: Option<u16>,
}

impl EdgeConfig {
    /// Loads and validates the runtime config from a TOML file.
    pub(crate) fn load(config_path: &Path) -> anyhow::Result<Self> {
        let contents = stdfs::read_to_string(config_path)
            .with_context(|| format!("failed to read config {}", config_path.display()))?;
        let file: EdgeConfigFile = toml::from_str(&contents)
            .with_context(|| format!("failed to parse config {}", config_path.display()))?;
        Self::from_file(config_path, file)
    }

    pub(crate) fn from_file(config_path: &Path, file: EdgeConfigFile) -> anyhow::Result<Self> {
        let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
        let device_id = trimmed(file.device.id).unwrap_or_else(detect_device_id);
        let device_name =
            trimmed(file.device.name).unwrap_or_else(|| detect_device_name(&device_id));
        let workspace_root =
            resolve_path_or_default(config_dir, file.repositories.workspace_root, "/workspace")?;
        let worktree_root = match file.repositories.worktree_root {
            Some(path) => resolve_path(config_dir, path)?,
            None => workspace_root.join(".elowen").join("worktrees"),
        };
        let allowed_repo_roots =
            resolve_existing_dirs(config_dir, file.repositories.allowed_roots)?;
        let excluded_repo_paths = resolve_policy_paths(
            config_dir,
            file.repositories.excluded_paths,
            &allowed_repo_roots,
        )?;
        let allowed_repos =
            if file.repositories.allowed_repos.is_empty() && allowed_repo_roots.is_empty() {
                DEFAULT_REPOS
                    .iter()
                    .map(|value| value.to_string())
                    .collect()
            } else {
                unique_strings(file.repositories.allowed_repos)
            };
        let state_dir = match file.runtime.state_dir {
            Some(path) => resolve_path(config_dir, path)?,
            None => workspace_root.join(".elowen").join("edge-state"),
        };
        let orchestrator_keys_path = file
            .trust
            .orchestrator_keys_path
            .as_ref()
            .map(|path| resolve_path(config_dir, path.clone()))
            .transpose()?;
        let edge_signing_key_ref = normalized_secret_reference(
            "edge signing key",
            file.trust.edge_signing_key,
            file.trust.edge_signing_key_path,
        )?;
        let previous_edge_signing_key_ref = normalized_secret_reference(
            "previous edge signing key",
            file.trust.previous_edge_signing_key,
            file.trust.previous_edge_signing_key_path,
        )?;

        Ok(Self {
            config_path: config_path.to_path_buf(),
            api_url: trimmed(file.orchestrator.api_url)
                .unwrap_or_else(|| "http://elowen-api:8080".to_string())
                .trim_end_matches('/')
                .to_string(),
            nats_url: trimmed(file.orchestrator.nats_url)
                .context("missing orchestrator.nats_url")?,
            device_id,
            device_name,
            primary_flag: file.device.primary.unwrap_or(true),
            allowed_repos,
            allowed_repo_roots,
            hidden_repos: unique_strings(file.repositories.hidden_repos),
            excluded_repo_paths,
            capabilities: defaulted_strings(file.device.capabilities, DEFAULT_CAPABILITIES),
            workspace_root,
            worktree_root,
            codex_command: trimmed(file.runner.codex_command),
            codex_args: file.runner.codex_args,
            simulated_run_ms: file.runner.simulated_run_ms.unwrap_or(1500),
            validation_timeout_secs: file.runner.validation_timeout_secs.unwrap_or(600),
            sandbox_mode: SandboxMode::from_env(file.runner.sandbox_mode.as_deref())?,
            trusted_orchestrator_keys: load_trusted_orchestrator_keys(
                config_dir,
                file.trust.orchestrator_keys_path.as_deref(),
            )?,
            orchestrator_keys_path,
            edge_signing_key: load_secret_reference(config_dir, edge_signing_key_ref.as_ref())?,
            edge_signing_key_ref,
            previous_edge_signing_key: load_secret_reference(
                config_dir,
                previous_edge_signing_key_ref.as_ref(),
            )?,
            previous_edge_signing_key_ref,
            status_path: state_dir.join("status.json"),
            state_dir,
            service_name: default_service_name(file.service.name),
            log_format: file
                .runtime
                .log_format
                .unwrap_or_else(|| "plain".to_string()),
            rust_log: trimmed(file.runtime.rust_log),
        })
    }
}

/// Parses the top-level edge CLI.
pub(crate) fn parse_command() -> anyhow::Result<EdgeCommand> {
    let mut args = env::args().skip(1).collect::<Vec<_>>();
    if args.is_empty() {
        anyhow::bail!("{}", startup_usage());
    }

    if matches!(args[0].as_str(), "--help" | "-h") {
        print!("{}", startup_usage());
        std::process::exit(0);
    }

    if args[0] == "--generate-trust-keypair" {
        return Ok(EdgeCommand::GenerateTrustKeypair);
    }

    let command = args.remove(0);
    match command.as_str() {
        "run" => Ok(EdgeCommand::Run {
            config_path: take_path_arg(&mut args, "--config")?,
        }),
        "tui" => Ok(EdgeCommand::Tui {
            config_path: take_path_arg(&mut args, "--config")?,
        }),
        "trust" if args.first().map(String::as_str) == Some("generate-keypair") => {
            Ok(EdgeCommand::GenerateTrustKeypair)
        }
        "trust" if args.first().map(String::as_str) == Some("import-key") => {
            args.remove(0);
            Ok(EdgeCommand::ImportTrustKey {
                from_path: take_path_arg(&mut args, "--from")?,
                to_path: take_path_arg(&mut args, "--to")?,
                provider: take_provider_arg(&mut args, "--provider")?,
            })
        }
        "config" if args.first().map(String::as_str) == Some("import-env") => {
            args.remove(0);
            Ok(EdgeCommand::ImportEnv {
                env_file: take_path_arg(&mut args, "--env-file")?,
                config_path: take_path_arg(&mut args, "--config")?,
            })
        }
        _ => anyhow::bail!("unsupported command `{command}`\n\n{}", startup_usage()),
    }
}

/// Converts a legacy env file into the Slice 41 TOML shape.
pub(crate) fn import_env_file(env_file: &Path, config_path: &Path) -> anyhow::Result<()> {
    let overlay = load_env_overlay(env_file)?;
    let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    let secret_dir = config_dir.join("secrets");
    stdfs::create_dir_all(&secret_dir)
        .with_context(|| format!("failed to create {}", secret_dir.display()))?;

    let trust = import_trust_files(&overlay, &secret_dir)?;
    let file = EdgeConfigFile {
        orchestrator: OrchestratorSection {
            api_url: overlay.get("ELOWEN_API_URL").cloned(),
            nats_url: overlay.get("ELOWEN_NATS_URL").cloned(),
        },
        device: DeviceSection {
            id: overlay.get("ELOWEN_DEVICE_ID").cloned(),
            name: overlay.get("ELOWEN_DEVICE_NAME").cloned(),
            primary: overlay
                .get("ELOWEN_DEVICE_PRIMARY")
                .map(|value| parse_bool(value)),
            capabilities: overlay
                .get("ELOWEN_DEVICE_CAPABILITIES")
                .map(|value| parse_list_value(value))
                .unwrap_or_default(),
        },
        repositories: RepositorySection {
            workspace_root: overlay.get("ELOWEN_EDGE_WORKSPACE_ROOT").map(PathBuf::from),
            worktree_root: overlay.get("ELOWEN_EDGE_WORKTREE_ROOT").map(PathBuf::from),
            allowed_roots: overlay
                .get("ELOWEN_ALLOWED_REPO_ROOTS")
                .map(|value| parse_paths_value(value))
                .unwrap_or_default(),
            allowed_repos: overlay
                .get("ELOWEN_ALLOWED_REPOS")
                .map(|value| parse_list_value(value))
                .unwrap_or_default(),
            hidden_repos: overlay
                .get("ELOWEN_HIDDEN_REPOS")
                .map(|value| parse_list_value(value))
                .unwrap_or_default(),
            excluded_paths: overlay
                .get("ELOWEN_REPO_SCAN_EXCLUDE_PATHS")
                .map(|value| parse_paths_value(value))
                .unwrap_or_default(),
        },
        runtime: RuntimeSection {
            state_dir: None,
            log_format: overlay.get("ELOWEN_LOG_FORMAT").cloned(),
            rust_log: overlay.get("RUST_LOG").cloned(),
        },
        runner: RunnerSection {
            codex_command: overlay.get("ELOWEN_CODEX_COMMAND").cloned(),
            codex_args: overlay
                .get("ELOWEN_CODEX_ARGS_JSON")
                .map(|value| serde_json::from_str(value))
                .transpose()
                .context("failed to parse ELOWEN_CODEX_ARGS_JSON")?
                .unwrap_or_default(),
            simulated_run_ms: overlay
                .get("ELOWEN_SIMULATED_RUN_MS")
                .and_then(|value| value.parse().ok()),
            validation_timeout_secs: overlay
                .get("ELOWEN_VALIDATION_TIMEOUT_SECS")
                .and_then(|value| value.parse().ok()),
            sandbox_mode: overlay.get("ELOWEN_SANDBOX_MODE").cloned(),
        },
        trust,
        service: ServiceSection::default(),
        tunnel: TunnelSection::default(),
    };

    let body = toml::to_string_pretty(&file).context("failed to serialize TOML config")?;
    stdfs::write(config_path, body)
        .with_context(|| format!("failed to write {}", config_path.display()))?;
    Ok(())
}

/// Imports existing edge key material into a supported secret backend.
pub(crate) fn import_trust_key(
    from_path: &Path,
    to_path: &Path,
    provider: SecretProviderKind,
) -> anyhow::Result<()> {
    let value = stdfs::read_to_string(from_path)
        .with_context(|| format!("failed to read source key {}", from_path.display()))?
        .trim()
        .to_string();
    if value.is_empty() {
        anyhow::bail!("source key {} is empty", from_path.display());
    }
    write_secret_value(to_path, provider, &value).with_context(|| {
        format!(
            "failed to write {} secret {}",
            provider.label(),
            to_path.display()
        )
    })
}

fn import_trust_files(overlay: &EnvOverlay, secret_dir: &Path) -> anyhow::Result<TrustSection> {
    let mut trust = TrustSection::default();

    if let Some(value) = overlay.get("ELOWEN_TRUSTED_ORCHESTRATOR_KEYS_JSON") {
        let path = secret_dir.join("orchestrator-trust.json");
        stdfs::write(&path, value)
            .with_context(|| format!("failed to write {}", path.display()))?;
        trust.orchestrator_keys_path = Some(path);
    } else if let Some(value) = overlay
        .get("ELOWEN_ORCHESTRATOR_PUBLIC_KEYS")
        .or_else(|| overlay.get("ELOWEN_ORCHESTRATOR_PUBLIC_KEY"))
    {
        let keys = parse_list_value(value)
            .into_iter()
            .map(|public_key| TrustedOrchestratorKey {
                key_id: None,
                public_key,
            })
            .collect::<Vec<_>>();
        let path = secret_dir.join("orchestrator-trust.json");
        let body = serde_json::to_string_pretty(&keys)?;
        stdfs::write(&path, body).with_context(|| format!("failed to write {}", path.display()))?;
        trust.orchestrator_keys_path = Some(path);
    }

    if let Some(value) = overlay.get("ELOWEN_EDGE_SIGNING_KEY") {
        let path = secret_dir.join("edge-signing-key.txt");
        stdfs::write(&path, value)
            .with_context(|| format!("failed to write {}", path.display()))?;
        trust.edge_signing_key_path = Some(path);
    }

    if let Some(value) = overlay.get("ELOWEN_PREVIOUS_EDGE_SIGNING_KEY") {
        let path = secret_dir.join("previous-edge-signing-key.txt");
        stdfs::write(&path, value)
            .with_context(|| format!("failed to write {}", path.display()))?;
        trust.previous_edge_signing_key_path = Some(path);
    }

    Ok(trust)
}

fn take_path_arg(args: &mut Vec<String>, name: &str) -> anyhow::Result<PathBuf> {
    let Some(index) = args.iter().position(|arg| arg == name) else {
        anyhow::bail!("missing required {name}\n\n{}", startup_usage());
    };
    if index + 1 >= args.len() {
        anyhow::bail!("missing value after {name}");
    }
    let value = args.remove(index + 1);
    args.remove(index);
    Ok(PathBuf::from(value))
}

fn take_provider_arg(args: &mut Vec<String>, name: &str) -> anyhow::Result<SecretProviderKind> {
    let Some(index) = args.iter().position(|arg| arg == name) else {
        anyhow::bail!("missing required {name}\n\n{}", startup_usage());
    };
    if index + 1 >= args.len() {
        anyhow::bail!("missing value after {name}");
    }
    let value = args.remove(index + 1);
    args.remove(index);
    parse_secret_provider(&value)
}

fn parse_secret_provider(value: &str) -> anyhow::Result<SecretProviderKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "file" => Ok(SecretProviderKind::File),
        "dpapi" | "windows-dpapi" => Ok(SecretProviderKind::Dpapi),
        "windows-credential" | "credential" => Ok(SecretProviderKind::WindowsCredential),
        _ => anyhow::bail!("unsupported secret provider `{value}`"),
    }
}

fn startup_usage() -> &'static str {
    "Usage:\n\
  elowen-edge run --config PATH\n\
  elowen-edge tui --config PATH\n\
  elowen-edge config import-env --env-file PATH --config PATH\n\
  elowen-edge trust generate-keypair\n\
  elowen-edge trust import-key --from PATH --to PATH --provider file|dpapi\n\n\
TOML config is required for runtime startup. Legacy env files are supported only by import-env.\n"
}

fn resolve_path_or_default(
    base: &Path,
    path: Option<PathBuf>,
    default: &str,
) -> anyhow::Result<PathBuf> {
    resolve_path(base, path.unwrap_or_else(|| PathBuf::from(default)))
}

pub(crate) fn resolve_path(base: &Path, path: PathBuf) -> anyhow::Result<PathBuf> {
    let resolved = if path.is_absolute() {
        path
    } else {
        base.join(path)
    };
    Ok(resolved)
}

fn resolve_existing_dirs(base: &Path, paths: Vec<PathBuf>) -> anyhow::Result<Vec<PathBuf>> {
    let mut resolved = Vec::new();
    for path in paths {
        let canonical = stdfs::canonicalize(resolve_path(base, path)?)
            .context("failed to resolve repository root")?;
        if !canonical.is_dir() {
            anyhow::bail!(
                "configured repository root {} is not a directory",
                canonical.display()
            );
        }
        if !resolved.iter().any(|existing| existing == &canonical) {
            resolved.push(canonical);
        }
    }
    Ok(resolved)
}

fn resolve_policy_paths(
    base: &Path,
    paths: Vec<PathBuf>,
    allowed_roots: &[PathBuf],
) -> anyhow::Result<Vec<PathBuf>> {
    let mut resolved = Vec::new();
    for path in paths {
        let canonical = stdfs::canonicalize(resolve_path(base, path)?)
            .context("failed to resolve repository policy path")?;
        if !canonical.exists() {
            anyhow::bail!(
                "configured repository policy path {} does not exist",
                canonical.display()
            );
        }
        if !allowed_roots.iter().any(|root| canonical.starts_with(root)) {
            anyhow::bail!(
                "configured repository policy path {} is outside the trusted repository roots",
                canonical.display()
            );
        }
        if !resolved.iter().any(|existing| existing == &canonical) {
            resolved.push(canonical);
        }
    }
    Ok(resolved)
}

fn normalized_secret_reference(
    label: &str,
    reference: Option<SecretReference>,
    legacy_path: Option<PathBuf>,
) -> anyhow::Result<Option<SecretReference>> {
    match (reference, legacy_path) {
        (Some(_), Some(path)) => {
            anyhow::bail!(
                "configure either the provider-backed {label} or legacy path {}, not both",
                path.display()
            )
        }
        (Some(reference), None) => Ok(Some(reference)),
        (None, Some(path)) => Ok(Some(SecretReference::file(path))),
        (None, None) => Ok(None),
    }
}

fn load_secret_reference(
    base: &Path,
    reference: Option<&SecretReference>,
) -> anyhow::Result<Option<String>> {
    let Some(reference) = reference else {
        return Ok(None);
    };
    let Some(path) = reference.path.as_deref() else {
        anyhow::bail!(
            "{} secret provider requires a path in this release",
            reference.label()
        );
    };
    let path = resolve_path(base, path.to_path_buf())?;
    load_secret_from_path(reference.provider, &path)
}

fn load_secret_from_path(
    provider: SecretProviderKind,
    path: &Path,
) -> anyhow::Result<Option<String>> {
    match provider {
        SecretProviderKind::File => load_file_secret(path),
        SecretProviderKind::Dpapi => read_dpapi_secret(path),
        SecretProviderKind::WindowsCredential => anyhow::bail!(
            "windows-credential secret provider is reserved for a future backend; use dpapi or file"
        ),
    }
}

fn write_secret_value(
    path: &Path,
    provider: SecretProviderKind,
    value: &str,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        stdfs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    match provider {
        SecretProviderKind::File => {
            stdfs::write(path, value)
                .with_context(|| format!("failed to write secret file {}", path.display()))?;
            Ok(())
        }
        SecretProviderKind::Dpapi => write_dpapi_secret(path, value),
        SecretProviderKind::WindowsCredential => anyhow::bail!(
            "windows-credential secret provider is reserved for a future backend; use dpapi or file"
        ),
    }
}

fn load_file_secret(path: &Path) -> anyhow::Result<Option<String>> {
    warn_if_secret_permissions_are_broad(path)?;
    let value = stdfs::read_to_string(path)
        .with_context(|| format!("failed to read secret file {}", path.display()))?
        .trim()
        .to_string();
    if value.is_empty() {
        anyhow::bail!("secret file {} is empty", path.display());
    }
    Ok(Some(value))
}

fn load_trusted_orchestrator_keys(
    base: &Path,
    path: Option<&Path>,
) -> anyhow::Result<Vec<TrustedOrchestratorKey>> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    let path = resolve_path(base, path.to_path_buf())?;
    let _ = stdfs::metadata(&path)?;
    let body = stdfs::read_to_string(&path)
        .with_context(|| format!("failed to read trust bundle {}", path.display()))?;
    let parsed: Vec<TrustedOrchestratorKey> = serde_json::from_str(&body)
        .with_context(|| format!("failed to parse trust bundle {}", path.display()))?;
    for key in &parsed {
        if key.public_key.trim().is_empty() {
            anyhow::bail!(
                "trust bundle {} contains an empty public key",
                path.display()
            );
        }
    }
    Ok(parsed)
}

#[cfg(unix)]
fn warn_if_secret_permissions_are_broad(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if env::var("ELOWEN_EDGE_ALLOW_BROAD_SECRET_PERMISSIONS")
        .ok()
        .is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes"
            )
        })
    {
        let _ = stdfs::metadata(path)?;
        return Ok(());
    }
    let mode = stdfs::metadata(path)?.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        anyhow::bail!(
            "secret file {} is readable by group/other; restrict it to the service user",
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn warn_if_secret_permissions_are_broad(path: &Path) -> anyhow::Result<()> {
    let _ = stdfs::metadata(path)?;
    Ok(())
}

#[cfg(windows)]
fn write_dpapi_secret(path: &Path, value: &str) -> anyhow::Result<()> {
    let protected = dpapi_protect(value.as_bytes())?;
    stdfs::write(path, protected)
        .with_context(|| format!("failed to write DPAPI secret {}", path.display()))
}

#[cfg(not(windows))]
fn write_dpapi_secret(path: &Path, _value: &str) -> anyhow::Result<()> {
    anyhow::bail!(
        "dpapi secret provider is only available on Windows; cannot write {}",
        path.display()
    )
}

#[cfg(windows)]
fn read_dpapi_secret(path: &Path) -> anyhow::Result<Option<String>> {
    let protected = stdfs::read(path)
        .with_context(|| format!("failed to read DPAPI secret {}", path.display()))?;
    let bytes = dpapi_unprotect(&protected)?;
    let value = String::from_utf8(bytes)
        .with_context(|| format!("DPAPI secret {} is not valid UTF-8", path.display()))?
        .trim()
        .to_string();
    if value.is_empty() {
        anyhow::bail!("DPAPI secret {} is empty", path.display());
    }
    Ok(Some(value))
}

#[cfg(not(windows))]
fn read_dpapi_secret(path: &Path) -> anyhow::Result<Option<String>> {
    anyhow::bail!(
        "dpapi secret provider is only available on Windows; cannot read {}",
        path.display()
    )
}

#[cfg(windows)]
fn dpapi_protect(value: &[u8]) -> anyhow::Result<Vec<u8>> {
    use std::{ptr, slice};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData},
    };

    let input = CRYPT_INTEGER_BLOB {
        cbData: value.len() as u32,
        pbData: value.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: ptr::null_mut(),
    };
    let ok = unsafe {
        CryptProtectData(
            &input,
            ptr::null(),
            ptr::null(),
            ptr::null_mut(),
            ptr::null_mut(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        anyhow::bail!("CryptProtectData failed");
    }
    let protected =
        unsafe { slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
    unsafe {
        LocalFree(output.pbData.cast());
    }
    Ok(protected)
}

#[cfg(windows)]
fn dpapi_unprotect(value: &[u8]) -> anyhow::Result<Vec<u8>> {
    use std::{ptr, slice};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData,
        },
    };

    let input = CRYPT_INTEGER_BLOB {
        cbData: value.len() as u32,
        pbData: value.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: ptr::null_mut(),
    };
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        anyhow::bail!("CryptUnprotectData failed");
    }
    let cleartext =
        unsafe { slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
    unsafe {
        LocalFree(output.pbData.cast());
    }
    Ok(cleartext)
}

fn trimmed(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn defaulted_strings(values: Vec<String>, default: &[&str]) -> Vec<String> {
    if values.is_empty() {
        return default.iter().map(|value| value.to_string()).collect();
    }
    unique_strings(values)
}

fn unique_strings(values: Vec<String>) -> Vec<String> {
    let mut unique = Vec::new();
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() || unique.iter().any(|existing| existing == trimmed) {
            continue;
        }
        unique.push(trimmed.to_string());
    }
    unique
}

fn default_service_name(value: Option<String>) -> String {
    trimmed(value).unwrap_or_else(|| "elowen-edge".to_string())
}

type EnvOverlay = HashMap<String, String>;

fn load_env_overlay(env_file: &Path) -> anyhow::Result<EnvOverlay> {
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
        let is_quoted = (bytes[0] == b'\"' && bytes[raw_value.len() - 1] == b'\"')
            || (bytes[0] == b'\'' && bytes[raw_value.len() - 1] == b'\'');
        if is_quoted {
            return raw_value[1..raw_value.len() - 1].to_string();
        }
    }

    raw_value.to_string()
}

fn parse_paths_value(value: &str) -> Vec<PathBuf> {
    parse_list_value(value)
        .into_iter()
        .map(PathBuf::from)
        .collect()
}

fn parse_list_value(value: &str) -> Vec<String> {
    unique_strings(value.split(',').map(str::to_string).collect())
}

#[cfg(test)]
mod tests {
    use super::{
        EdgeConfig, EdgeConfigFile, OrchestratorSection, RepositorySection, SecretProviderKind,
        SecretReference, ServiceSection, TrustSection, import_env_file, import_trust_key,
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
    fn toml_config_resolves_repo_roots_relative_to_config() {
        let root = unique_temp_dir("toml-root");
        let repos = root.join("repos");
        fs::create_dir_all(&repos).unwrap();
        let file = EdgeConfigFile {
            orchestrator: OrchestratorSection {
                nats_url: Some("nats://127.0.0.1:4222".to_string()),
                ..Default::default()
            },
            repositories: RepositorySection {
                workspace_root: Some(root.clone()),
                allowed_roots: vec![PathBuf::from("repos")],
                ..Default::default()
            },
            ..Default::default()
        };

        let config = EdgeConfig::from_file(&root.join("edge.toml"), file).unwrap();

        assert_eq!(
            config.allowed_repo_roots,
            vec![fs::canonicalize(repos).unwrap()]
        );
        assert!(config.allowed_repos.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn explicit_allowlist_defaults_when_roots_are_absent() {
        let root = unique_temp_dir("default-repos");
        let file = EdgeConfigFile {
            orchestrator: OrchestratorSection {
                nats_url: Some("nats://127.0.0.1:4222".to_string()),
                ..Default::default()
            },
            repositories: RepositorySection {
                workspace_root: Some(root.clone()),
                ..Default::default()
            },
            ..Default::default()
        };

        let config = EdgeConfig::from_file(&root.join("edge.toml"), file).unwrap();

        assert!(config.allowed_repos.iter().any(|repo| repo == "elowen-api"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn service_name_defaults_and_trims() {
        let root = unique_temp_dir("service-name");
        let file = EdgeConfigFile {
            orchestrator: OrchestratorSection {
                nats_url: Some("nats://127.0.0.1:4222".to_string()),
                ..Default::default()
            },
            repositories: RepositorySection {
                workspace_root: Some(root.clone()),
                ..Default::default()
            },
            service: ServiceSection {
                name: Some(" ElowenEdge ".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };

        let config = EdgeConfig::from_file(&root.join("edge.toml"), file).unwrap();

        assert_eq!(config.service_name, "ElowenEdge");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn import_env_writes_toml_and_secret_files() {
        let root = unique_temp_dir("import");
        let env_file = root.join("edge.env");
        let config_file = root.join("edge.toml");
        fs::write(
            &env_file,
            "ELOWEN_NATS_URL=nats://127.0.0.1:4222\nELOWEN_EDGE_SIGNING_KEY=secret\n",
        )
        .unwrap();

        import_env_file(&env_file, &config_file).unwrap();

        let body = fs::read_to_string(&config_file).unwrap();
        assert!(body.contains("nats://127.0.0.1:4222"));
        assert!(root.join("secrets").join("edge-signing-key.txt").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn missing_secret_file_fails_config_load() {
        let root = unique_temp_dir("missing-secret");
        let file = EdgeConfigFile {
            orchestrator: OrchestratorSection {
                nats_url: Some("nats://127.0.0.1:4222".to_string()),
                ..Default::default()
            },
            repositories: RepositorySection {
                workspace_root: Some(root.clone()),
                ..Default::default()
            },
            trust: TrustSection {
                edge_signing_key_path: Some(PathBuf::from("missing.txt")),
                ..Default::default()
            },
            ..Default::default()
        };

        let error = match EdgeConfig::from_file(&root.join("edge.toml"), file) {
            Ok(_) => panic!("config load should fail for a missing secret file"),
            Err(error) => error,
        };

        assert!(
            error.to_string().contains("No such file")
                || error.to_string().contains("system cannot find")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn provider_file_secret_loads_without_legacy_path() {
        let root = unique_temp_dir("provider-file");
        let secret = root.join("edge.key");
        fs::write(&secret, "provider-secret").unwrap();
        let file = EdgeConfigFile {
            orchestrator: OrchestratorSection {
                nats_url: Some("nats://127.0.0.1:4222".to_string()),
                ..Default::default()
            },
            repositories: RepositorySection {
                workspace_root: Some(root.clone()),
                ..Default::default()
            },
            trust: TrustSection {
                edge_signing_key: Some(SecretReference {
                    provider: SecretProviderKind::File,
                    path: Some(PathBuf::from("edge.key")),
                    name: None,
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        let config = EdgeConfig::from_file(&root.join("edge.toml"), file).unwrap();

        assert_eq!(config.edge_signing_key.as_deref(), Some("provider-secret"));
        assert_eq!(
            config
                .edge_signing_key_ref
                .as_ref()
                .map(|reference| reference.provider),
            Some(SecretProviderKind::File)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn provider_and_legacy_key_path_conflict() {
        let root = unique_temp_dir("provider-conflict");
        let file = EdgeConfigFile {
            orchestrator: OrchestratorSection {
                nats_url: Some("nats://127.0.0.1:4222".to_string()),
                ..Default::default()
            },
            repositories: RepositorySection {
                workspace_root: Some(root.clone()),
                ..Default::default()
            },
            trust: TrustSection {
                edge_signing_key_path: Some(PathBuf::from("edge.key")),
                edge_signing_key: Some(SecretReference {
                    provider: SecretProviderKind::File,
                    path: Some(PathBuf::from("edge.key")),
                    name: None,
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        let error = match EdgeConfig::from_file(&root.join("edge.toml"), file) {
            Ok(_) => panic!("config load should fail when provider and legacy path both exist"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("configure either"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn import_key_file_provider_writes_trimmed_secret() {
        let root = unique_temp_dir("import-key-file");
        let source = root.join("source.key");
        let target = root.join("nested").join("target.key");
        fs::write(&source, " secret-value \n").unwrap();

        import_trust_key(&source, &target, SecretProviderKind::File).unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "secret-value");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn import_key_dpapi_provider_round_trips_on_windows() {
        let root = unique_temp_dir("import-key-dpapi");
        let source = root.join("source.key");
        let target = root.join("target.dpapi");
        fs::write(&source, "secret-value").unwrap();

        import_trust_key(&source, &target, SecretProviderKind::Dpapi).unwrap();
        let file = EdgeConfigFile {
            orchestrator: OrchestratorSection {
                nats_url: Some("nats://127.0.0.1:4222".to_string()),
                ..Default::default()
            },
            repositories: RepositorySection {
                workspace_root: Some(root.clone()),
                ..Default::default()
            },
            trust: TrustSection {
                edge_signing_key: Some(SecretReference {
                    provider: SecretProviderKind::Dpapi,
                    path: Some(target),
                    name: None,
                }),
                ..Default::default()
            },
            ..Default::default()
        };

        let config = EdgeConfig::from_file(&root.join("edge.toml"), file).unwrap();

        assert_eq!(config.edge_signing_key.as_deref(), Some("secret-value"));
        let _ = fs::remove_dir_all(root);
    }
}
