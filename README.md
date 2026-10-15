# Harness teammate runtime

Harness is the in-progress runtime for long-lived, model-agnostic agents that
work as teammates in Agent Workspace and are deployed by Agent OS. It provides
the small, safety-critical core of an agent: a durable workday conversation,
event-driven wakes, provider-independent model messages, policy-checked tools,
and a deterministic simulator.

The project is currently a runtime library and simulation CLI, not a complete
daemon. The fake provider is fully local and is the supported way to explore
the system today; real-model wiring, Agent Workspace integration, and Agent OS
deployment arrive in later milestones. See [PLAN.md](PLAN.md) for the precise
status and roadmap.

## Design goals

- **Continuous, proactive work.** An agent keeps one conversation for a
  workday, wakes for relevant events, and chooses when to look again.
- **Model independence.** Runtime code uses one internal message format;
  provider-specific JSON stays in provider adapters.
- **Enforced safety.** Every tool call passes through the policy executor,
  including filesystem boundaries, approval walls, and circuit breakers.
- **Deterministic testing.** Time comes from a `Clock`, and the simulator runs
  the real loop, store, and policy code against a fake clock and fake model.
- **Crash-aware durability.** Wakes and tool progress are checkpointed to
  SQLite so a restart can recover what happened and retain its cost.

## Prerequisites

- Rust **1.85 or newer** (edition 2024). The checked-in
  [`rust-toolchain.toml`](rust-toolchain.toml) asks rustup for the stable
  toolchain plus `clippy` and `rustfmt`.
- A C toolchain supported by Rust. SQLite is built from the bundled source, so
  no system SQLite package is required.

Install Rust with [rustup](https://rustup.rs/), clone the repository, and let
rustup install the declared toolchain:

```console
git clone <repository-url>
cd harness
rustup show
cargo check --workspace
```

The runtime does not need credentials for local development or fake-provider
scenarios. Never put model keys in this repository or in state visible to an
agent; production credentials belong in the broker or forwarding proxy.

## Quick start

Run all unit and integration tests:

```console
cargo test --workspace
```

List the bundled simulator scenarios and run one locally:

```console
cargo run -p sim -- list
cargo run -p sim -- run quiet-week --provider fake
cargo run -p sim -- run busy-group-chat --provider fake
```

The simulator advances a fake clock to each event or scheduled next look,
runs the production wake loop, and prints a scored report. It exits with status
`0` when every hard check passes, `1` when the scenario runs but fails a check,
and `2` for invalid arguments or a runtime/setup error. A scenario can also be
selected by path:

```console
cargo run -p sim -- run crates/sim/scenarios/quiet-week.toml --provider fake
```

`--model <profile>` is reserved by the CLI but intentionally not implemented
yet. Do not run real-model scenarios against real projects or conversations.

## Development workflow

The same commands run in CI:

```console
cargo fmt --all --check
cargo check --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

Use `cargo fmt --all` to apply formatting. During development, run
`cargo check --workspace` plus focused tests for the crate being changed; run
the complete command set before pushing. Changes to prompts, the loop, wake
filtering, or the state view must also rerun the affected simulation scenarios.
Repository-specific safety and contribution rules live in
[AGENTS.md](AGENTS.md).

### Writing a scenario

Scenarios live in `crates/sim/scenarios/` as TOML. Each declares a start time,
duration, messages, and optional limits:

```toml
name = "example"
summary = "Replies to one direct message"
agent = "Mara"
start = "2026-01-05T09:00:00Z"
length = "1d"

[[event]]
at = "2h"
conversation = "dm"
from = "Sam"
text = "Can you check the build?"

[expect]
max_messages = 1
max_wakes = 24
```

Offsets accept days, hours, and minutes (for example `2d 14h30m`). Direct
messages and whole-handle mentions wake the agent; other channel messages wait
until its next scheduled look. Add scenario assertions in
`crates/sim/tests/scenarios.rs` when introducing behavior that should remain
stable.

## Architecture

```text
Agent Workspace / Agent OS events
                │
                ▼
       WakeReason + wake message
                │
                ▼
 Workday ──► context assembler ──► Provider trait ──► model adapter
    │                                      │
    │                                      ▼
    │                              internal response
    │                                      │
    │                                      ▼
    │                           PolicyExecutor ──► Tool
    │                              │ walls / breakers
    ▼                              ▼
 SQLite store ◄──────── checkpoint after every turn and before effects
```

A **workday** is one durable conversation. `Workday::wake` opens a wake record,
loads that day's transcript, and drives `run_wake`. The context assembler keeps
stable material first for provider prompt caching. The provider returns only
internal message types; the loop sends every requested tool through
`PolicyExecutor`. Before effects run and after every model turn, a checkpoint
updates the transcript and usage record atomically. Calling `sleep` ends the
wake and schedules the next look; the workday ends at the owner's quiet hour or
when its context limit is reached.

Important invariants:

1. Provider wire formats do not escape `crates/providers`.
2. Tool calls cannot bypass the policy executor.
3. Runtime time reads go through `Clock`; only daemon wiring creates a real
   system clock.
4. Untrusted external content must be converted to quarantined claims before
   entering the main thread.
5. Agent-visible state, logs, prompts, and fixtures contain no secrets.

[ARCHITECTURE.md](ARCHITECTURE.md) describes the full target design, including
workers, memory, claims, authority, and deployment boundaries. [PLAN.md](PLAN.md)
records design decisions, milestones, and the behavioral exam.

## Repository layout

| Path | Purpose |
| --- | --- |
| `crates/core` | Shared `Clock` implementations and wake reasons/urgency. |
| `crates/providers` | Internal messages, capability profiles, provider trait, HTTP transport, Anthropic and OpenAI-compatible adapters, and recorded response fixtures. |
| `crates/tools` | Tool interface, flat-schema validation, policy executor, safety walls, circuit breaker, and `sleep`. |
| `crates/loop` | Cache-friendly context assembly, the per-wake model/tool loop, checkpointing, and durable workday orchestration. |
| `crates/store` | SQLite schema and APIs for workdays, transcripts, wake usage, spend, and evening policy. |
| `crates/sim` | Scenario parser, deterministic world, fake teammate, runner, scoring report, and `sim` CLI. |
| `crates/sim/scenarios` | Human-readable behavioral scenarios used by the simulator. |
| `prompts` | Product prompts. Prompt changes are behavior changes and require scenario evidence. |
| `.github/workflows/ci.yml` | Formatting, build, lint, test, and rustdoc checks. |

## API entry points

Each library crate re-exports its primary API from `lib.rs`. Most integrations
start with these types:

- `harness_core::{Clock, FakeClock, WakeReason}`
- `harness_providers::{Provider, ModelRequest, ModelResponse, CapabilityProfile}`
- `harness_tools::{Tool, Policy, PolicyExecutor, SleepTool}`
- `harness_loop::{ContextAssembler, Workday, run_wake}`
- `harness_store::{Store, WorkdayPolicy}`
- `sim::{Scenario, run}`

Generate and open local API documentation with:

```console
cargo doc --workspace --no-deps --open
```

## License

This repository is currently private and marked `UNLICENSED`; no permission to
copy, distribute, or publish it is granted.
