//! Terminal operator interface for edge setup, health, and service controls.

use anyhow::Context;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};
use std::{
    io::{self, Stdout},
    path::{Path, PathBuf},
    time::Duration,
};

use crate::{
    config::{EdgeConfig, EdgeConfigFile},
    execution::{discover_codex_command, preflight_codex_runner},
    service::{self, ServiceAction},
    status::{EdgeStatus, read_status},
};

struct TuiGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TuiGuard {
    fn enter() -> anyhow::Result<Self> {
        enable_raw_mode().context("failed to enable raw mode")?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen).context("failed to enter alternate screen")?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend).context("failed to create terminal")?;
        Ok(Self { terminal })
    }
}

impl Drop for TuiGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TuiTab {
    Dashboard,
    Wizard,
    Config,
    Service,
    Diagnostics,
}

impl TuiTab {
    fn next(self) -> Self {
        match self {
            Self::Dashboard => Self::Wizard,
            Self::Wizard => Self::Config,
            Self::Config => Self::Service,
            Self::Service => Self::Diagnostics,
            Self::Diagnostics => Self::Dashboard,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Dashboard => Self::Diagnostics,
            Self::Wizard => Self::Dashboard,
            Self::Config => Self::Wizard,
            Self::Service => Self::Config,
            Self::Diagnostics => Self::Service,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Dashboard => "Dashboard",
            Self::Wizard => "First Run",
            Self::Config => "Config",
            Self::Service => "Service",
            Self::Diagnostics => "Diagnostics",
        }
    }
}

struct TuiModel {
    config_path: PathBuf,
    config: Result<EdgeConfig, String>,
    file: Result<EdgeConfigFile, String>,
    tab: TuiTab,
    message: String,
}

impl TuiModel {
    fn load(config_path: PathBuf) -> Self {
        let file = load_file(&config_path).map_err(|error| error.to_string());
        let config = EdgeConfig::load(&config_path).map_err(|error| error.to_string());
        Self {
            config_path,
            config,
            file,
            tab: TuiTab::Dashboard,
            message: "Arrow keys switch views. i installs, s starts, x stops, r restarts, q quits."
                .to_string(),
        }
    }

    fn reload(&mut self) {
        self.file = load_file(&self.config_path).map_err(|error| error.to_string());
        self.config = EdgeConfig::load(&self.config_path).map_err(|error| error.to_string());
    }

    fn apply_service_action(&mut self, action: ServiceAction) {
        let Ok(file) = self.file.as_ref() else {
            self.message = "Config must parse before service actions are available.".to_string();
            return;
        };
        self.message = match service::apply_service_action(action, &self.config_path, file) {
            Ok(output) => output,
            Err(error) => error.to_string(),
        };
    }

    fn configure_codex_command(&mut self) {
        self.message = match discover_codex_command_blocking() {
            Ok(Some(discovery)) => {
                match write_codex_command(&self.config_path, &discovery.command) {
                    Ok(()) => {
                        self.reload();
                        format!(
                            "Configured Codex command: {} ({})",
                            discovery.command, discovery.version
                        )
                    }
                    Err(error) => format!("Failed to write Codex command: {error}"),
                }
            }
            Ok(None) => {
                "No runnable Codex command was found. Install Codex or add it to PATH, then press a again.".to_string()
            }
            Err(error) => format!("Codex discovery failed: {error}"),
        };
    }
}

