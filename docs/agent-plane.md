# Hearth Agent Plane — Design Proposal

Status: **proposed, revised** (2026-07-14). Builds on `ARCHITECTURE.md`
(machine plane, unchanged) and `REFACTOR_PROPOSAL.md` (cited by workaround
number). Nothing here ships until Phase 1 of §11 is accepted.

### Revisions (2026-07-14, after external review)

1. **Task/thread/run split.** The first draft conflated the durable delegated
   goal, the CLI conversation, and one AG-UI execution interval. They are now
   three ids (§3.1); runs are terminal per AG-UI's interrupt lifecycle, and
   answering `awaiting_input` starts a *new* run on the same thread instead of
   reopening the old one. The HTTP run endpoint is now genuinely
   AG-UI-conformant, not "AG-UI-flavored" (§4.2).
2. **Signed task references + delegation ledger.** Bare ULIDs were routable
   only by scanning guests and forgeable by an adversarial callee. Task refs
   are now HMAC-signed and self-routing; wake-up authority and rejections live
   in a small host-side ledger. agentd is content-stateless, not
   authorization-stateless (§4.4). Wake-up authority is never reconstructed
   from callee-controlled data.
3. **Durable wake-up delivery.** Upcall → inject is now outbox → ack → dedup:
   at-least-once delivery with idempotent injection, surviving agentd restarts
   and initiator downtime (§7.2).
4. **Security claims now match enforcement.** hearthd gains a per-peer-UID
   verb policy (verified absent today — peer creds are audit-only in
   `lib.rs`); the group-writable vsock directory is replaced by an FD-passing
   socket broker; the "no guest↔guest path" claim is scoped to the truth
   (shared `hearth0` bridge reachability exists; filtering is future work).
5. **MCP framing fixed.** The shim is a dumb pipe over stdio JSON-RPC framing
   end-to-end; Streamable HTTP is gone from the guest leg (a pipe cannot
   translate HTTP session negotiation and SSE).
