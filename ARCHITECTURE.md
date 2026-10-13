# Teammate runtime: architecture

This is the design for a team of AI agents that work as real teammates in your projects. They're proactive, they keep a continuous train of thought, they delegate to coding workers like Claude Code and Codex, and they learn your taste. The runtime runs its own agent loop and is **model-agnostic**: each agent's models are settings, and any provider works. It's its own project, built to plug directly into the two systems you already have:

- **Agent Workspace** is where the team lives: chat, DMs and groups, reply requests, tasks with claims, pages and decisions, Codex runs and GitHub.
- **Agent OS** defines, deploys and manages the agents: their identity, models, skills, MCP servers, credentials (through the broker), host environment, temporary agents, and triggers.

This project is the part neither of those has: **the mind**. That means the always-on loop that decides when to wake, what to pay attention to, what's worth doing, when to hand work off and whether the result is good enough, and how to get better at all of it. Agent Workspace keeps its rule of no model calls in the server, because all the model calls in this design live here.

The prompts in `prompts/` hold the judgment. This document describes the system. [PLAN.md](PLAN.md) owns the decisions behind it (with the options considered), the cost model, the testing approach and the milestones.

## Where it fits

Agent OS's vision already has a slot for this: "always-on runtimes (Hermes first, then OpenClaw, and possibly the owner's own runtime)". This project is that runtime, designed as a teammate.

```
                ┌──────────────────────────── Agent OS ─────────────────────────────┐
                │ declarations · identity · models · skills · MCP · triggers         │
                │ broker (credentials) · NixOS realization · temporary agents        │
                └───────┬───────────────────────────────────────┬───────────────────┘
                        │ deploys + manages                     │ runs workers in fresh
                        ▼                                       ▼ declared environments
 ┌──────────────── Teammate runtime (this project), one per agent ─────────────────┐
 │ wake policy · main thread · state view · schedule · reflection · second opinion  │
 │ worker delegation + judgment · journal and memory · taste store · trust ledger    │
 │ circuit breakers · budget · quarantine for untrusted input                        │
 │ engine: its own agent loop · any model through the broker · policy executor     │
 └───────┬──────────────────────────────────────────────▲───────────────────────────┘
         │ MCP tools (acts as itself)                     │ signed wakeups
         ▼                                                │
 ┌──────────────────────────────── Agent Workspace ───────────────────────────────┐
 │ chat · DMs/groups · reply requests (claim/pass/turn allowance) · briefing        │
 │ tasks with claims · threads with status/outcome · pages · decisions · search     │
 │ approvals · Codex runs · GitHub App (PRs, checks, reviews)                       │
 └────────────────────────────────────────────────────────────────────────────────┘
                                         ▲
                                     You, from anywhere
```

### Who owns what

| Concern | Owner | How this project uses it |
|---|---|---|
| Conversations, reply requests, who answers, turn allowance | Agent Workspace | The runtime answers through `reply_claim` / `reply_send` / `reply_pass`. It replaces the Nyx bridge script. |
| Wakeups | Agent Workspace (signed `inbox.available` webhooks), Agent OS (triggers) | A wakeup starts or resumes the agent's main thread. The database stays the source of truth, as Workspace intends. |
| The work board, claims and leases | Agent Workspace tasks | An agent's to-do list is its tasks. It creates, claims, reorders and updates them itself. |
| Project memory | Agent Workspace pages, decisions, thread outcomes | Project briefs are pages, settled things are decisions, and every pitch is a thread with a status and an outcome. |
| Codex runs, worktrees, GitHub PRs/checks/reviews, approvals | Agent Workspace | One way to run workers (needs your approval through `run_request`), and the path for PRs and approvals. |
| Agent identity, model choices, skills, MCP, credentials, host environment, deployment | Agent OS | The runtime is declared and deployed like any Agent OS agent. Model choices per role live in the declaration. Credentials come through the broker, never in files. |
| Model access | Agent OS broker | Every model call goes through the broker, which holds provider keys and forwards requests with scoped grants. A local forwarding proxy in the daemon fills in until the broker covers each provider. |
| Temporary agents (Claude Code, Codex, T3 Code in fresh Nix environments) | Agent OS | The main way to run workers once that's in the alpha. |
| The mind: wake policy, main thread, state view, reflection, second opinion, worker judgment, journal, taste learning, trust ledger, circuit breakers, budget, quarantine | **This project** | All model calls live here. |

## Rules the design follows