/// Runs the edge TUI until the operator quits.
pub(crate) fn run(config_path: PathBuf) -> anyhow::Result<()> {
    let mut guard = TuiGuard::enter()?;
    let mut model = TuiModel::load(config_path);

    loop {
        guard
            .terminal
            .draw(|frame| draw(frame, &model))
            .context("failed to draw TUI")?;

        if !event::poll(Duration::from_millis(250)).context("failed to poll terminal input")? {
            continue;
        }

        if let Event::Key(key) = event::read().context("failed to read terminal input")? {
            if key.kind != KeyEventKind::Press {
                continue;
            }

            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break,
                KeyCode::Down | KeyCode::Right => model.tab = model.tab.next(),
                KeyCode::Up | KeyCode::Left => model.tab = model.tab.previous(),
                KeyCode::Char('d') => model.tab = TuiTab::Dashboard,
                KeyCode::Char('w') => model.tab = TuiTab::Wizard,
                KeyCode::Char('c') => model.tab = TuiTab::Config,
                KeyCode::Char('v') => model.tab = TuiTab::Service,
                KeyCode::Char('g') => model.tab = TuiTab::Diagnostics,
                KeyCode::Char('R') => model.reload(),
                KeyCode::Char('a') => model.configure_codex_command(),
                KeyCode::Char('i') => model.apply_service_action(ServiceAction::Install),
                KeyCode::Char('s') => model.apply_service_action(ServiceAction::Start),
                KeyCode::Char('x') => model.apply_service_action(ServiceAction::Stop),
                KeyCode::Char('r') => model.apply_service_action(ServiceAction::Restart),
                _ => {}
            }
        }
    }

    Ok(())
}

fn load_file(config_path: &Path) -> anyhow::Result<EdgeConfigFile> {
    let body = std::fs::read_to_string(config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    toml::from_str(&body).with_context(|| format!("failed to parse {}", config_path.display()))
}

fn write_codex_command(config_path: &Path, command: &str) -> anyhow::Result<()> {
    let mut file = load_file(config_path)?;
    file.runner.codex_command = Some(command.to_string());
    let body = toml::to_string_pretty(&file).context("failed to serialize updated config")?;
    std::fs::write(config_path, body)
        .with_context(|| format!("failed to write {}", config_path.display()))
}

fn discover_codex_command_blocking()
-> anyhow::Result<Option<crate::execution::CodexCommandDiscovery>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to start Codex discovery runtime")?;
    runtime.block_on(discover_codex_command())
}

fn draw(frame: &mut Frame<'_>, model: &TuiModel) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(3),
        ])
        .split(frame.area());

    draw_tabs(frame, model, layout[0]);
    match model.tab {
        TuiTab::Dashboard => draw_dashboard(frame, model, layout[1]),
        TuiTab::Wizard => draw_wizard(frame, model, layout[1]),
        TuiTab::Config => draw_config(frame, model, layout[1]),
        TuiTab::Service => draw_service(frame, model, layout[1]),
        TuiTab::Diagnostics => draw_diagnostics(frame, model, layout[1]),
    }
    frame.render_widget(
        Paragraph::new(model.message.clone()).block(Block::default().borders(Borders::ALL)),
        layout[2],
    );
}

fn draw_tabs(frame: &mut Frame<'_>, model: &TuiModel, area: ratatui::layout::Rect) {
    let tabs = [
        TuiTab::Dashboard,
        TuiTab::Wizard,
        TuiTab::Config,
        TuiTab::Service,
        TuiTab::Diagnostics,
    ]
    .into_iter()
    .map(|tab| {
        let style = if tab == model.tab {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        Span::styled(format!(" {} ", tab.title()), style)
    })
    .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(Line::from(tabs)).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn draw_dashboard(frame: &mut Frame<'_>, model: &TuiModel, area: ratatui::layout::Rect) {
    let items = match &model.config {
        Ok(config) => {
            let status = read_status(&config.status_path).ok().flatten();
            vec![
                ListItem::new(format!(
                    "Device: {} ({})",
                    config.device_name, config.device_id
                )),
                ListItem::new(format!("API: {}", config.api_url)),
                ListItem::new(format!("NATS: {}", config.nats_url)),
                ListItem::new(format!(
                    "Runner: {}",
                    if config.codex_command.is_some() {
                        "codex-cli"
                    } else {
                        "simulated"
                    }
                )),
                ListItem::new(format!(
                    "Repositories: {} roots, {} explicit",
                    config.allowed_repo_roots.len(),
                    config.allowed_repos.len()
                )),
                ListItem::new(format!("Status file: {}", config.status_path.display())),
                ListItem::new(format!(
                    "Last registration: {}",
                    format_last_registration(status.as_ref())
                )),
            ]
        }
        Err(error) => vec![ListItem::new(format!("Config error: {error}"))],
    };
    frame.render_widget(
        List::new(items).block(Block::default().title("Edge Health").borders(Borders::ALL)),
        area,
    );
}

fn format_last_registration(status: Option<&EdgeStatus>) -> String {
    status
        .and_then(|value| value.last_registration_at)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S %:z")
                .to_string()
        })
        .unwrap_or_else(|| "not observed locally".to_string())
}

