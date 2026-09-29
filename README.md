# Phoenix CI — Self-Healing PR Review

*(formerly “Swarm CI” — renamed for the demo; internal crate names still say `swarm-*`.)*


A planner agent and an implementer agent review and fix a pull request behind a **deterministic merge gate**. If the implementer process is killed mid-task, a supervisor detects the failure via a heartbeat/lease mechanism, reassigns the work to a fresh worker, and the system finishes correctly — demonstrated live.

Built for the Swarm Village hackathon. See `BUILD_PROMPT.md` for the full spec and operating contract, and `DEVLOG.md` for the living build log (source of truth for current status).

## Architecture (one paragraph)

The user submits a PR → the **planner agent** (LLM) reads the diff and produces a typed plan → a deterministic dispatcher hands the plan to an **implementer worker** (Tokio task) that edits a sandboxed working copy, sends heartbeats, and runs tests → the **supervisor** tracks a lease per in-flight task and reassigns to a fresh worker whenever heartbeats stop → a **deterministic merge gate** opens only on a real passing `cargo test` run (never on agent self-report).

Agentic layers may be wrong and are never trusted blindly. Deterministic layers own the truth: state transitions, leases, reassignment, test execution, the merge gate.

## Layout


```
crates/
  core/        domain types: task state machine, events, lease config, merge gate (pure)
  supervisor/  lease tracking, reap loop, reassignment with fencing (attempt numbers)
  worker/      killable Tokio worker runtime with pluggable TaskExecutor
  api/         Axum HTTP API + SSE event stream (Phase 2)
  agents/      LlmProvider trait, planner + implementer agents (Phase 3)
```

## Quickstart

```bash
cargo build                              # workspace build
cargo test                               # 10 unit + 2 integration tests (kill-and-reassign lives in swarm-supervisor)

# One command: auto-loads .env, pre-flights keys/port, runs dashboard + API
./scripts/swarm.sh              # add --mock for offline mode, --release for optimized
# then open http://localhost:3000 — submit a PR, watch the event feed, hit ⚡ KILL mid-flight

# Manual alternative (plain env vars):
# set -a; source .env; set +a && RUST_LOG=info cargo run -p swarm-api
```

Then exercise the self-healing flow live:

```bash
TID=$(curl -s -XPOST localhost:3000/tasks -H 'content-type: application/json' \
  -d '{"pr_id":"PR-7","title":"Fix off-by-one","bug_description":"sum skips last element"}' \
  | sed -E 's/.*"task_id":"([^"]+)".*/\1/')

curl -sN localhost:3000/events &                    # watch real backend events (SSE)
sleep 1 && curl -s -XPOST localhost:3000/tasks/$TID/kill   # THE DEMO MOMENT: kill mid-task
sleep 6    && curl -s localhost:3000/tasks/$TID            # reassigned + merged (attempt: 2)
```

## HTTP API

| Endpoint                 | Method | Purpose                                                        |
|--------------------------|--------|----------------------------------------------------------------|
| `/tasks`                 | POST   | Submit a PR (`{pr_id,title,bug_description}`) → `{task_id}`     |
| `/tasks`                 | GET    | List submitted tasks with live status/attempt                   |
| `/tasks/:id`             | GET    | Deterministic snapshot of the task record                       |
| `/tasks/:id/kill`        | POST   | Demo fault injection: abort current worker; supervisor recovers |
| `/events`                | GET    | SSE; `event:` lines use spec names (`task.created`, …)          |

