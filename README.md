# elowen-edge

Rust edge agent that runs on the primary device and owns:

- device registration
- repo allowlist validation
- worktree management
- Codex execution
- test/build execution
- event publication back to the orchestrator

The scaffold keeps the execution boundary explicit while the worktree manager and Codex wrapper are designed.