fn draw_wizard(frame: &mut Frame<'_>, model: &TuiModel, area: ratatui::layout::Rect) {
    let checks = first_run_checks(model);
    let items = checks
        .into_iter()
        .map(|check| {
            let state = if check.ok { "OK" } else { "Needs setup" };
            if check.ok {
                ListItem::new(format!("{:>11}  {} - {}", state, check.label, check.detail))
            } else {
                ListItem::new(Text::from(vec![
                    Line::from(format!("{:>11}  {} - {}", state, check.label, check.detail)),
                    Line::from(format!("{:>11}  Next: {}", "", check.action)),
                ]))
            }
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        List::new(items).block(
            Block::default()
                .title("First Run Readiness")
                .borders(Borders::ALL),
        ),
        area,
    );
}

struct ReadinessCheck {
    label: &'static str,
    ok: bool,
    detail: String,
    action: &'static str,
}

fn first_run_checks(model: &TuiModel) -> Vec<ReadinessCheck> {
    vec![
        ReadinessCheck {
            label: "Orchestrator",
            ok: has_orchestrator(model),
            detail: match &model.config {
                Ok(config) => format!("API {} and NATS {}", config.api_url, config.nats_url),
                Err(_) => "config must parse before connection settings can be checked".to_string(),
            },
            action: "edit [orchestrator].api_url and [orchestrator].nats_url in edge.toml, then press Shift+R",
        },
        ReadinessCheck {
            label: "Device identity",
            ok: has_device(model),
            detail: match &model.config {
                Ok(config) => format!("{} ({})", config.device_name, config.device_id),
                Err(_) => "config must parse before device identity can be checked".to_string(),
            },
            action: "set [device].id and [device].name in edge.toml, then press Shift+R",
        },
        ReadinessCheck {
            label: "Work exposure",
            ok: has_work(model),
            detail: match &model.config {
                Ok(config) => format!(
                    "{} repo roots, {} explicit repos, {} capabilities",
                    config.allowed_repo_roots.len(),
                    config.allowed_repos.len(),
                    config.capabilities.len()
                ),
                Err(_) => "config must parse before work exposure can be checked".to_string(),
            },
            action: "add [repositories].allowed_roots or [device].capabilities in edge.toml, then press Shift+R",
        },
        ReadinessCheck {
            label: "Codex runner",
            ok: has_codex(model),
            detail: match &model.config {
                Ok(config) if config.codex_command.is_some() => {
                    "codex command configured".to_string()
                }
                Ok(_) => "not configured; edge will use simulated execution".to_string(),
                Err(_) => "config must parse before runner settings can be checked".to_string(),
            },
            action: "press a to auto-discover and save [runner].codex_command, or set it manually then press Shift+R",
        },
        ReadinessCheck {
            label: "Trust material",
            ok: has_trust(model),
            detail: match &model.config {
                Ok(config) => format!(
                    "{} trusted orchestrator key(s), edge signing key {}",
                    config.trusted_orchestrator_keys.len(),
                    if config.edge_signing_key.is_some() {
                        "loaded"
                    } else {
                        "missing"
                    }
                ),
                Err(_) => "config must parse before trust files can be checked".to_string(),
            },
            action: "create the trust files and set [trust].orchestrator_keys_path and [trust].edge_signing_key_path",
        },
        ReadinessCheck {
            label: "Service install",
            ok: service_is_installed(model),
            detail: service_readiness_detail(model),
            action: "press i to install the service, then press s to start it",
        },
    ]
}

fn service_is_installed(model: &TuiModel) -> bool {
    let Ok(config) = model.config.as_ref() else {
        return false;
    };
    let status = service::query_service_status(config);
    status.detail.contains("registered")
        || status.detail.contains("Running")
        || status.detail.contains("Ready")
        || status.detail.contains("active")
}

fn service_readiness_detail(model: &TuiModel) -> String {
    match model.config.as_ref() {
        Ok(config) => {
            let status = service::query_service_status(config);
            format!("{}: {}", status.manager, status.detail)
        }
        Err(_) => "config must parse before service status can be checked".to_string(),
    }
}

fn draw_config(frame: &mut Frame<'_>, model: &TuiModel, area: ratatui::layout::Rect) {
    let text = match &model.config {
        Ok(config) => format!(
            "Config: {}\nState: {}\nWorkspace: {}\nWorktrees: {}\nCapabilities: {}\n\nEdit this TOML file in your editor, then press Shift+R here to reload validation.",
            model.config_path.display(),
            config.state_dir.display(),
            config.workspace_root.display(),
            config.worktree_root.display(),
            config.capabilities.join(", ")
        ),
        Err(error) => format!("Config failed validation:\n\n{error}"),
    };
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
            Block::default()
                .title("Configuration")
                .borders(Borders::ALL),
        ),
        area,
    );
}