| Variable           | Required | Default                        | Purpose                                   |
|--------------------|----------|--------------------------------|-------------------------------------------|
| `LLM_PROVIDER`     | no       | `mock`                         | `mock` \| `anthropic` (extensible, Phase 3)|
| `ANTHROPIC_API_KEY`| yes if provider=anthropic | —              | API key for the Anthropic implementation   |
| `ANTHROPIC_BASE_URL`| no      | `https://api.anthropic.com`    | Override for proxies/local gateways        |
| `SWARM_BIND`       | no       | `127.0.0.1:3000`               | API listen address                         |
| `SWARM_ALLOW_UNSANDBOXED_AGENT` | yes for a real LLM provider | — | Exact value `I_UNDERSTAND_HOST_EXECUTION` permits the experimental host runner |
| `SWARM_API_TOKEN` | yes for a real LLM provider | — | At least 32 non-whitespace characters; bearer token for task submit and kill writes |
| `SWARM_HEARTBEAT_MS`| no      | `500`                          | Worker heartbeat interval                  |
| `SWARM_LEASE_TIMEOUT_MS`| no  | `1500`                         | Lease expiry (≈3 missed heartbeats)        |
| `SWARM_REAP_INTERVAL_MS`| no  | `250`                          | Supervisor lease-reaper scan cadence       |
| `SWARM_MAX_ATTEMPTS`| no      | `3`                            | Reassignments before the task fails        |
| `SWARM_SIM_WORK_MS`| no       | `3000`                         | Simulated implementer duration (Phase 2)   |
| `SWARM_FIXTURE_DIR`| no       | `fixtures/demo-pr`             | PR fixture copied into sandboxes (agent mode) |
| `SWARM_SANDBOX_ROOT`| no      | `$TMPDIR/swarm-ci-sandbox`     | Where per-attempt sandboxes are created    |
| `GROQ_API_KEY`     | yes if provider=groq      | —                              | Primary LLM (Groq Cloud, OpenAI-compatible)|
| `GROQ_MODEL`       | no       | `openai/gpt-oss-120b`          | Groq chat model id                         |
| `GOOGLE_API_KEY`   | no       | —                              | Gemini heavy-reasoning **auto-fallback**   |
| `GOOGLE_MODEL`     | no       | `gemini-3.6-flash`             | Google model id                            |
| `JINA_API_KEY`     | no       | —                              | Reserved: diff/page fetching (later phase) |

Never commit API keys. Copy `.env.example` → `.env`, fill it in, and load it
before running (`set -a; source .env; set +a`) — the binary reads plain
environment variables.

### Host execution boundary

The current real-agent worker clones submitted repositories, runs `cargo test`
on the host, and can use the host's GitHub credentials to push fixes and merge
PRs. The per-attempt directory is a working copy, not a security sandbox.
Simulated reports can open the demo gate, but the GitHub publisher is disabled
in mock mode, so they cannot trigger a real PR merge.
Real-agent mode refuses to start unless an operator explicitly sets
`SWARM_ALLOW_UNSANDBOXED_AGENT=I_UNDERSTAND_HOST_EXECUTION` and a long
`SWARM_API_TOKEN` (generate one with `openssl rand -hex 32`). Task submission
and worker-kill writes require `Authorization: Bearer <token>`; child Cargo,
git and gh processes do not inherit this token. The dashboard
accepts it in a password field without persisting it. Read-only task snapshots,
the event stream and presets remain unauthenticated. The API listens on
loopback by default. These controls do not isolate untrusted code; use real
mode only on a disposable development machine with test credentials. A
contained worker, resource limits, network restrictions, credential
separation, full read authorization and cancellation of child processes are
required before hosting it for others.

## Solving real pull requests

Point Swarm CI at any PR link — it clones that exact head (`refs/pull/N/head`),
the planner reads the real code, and only a genuine `cargo test` pass opens the
gate:

```jsonc
// POST /tasks  (or paste the link into the dashboard's "PR link" field)
{
  "pr_id": "102",
  "title": "FX conversion loses cents",
  "bug_description": "convert() must apply rate_bps once and preserve cents.",
  "pr_url": "https://github.com/<owner>/<repo>/pull/102"
}
```

Also accepted: `"repo_url"` (any git URL incl. local paths) plus optional
`"git_ref"` (branch/tag/full ref) instead of `pr_url`. The bundled demo target
lives at `github.com/Ay-obami/swarm-demo-target` — three seeded bug PRs
(discount-cap, FX rounding, oversell boundary), each taking ~10–20 s end-to-end
(real planning + cold `cargo test`). Use `SWARM_PLAN_DELAY_MS` if you want an
even wider live-kill window.

### Agent mode vs dev mode

With a real provider configured (`LLM_PROVIDER=groq|google|anthropic`), the
planner produces a typed JSON plan, the implementer clones the PR head into a
fresh per-attempt sandbox and runs **real `cargo test`**, and the gate opens
ONLY for reports with provenance `real_cargo_test`. With `LLM_PROVIDER=mock`
(no keys needed) everything stays offline: simulated implementers and a
dev-mode gate that loudly warns it accepts simulated reports; the real GitHub
publisher stays disabled.





The LLM layer is provider-agnostic behind a trait (`LlmProvider::complete`); the supervisor/worker/gate logic never talks to a vendor SDK.