1. **The agent is the loop.** Nothing assigns it work, gates its ideas or tells it when to stop. It decides, the way a person on a small team does.
2. **Information, not gates.** The agent sees what a good teammate would weigh (your review backlog, its track record, the budget) and adjusts on its own.
3. **Code enforces only walls and circuit breakers.** Walls are things that must never happen. Circuit breakers are set so high that tripping one means something is broken.
4. **Untrusted text never reaches an agent raw.** Email, web content and outside issues arrive as quarantined claims that must be verified.
5. **Every pitch is logged next to its outcome.** That's how agents learn your taste and how you see whether they are learning.
6. **Reuse, don't duplicate.** If Agent Workspace or Agent OS already does something, the runtime calls it. This project adds only the mind.
7. **Models and runtimes are plugins.** The main thread's models are settings, and workers are adapters. Switching provider is a configuration change.

## Anatomy of an agent

### 1. Its own computer

Each agent is an Agent OS deployment: a dedicated service account and environment on a NixOS host, with the isolation Agent OS already enforces. Its home folder persists between wakes: scratch notes, half-finished branches, helper scripts, its journal. Waking up where it left off is a big part of what makes it feel like a person.

Credentials never sit in the agent's files. Model calls go through the Agent OS broker, which injects provider keys. Until the broker covers a provider, a small forwarding proxy in the daemon does the same job. Other credentials also come through the broker as scoped, revocable grants.

The mind itself is light: model calls over HTTP, a few tools, SQLite. The first agent runs on your laptop during development, then on an always-on NixOS host. Nyx stays dedicated to the crew. Heavy builds go to workers elsewhere (see PLAN.md, D5).

### 2. The main thread

One ongoing conversation per day: the agent's train of thought. It's where it talks with you, plans, decides, and judges what workers bring back.

- **The engine is the runtime's own agent loop.**
  - Each turn assembles the context, calls the model through the provider layer, runs any tool calls through the **policy executor**, and repeats until the agent calls `sleep`.
  - The provider layer has three adapters: OpenAI-compatible (OpenRouter, NVIDIA NIM, local servers and most vendors), Anthropic Messages, and OpenAI Responses. Each converts to one internal format, and each model has a capability profile (context size, tool-call style, caching type).
  - Every call records usage and cost.
- **Models are chosen per role** in the agent's declaration, each with a fallback:
  - a mid-tier model for the main thread
  - optionally a stronger one for the evening turn and big judgment calls, switched only at the start of a wake so the cache isn't broken
  - a cheap model for helpers
  - a *different family* for the critic
- **Context is assembled in a fixed, cache-friendly order:** static prefix (charter, SOUL, tool definitions), then the memory snapshot, then today's transcript, then the wake message. Anthropic's explicit cache breakpoints and OpenAI's automatic prefix caching both reward that order.
- **Workdays:** one conversation per agent per day.
  - The transcript is stored in SQLite, and each wake appends to it.
  - At a quiet hour you choose, or early once the transcript passes a size limit (start at 80K tokens), the agent gets an **evening turn**. It writes its journal, updates `MEMORY.md` and `USER.md`, records outcomes and decisions in Workspace, runs its reflection habit, and leaves a **handoff note**.
  - The next day starts fresh from the charter, SOUL, memory files, the handoff and its open tasks.
  - Within a day, old tool outputs can be trimmed to one-line summaries. Nothing depends on a vendor's compaction.
  - This bounds drift and cost, and gives the agent a natural nightly point to consolidate, like sleep.
- **Each wake message** contains the wake reason, the state view (below), and any due follow-ups.
- **Short wakes:** the main thread is for thinking and deciding. Anything that would take more than about 10 minutes goes to a worker, so the agent stays responsive.
- **Interrupts:** a `now`-class event arriving mid-wake cancels the in-flight model call at the next safe point (never in the middle of a running tool) and continues the same wake with the urgent message. Everything else is queued.
- **Going back to sleep:** the agent ends a wake with `sleep(next_look)`, choosing when it wants to look again ("in 3 hours, or sooner if something important arrives"). The runtime enforces a floor of a few minutes and a ceiling of once a day.

### 3. Senses: what wakes it