fn draw_service(frame: &mut Frame<'_>, model: &TuiModel, area: ratatui::layout::Rect) {
    let text = match &model.config {
        Ok(config) => {
            let status = service::query_service_status(config);
            format!(
                "Manager: {}\nStatus: {}\n\nWhat this screen does:\n- i install: creates or rewrites the background service task using the hidden launcher\n- s start: starts the already-installed background task\n- x stop: stops the background task\n- r restart: stops then starts the background task\n\nThe TUI is only the control panel; closing it does not stop the background edge service.",
                status.manager, status.detail
            )
        }
        Err(error) => {
            format!("Config must validate before service controls are available:\n\n{error}")
        }
    };
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
            Block::default()
                .title("Service Controls")
                .borders(Borders::ALL),
        ),
        area,
    );
}

fn draw_diagnostics(frame: &mut Frame<'_>, model: &TuiModel, area: ratatui::layout::Rect) {
    let text = match &model.config {
        Ok(config) => {
            let codex = if config.codex_command.is_some() {
                run_codex_preflight(config)
            } else {
                discover_codex_diagnostic()
            };
            format!(
                "Config parses successfully.\nSecret files passed permission checks.\n{}\n\nPress a to auto-discover and save the Codex command.\nNATS/API checks run when the service starts; see the dashboard status file after startup.",
                codex
            )
        }
        Err(error) => format!("Config diagnostics:\n\n{error}"),
    };
    frame.render_widget(
        Paragraph::new(text)
            .wrap(Wrap { trim: false })
            .block(Block::default().title("Diagnostics").borders(Borders::ALL)),
        area,
    );
}

fn discover_codex_diagnostic() -> String {
    match discover_codex_command_blocking() {
        Ok(Some(discovery)) => format!(
            "Codex command is not configured; edge will use simulated execution.\nDiscovered runnable Codex command: {} ({})",
            discovery.command, discovery.version
        ),
        Ok(None) => {
            "Codex command is not configured; edge will use simulated execution.\nNo runnable Codex command was found with Get-Command/where/which.".to_string()
        }
        Err(error) => format!(
            "Codex command is not configured; edge will use simulated execution.\nCodex discovery failed: {error}"
        ),
    }
}

fn run_codex_preflight(config: &EdgeConfig) -> String {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return format!("Codex preflight could not start: {error}"),
    };
    match runtime.block_on(preflight_codex_runner(config)) {
        Ok(()) => "Codex preflight passed.".to_string(),
        Err(error) => format!("Codex preflight failed: {error}"),
    }
}

fn has_orchestrator(model: &TuiModel) -> bool {
    model
        .config
        .as_ref()
        .map(|config| !config.api_url.is_empty() && !config.nats_url.is_empty())
        .unwrap_or(false)
}

fn has_device(model: &TuiModel) -> bool {
    model
        .config
        .as_ref()
        .map(|config| !config.device_id.is_empty() && !config.device_name.is_empty())
        .unwrap_or(false)
}

fn has_work(model: &TuiModel) -> bool {
    model
        .config
        .as_ref()
        .map(|config| !config.allowed_repo_roots.is_empty() || !config.capabilities.is_empty())
        .unwrap_or(false)
}

