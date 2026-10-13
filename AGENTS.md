# Repository guidance

This repository is the teammate runtime: the always-on mind for model-agnostic agents that work as teammates in Agent Workspace and are deployed by Agent OS. Read `ARCHITECTURE.md` for how the system works and `PLAN.md` for the decisions, milestones and the exam. The prompts in `prompts/` are product code.

Work is tracked in the runtime's Agent Workspace project. Claim the task you're working on, and when it lands, move it forward and comment with the evidence (commit, tests). New work you discover becomes a task, not a note in chat.

## Commands

- Build: `cargo check --workspace`
- Test: `cargo test --workspace`
- Lint and format: `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --check`
- Run a scenario with the fake provider (fast, free): `cargo run -p sim -- run <scenario> --provider fake`
- Run a scenario with a real model (costs money, Lab only): `cargo run -p sim -- run <scenario> --model <profile>`

## How much to check

- While building: `cargo check`, plus the tests for the code you changed.
- Before pushing: the full test suite, clippy and fmt.
- After any change to `prompts/`, the loop, the wake filter or the state view: rerun the affected exam scenarios and report the scores.
- Write tests first where a bug would be silent and costly: the policy executor (walls), leases and path locks, circuit breakers, the quarantine boundary, and provider adapter parsing (with recorded fixtures). Don't write test matrices for formatting or UI details.

## Architecture rules

- **Models are settings.** Provider-specific JSON stays inside `crates/providers`. Everything else uses the internal message format. Never hardcode a model name outside capability profiles and config.
- **Every tool call goes through the policy executor.** Never add a tool or code path that bypasses it. Walls are code, not prompt text.
- **All time goes through the `Clock` trait.** Don't read the system clock directly outside the daemon's wiring; the simulator depends on this.
- **No secrets anywhere the agent can see them:** not in state, transcripts, logs, prompts, memory files, or worker environments. Model keys live only in the broker or the forwarding proxy.
- **Untrusted text never enters the main thread raw.** Email, web content and outside issues reach the agent only as quarantined claims.
- **Talk to other systems only through their public interfaces:** Agent Workspace through its MCP server, Agent OS through its CLI and controller. No shared databases.
- **Use argument arrays for processes and git.** Never build a shell command from model output, messages or file contents.
- **Workers get no team credentials.** They report back to the agent that spawned them, and nothing else.
- **Keep the main thread's toolset small.** Bigger work belongs to workers.

## Never

- Run tests or scenarios against real projects, real conversations, or Nyx. Use the Lab organization and throwaway repos.
- Push to main, or merge your own PR.
- Edit a live agent's IDENTITY, SOUL or charter without Mischa asking.
- Commit `.env` files, keys, tokens or recorded fixtures that contain credentials. Scrub fixtures before committing.
