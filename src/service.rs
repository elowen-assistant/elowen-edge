//! Cross-platform service manager helpers for the edge runtime.

use anyhow::Context;
use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

use crate::config::{EdgeConfig, EdgeConfigFile};

/// Service manager operation exposed to the TUI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ServiceAction {
    Install,
    Start,
    Stop,
    Restart,
}

/// Minimal service status shown in the TUI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ServiceStatus {
    pub(crate) manager: String,
    pub(crate) detail: String,
}

pub(crate) fn service_name(file: &EdgeConfigFile) -> String {
    file.service
        .name
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "elowen-edge".to_string())
}

#[allow(dead_code)]
pub(crate) fn render_systemd_unit(
    service_name: &str,
    binary_path: &Path,
    config_path: &Path,
    user: Option<&str>,
) -> String {
    let user_line = user
        .map(|value| format!("User={value}\n"))
        .unwrap_or_default();
    format!(
        "[Unit]\n\
Description=Elowen Edge Runtime ({service_name})\n\
After=network-online.target\n\
Wants=network-online.target\n\n\
[Service]\n\
Type=simple\n\
{user_line}\
ExecStart={} run --config {}\n\
Restart=always\n\
RestartSec=10\n\n\
[Install]\n\
WantedBy=multi-user.target\n",
        binary_path.display(),
        config_path.display()
    )
}

#[allow(dead_code)]
pub(crate) fn render_windows_task_command(
    service_name: &str,
    binary_path: &Path,
    config_path: &Path,
) -> String {
    format!(
        "Register-ElowenEdgeTask.ps1 -TaskName {service_name} -BinaryPath \"{}\" -ConfigFile \"{}\"",
        binary_path.display(),
        config_path.display()
    )
}

pub(crate) fn query_service_status(config: &EdgeConfig) -> ServiceStatus {
    #[cfg(windows)]
    {
        windows_task_status(config)
    }
    #[cfg(all(unix, not(windows)))]
    {
        systemd_status(config)
    }
    #[cfg(not(any(unix, windows)))]
    {
        ServiceStatus {
            manager: "unsupported".to_string(),
            detail: "No service manager integration is available for this platform.".to_string(),
        }
    }
}

pub(crate) fn apply_service_action(
    action: ServiceAction,
    config_path: &Path,
    file: &EdgeConfigFile,
) -> anyhow::Result<String> {
    #[cfg(windows)]
    {
        apply_windows_task_action(action, config_path, file)
    }
    #[cfg(all(unix, not(windows)))]
    {
        apply_systemd_action(action, config_path, file)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (action, config_path, file);
        anyhow::bail!("service management is not available for this platform")
    }
}

fn current_binary_path() -> anyhow::Result<PathBuf> {
    env::current_exe().context("failed to resolve current executable path")
}

#[cfg(windows)]
fn windows_task_status(config: &EdgeConfig) -> ServiceStatus {
    let output = Command::new("schtasks")
        .args(["/Query", "/TN", &config.service_name])
        .output();
    match output {
        Ok(output) if output.status.success() => ServiceStatus {
            manager: "Windows Task Scheduler".to_string(),
            detail: format!(
                "task `{}` is registered for {}",
                config.service_name, config.device_id
            ),
        },
        Ok(output) => ServiceStatus {
            manager: "Windows Task Scheduler".to_string(),
            detail: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        },
        Err(error) => ServiceStatus {
            manager: "Windows Task Scheduler".to_string(),
            detail: error.to_string(),
        },
    }
}

#[cfg(windows)]
fn apply_windows_task_action(
    action: ServiceAction,
    config_path: &Path,
    file: &EdgeConfigFile,
) -> anyhow::Result<String> {
    let name = service_name(file);
    match action {
        ServiceAction::Install => install_windows_task(&name, config_path),
        ServiceAction::Start => {
            let mut command = Command::new("schtasks");
            command.args(["/Run", "/TN", &name]);
            run_service_command(command)
        }
        ServiceAction::Stop => {
            let mut command = Command::new("schtasks");
            command.args(["/End", "/TN", &name]);
            run_service_command(command)
        }
        ServiceAction::Restart => {
            let _ = apply_windows_task_action(ServiceAction::Stop, config_path, file);
            apply_windows_task_action(ServiceAction::Start, config_path, file)
        }
    }
}

#[cfg(windows)]
fn install_windows_task(service_name: &str, config_path: &Path) -> anyhow::Result<String> {
    let binary_path = current_binary_path()?;
    let register_script = windows_register_script_path(&binary_path)?;
    let mut command = Command::new(powershell_path());
    command.args([
        "-NoProfile",
        "-WindowStyle",
        "Hidden",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
    ]);
    command.arg(register_script);
    command.args(["-TaskName", service_name, "-ConfigFile"]);
    command.arg(config_path);
    command.args(["-BinaryPath"]);
    command.arg(binary_path);
    command.arg("-SkipTunnel");
    run_service_command(command)
}

