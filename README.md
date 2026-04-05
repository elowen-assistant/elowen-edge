# elowen-edge

Rust edge agent that runs on the primary device and owns:

- device registration
- repo allowlist validation
- worktree management
- Codex execution
- test/build execution
- event publication back to the orchestrator

## Current behavior

- registers itself with `elowen-api`
- renews presence on a heartbeat loop
- answers availability probes on `elowen.devices.availability.probe.{device_id}`
- accepts job dispatches over NATS
- can run from a standalone laptop checkout using an env file instead of a long one-off shell command

## Runtime configuration

`elowen-edge` still reads its runtime configuration from environment variables, but it now also accepts an env file:

```powershell
elowen-edge --env-file .\edge.env.local
```

You can also set `ELOWEN_EDGE_ENV_FILE` instead of passing `--env-file`.

The env file is parsed first and the current process environment still wins on conflicts. That makes it practical to keep a stable local config file while still overriding one or two values for debugging.

Useful variables:

- `ELOWEN_API_URL`
- `ELOWEN_NATS_URL`
- `ELOWEN_DEVICE_ID`
- `ELOWEN_DEVICE_NAME`
- `ELOWEN_DEVICE_PRIMARY`
- `ELOWEN_ALLOWED_REPOS`
- `ELOWEN_ALLOWED_REPO_ROOTS`
- `ELOWEN_DEVICE_CAPABILITIES`
- `ELOWEN_EDGE_WORKSPACE_ROOT`
- `ELOWEN_EDGE_WORKTREE_ROOT`
- `ELOWEN_CODEX_COMMAND`
- `ELOWEN_CODEX_ARGS_JSON`
- `ELOWEN_SANDBOX_MODE`
- `ELOWEN_LOG_FORMAT`
- `RUST_LOG`

See [edge.env.example](D:/Projects/elowen/elowen-edge/edge.env.example) for a standalone laptop template.

`ELOWEN_ALLOWED_REPO_ROOTS` is the preferred way to declare accessible repositories. The edge discovers nested git repositories under those parent directories during registration. `ELOWEN_ALLOWED_REPOS` remains available as an explicit overlay when you need a manual supplement or exception.

## Real Codex runner

The supported real runner path is the Codex CLI:

```powershell
ELOWEN_CODEX_COMMAND=codex
```

Optional extra CLI flags go in `ELOWEN_CODEX_ARGS_JSON`, for example:

```json
["--model","gpt-5.4"]
```

`elowen-edge` manages the rest of the invocation itself:

- it runs `codex exec`
- it passes the job request file over stdin
- it sets the worktree as the Codex working directory
- it captures JSONL runner output and the final assistant message into worktree-local files
- it runs a startup preflight against the configured Codex CLI

`ELOWEN_CODEX_ARGS_JSON` is for extra `codex exec` flags only. Do not include `exec`, `-C`, `--cd`, `-o`, or `--output-last-message` there.

## Sandbox boundary

`elowen-edge` now enforces a workspace sandbox by default:

- validation working directories must stay inside the job worktree
- validation commands cannot invoke shell entry points such as `powershell`, `pwsh`, `cmd`, `sh`, or `bash`
- temp and cache writes are redirected into `.elowen-sandbox/` inside the worktree
- each job writes a sandbox policy artifact to `.elowen-sandbox/policy.json`
- sandbox-blocked runs surface as failure class `sandbox`

Set `ELOWEN_SANDBOX_MODE=off` only for local debugging when you explicitly need to bypass those checks.

## Windows helper scripts

The Windows helper scripts live under [scripts/windows](D:/Projects/elowen/elowen-edge/scripts/windows):

- [Start-ElowenEdge.ps1](D:/Projects/elowen/elowen-edge/scripts/windows/Start-ElowenEdge.ps1) starts the SSH tunnel and edge process from a checked-in env file path.
- [Install-ElowenEdgeStartup.ps1](D:/Projects/elowen/elowen-edge/scripts/windows/Install-ElowenEdgeStartup.ps1) installs a per-user Startup-folder launcher that runs the same wrapper at logon.
- [Register-ElowenEdgeTask.ps1](D:/Projects/elowen/elowen-edge/scripts/windows/Register-ElowenEdgeTask.ps1) registers a per-user Task Scheduler entry that launches the same wrapper at logon.

The platform-level runbook for the standalone laptop path lives in [laptop-edge.md](D:/Projects/elowen/elowen-platform/docs/laptop-edge.md).
