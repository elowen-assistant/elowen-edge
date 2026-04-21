# elowen-edge

## Purpose

Rust edge runtime that executes dispatched work on a trusted device. It registers with the orchestrator, manages worktrees, runs Codex and validation commands, and reports lifecycle events back over NATS.

## Current Responsibilities

- register the device with `elowen-api` and renew that registration on a heartbeat
- answer availability probes and accept job dispatch and approval subjects
- resolve allowed repositories from explicit repo names and discovered parent-directory roots
- create disposable git worktrees for dispatched jobs
- run Codex or the simulated runner inside the worktree
- execute repository validation commands and classify sandbox or validation failures
- gate pushes behind explicit approval commands
- generate and verify trusted registration material for orchestrator enrollment, rotation, and re-enrollment

## Repository Layout

- `src/runtime.rs` - startup, subscriptions, and long-running loops
- `src/registration.rs` - registration heartbeat and trust proof helpers
- `src/discovery.rs` - repo discovery and repo-root resolution
- `src/execution.rs` - worktree, Codex, validation, commit, and push flows
- `src/sandbox.rs` - sandbox policy creation and containment checks
- `src/events.rs` - lifecycle event publication
- `src/config.rs` - environment parsing and startup options
- `scripts/windows/` - local laptop startup and persistence helpers
- `edge.env.example` - example standalone edge configuration

## Runtime And Config Entrypoints

Run locally with:

```powershell
elowen-edge --env-file .\edge.env.local
```

You can also set `ELOWEN_EDGE_ENV_FILE` instead of passing `--env-file`.

Important environment variables:

- `ELOWEN_API_URL`
- `ELOWEN_NATS_URL`
- `ELOWEN_DEVICE_ID`
- `ELOWEN_DEVICE_NAME`
- `ELOWEN_ALLOWED_REPOS`
- `ELOWEN_ALLOWED_REPO_ROOTS`
- `ELOWEN_EDGE_WORKSPACE_ROOT`
- `ELOWEN_EDGE_WORKTREE_ROOT`
- `ELOWEN_CODEX_COMMAND`
- `ELOWEN_CODEX_ARGS_JSON`
- `ELOWEN_SANDBOX_MODE`
- `ELOWEN_ORCHESTRATOR_PUBLIC_KEY` or `ELOWEN_ORCHESTRATOR_PUBLIC_KEYS`
- `ELOWEN_TRUSTED_ORCHESTRATOR_KEYS_JSON`
- `ELOWEN_EDGE_SIGNING_KEY`
- `ELOWEN_PREVIOUS_EDGE_SIGNING_KEY`

Generate trust key material with:

```powershell
elowen-edge --generate-trust-keypair
```

## Trusted Registration Lifecycle

The edge keeps orchestrator challenge verification strict:

- a registration challenge is accepted only if its public key matches a locally pinned orchestrator key
- if the API includes an `orchestrator_key_id`, the edge also requires that key id to match a pinned entry
- unknown orchestrator keys are rejected even during a rotation window

Recommended operator setup:

1. For steady state, pin the orchestrator with `ELOWEN_TRUSTED_ORCHESTRATOR_KEYS_JSON` or `ELOWEN_ORCHESTRATOR_PUBLIC_KEYS`.
2. During orchestrator rotation, keep both the current and next orchestrator public keys pinned locally until the old signer is retired.
3. During edge signing-key rotation, set `ELOWEN_EDGE_SIGNING_KEY` to the new private key and `ELOWEN_PREVIOUS_EDGE_SIGNING_KEY` to the old private key for the re-enrollment window.
4. After the API confirms the new edge key is trusted, remove `ELOWEN_PREVIOUS_EDGE_SIGNING_KEY`.

Multi-edge enrollment guidance:

- assign each edge a stable `ELOWEN_DEVICE_ID`; do not reuse another device's id for an additional laptop or host
- treat `ELOWEN_PREVIOUS_EDGE_SIGNING_KEY` as a re-enrollment tool for one existing device, not as a way to clone trust onto a second device
- keep per-device env files separate so orchestrator pins and edge keys cannot be mixed up across machines

## Local Verification

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --quiet
cargo doc --no-deps
```

## Related Docs

- `scripts/windows/`
- `edge.env.example`
- `../elowen-platform/docs/laptop-edge.md`