#[cfg(windows)]
fn powershell_path() -> PathBuf {
    let path = PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe");
    if path.exists() {
        path
    } else {
        PathBuf::from("powershell.exe")
    }
}

#[cfg(windows)]
fn windows_register_script_path(binary_path: &Path) -> anyhow::Result<PathBuf> {
    let app_dir = binary_path
        .parent()
        .context("failed to resolve edge binary parent directory")?;
    let installed_script = app_dir
        .join("scripts")
        .join("windows")
        .join("Register-ElowenEdgeTask.ps1");
    if installed_script.exists() {
        return Ok(installed_script);
    }

    let working_dir_script = env::current_dir()
        .context("failed to resolve current directory")?
        .join("scripts")
        .join("windows")
        .join("Register-ElowenEdgeTask.ps1");
    if working_dir_script.exists() {
        return Ok(working_dir_script);
    }

    anyhow::bail!(
        "Register-ElowenEdgeTask.ps1 was not found next to the installed edge binary or in the current working tree"
    )
}

#[cfg(all(unix, not(windows)))]
fn systemd_status(config: &EdgeConfig) -> ServiceStatus {
    let unit = format!("{}.service", config.service_name);
    let output = Command::new("systemctl")
        .args(["is-active", unit.as_str()])
        .output();
    match output {
        Ok(output) => ServiceStatus {
            manager: "systemd".to_string(),
            detail: format!(
                "{} for {}",
                String::from_utf8_lossy(&output.stdout).trim(),
                config.device_id
            ),
        },
        Err(error) => ServiceStatus {
            manager: "systemd".to_string(),
            detail: error.to_string(),
        },
    }
}

#[cfg(all(unix, not(windows)))]
fn apply_systemd_action(
    action: ServiceAction,
    config_path: &Path,
    file: &EdgeConfigFile,
) -> anyhow::Result<String> {
    let name = service_name(file);
    let unit = format!("{name}.service");
    match action {
        ServiceAction::Install => {
            let binary_path = current_binary_path()?;
            let unit_body = render_systemd_unit(
                &name,
                &binary_path,
                config_path,
                file.service.run_as_user.as_deref(),
            );
            let unit_path = PathBuf::from("/etc/systemd/system").join(&unit);
            if unsafe { libc_geteuid() } == 0 {
                std::fs::write(&unit_path, unit_body)
                    .with_context(|| format!("failed to write {}", unit_path.display()))?;
                let mut daemon_reload = Command::new("systemctl");
                daemon_reload.arg("daemon-reload");
                run_service_command(daemon_reload)?;
                Ok(format!("installed {}", unit_path.display()))
            } else {
                Ok(format!(
                    "Run as root to install, or write this unit to {}:\n\n{}",
                    unit_path.display(),
                    unit_body
                ))
            }
        }
        ServiceAction::Start => systemctl(&["start", &unit]),
        ServiceAction::Stop => systemctl(&["stop", &unit]),
        ServiceAction::Restart => systemctl(&["restart", &unit]),
    }
}

#[cfg(all(unix, not(windows)))]
unsafe fn libc_geteuid() -> u32 {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    unsafe { geteuid() }
}

#[cfg(all(unix, not(windows)))]
fn systemctl(args: &[&str]) -> anyhow::Result<String> {
    let mut command = Command::new("systemctl");
    command.args(args);
    run_service_command(command)
}

fn run_service_command(mut command: Command) -> anyhow::Result<String> {
    let output = command.output().context("failed to run service command")?;
    if !output.status.success() {
        anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        Ok("service command completed".to_string())
    } else {
        Ok(stdout)
    }
}

#[cfg(test)]
mod tests {
    use super::{render_systemd_unit, render_windows_task_command};
    use std::path::Path;

    #[test]
    fn systemd_unit_runs_edge_with_config() {
        let unit = render_systemd_unit(
            "elowen-edge",
            Path::new("/usr/local/bin/elowen-edge"),
            Path::new("/etc/elowen/edge.toml"),
            Some("elowen"),
        );

        assert!(unit.contains("User=elowen"));
        assert!(unit.contains("elowen-edge run --config /etc/elowen/edge.toml"));
        assert!(unit.contains("Restart=always"));
    }

    #[test]
    fn windows_task_command_uses_hidden_registration_script() {
        let command = render_windows_task_command(
            "ElowenEdge",
            Path::new("C:\\Elowen\\elowen-edge.exe"),
            Path::new("C:\\Elowen\\edge.toml"),
        );

        assert!(command.contains("Register-ElowenEdgeTask.ps1"));
        assert!(command.contains("-TaskName ElowenEdge"));
        assert!(command.contains("-ConfigFile"));
    }
}
