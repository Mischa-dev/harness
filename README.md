# harness

The teammate runtime: the always-on mind for model-agnostic agents that work as teammates in Agent Workspace and are deployed by Agent OS.

- [ARCHITECTURE.md](ARCHITECTURE.md) describes the system.
- [PLAN.md](PLAN.md) records the decisions, the milestones and the exam.
- [AGENTS.md](AGENTS.md) is the guide for anyone (or any agent) working in this repo.

## Crates so far

| Crate | What's in it |
|---|---|
| `crates/core` (`harness-core`) | The `Clock` trait with a real and a fake clock; wake reasons and their urgency |
| `crates/providers` (`harness-providers`) | The internal message format, the `Provider` trait, capability profiles loaded from TOML, and a scripted fake model |
| `crates/tools` (`harness-tools`) | The policy executor (walls, approvals, the message breaker and pause), strict flat-schema validation, and the `sleep` tool |
| `crates/loop` (`harness-loop`) | Cache-friendly context assembly and `run_wake`: model turns and tool calls until the agent sleeps |

```
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```