6. **Corrections.** Codex adapter targets `codex app-server` (not `proto`);
   guest-reported IPs are telemetry, not routing truth (hearthd already
   resolves addresses from dnsmasq leases — workaround #7 is fixed);
   segmented event logs; cursor incarnations invalidated on snapshot restore;
   secrets via systemd `LoadCredential=`. Sequencing now does one vertical
   adapter (codex) before the second CLI.

## Context

Hearth boots VMs whose workload is an autonomous coding agent (`claude`,
`codex`, `hermes`). Today the only ways to interact with those agents are SSH
into the VM and run the TUI, or grep the serial console. There is no way to:

- start an agent task programmatically and watch its progress from outside,
- reattach to a running task after a disconnect,
- answer an agent's permission prompt without an interactive terminal,
- let one agent delegate a task to another and collect the result.

Meanwhile readiness is still `wait --marker` string-matching on the serial
console (workaround #12), and `ARCHITECTURE.md` already predicts the fix
("`hearthctl exec` … likely belongs in a per-guest agent, not hearthd").

This proposal adds that per-guest agent and the host-side plane above it:

- **`hearth-guestd`** — one daemon inside every agent VM. Machine-plane duties
  (boot report, readiness, health telemetry) plus agent-plane duties (drive
  the agent CLIs, own the durable *task registry*, deliver wake-ups).
- **`hearth-agentd`** — one unprivileged daemon on the host. Terminates AG-UI
  over HTTP for UIs, serves one MCP server for agent-to-agent delegation,
  relays everything to guestds over vsock, enforces delegation policy, audits.
- **`hearthd`** — unchanged in role: machine plane only. Gains a per-peer verb
  policy, a discovery verb, and a socket broker; learns nothing about tasks.

## Decisions at a glance

| # | Decision | Short reason (long form in §13) |
|---|---|---|
| D1 | Agent plane is a separate daemon, not HTTP inside hearthd | hearthd is root and drives `systemd-run`/disk provisioning; an internet-adjacent SSE server does not belong in that process. Charter stays intact: hearthd remains unix-socket line-JSON only. |
| D2 | AG-UI **events** end-to-end (adapter → UI, one vocabulary); AG-UI **protocol** conformance exactly at the HTTP run endpoint | One event schema everywhere; an unmodified AG-UI `HttpAgent` works against §4.2. Everything else (task API, cursors) is honestly named a Hearth extension. |
| D3 | Task ≠ thread ≠ run: durable goal / CLI session / one terminal AG-UI interval | Interrupts end runs (AG-UI lifecycle); tasks span them; threads map to CLI sessions. Collapsing these was the first draft's central modeling error. |
| D4 | Task *content* (events, results) lives in the guest; delegation *authority* lives in a host ledger + signed refs | Content survives agentd restarts and travels with snapshots. Authority must not be reconstructible from callee-controlled data. Content-stateless ≠ authorization-stateless. |
| D5 | No A2A on any internal wire; task model kept **isomorphic** to A2A's | Both wire ends are ours; A2A would add JSON-RPC, agent cards, and spec churn without capability. Isomorphism keeps a future façade mechanical (§13.2). |
| D6 | Agent-to-agent delegation = MCP tools against agentd; the calling agent occupies the "user" seat of the callee's task | Delegation has session shape (post task, watch, answer input-required, collect result). The CLIs consume MCP natively; zero glue. |
| D7 | No push protocol to agents; wake-ups are **session-resume injection** with outbox/ack/dedup durability | An LLM agent's entire inbox is tool results and new turns; between turns nobody listens on any socket. Delivery must therefore be queued and idempotent, not fire-and-forget. |
| D8 | All guest channels ride CHV's **hybrid vsock** unix sockets; identity = socket path; agent plane never uses the IP network | Per-VM identity for free; works for an unprivileged agentd via an FD-passing broker; keeps every cross-agent byte host-mediated and audited. (Inter-guest *IP* reachability on `hearth0` still exists today — see §8.) |
| D9 | Streaming to LLM consumers is cursor-based polling + long-poll, not SSE | Models can't consume mid-turn streams; cursors give replay, backpressure-freedom, and caller-controlled context spend. SSE exists only on the human leg and as display-only MCP progress garnish. |

## Goals

- Start, observe, answer, and cancel agent tasks from outside the VM — human
  or program — with reattach-after-disconnect that loses no events.
- Agent-to-agent delegation with per-pair policy, durable wake-up delivery,
  and a complete audit trail, without granting guests any new network paths.
- Kill workaround #12 (readiness-by-serial-marker) with a first-class
  in-guest reporter.
- Keep hearthd's charter untouched and the new host surface unprivileged —
  with the privilege boundaries actually enforced, not asserted.

## Non-goals

- A web UI. This defines the protocol surface a UI would consume; the UI
  itself is a separate project.
- A2A, MCP-tasks, or any interop façade on day 1 (§13.2 keeps the door open;
  MCP tasks are still spec-experimental with thin client support).
- Per-caller capability models beyond the delegation allowlist and the new
  per-peer verb policy. Same "day 1" stance as `ARCHITECTURE.md`.
- Multi-host anything.
- Intra-guest process isolation. The VM is the trust unit; any process inside
  a guest can reach that guest's guestd. (House rule: incurious about what
  VMs run.)
- Inter-guest bridge filtering. The agent plane doesn't depend on it, but see
  §8 and §14 — the current shared-bridge reachability is acknowledged, not
  solved, here.

## 1. Topology

```
        human UI (AG-UI HttpAgent)      hearthctl agent …
             │ AG-UI: HTTP POST + SSE        │ line-JSON
             │ (token auth, tailnet bind)    │
             ▼                               ▼
       ┌──────────────────────────────────────────┐
       │ hearth-agentd  (unprivileged, hardened)  │  restricted verb channel  ┌─────────┐
       │  AG-UI run endpoint · task API · MCP     │──unix line-JSON──────────▶│ hearthd │
       │  server · delegation ledger · audit      │  (per-UID verb policy,    │ (root)  │
       └───────┬──────────────────────────▲───────┘   discovery + FD broker)  └────┬────┘
               │ hybrid vsock             │ hybrid vsock                           │ machine plane:
               │ host→guest:              │ guest→host:                            │ boot report /
               │ <vm>.sock CONNECT 1027   │ <vm>.sock_1026                         │ readiness on
               ▼ (task verbs, attach,     │ (MCP frames, upcalls)                  │ <vm>.sock_1025
   ┌───────────────────────────┐          │                                        │
   │ VM                        │          │              ┌─────────────────────────┘
   │  hearth-guestd ───────────┼──────────┴──────────────┤
   │   │  task registry (disk) │                         ▼
   │   │  outbox · dedup ·     │            (all VM sockets brokered by hearthd;
   │   │  turn queue           │             caller identity = socket path)
   │   ├── codex adapter       │
   │   ├── claude adapter      │
   │   └── hermes adapter      │
   │  agent CLI processes      │
   └───────────────────────────┘
```

Two planes, one guest daemon:

| Plane | Owner | Traffic | Persisted state |
|---|---|---|---|
| Machine | hearthd | lifecycle verbs, boot report, readiness, heartbeat | registry (`/etc/hearth`), disks |
| Agent | hearth-agentd | tasks, events, input, delegation, wake-ups | delegation ledger only (§4.4) — never task content |

## 2. hearth-guestd

One static binary (musl-linked so it drops into any Dockerfile image
regardless of distro libc), installed in `vm-base` at
`/usr/local/bin/hearth-guestd` with `hearth-guestd.service` wanted by
`multi-user.target`. Images built on the new vm-base therefore carry it for
free; images that don't are still first-class citizens under the §2.5
compatibility contract (the linter warns, it does not fail).

### 2.1 Machine-plane duties (Phase 1, valuable alone)

On boot and on change, guestd connects out to `CID 2 port 1025` (lands on
`<vm>.sock_1025`, bound by hearthd) and reports:

```json
{"hello": {"proto": 1, "component": "guestd", "version": "0.1.0"},
 "report": {"ready": true, "addrs": ["192.168.122.31/24"],
            "hostname": "web-a", "agents": ["codex"], "boot_id": "…",
            "restored": false}}
```

- `hearthctl wait <name>` blocks on this report instead of a serial-log
  marker (kills workaround #12; marker mode stays as a fallback for
  guestd-less images).
- Reported addresses are **corroborating telemetry only**. hearthd already
  resolves routing-truth addresses from dnsmasq leases with static-reservation
  fallback (`resolved_address`, `crates/hearthd/src/lib.rs`); a divergence
  between lease and report is surfaced in `status` as a warning, and the
  lease always wins.
- `status` additionally shows guestd/adapter versions (surfaces skew, §6 of
  `REFACTOR_PROPOSAL.md`) and heartbeat `last_seen`.
- The `restored` flag is set when hearthd told guestd (via this channel's
  handshake) that the boot follows a `restore` — used for cursor
  invalidation (§3.4).

Replaces, in agent images: `hermes-probe` and the
`hearth-proxy.{socket,service}` socat pair (guestd serves the same in-guest
`/run/hearth.sock` forward itself, for the agent-in-charge only). `netdiag`
retires when the telemetry above ships.

### 2.2 Agent adapters

One adapter per CLI, translating its native stream into the AG-UI event
vocabulary (§5.1) and mapping the triad (§3.1) onto its native concepts:

Ordinary message, reasoning, tool, and RAW events are appended to the durable
task log as the native stream emits them, while the adapter run is still in
progress. Approval and terminal transitions remain ordered after those live
events. Attach and SSE consumers therefore render work immediately through the
same cursor/replay path used after completion.

| Adapter | Drive | Native ↔ triad mapping | Approvals |
|---|---|---|---|
| codex (**first**, Phase 2) | `codex app-server`: JSONL/stdio; threads, turns, streamed items, server-initiated approvals; version-matched JSON schemas | thread ↔ `thread_id`, turn ↔ `run`, streamed items → events | server-initiated approval requests → task `awaiting_input` |
| claude (second, Phase 5) | headless `-p` with `stream-json` in/out, resumable sessions | session ↔ `thread_id`, one `-p` invocation ↔ `run` | permission prompts via its MCP permission-prompt hook → `awaiting_input` |
| hermes (Phase 6) | pinned `hermes acp`: ACP v1 JSON-RPC/stdio; `session/new`/`load`/`prompt`, streamed `session/update`, server-initiated `session/request_permission` | ACP session ↔ `thread_id`, prompt ↔ `run`, message/tool updates → AG-UI events; the per-session ACP MCP server launches the §2.4 shim with Hearth's thread id | permission request → task `awaiting_input`; guestd parks the live ACP process and `task.respond` answers the exact JSON-RPC request |

Codex was implemented first because app-server has generatable schemas. The
current deployable path is Hermes ACP v1 at version `0.18.2`, source commit
`2ea39dae`; presentation-oriented `hermes chat -q` output is deliberately not
an adapter contract. Adapters couple to native protocols, which drift. Rule
(workaround #13's lesson): agent CLIs are **pinned by version, protocol, and
source revision in the image**, the manifest records adapter compatibility,
and guestd refuses (loudly, at boot report) to adapt a CLI version it doesn't
know. An image registers only adapters for CLIs it actually configures; the
Hermes image therefore does not advertise codex or claude.

### 2.3 Turn queue and wake-up injection

Per thread, guestd serializes turns: you cannot inject into a running turn,
so wake-ups (peer task completed / needs input / failed) queue until the
current turn ends, then enter the thread as a new user-role turn via the
CLI's resume mechanism. Injection is idempotent per `delivery_id` (§7.2) and
provenance-framed (§7.3).

### 2.4 MCP shim

`hearth-guestd mcp --thread <thread_id>` — a stdio subcommand the CLIs launch
as a local MCP server. It is a dumb frame pipe in the literal sense: MCP's
stdio JSON-RPC framing flows unmodified from the CLI's stdio, over one vsock
connection (port 1026, hello frame `channel: "mcp"`), to agentd's MCP server,
which speaks the same framing natively. No HTTP anywhere on this path
(§13.7).

guestd launches every CLI session itself, so it templates the session's
`thread_id` into the shim invocation in the CLI's MCP config; the shim's
hello carries it. A guest can therefore only mislabel *its own* threads
(VM identity comes from the socket, §6) — worst case it misroutes wake-ups
between its own sessions.

### 2.5 Guestd-less VMs: the compatibility contract

The agent plane is strictly additive. A VM whose image predates guestd (or
deliberately omits it) keeps working exactly as today, indefinitely:

- **Lifecycle, networking, publish, snapshots, SSH, serial logs** — untouched;
  none of them ever depended on guestd. Every VM already gets a vsock device
  (`append_vsock` in `crates/hearthd/src/host.rs` is unconditional), so
  hearthd binds its per-VM listeners uniformly; for guestd-less guests they
  are simply never connected to, which costs nothing.
- **Readiness is declared, not guessed.** The image manifest gains
  `guestd = true` (automatic for images built on the new vm-base). Only then
  does `hearthctl wait` expect a boot report; otherwise it requires
  `--marker` exactly as today. No timeouts are inflicted on old images, and
  `wait` never has to guess which signal it is waiting for.
- **Health**: heartbeat expectations apply only to guestd-declaring services.
  For the rest, `status` shows the agent-plane columns (guestd version,
  `last_seen`, agents) as absent — absent, not unhealthy.
- **Invisible to the agent plane**: `agent-endpoints` and `list_agents`
  return only services with `agent = true`, and setting that flag at
  `create`/`spawn` requires a guestd-declaring image. `hearthctl agent run`
  against anything else fails with `agent.not_enabled`; a delegation
  targeting one is rejected, ledgered, and audited like any policy denial.
- **Agent-in-charge continuity**: port 1024 keeps its wire contract. An
  existing VM carrying the `hearth-proxy` socat units works against the
  Phase 0 hybrid listener with no image change (better than today, where the
  `AF_VSOCK` listener bug means it does not work at all). guestd replaces
  socat in new images; it does not obsolete old ones.

Install and upgrade paths:

1. **Rebuild + respawn** (hearth-native): rebuild the image on the new
   vm-base — guestd arrives for free — then `destroy`/`spawn`. Right whenever
   VM-local state is disposable, which is the design assumption everywhere
   else in Hearth.
2. **Update an existing guestd over SSH**: `hearthctl upgrade [name]` copies the
   packaged static binary through the logged-in operator's SSH agent, replaces
   `/usr/local/bin/hearth-guestd` atomically, and restarts the existing unit.
   It verifies the new boot report and rolls back the binary on failure. The
   fleet form skips stopped, guestd-less, unreachable, and active-task VMs;
   `--force` overrides only the active-task guard. This deliberately does not
   install or change a unit, retrofit old images, or mutate manifests.

Linter policy is a gradient, mirroring `min_kernel_contract`: the linter
**warns** on a missing guestd (old vm-base) rather than failing the build,
and the hard requirement lives where it is actually needed — `create`/`spawn`
with `agent = true` fails against an image that does not declare guestd.

## 3. The task registry

Owned by guestd, on the guest disk. Terminology (the triad):

### 3.1 Task, thread, run

```
task_id    durable goal spanning interruptions; the unit of delegation,
           state, and event history. What A2A calls a task.
thread_id  conversation; maps 1:1 to a CLI-native session/thread.
           A task binds to exactly one thread; a thread may serve
           successive tasks.
run_id     one AG-UI execution interval within a task. Always terminal:
           finished | error | interrupted. Never reopened.
```

`awaiting_input` is a **task** state; reaching it *ends the current run*
(AG-UI's interrupt lifecycle: the stream closes, `RUN_FINISHED` after a
`CUSTOM hearth.permission_request` carrying the prompt). Answering
(`task.respond`) starts a **new run on the same thread** whose input carries
the resume payload. This is what lets an ordinary AG-UI client drive the
whole loop (§4.2).

### 3.2 Task states (A2A-isomorphic by construction)

| hearth state | A2A equivalent | entered when |
|---|---|---|
| `queued` | `submitted` | accepted, waiting for the thread's turn queue |
| `running` | `working` | a run is active |
| `awaiting_input` | `input-required` | agent asked something; current run ended `interrupted` |
| `completed` | `completed` | terminal, result recorded |
| `failed` | `failed` | terminal, error recorded (incl. `guestd_restart`, §9) |
| `canceled` | `canceled` | terminal, by `task.cancel` |

Delegation **rejection** (A2A `rejected`) is deliberately *not* a task state:
it happens at agentd before any guest task exists, so it is recorded in the
delegation ledger and audit log only (§4.4). The A2A façade (§13.2) maps it
from the ledger.

Isomorphism is a design constraint, not decoration: state names, transition
semantics, and event payload shapes stay mappable 1:1 onto A2A task
status/artifact updates so a façade is translation, not redesign.

### 3.3 Storage

```
/var/lib/hearth-guestd/
  tasks/<task_id>/meta.toml      # thread_id, agent, initiator (advisory copy),
                                 # state, run history [{run_id, outcome, t…}],
                                 # timestamps, result summary, incarnation
  tasks/<task_id>/events/        # segmented log: 000001.jsonl, 000002.jsonl …
  tasks.index                    # id → state, for cheap listing
  outbox/                        # §7.2 pending deliveries
  inbox-dedup/                   # §7.2 seen delivery_ids
```

- `task_id`, `run_id`, `delivery_id` are ULIDs.
- Events carry `(seq, run_id, ag-ui event)`; `seq` is monotonic per task
  across runs.
- The event log is **segmented** because a single append-only file cannot
  cheaply drop its head: segments rotate at a fixed size, retention deletes
  whole oldest segments, and a truncation-marker event at the oldest
  surviving head lets a stale cursor detect the gap instead of silently
  skipping. (SQLite/WAL is the fallback if segment bookkeeping grows hairy;
  line-JSON segments match house debuggability.)
- Retention: terminal tasks pruned by count/age (configurable); `task.gc`.
- Because all of this is guest disk: agentd restarts lose nothing, and a VM
  snapshot/restore carries task history with the VM.

### 3.4 Cursors and incarnations

A cursor is `(task_id, incarnation, seq)`. The incarnation is a ULID stored
in `meta.toml`, rotated when hearthd signals (via the §2.1 handshake) that
the boot follows a `restore` — the one event that can silently rewind `seq`
and re-issue the same numbers for different events. A cursor with a stale
incarnation gets error `cursor.stale`; the client re-syncs via `task.status`
and replays from a fresh cursor. Restore is hearth-mediated, so no heuristic
rollback detection is needed.

### 3.5 Task verbs (guestd's host-facing API, vsock port 1027)

`task.start` (task text, agent, initiator meta, detach) · `task.status` ·
`task.events` (cursor, filter, max) · `task.attach` (replay-from-cursor then
follow; streaming responses framed like `logs --follow`) · `task.respond`
(resume payload → new run) · `task.followup` (ordinary user turn on a settled
task's existing thread) · `task.cancel` · `task.list` · `task.gc` ·
`inject.turn` (wake-up delivery, agentd-only, idempotent per `delivery_id`).
Every channel opens with the §5.3 hello.

## 4. hearth-agentd

Host daemon, own crate, own unit. Runs as `hearth-agent` (dedicated user),
hardened: `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`,
`PrivateTmp`, writable paths limited to `/var/lib/hearth-agentd`. Secrets
(HTTP bearer token, task-ref HMAC key) arrive via systemd `LoadCredential=`,
not file-permission gymnastics.

### 4.1 Local control socket

`/run/hearth/agent.sock` (`0660 root:hearth`), line-JSON, same
`Request`/`Response`/streaming framing as hearthd. This is what
`hearthctl agent …` speaks. Verbs mirror §3.5 plus `agent-ls`.

### 4.2 AG-UI over HTTP (human/UI leg)

- `POST /v1/agents/{name}/agui` — **standard AG-UI**: accepts `RunAgentInput`
  (`threadId`, `runId`, messages, `forwardedProps`), streams `BaseEvent`s
  (SSE). A fresh `threadId` creates a task (and thread); a resume after
  `awaiting_input` is a new run on the same thread carrying the resume
  payload, per AG-UI's interrupt lifecycle. For a completed or failed task,
  the same ref starts an ordinary follow-up run on its existing thread.
  `forwardedProps.task_ref` attaches the run to an existing task explicitly.
  An unmodified AG-UI `HttpAgent` works against this endpoint — that is the
  conformance bar, and Phase 3's acceptance test.
- **Hearth task API** (extensions, not AG-UI, honestly namespaced):
  `GET /v1/agents` · `GET /v1/tasks` · `GET /v1/tasks/{task_ref}` ·
  `GET /v1/tasks/{task_ref}/events?cursor=…` (SSE: exact replay from cursor,
  then follow) · `POST /v1/tasks/{task_ref}/cancel`. There is no
  `POST /input` — input is always a new AG-UI run.
- Bind: loopback or a tailnet address, from config; never `0.0.0.0`
  silently. Bearer token required (via `LoadCredential=`). This surface
  drives code-executing agents — it is RCE-by-design and is treated like an
  SSH key. Browser UIs get strict CORS (explicit origin allowlist).

### 4.3 MCP server

One MCP server, speaking stdio JSON-RPC framing directly over brokered vsock
connections from guest shims (§2.4). Tools in §7.1. No Streamable HTTP
transport day 1 (nothing consumes it; a host-local HTTP exposure can be
added later without touching guests).

### 4.4 Signed task refs and the delegation ledger

agentd is **content-stateless**: it never persists events, results, or task
content, and restart costs nothing but reconnects. It is deliberately **not
authorization-stateless**:

- **Task refs.** Every externally visible task handle is an opaque signed
  reference: `{v, target_service, task_id, initiator, initiator_thread,
  expiry}` + HMAC (key via `LoadCredential=`; current + previous key accepted
  to allow rotation). Refs are self-routing — agentd resolves the target VM
  from the ref without scanning guests — and unforgeable: a guest cannot
  mint, retarget, or claim another VM's task. Guests and UIs treat refs as
  opaque tokens.
- **Delegation ledger.** Append-only records at
  `/var/lib/hearth-agentd/ledger/`: delegation granted (initiator,
  initiator_thread, target, task_id, timestamp), delegation **rejected**
  (policy denials — the A2A `rejected` state lives here, §3.2), and
  cancellations/revocations. The ledger is authoritative for **wake-up
  authority**: when a callee upcalls a state change, the initiator to wake is
  looked up here — never reconstructed from callee-controlled `meta.toml`
  (the guest's copy of initiator metadata is advisory/debugging only).
  Refs route; the ledger authorizes. Both exist because refs alone cannot
  express revocation without painful expiry tuning, and the ledger alone
  would put a host lookup on every routine read.

Audit: every task start, input, cancel, delegation attempt (granted or
rejected), and wake-up delivery → journald structured fields
(`HEARTH_INITIATOR`, `HEARTH_AGENT`, `HEARTH_TASK`, `HEARTH_DELIVERY`, verb,
result, duration), mirroring hearthd's audit contract. Token-level event
*content* is never audited — it lives in the guest event log.

Relay behavior: agentd holds one attach per interested task, fans out to N
SSE clients, and keeps a small in-memory tail purely as an optimization —
the guest log is the source of truth for replay.

## 5. Protocols

### 5.1 AG-UI as the event vocabulary

Adapters emit, and every layer above transports **unmodified**, the AG-UI
event set: `RUN_STARTED/FINISHED/ERROR`, `STEP_STARTED/FINISHED`,
`TEXT_MESSAGE_START/CONTENT/END`, `TOOL_CALL_START/ARGS/END/RESULT`,
`STATE_SNAPSHOT/DELTA`, `MESSAGES_SNAPSHOT`, `RAW`, `CUSTOM`. Types are
hand-rolled in a new `hearth-agent-proto` crate (the ecosystem SDKs are
TS/Python; the vocabulary is small and stable enough to own).

At the start of every run, guestd records the textual user input as a
`TEXT_MESSAGE_START/CONTENT/END` sequence with role `user`, before adapter
output. Consequently the guest event log reconstructs follow-up turns as well
as assistant/tool/reasoning output; the UI does not rely on optimistic browser
state for conversation history.

Initial conformance subset: run lifecycle, text, tool calls, `CUSTOM`.
Hearth-specific moments ride namespaced `CUSTOM` events:
`hearth.permission_request` (payload: the approval prompt; immediately
precedes the run ending `interrupted`), `hearth.session_name` (the agent
replaced the thread's display name), `hearth.state` (task state transitions),
`hearth.truncation`.

Representative mapping (adapters own the details; pinned per CLI version):

| CLI stream item | AG-UI |
|---|---|
| assistant text delta | `TEXT_MESSAGE_CONTENT` |
| tool/exec begin, args, end, output | `TOOL_CALL_START/ARGS/END/RESULT` |
| approval / permission request | `CUSTOM hearth.permission_request`, then `RUN_FINISHED` (run `interrupted`, task → `awaiting_input`) |
| turn complete, final result | `RUN_FINISHED` (+ result summary into `meta.toml`) |
| CLI-specific extras (token counts, …) | `RAW` |

### 5.2 What is *not* on the wire, and why

- **A2A**: both endpoints of every internal hop are Hearth components
  versioned together; a stranger-interop protocol there buys schema and churn,
  not capability. The task model stays isomorphic (§3.2) so an A2A façade on
  agentd is mechanical the week a foreign agent actually appears.
- **In-band streaming to LLM callers**: MCP tool calls return exactly one
  result to the model; MCP progress notifications and A2A SSE both die at the
  harness — the model sees final tool results and new turns, nothing else.
  This is the load-bearing constraint (D7/D9): *the last mile into an LLM
  agent is turns and tool calls*. Hence cursors + long-poll + turn injection,
  and MCP progress used only as display garnish (§7.2).

### 5.3 Version handshake

Every channel (vsock both directions, agent.sock, HTTP via
`X-Hearth-Proto`) opens with `{proto, component, version}` — the vsock
guest→host hello additionally selects `channel: "mcp" | "upcall"` on
port 1026 and carries `thread_id` for MCP (§2.4). `hearth-agent-proto`
carries `AGENT_PROTOCOL_VERSION` and publishes verb/tool lists the way
`hearth-proto` does for hearthd, so version skew is a clean error (the
workaround #9 rule, applied from day 1).

## 6. Vsock transport, identity, and the socket broker

CHV's vsock device is the Firecracker-style **hybrid** model — there is no
host-side `AF_VSOCK` at all:

- **Host → guest**: connect to `/run/hearth/vsock/<vm>.sock`, send
  `CONNECT <port>\n`, await `OK …\n`, then raw stream. Guestd listens on
  in-guest `AF_VSOCK` port 1027 (guest kernel: `CONFIG_VIRTIO_VSOCKETS=y`).
- **Guest → host**: guest connects to CID 2 port P; CHV hands it to whoever
  is listening on the host unix socket `/run/hearth/vsock/<vm>.sock_P`.

Port map (constants in `hearth-agent-proto`):

| Port | Direction | Host endpoint | Purpose |
|---|---|---|---|
| 1024 | guest → host | `<vm>.sock_1024`, bound by **hearthd** | hearthd verb channel (agent-in-charge only — existing contract) |
| 1025 | guest → host | `<vm>.sock_1025`, bound by **hearthd** | boot report / readiness / heartbeat / restore signal |
| 1026 | guest → host | `<vm>.sock_1026`, bound by **hearthd, FD-passed to agentd** | MCP frames + guestd upcalls (hello selects channel) |
| 1027 | host → guest | guestd in-guest listener | task verbs, attach streams, `inject.turn` |

**Identity is the socket path.** Whichever VM's socket a connection arrives
on *is* the caller — VMM-attested, no tokens between host and guest. The
delegation allowlist (§7) keys on it directly.

**Socket broker, not a shared directory.** `/run/hearth/vsock/` stays
root-owned `0750`. agentd never opens paths in it. Instead, over its
restricted hearthd channel it requests:

- `guest-listener {vm, port}` → hearthd binds `<vm>.sock_<port>` (validating
  the port is agent-plane) and passes the listening FD via `SCM_RIGHTS`;
- `guest-connect {vm}` → hearthd connects `<vm>.sock` and passes the
  connected FD; agentd performs the `CONNECT 1027` handshake in-band.

This closes the hole a group-writable (`2770`) directory would open — a
group member can unlink or squat root-owned socket names there — and gives
hearthd a natural enforcement point (which VM, which port, audited). A
sticky-bit directory with strict ownership checks is the documented fallback
if FD passing proves awkward, but the broker is the design.

> **Migration note / existing bug.** `crates/hearthd/src/vsock.rs` binds a
> host-side `AF_VSOCK` listener (`VMADDR_CID_ANY` + peer-CID filter). That is
> the vhost-vsock model; with CHV's `--vsock cid=…,socket=…` hybrid backend,
> guest-initiated connections land on `<vm>.sock_<port>` and the `AF_VSOCK`
> listener never sees them — the in-guest `hearth-proxy` socat unit
> (`VSOCK-CONNECT:2:1024`) currently has no working peer. Port 1024 must move
> to a `<vm>.sock_1024` unix listener as part of Phase 0, independent of
> everything else here.

## 7. Agent-to-agent delegation

The calling agent occupies the user seat of the callee's task. No third
protocol: delegation is MCP tools (out) + task registry (state) + durable
turn injection (back in).

### 7.1 MCP tool surface (served by agentd)

| Tool | Behavior |
|---|---|
| `set_session_name(name)` | replace the calling thread's display name in its own guestd and emit durable `CUSTOM hearth.session_name`; the shim-supplied thread id is authoritative |
| `list_agents()` | agent-enabled VMs, their adapters, current task counts |
| `delegate(agent, task, wait_seconds=0)` | policy check → ledger write → `task.start` on callee's guestd → returns `{task_ref, state}`; with `wait_seconds`, long-polls first and may return terminal state + result in one call. Denial returns (and ledgers) a rejection. |
| `wait_for(task_ref, timeout_seconds)` | long-poll until state change / input-required / terminal; returns state, summary of new events, next cursor. The streaming workhorse. |
| `task_events(task_ref, cursor, filter, max_events)` | paged log read; filters like `assistant_text`, `tool_summaries` so callers spend context deliberately |
| `task_respond(task_ref, response)` | answer `awaiting_input` (starts a new run on the callee) |
| `task_status(task_ref)` / `task_cancel(task_ref)` | as named (cancel also revokes in the ledger) |

All refs are the §4.4 signed kind; agentd verifies signature, expiry, and
that the presenting VM is the ref's `initiator` (or a UI bearing the HTTP
token) before routing.

### 7.2 Durable wake-ups, not fire-and-forget

Short sub-task: `delegate(..., wait_seconds=120)` — one tool call, one
result. Long task: `delegate` detached, caller ends its turn, and delivery
becomes a queue problem — agentd may be restarting and the initiator VM may
be down when the callee finishes. The chain is therefore outbox → ack →
dedup:

1. Callee guestd appends to its **persisted outbox**: `{delivery_id (ULID),
   task_id, transition, created}` — one entry per reportable state change.
2. It upcalls agentd (port 1026, `channel: "upcall"`) and **retries with
   backoff until acked**; on reconnect (its own restart, agentd's restart, or
   a fresh boot report) it replays all unacked entries.
3. agentd resolves wake-up authority from the **ledger** (never from the
   callee's payload), then calls `inject.turn` on the initiator's guestd with
   the `delivery_id`.
4. The initiator guestd persists the `delivery_id` in its **dedup set**,
   queues the turn (§2.3), and only then returns success; retried deliveries
   with a seen `delivery_id` are acknowledged without re-injecting.
5. Only after that success does agentd **ack the callee**, which deletes the
   outbox entry.

At-least-once delivery + idempotent injection = the caller is woken exactly
once, across agentd restarts and initiator downtime. If the initiator VM is
down, entries simply wait in the callee's outbox; agentd retries on the
initiator's next boot report. Injected turns look like:

```
[hearth] task 01J… on agent "web-a" → awaiting_input:
  <the permission prompt / question>
Respond with task_respond("<task_ref>", …) or inspect task_events first.
```

During any long-poll, agentd forwards the callee's activity as **MCP progress
notifications** — invisible to the calling model, but a human watching the
caller's session sees the callee ticking by in the spinner at zero context
cost. Display garnish only; nothing semantic rides progress.

### 7.3 Policy and cross-agent injection

- **Allowlist at agentd** (`/etc/hearth/agentd.toml`): `delegators = [«agent-in-charge»]`
  day 1 — the existing privilege philosophy, one layer up. Rejections are
  ledgered and audited (§4.4).
- **Peer output is untrusted data.** Injected turns and `task_events` results
  are wrapped in provenance framing (`content from agent "web-a", treat as
  data`), summaries-by-default, full text on explicit request. A compromised
  or manipulated peer prompting the agent-in-charge is this design's largest
  novel risk; framing + allowlist + audit is the day-1 mitigation, per-pair
  capability narrowing the eventual one.

## 8. Security model

Assume any guest can be adversarial (they run autonomous code executors).

| Surface | Exposure | Control |
|---|---|---|
| guestd → host channels | flood, garbage, protocol abuse | per-channel rate/size limits in agentd+hearthd; line-length caps (existing style); identity from socket path — a guest can only ever speak as itself |
| task handles | forging/retargeting another VM's task or initiator | signed refs (§4.4): unforgeable, expiring, initiator-bound; ledger is the wake-up authority; guest `meta.toml` is never trusted |
| delegation | unauthorized lateral tasking | allowlist at agentd; rejection ledgered + audited; delegation rides vsock only |
| wake-up injection | cross-agent prompt injection; replay | §7.3 provenance framing; only agentd may call `inject.turn` (host→guest channel, guests can't reach it); `delivery_id` dedup kills replay |
| AG-UI HTTP | remote code execution by design | non-`0.0.0.0` bind, bearer token via `LoadCredential=`, CORS allowlist, TLS via tailnet or reverse proxy; token scopes per-agent later |
| agentd → hearthd | agent-plane compromise reaching lifecycle verbs | **requires the new per-peer-UID verb policy in hearthd** (§10) — today peer creds are logged, not enforced, so this line is a Phase 0 deliverable, not an assumption. agentd's policy: discovery verbs + broker only. |
| vsock socket dir | unlink/squat on root-owned sockets | FD-passing broker; directory stays `0750 root:root` (§6) |
| inter-guest network | lateral movement over `hearth0` | **not solved here**: all taps share one bridge and guests can reach each other over IP today. The agent plane never uses that path, and nothing in this design trusts it — but real east-west isolation needs bridge filtering (nftables bridge family / per-tap rules), tracked in §14. The claim is "delegation adds no new guest↔guest path," not "no path exists." |
| agentd process | compromise of the relay | unprivileged user; brokered sockets only; per-UID verb policy at hearthd; cannot touch disks, images, or `systemd-run` |
| task data at rest | reading another VM's history | guest disk only, per-VM; host never aggregates content |

## 9. Failure modes

| Failure | Detection | Recovery |
|---|---|---|
| agentd crash/restart | systemd | content-stateless: re-resolve from hearthd, re-broker listeners; ledger persists authority; guestds replay unacked outboxes; clients reattach with cursors; zero event loss (guest log is truth) |
| guestd crash mid-run | exit noticed via in-guest systemd; heartbeat gap on host | in-flight turn is lost: run → `error`, task → `failed` (`reason=guestd_restart`, marker event); thread itself is resumable, caller may start a follow-up task on the same thread; outbox/dedup are on disk, so pending wake-ups survive |
| hearthd restart | existing reconcile | agent plane loses its broker/discovery channel briefly; agentd retries; tasks unaffected |
| VM snapshot/restore | hearthd knows it performed `restore`; signals guestd (§2.1) | task history travels with the disk; `running` at snapshot time restores as `failed(interrupted)` unless the adapter proves the CLI process survived (memory snapshots may preserve it — verify in Phase 2 acceptance); **incarnation rotates, outstanding cursors go `cursor.stale`** (§3.4) |
| UI disconnect mid-run | SSE drop | task unaffected; reattach with cursor, exact replay |
| initiator VM down at wake-up time | inject.turn fails | outbox entry waits in callee; agentd retries on initiator's next boot report (§7.2) |
| duplicate wake-up delivery | retry after partial failure | initiator dedup set by `delivery_id`; ack-after-inject ordering (§7.2) |
| CLI version drift breaks an adapter | guestd refuses at boot report | visible in `hearthctl status`; image rebuild with pinned CLI |
| Guest floods event log | segment caps | segment rotation + truncation marker (§3.3) |

SSH recovery access (mandatory since e4880b5) remains the rescue path when
the agent plane itself is wedged.

## 10. Changes to existing components

**hearthd** (machine-plane only; it never learns what a task is):

1. **Per-peer-UID verb policy.** Today `peer_credentials()` feeds audit
   fields only; dispatch is unauthorized. Add a config-driven map
   `uid/gid → allowed verbs`, default-allow for root/`hearth` group
   (compatibility), and a minimal set for `hearth-agent`: `ping`, `version`,
   `ls`, `status`, `agent-endpoints`, `guest-listener`, `guest-connect`.
   (A separate read-only discovery socket is the acceptable alternative;
   the policy keeps one socket and matches "policy at the daemon".)
2. **Socket broker verbs** (`guest-listener`, `guest-connect`) with
   `SCM_RIGHTS` FD passing and port/VM validation (§6).
3. **Hybrid vsock migration**: bind `_1024`/`_1025` listeners; delete the
   `AF_VSOCK` listener (§6 migration note — independent bug).
4. `agent-endpoints` discovery verb; `agent = true` flag in service TOML;
   `wait`/`status` consume boot reports (readiness + telemetry; lease-based
   address resolution stays authoritative); restore signal to guestd.

**hearthctl**: `hearthctl agent ls|run|ps|status|events|respond|cancel|attach`
against `/run/hearth/agent.sock`, human + `--json` as usual.

**vm-base / linter**: install guestd + unit in vm-base; linter warns on
absence, with the hard requirement enforced at `agent = true`
`create`/`spawn` (§2.5); manifest gains `guestd = true`; retire
`hearth-proxy.{socket,service}`, `netdiag`, `hermes-probe` from example
images (existing images keep working under §2.5).

**Workspace**: new crates `hearth-agent-proto`, `hearth-agentd`,
`hearth-guestd` (musl target for guestd); `make install` gains the agentd
unit (opt-in) with `LoadCredential=` wiring; image tooling gains guestd.

**ARCHITECTURE.md amendments**: non-goal "no web UI/REST/anything beyond the
unix-socket JSON protocol" is restated as *scoped to hearthd*, with the agent
plane called out as the sanctioned HTTP surface; topology diagram gains
agentd/guestd; "Host ↔ guest channel" section rewritten for hybrid vsock;
authorization section gains the per-peer verb policy.

## 11. Sequencing

Each phase lands alone and is useful alone. One vertical adapter (codex)
proves the whole stack before the second CLI is added.

1. **Phase 0 — transport and authorization truth.** `hearth-agent-proto`
   (hello, port constants); hybrid-vsock listener/dialer utilities; migrate
   port 1024 off `AF_VSOCK` (fixes the §6 bug); per-peer-UID verb policy;
   broker verbs. Acceptance: agent-in-charge `hearthctl ls` works over vsock
   against a real CHV guest; a `hearth-agent`-uid client can run exactly the
   allowlisted verbs and nothing else.
2. **Phase 1 — guestd, machine plane.** Boot report / readiness / heartbeat /
   telemetry; vm-base + linter; retire serial-marker readiness. Acceptance:
   `spawn` → `wait` with no marker; `status` shows corroborated addresses +
   versions + last_seen.
3. **Phase 2 — tasks, one vertical adapter (codex app-server).** Task
   registry (segmented logs, incarnations) + codex adapter + task verbs;
   minimal agentd (unix socket, signed refs, ledger) + `hearthctl agent`.
   Acceptance: start, tail, answer a real approval via interrupt→new-run,
   cancel; kill -9 agentd mid-task and reattach with zero event loss;
   snapshot→restore rotates incarnation and stales cursors.
4. **Phase 3 — AG-UI HTTP.** Conformant run endpoint + task API + auth via
   `LoadCredential=` + CORS. Acceptance: an **unmodified AG-UI `HttpAgent`**
   drives task → interrupt → resume; SSE detach/reattach losslessly; two UIs,
   one task; auth required end-to-end.
5. **Phase 4 — delegation.** MCP server + stdio-framing shim, allowlist,
   outbox/ack/dedup wake-ups, `inject.turn`, progress garnish. Acceptance:
   agent-in-charge delegates, ends its turn, callee hits `awaiting_input`
   **while agentd is stopped**, agentd restarts, initiator is woken exactly
   once, responds, collects the result — full ledger + audit trail; a
   non-allowlisted VM's `delegate` is rejected, ledgered, audited.
6. **Phase 5–6 — additional adapters and beyond.** claude adapter, then pinned
   Hermes ACP v1. Hermes acceptance covers a Hermes-only guest, automatic
   healthy-adapter selection, streamed message/tool events, per-session Hearth
   MCP registration, completion, a wake-up that loads the same native session,
   and a permission request answered through a second Hearth run. A2A façade,
   MCP-tasks façade, per-pair delegation capabilities, token scopes, and bridge
   east-west filtering follow.

## 12. Paper traces

The flows that must survive every layer. (a)–(e) are Phase 2–4 acceptance
tests verbatim.

- **(a) Token streaming**: codex streamed item → adapter →
  `TEXT_MESSAGE_CONTENT` seq=n → segment append → agentd attach → SSE → UI.
  One schema end-to-end; no translation after the adapter.
- **(b) Tool call render**: exec begin/args/end/output →
  `TOOL_CALL_START/ARGS/END/RESULT` with stable `tool_call_id` — UI renders
  incrementally from the same four events for all CLIs.
- **(c) Interrupt → resume (the triad in motion)**: callee CLI raises an
  approval → adapter emits `CUSTOM hearth.permission_request` then
  `RUN_FINISHED` (run `interrupted`, task `awaiting_input`) → outbox entry →
  upcall → ledger lookup → `inject.turn` (dedup, ack) → caller wakes,
  `task_events` for context, `task_respond` → **new run, same thread** →
  callee resumes → task `running`. Human case identical with the AG-UI
  endpoint: interrupt ends the SSE run; the answer is a new `RunAgentInput`
  on the same `threadId`.
- **(d) Disconnect/reattach**: UI drops at seq 141 → task continues → log
  grows → reattach `cursor=(task, inc, 141)` → replay 142… → follow. Same
  path agents use via `wait_for`'s returned cursor.
- **(e) Restart matrix**: agentd restart (ledger + outbox replay make it
  invisible; cursors unaffected); hearthd restart (brief broker blip);
  guestd restart (honest `failed(guestd_restart)`, never a silent hang;
  outbox survives); VM restore (incarnation rotates, cursors stale cleanly);
  host reboot (terminal tasks persist, in-flight marked, VMs reconcile,
  unacked wake-ups replay).

## 13. Alternatives considered

1. **HTTP inside hearthd.** Rejected: root daemon, blast radius, charter
   (D1). The split also keeps agent-plane churn away from the component that
   can destroy VMs.
2. **A2A as the internal wire (hearthd↔guest or agentd↔guest).** Rejected
   for now: every feature it would contribute (task states, resubscribe,
   push webhooks) is either rebuilt here stronger (cursors + incarnations
   vs. resubscribe's thin replay guarantees; outbox/dedup vs. webhooks that
   still can't reach a model between turns) or lands on the wrong side of
   the harness wall. Kept: full state-model isomorphism (§3.2) so agentd can
   grow an A2A server/client façade without touching guests. Trigger to
   revisit: the first real foreign agent, either direction.
3. **AG-UI without a task registry.** Rejected: AG-UI is a live view with
   client-held thread state; it has no durable task lifecycle, no reattach
   guarantee, nothing for an async agent caller to poll. The registry is the
   durability layer both consumer types share.
4. **Reopenable runs (`POST /input` on a live run).** Rejected on review:
   AG-UI's interrupt model makes runs terminal and resumes on the thread;
   a reopenable-run surface would be AG-UI-flavored but incompatible with
   ordinary clients. Tasks span runs instead (D3).
5. **Bare ULIDs as task handles.** Rejected on review: unroutable without
   scanning guests after an agentd restart, and forgeable by an adversarial
   callee. Signed refs + ledger (§4.4).
6. **guestd endpoints on the bridge network.** Rejected: N authenticated
   network services reachable from adversarial neighbors, lateral-movement
   surface, and cross-agent traffic invisible to the daemon audit. Hybrid
   vsock gives attested identity and forces host mediation.
7. **Per-guest MCP servers / Streamable HTTP behind the shim.** Rejected:
   N protocol implementations, N places to enforce policy — and a dumb pipe
   cannot translate stdio framing into HTTP session negotiation + SSE, so
   the first draft's hybrid was internally contradictory. One agentd server
   speaking stdio JSON-RPC framing over vsock; shims stay dumb pipes.
8. **Group-writable vsock directory (`2770`).** Rejected on review: group
   members can unlink/squat root-owned socket names. FD-passing broker (§6).
9. **MCP-tasks / elicitation instead of the task registry.** Premature: the
   tasks capability is spec-experimental and CLI client support is thin.
   Same treatment as A2A: isomorphic semantics now, façade when the clients
   are ready.

## 14. Open questions

- **Concurrency per agent**: day 1 is one active task per agent (others
  `queued`). Parallel tasks = parallel threads in one guest — allowed
  eventually? Resource story inside the VM?
- **Bridge east-west filtering**: guests currently reach each other over
  `hearth0`. Per-tap nftables isolation with explicit opt-in flows is the
  natural shape — needed before "compromised guest" claims can extend to the
  network plane. Owner: machine plane, not this proposal.
- **Ledger + outbox retention**: prune ledgered delegations when their tasks
  reach terminal states + N days? Dedup-set retention must exceed maximum
  outbox retry horizon — pick the constants.
- **Ref key rotation cadence** and whether refs need per-verb scoping
  (read-only refs for observers vs. respond-capable refs for initiators).
- **Hermes ACP interruption lifetime**: an outstanding permission request is
  process-local and the pinned Hermes callback currently waits 60 seconds.
  Hearth preserves that process across `awaiting_input`; verify expiry and
  guestd-restart behavior explicitly before promising offline/indefinite
  approval prompts.
- **Result artifacts**: tasks produce files (diffs, reports). First-class
  artifact events (A2A has them; we'd mirror) or leave it to the guest FS +
  SSH/`provision`-style copy-out?
- **Event schema evolution**: `hearth-agent-proto` versioning discipline once
  a UI ships — additive-only within a proto version?
- **agentd → hearthd surface**: does the agent-in-charge's lifecycle
  privilege ever route *through* agentd, or stay on its dedicated 1024
  channel? (Proposal: stays on 1024; agentd never proxies lifecycle.)
- **Backpressure on attach streams**: slow SSE client policy — drop to
  cursor-and-reconnect after N buffered events?
