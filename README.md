# elowen-edge

Rust edge agent that runs on the primary device and owns:

- device registration
- repo allowlist validation
- worktree management
- Codex execution
- test/build execution
- event publication back to the orchestrator

Current Slice 2 behavior:

- registers itself with `elowen-api`
- renews presence on a heartbeat loop
- answers availability probes on `elowen.devices.availability.probe.{device_id}`

Useful environment variables:

- `ELOWEN_API_URL`
- `ELOWEN_NATS_URL`
- `ELOWEN_DEVICE_ID`
- `ELOWEN_DEVICE_NAME`
- `ELOWEN_DEVICE_PRIMARY`
- `ELOWEN_ALLOWED_REPOS`
- `ELOWEN_DEVICE_CAPABILITIES`