fn has_codex(model: &TuiModel) -> bool {
    model
        .config
        .as_ref()
        .map(|config| config.codex_command.is_some())
        .unwrap_or(false)
}

fn has_trust(model: &TuiModel) -> bool {
    model
        .config
        .as_ref()
        .map(|config| {
            !config.trusted_orchestrator_keys.is_empty() && config.edge_signing_key.is_some()
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::{
        TuiModel, TuiTab, first_run_checks, format_last_registration, has_codex, has_device,
        has_orchestrator, write_codex_command,
    };
    use crate::config::{EdgeConfigFile, OrchestratorSection, RepositorySection, RunnerSection};
    use crate::status::EdgeStatus;
    use chrono::{Local, TimeZone, Utc};
    use std::{fs, path::PathBuf};

    #[test]
    fn wizard_checks_reflect_loaded_config() {
        let root = std::env::temp_dir().join(format!("elowen-edge-tui-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let config_path = root.join("edge.toml");
        let file = EdgeConfigFile {
            orchestrator: OrchestratorSection {
                nats_url: Some("nats://127.0.0.1:4222".to_string()),
                ..Default::default()
            },
            repositories: RepositorySection {
                workspace_root: Some(root.clone()),
                ..Default::default()
            },
            runner: RunnerSection {
                codex_command: Some("codex".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        fs::write(&config_path, toml::to_string(&file).unwrap()).unwrap();

        let model = TuiModel::load(PathBuf::from(&config_path));

        assert!(has_orchestrator(&model));
        assert!(has_device(&model));
        assert!(has_codex(&model));
        let checks = first_run_checks(&model);
        assert!(
            checks
                .iter()
                .any(|check| check.label == "Orchestrator" && check.ok)
        );
        assert!(
            checks
                .iter()
                .any(|check| check.label == "Codex runner" && check.ok)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn tabs_support_arrow_key_ordering() {
        assert_eq!(TuiTab::Dashboard.next(), TuiTab::Wizard);
        assert_eq!(TuiTab::Wizard.next(), TuiTab::Config);
        assert_eq!(TuiTab::Diagnostics.next(), TuiTab::Dashboard);

        assert_eq!(TuiTab::Dashboard.previous(), TuiTab::Diagnostics);
        assert_eq!(TuiTab::Config.previous(), TuiTab::Wizard);
        assert_eq!(TuiTab::Diagnostics.previous(), TuiTab::Service);
    }

    #[test]
    fn last_registration_uses_readable_utc_seconds() {
        let status = EdgeStatus {
            config_path: PathBuf::from("edge.toml"),
            process_started_at: Utc.timestamp_opt(1_777_077_000, 0).unwrap(),
            updated_at: Utc.timestamp_opt(1_777_077_001, 0).unwrap(),
            device_id: "edge".to_string(),
            runner_mode: "simulated".to_string(),
            nats_status: "connected".to_string(),
            last_registration_at: Some(Utc.with_ymd_and_hms(2026, 4, 25, 2, 49, 39).unwrap()),
            last_registration_error: None,
            service_status: None,
        };

        assert_eq!(
            format_last_registration(Some(&status)),
            Utc.with_ymd_and_hms(2026, 4, 25, 2, 49, 39)
                .unwrap()
                .with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S %:z")
                .to_string()
        );
        assert_eq!(format_last_registration(None), "not observed locally");
    }

    #[test]
    fn codex_command_can_be_written_to_config() {
        let root = std::env::temp_dir().join(format!("elowen-edge-codex-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let config_path = root.join("edge.toml");
        fs::write(
            &config_path,
            "[orchestrator]\nnats_url = 'nats://127.0.0.1:4222'\n\n[runner]\ncodex_args = []\n",
        )
        .unwrap();

        write_codex_command(
            &config_path,
            r"C:\Users\ericw\AppData\Roaming\npm\codex.cmd",
        )
        .unwrap();
        let file = super::load_file(&config_path).unwrap();

        assert_eq!(
            file.runner.codex_command.as_deref(),
            Some(r"C:\Users\ericw\AppData\Roaming\npm\codex.cmd")
        );
        let _ = fs::remove_dir_all(root);
    }
}