| Wakes it now | Waits for its next look |
|---|---|
| A Workspace wakeup: a required reply (a DM), an @mention, a task assignment, an approval | Optional group replies (the filter passes or wakes) and routine chat it wasn't asked about |
| An Agent OS trigger aimed at it (a GitHub event, a connector event) | Low-importance outside news |
| One of its own follow-ups or routines | Activity in projects it doesn't own |
| One of its workers finished | |
| A quarantined claim with importance 8+ in its area | |
| A wall or circuit breaker blocked one of its actions | |

For everything in between, a cheap **peripheral filter** (`prompts/peripheral.md`, on the helper model) looks at the batch against the agent's current focus. It answers: wake now, keep for the next look, or drop. That's how the agent notices things without a full wake on every event.

The filter also handles **optional group replies**. When a group message asks every agent for an optional reply, the filter decides whether this agent would genuinely add something. If not, it passes without waking the main thread. A pass posts nothing, so it's safe to decide cheaply, and it saves a lot of wakes.

Workspace's reply rules stay in charge of conversation. A DM asks for a required reply. A group message asks every agent for an optional reply, which it can pass on. Agent-to-agent chatter is limited by the turn allowance, and a newer human message supersedes a stale request. The runtime doesn't reinvent any of that.

### 4. Its own clock

A `schedule` tool for follow-ups ("check tomorrow whether she looked at #42") and for routines it decides it wants ("Friday afternoon: look over dependency updates in project A"). These are the agent's *runtime* habits. The runtime keeps them, reports them to Agent OS as observed state, and shows them in Workspace. Anything you want to keep permanently can be promoted into the agent's declaration as an Agent OS scheduled mission. Project-level automation (CI workflows in a repo) still goes through a PR.

### 5. Its to-do list

Its Workspace tasks: assigned to it, claimed by it, or created by it. It creates, reorders, splits and closes them with the existing task tools, and claims before starting so teammates see "Mara is working on this". There's no separate to-do system.

### 6. Hands: its tools

**From Agent Workspace's MCP**, acting as itself:
- `briefing`, `conversation_open`, `search`, `mark_read`
- `message_send`, `message_react`, and `reply_claim`, `reply_send`, `reply_pass`, `reply_release`
- `thread_start`, `thread_update` (status and outcome)
- `task_create`, `task_claim`, `task_update`, `task_mark_blocked`, `task_mark_ready_for_review`, `task_link`
- `page_get`, `page_write`, `decision_record`
- `pull_request_get`, `run_request`, `approval_get`, `approval_respond`

**From this runtime** (native Rust tools, so no extra server is needed):
- `sleep`, `schedule`, `unschedule`: its own clock
- `spawn_worker`, `worker_status`, `steer_worker`, `stop_worker`: see Workers
- `second_opinion(idea)`: runs `prompts/critic.md` on another model family, with your taste history
- `read_inbox(source)`: untrusted sources as quarantined claims, never raw text
- `journal_write`, `memory_read`, `memory_write`, `search_sessions`: its own memory
- `flag_impossible(reason)`

**On its computer:** read, search, edit, git, and a shell that runs inside a sandbox in its Agent OS environment. These are meant for small things. Bigger changes go to workers.

**The policy executor.** Every tool call, whether native or through Workspace's MCP, passes through one policy executor before it runs. That's where the walls are checked: protected paths, forbidden commands, actions that need your yes, breaker limits. It's code in front of the tools, not an instruction in a prompt, and it doesn't depend on any vendor's hook system.

### 7. Workers

Sometimes the right move is to describe the work and hand it off. The agent decides when. Small things it does itself. Workers are for well-defined tasks big enough to focus on, for parallel work, or for a fresh pair of eyes from another model.

```
idea or task
   │  agent writes a worker brief: goal, context, acceptance check, constraints, out of scope
   ▼
spawn_worker(kind, brief)
   │  fresh context, its own sandbox and worktree, no memory of the agent's life
   ▼
worker returns: diff, check results, notes, open questions
   │
   ▼
agent judges: reads the diff, runs the checks itself, maybe asks a worker from the other model family to review
   ├─ accept   → push the branch, open the PR, pitch it
   ├─ revise   → steer the same worker with specific feedback
   ├─ redo     → spawn a new worker with a better brief
   └─ discard  → journal why
   ▼
journal: what the brief got right or wrong
```

Several kinds of worker, one interface:

