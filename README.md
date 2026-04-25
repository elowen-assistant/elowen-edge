# elowen-edge

## Purpose

Rust edge runtime that executes dispatched work on a trusted device. It registers with the orchestrator, manages worktrees, runs Codex and validation commands, and reports lifecycle events back over NATS.

## Current Responsibilities

- register the device with `elowen-api` and renew that registration on a heartbeat
- answer availability probes and accept job dispatch and approval subjects
- resolve allowed repositories from explicit repo names and discovered parent-directory roots
- create disposable git worktrees for dispatched repository jobs
- run Codex or the simulated runner inside the worktree
- execute repository validation commands and classify sandbox or validation failures
- gate pushes behind explicit approval commands
- expose a local TUI for configuration, diagnostics, and service controls
- write a local JSON status file for operator diagnostics
- generate and verify trusted registration material for orchestrator enrollment, rotation, and re-enrollment

## Repository Layout

- `src/runtime.rs` - command routing, startup, subscriptions, and long-running loops
- `src/config.rs` - TOML configuration, validation, and legacy env-file import
- `src/tui.rs` - terminal dashboard, first-run checklist, diagnostics, and service controls
- `src/service.rs` - Windows Task Scheduler and Linux systemd helpers
- `src/status.rs` - local JSON status snapshots
- `src/registration.rs` - registration heartbeat and trust proof helpers
- `src/discovery.rs` - repo discovery and repo-root resolution
- `src/execution.rs` - worktree, Codex, validation, commit, and push flows
- `src/sandbox.rs` - sandbox policy creation and containment checks
- `edge.toml.example` - example standalone edge configuration

## Runtime And Config Entrypoints

Slice 41 makes TOML the only runtime configuration format:

```powershell
elowen-edge run --config .\edge.toml
```

Open the TUI with:

```powershell
elowen-edge tui --config .\edge.toml
```

Import one existing legacy env file with:

```powershell
elowen-edge config import-env --env-file .\edge.env.local --config .\edge.toml
```

The runtime no longer accepts `--env-file` or `ELOWEN_EDGE_ENV_FILE`; those are migration inputs only.

Generate trust key material with:

```powershell
elowen-edge trust generate-keypair
```

For compatibility during local muscle-memory cleanup, `elowen-edge --generate-trust-keypair` still prints a keypair.

## TOML Configuration

Start from `edge.toml.example`. The main sections are:

- `[orchestrator]` for API and NATS URLs
- `[device]` for stable device identity and advertised capabilities
- `[repositories]` for workspace, worktree, repo roots, hidden repos, and explicit repo overlays
- `[runner]` for Codex command, extra args, validation timeout, and sandbox mode
- `[trust]` for paths to local trust material
- `[runtime]` for status/log configuration
- `[service]` and `[tunnel]` for operator service setup

Secrets are stored in separate local files and referenced from TOML. On Unix-like hosts, secret files must not be group- or world-readable. Keep the TOML config and the `secrets/` directory out of git.

The trust bundle file is JSON:

```json
[
  { "key_id": "current", "public_key": "base64url-public-key" }
]
```

The edge signing key files contain a single base64url private key each.

On Windows, prefer an absolute path to the executable shim that actually runs from a background task. The Microsoft Store app path under `C:\Program Files\WindowsApps` may be visible but can fail with `Access is denied`; the npm shim is usually the usable path:

```powershell
Get-Command codex.cmd
```

Then set:

```toml
[runner]
codex_command = 'C:\Users\<you>\AppData\Roaming\npm\codex.cmd'
codex_args = []
```

After saving, press `Shift+R` in the TUI and check Diagnostics for `Codex preflight passed`.

## TUI

The TUI is a local operator console. It supports:

- dashboard view for device identity, runner mode, repository exposure, and last local status
- first-run checklist for orchestrator, device identity, work exposure, Codex, trust, and service install
- config validation with reload via `Shift+R`
- service install/start/stop/restart controls
- diagnostics for config parsing, secret-file access, and Codex preflight

The TUI intentionally edits through the TOML file rather than inventing a second storage path. Use your editor for detailed field edits, then reload the TUI.

## Persistent Service Model

For the normal Windows operator flow, build an unsigned Inno Setup installer:

```powershell
.\scripts\windows\New-ElowenEdgeInnoInstaller.ps1 -Release
```

If `ISCC.exe` is not installed, install Inno Setup 6 first:

```powershell
winget install --id JRSoftware.InnoSetup -e
```

That writes `dist\ElowenEdgeSetup.exe`. Download or copy that installer to the target machine, then run it. For unattended local UAT with an existing config and secret directory:

```powershell
.\dist\ElowenEdgeSetup.exe /CURRENTUSER /CONFIGSOURCE="C:\path\to\edge.toml" /SECRETSOURCEDIR="C:\path\to\secrets"
```

The installer lays down `elowen-edge.exe`, the Windows helper scripts, the TOML config, and the optional local secret directory under `%LOCALAPPDATA%\Programs\Elowen\Edge`. It registers the scheduled task, optionally starts it, creates TUI shortcuts, and provides a standard Windows uninstall entry. The older `New-ElowenEdgeInstaller.ps1` PowerShell bootstrapper remains available as a debugging fallback.

Windows uses Task Scheduler as the service host. The TUI can install, start, stop, and restart the task. The existing PowerShell scripts remain as lower-level helpers while the operator flow moves to the TUI and TOML config.

Linux and VPS hosts use systemd. If the TUI is not running with permission to write `/etc/systemd/system`, it prints the unit content and exact target path for an elevated install.

Both service models run:

```bash
elowen-edge run --config <path>
```

Install a desktop or Start Menu shortcut for the TUI on Windows:

```powershell
.\scripts\windows\Install-ElowenEdgeTuiShortcut.ps1 `
  -ConfigFile .\edge.toml `
  -Release
```

The shortcut opens `elowen-edge tui --config <path>` on demand. It is separate from the background scheduled task, so closing the TUI does not stop the edge service.

## Local Status

The runtime writes `status.json` under `[runtime].state_dir`. The snapshot includes:

- config path
- process start time
- device id
- runner mode
- NATS status
- last registration timestamp or error

The TUI reads this file for passive status and uses active checks for local diagnostics.

## Trusted Registration Lifecycle

The edge keeps orchestrator challenge verification strict:

- a registration challenge is accepted only if its public key matches a locally pinned orchestrator key
- if the API includes an `orchestrator_key_id`, the edge also requires that key id to match a pinned entry
- unknown orchestrator keys are rejected even during a rotation window

Recommended operator setup:

1. Keep one TOML config and one secret directory per device.
2. Pin the orchestrator trust bundle with `[trust].orchestrator_keys_path`.
3. Store the edge private key in `[trust].edge_signing_key_path`.
4. During edge signing-key rotation, set `[trust].previous_edge_signing_key_path` only for the re-enrollment window.
5. After the API confirms the new edge key is trusted, remove the previous key path.

## Local Verification

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --quiet
cargo doc --no-deps
```

## Related Docs

- `edge.toml.example`
- `../elowen-platform/docs/laptop-edge.md`