| Kind | Runs where | When |
|---|---|---|
| **Coding CLIs** | Claude Code (`claude -p`, or a cloud session) on your Claude plan; Codex (`codex exec`, or cloud) on your ChatGPT plan; or an open CLI with any model through the broker. Runs in a worktree, under your login. | Available now. Claude Code and Codex are the strongest coders, and they run on plans you already pay for. |
| **Native worker** | The runtime's own loop with a worker profile: write tools in a worktree, no team tools, any model | Later. Fully owned and model-agnostic. |
| **Agent OS temporary agent** | A task runner in a fresh, declared Nix environment that is discarded afterward | Once temporary agents land in the alpha. The default after that: clean, reproducible, nothing accumulates. |
| **Workspace Codex run** | Agent Workspace's Codex worker, with its worktrees and GitHub App | When the work is tied to a Workspace task and should show up there. `run_request` asks you to approve it, which is a good fit for bigger jobs. |

Rules:
- **Workers have narrow powers.** They can work in their sandbox and report back to the agent that spawned them. They can't message anyone, touch tasks, schedule anything or spawn workers. The agent does all of that.
- **No overlapping writes.** Two workers in one repo can't hold overlapping path locks.
- **Judging isn't independent review.** The agent's judgment is its quality bar. The PR's required review still comes from a separate reviewer.

### 8. Memory

Memory is split by who it belongs to:

| Memory | Lives in | Notes |
|---|---|---|
| **The project's**: guide, conventions, what's paused and why, decisions, thread outcomes | Agent Workspace pages and decisions | Shared by everyone. Pages keep revisions, so agents can edit them and every edit can be undone. Keep one page per project, called **Intent**, that only you edit. The charter tells agents to read it and never change it. |
| **The agent's own**: `MEMORY.md` (capped index), `USER.md` (you and how you work, capped), journal, handoff notes | A git repo of Markdown in the agent's Agent OS state folder | Git gives history and undo. Agent OS memory policy covers backups, export and moving the agent. Who owns mutable memory is still an open Agent OS question. |
| **Runtime state**: wakes, schedules, workers, usage, breakers | SQLite (WAL) in the same state folder | Portable with the agent |
| **Skills** | Drafts in the agent's home; adopted ones in Agent OS | An agent writes a skill from experience. To make it real, it proposes it as an Agent OS skill assignment you review. Agent OS already owns skills. |
| **Taste and outcomes**: pitches, outcomes, your reasons, the trust ledger | SQLite with full-text search | A cache, rebuildable at any time from Workspace thread outcomes and GitHub PRs. No embeddings to start; `second_opinion` pulls about 20 similar pitches by full-text search and the critic reads them. |

The evening turn keeps `MEMORY.md` under its cap and records thread outcomes and decisions in Workspace for anything that settled.

### 9. Responsibilities and habits

- **Responsibilities** are where initiative comes from. Each agent's Agent OS declaration lists 2–5 standing responsibilities, things it *owns*, such as "keep Agent Workspace healthy: CI green, dependencies current, advisories handled" or "make the docs match the code." They say what it owns, not what to do. The charter receives them as `{{responsibilities}}`. Reflection and the state view both ask how each one is going. An agent with nothing to own has nothing to be proactive about.
- **Reflection** is a skill the agent runs during its evening turn, when it has slack, or as one of its own routines. It loads `prompts/idea-engine.md` with its journal, the `briefing`, recent decisions and its open questions. Ideas become Workspace tasks. Curiosity it can't resolve from notes becomes small read-only investigations.
- **Second opinion** is a tool, not a gate. The charter asks the agent to use it before speculative work that will cost you review time. It runs `prompts/critic.md` on another model family with the most similar past pitches and your reactions to them. The agent decides.

## The state view

Every wake starts with a short state view, built from Workspace's `briefing`, GitHub data through Workspace, and this project's own records:

```
Team state, Tue 00:24
- Mischa: active 4 min ago · 6 of your PRs waiting on her (oldest 3 days) · reviewing ~3/day lately
- Your last 10 pitches: 6 merged · 3 changed · 1 closed ("not now, paused project")
- Your workers: 1 running (#88, 22 min, tests passing)
- Briefing: 2 reply requests · 1 @mention · task AW-142 reassigned to you
- Teammates: Rook working on AOS-31 · Iona asleep until 09:00
- Budget: $3.10 of today's $15 model spend · Claude plan ≈ 55% used for the week (resets Thu)
```

A good teammate seeing "6 PRs waiting on her" doesn't stop working. It keeps building on branches, bundles small things into one pitch, polishes what's open, researches, or helps a teammate. It holds back new speculative pitches until you catch up. That's judgment, and the charter asks for it.

## Pitches and outcomes

A pitch is a Workspace thread, so nothing new is needed to start:
1. The agent opens a thread in the project channel. It says what it noticed, what it did and why, links the PR and task, and sets the thread status to `Proposed`.
2. A separate reviewer reviews the PR: another agent, or a worker from the other model family. Its verdict is a required status check from its GitHub App identity.
3. You merge, ask for changes in the thread, or close the PR.
4. The runtime records the outcome from the PR (merged, closed, reverted later) and the thread (your reply, or a status you set), and writes the thread outcome ("Merged; rate limit on /admin"). No action within 7 days counts as ignored.

**Possible small addition to Agent Workspace:** a pitch-style approval that an agent can open on a PR, with **Merge**, **Change** and **Close** plus a reason. It would make outcomes one tap for you and exact for the runtime. Until then, threads and PR events are enough.

**Merging** happens only through a credential no agent or worker holds: you on GitHub, or Workspace's GitHub module if it gains a merge action, or the runtime's control process for change kinds an agent has earned (see the trust ledger).

## Walls (enforced in code)

| Wall | Enforced by |
|---|---|
| Nothing reaches main without you, except earned change kinds | GitHub ruleset on main (PRs and required checks, no bypass for agents); merge credential held only by you or the control process |
| No secrets in agent homes or workers | Agent OS broker grants; Workspace's rule that secrets never reach provider child environments or repository files |
| Agents act only as themselves, only where they're members | Workspace checks MCP calls against current memberships every time; `conversation_visible_to` |
| No outside accounts, money, or contacting outsiders | No such credentials anywhere; network limits in the Agent OS environment |
| Every action the agent takes is checked | The policy executor in front of every tool call: protected paths, forbidden commands, actions that need your yes, breaker limits |
| Tests and CI can't be weakened to pass | The policy executor for the agent itself; protected-file rules in worker sessions (Claude Code `PreToolUse` hooks, the equivalent for other CLIs); CODEOWNERS on tests and CI; a CI check that fails on deleted or newly skipped tests |
| Codex runs in Workspace need a person | `run_request` asks you to approve |
| Identity and charter can't be edited by the agent | They're part of the Agent OS declaration; changes need a reviewed deployment |
| Untrusted text arrives only as claims | `read_inbox` and the quarantine; raw text is visible only to verify workers that can't push |
| Workers can't act on the team | Workers get no Workspace credential, only a report-back channel to their agent |
| Only you authorize anything behind a wall | Walls don't depend on who asked. Other agents' messages (the crew, Muse) are colleagues' requests, never authority. |
| Model keys never reach the agent | The Agent OS broker injects them (the daemon's forwarding proxy until the broker covers each provider) |

## Circuit breakers (high on purpose)

If one trips, something is probably broken (a loop, a bug, an injection). The runtime pauses the agent and sends you a message explaining what tripped.

| Breaker | Starting value |
|---|---|
| Messages from one agent in an hour | 30 |
| Open PRs by one agent | 20 |
| Concurrent workers per agent | 4 |
| Runtime routines per agent | 20 |
| Wakes per agent per hour | 30 |
| API spend per day | your choice, say $15 |

Workspace's turn allowance already limits agent-to-agent replies, so the runtime doesn't add its own. There's also a pause per agent and for the whole team, exposed as a Workspace command or an Agent OS lifecycle operation.

## Untrusted input

Email, web pages, advisories, release notes and outside issues come in through Agent OS connectors and triggers as they land in the alpha, or through small pollers in this project until then.

1. A small model with no tools turns the raw text into structured claims (`claim`, `about`, `source`, `how_to_verify`). The raw text is stored by reference only.
2. Claims are tagged against a dependency inventory built from your lockfiles, so "package X has a hole" links to the projects that use package X. Then each claim gets an importance score.
3. When an agent wants to act on a claim, it sends a **verify worker**. That worker may read the raw text and check it against official sources and your code, but it can't push or post. The agent escalates only once the claim is confirmed.

## The learning loop

- **Taste store:** every pitch is indexed with its outcome and your reason. `second_opinion` retrieves similar ones by full-text search.
- **Track record:** each agent sees its own recent acceptance rate in the state view.
- **Trust ledger:** a deterministic classifier labels each merged PR by kind (docs only, tests only, patch-level dependency bump, small change in its own area). After 10 accepted PRs of one kind with no reverts in 30 days, the runtime *asks you* whether to grant self-merge for that kind. A revert removes the grant immediately. Grants are recorded in Workspace as a decision, so they're visible and searchable.
- **Journals** carry the lessons: which ideas land, which worker briefs work.

## Budget

- **The mind runs on API keys:** the main thread, helpers and critic, with any provider. Every call goes through the broker or proxy and is counted exactly, per role and per provider.
- **Subscriptions only power workers.** A subscription can only be used through the vendor's own CLI, so Claude Code and Codex workers run on your Claude and ChatGPT plans. Their usage is estimated from each session's usage output, and rate-limit errors are the hard signal to back off.
- Both go into the state view, so agents budget like people do: lighter on speculative work when money or quota is tight, more just before a weekly reset. A reserve for your own subscription use (say 30%) is shown to agents as already spent.
- If a friend ever triggers an agent, everything that runs for them must use API keys, not your subscriptions.

## Three walkthroughs

None of these is special-cased.

### A vulnerability email at 2:13 a.m.

1. The email arrives through the inbox connector. The quarantine turns it into a claim about package X and tags it with project A from the dependency inventory. Importance 9 in its area wakes the agent that owns project A.
2. The agent spawns a verify worker. It confirms against the official advisory database and your lockfile that you're on an affected version.
3. The agent sends you a DM marked urgent ("Confirmed: project A runs an affected version of package X (link). Fixing it now."). It creates and claims a task, spawns a Claude Code worker with a tight brief, judges the diff, gets a review from a Codex worker, opens the PR, and starts a pitch thread. In the morning: one green PR.

If the claim had been false, it would have journaled it and gone back to sleep, and you wouldn't have been woken.

### An idea, a worker and a judgment call

1. During reflection, the agent notices "auth" coming up in its journal all week, and project B has no security page or decision. It creates a task to look into project B's security posture.
2. It investigates itself and finds an admin route with no rate limit. Its second opinion says you've merged hardening PRs before, so go ahead, but keep the plan short.
3. It spawns a Codex worker for the rate limiting, writes the security plan as a Workspace page itself, and rejects the worker's first diff for touching unrelated code. The second diff is clean.
4. The state view shows 6 PRs waiting on you, so it bundles both into one pitch thread instead of two pings.

### An agent picks up a habit

While on project B, the agent decides to look over its dependencies every Friday. It calls `schedule`, and the routine shows up as its observed state in Agent OS and on its Workspace agent card. If you like it, you promote it to a declared scheduled mission. A security scan inside the repo's CI would go through a PR instead.

## Where the prompts plug in

| Prompt | Used for |
|---|---|
| `teammate-charter.md` | First thing in every main thread, re-injected after compaction. Part of the agent's Agent OS declaration. |
| `peripheral.md` | The cheap wake filter |
| `idea-engine.md` | The reflection skill |
| `critic.md` | The `second_opinion` tool |

Still to write: the worker-brief skill, verify-worker instructions, reviewer instructions, and the quarantine summarizer prompt.

## Testing

Testing is part of the system, because proactivity can't be tuned by waiting for real weeks:
- **One clock.** Every time-based behavior goes through a `Clock` trait, which tests replace with a fake clock.
- **The Lab.** A separate Agent Workspace organization with throwaway repos.
- **Scenario simulations.** Scripted weeks of events, run with compressed time and scored. The exam in PLAN.md is the first scenario set and doubles as a regression suite after every prompt or model change.
- **Shadow mode.** For the first real week, everything the agent would say goes to a private channel and its PRs open as drafts.

PLAN.md has the details.

## Build order

The milestones, M0 (a spike to measure the engine) through M6 (a team), are in [PLAN.md](PLAN.md#milestones).

## Open questions, with defaults

1. **Where agent-made routines live.** Default: runtime state reported to Agent OS, which you can promote to a declared mission. The alternative is a declaration change you review each time.
2. **Who owns an agent's own memory.** Agent OS lists this as open. Default: runtime state under Agent OS memory policy, so it's included in export and move.
3. **A pitch approval in Agent Workspace.** Optional. Default: threads plus PR events until it exists.
4. **The name.** "Teammate runtime" is a placeholder.

## What to measure

- **Interruptions per merged PR**, the best single sign that the team adds to your attention instead of draining it
- acceptance rate per agent over time, and the critic's predicted acceptance against what you actually did
- worker briefs accepted on the first try
- revert rate and cost per merged PR
- how often verify workers refute claims
- circuit-breaker trips, which should be close to zero
