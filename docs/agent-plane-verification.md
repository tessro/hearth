# Agent Plane — Verification Report

Status: **implemented and self-verified, including a live KVM guest**
(2026-07-18; gaps 2 and 4 closed live on the NixOS-module install 2026-07-21).
Companion to
`docs/agent-plane.md`. Records what the acceptance tests actually exercise, and
— honestly — what they cannot, plus how a future session could close each gap.

## What ships

New workspace crates:

- `hearth-agent-proto` — agent-plane protocol: version + port constants, hello
  / boot-report frames, the AG-UI event vocabulary (typed), the task/thread/run
  model, signed task refs (hand-rolled HMAC-SHA256 pinned to RFC 4231 vectors),
  url-safe base64 (RFC 4648 vectors), the CHV hybrid-vsock `CONNECT` handshake,
  and `SCM_RIGHTS` fd passing.
- `hearth-guestd` — the in-guest daemon: durable task registry (segmented event
  logs, incarnations, outbox, dedup), codex, claude, and Hermes adapters, the
  task-verb server (port 1027), boot report + heartbeat + upcall loops, and the
  dumb MCP stdio↔vsock shim. Its deployable artifact is always a static musl
  binary built via `make guest-bin`.
- `hearth-agentd` — the unprivileged host daemon: control socket, hearthd
  broker client (fd receipt), delegation ledger + signed refs, AG-UI HTTP + SSE,
  the MCP server, and durable outbox→ack→dedup wake-up delivery.
- `hearth-e2e` — the acceptance harness: boots hearthd + guestd(s) + agentd in
  one process, wired through the **real** hearthd socket broker, with fake
  `codex`/`claude`/`hermes` binaries speaking the pinned native protocols.

Changed: `hearth-proto` (new verbs, manifest `guestd` flag); `hearthd`
(per-VM hybrid vsock listeners replacing the `AF_VSOCK` listener, per-peer-UID
verb policy, socket broker, `wait`/`agent-endpoints` verbs, guest telemetry in
`status`, `agent` service flag); `hearthctl` (`agent` subcommands, boot-report
`wait`, `--agent` on create/spawn); vm-base (guestd install), image build
(stamps `guestd = true`), linter (guestd WARN), Makefile, ARCHITECTURE.md.

## Verified here (automated, on production code paths)

Run: `devenv shell -- make check` (259 tests, plus one opt-in web-SDK test) and
`cargo clippy --release --all-targets -- -D warnings` (clean).
Before invoking the top-level live binaries, run `cargo build --release --bin
hearth-agentd --bin hearthctl`: Cargo's test command refreshes test harnesses
under `target/release/deps`, but does not guarantee that the executable at
`target/release/hearth-agentd` was relinked.

The `hearth-e2e` crate exercises the whole stack in-process. Because CHV's
hybrid vsock backend makes every host-side channel a plain unix socket, and the
harness runs guestd in a unix-socket emulation that plays CHV's role byte-for-
byte, everything below runs on the same code that runs in production **except**
the VM boot itself and the real CLIs:

| Phase | Test file | What it proves |
|---|---|---|
| 0 | `phase0_transport_auth.rs` | Per-UID policy allows the agentd allowlist and denies `destroy`; `agent-endpoints` lists only agent VMs; guestd rejects a missing or version-skewed port-1027 hello. **The full broker path (hearthd binds/​connects, `SCM_RIGHTS`-passes the fd, agentd adopts it) is real.** |
| 1 | `phase1_readiness.rs` | `wait` resolves on the guestd boot report with no marker; `status` surfaces guestd version, agents, `last_seen`; unknown service errors cleanly instead of hanging. |
| 2 | `phase2_tasks.rs` | Start → stream (durable user/assistant text and tool-call AG-UI events) → complete; follow-up user turns survive replay; approval **interrupt → new run on the same thread** with both run outcomes recorded; cancel; **cursor staleness by incarnation**. |
| 3 | `phase3_agui_http.rs` | Real HTTP/SSE: task → interrupt → resume and completed-task follow-up via `forwardedProps.task_ref`; lossless replay to two independent UIs; bearer auth required end-to-end; CORS origin echoed. The opt-in `make agui-conformance` test drives the same endpoint with the pinned, unmodified TypeScript `HttpAgent`. |
| 4 | `phase4_delegation.rs` | Delegate over MCP; **crash agentd while the callee is `awaiting_input`**; restart; initiator woken **exactly once** (dedup by `delivery_id`); respond; collect result; grant + rejection both ledgered. |
| 5 | `phase5_claude.rs` | The claude adapter registered alongside codex; stream + complete; permission prompt → `awaiting_input` → resume, on the same engine and traces as codex. |
| 6 | `phase6_hermes.rs` | A Hermes-only guest advertises only Hermes; agentd selects its first healthy boot-reported adapter; ACP v1 message/tool/thought updates map to AG-UI and a deliberately slow `REASONING_MESSAGE_CONTENT` is durable before `RUN_FINISHED`; `session/new` receives the per-thread Hearth MCP shim; a wake-up uses `session/load`; a server-initiated ACP permission request becomes `awaiting_input` and `task.respond` answers it on the still-live prompt. |

Unit-level: HMAC/base64/taskref against published vectors; the segmented event
log's rotate/prune/truncation-marker and restart-survival; ledger replay across
reopen; fd passing (a passed listener still accepts); the hybrid handshake
leaves the stream byte-clean; the verb policy's allow/deny matrix.

## Verified live on NixOS + KVM

The following was exercised on a real NixOS host on 2026-07-15, beyond the
in-process acceptance harness:

- `hearthctl host check` passes every prerequisite, including `/dev/kvm` as a
  character device, `kvm`, `vhost_vsock`, the `hearth0` bridge, guest kernel,
  recovery keyring, and all required host commands.
- `hearth-guestd` was cross-compiled with the pinned musl toolchain via `make
  guest-bin`; `file` reports a statically linked executable, and the staged
  vm-base binary is byte-identical to the target artifact. The Cargo target
  configuration forces `crt-static` and the Makefile refuses to stage any
  binary with a dynamic interpreter.
- A root-ownership-preserving Buildah image was materialized and imported as
  `agent-plane-smoke-clean`. On a rootless NixOS host the working invocation is
  to run `hearthctl image build` *inside* `buildah unshare`, without passing
  hearthctl's `--rootless` flag. The explicit flag currently flattens root-owned
  guest files to the invoking uid and produces an invalid sudo installation;
  that path remains an open tooling bug.
- A real Cloud Hypervisor VM (`agent-plane-clean-vm`, vsock CID 101) booted the
  image. `/usr/bin/sudo` retained `root:root 4755`; `sudo -n true` succeeded;
  `hearth-guestd.service` was active; and `hearthctl wait
  agent-plane-clean-vm` returned `ready (boot report)` without a console marker.
- The real `AF_VSOCK` guest transport established the boot-report/heartbeat
  channel. `status` reports guestd `0.1.0`, `connected: true`, `ready: true`, and
  a fresh `last_seen`. This found and fixed an edge-triggered `AsyncFd` bug that
  cleared connect readiness before the first write and could hang forever.
- Restarting `hearthd` twice left both Cloud Hypervisor processes and guest
  boot IDs unchanged. Guest telemetry reconnected, readiness remained green,
  and both the long-running legacy VM and fresh agent VM report
  `boot_config: current`.
- A temporary `hearth-agentd --no-http` process used the production hearthd
  socket broker to discover the live agent VM and list its empty task set. A
  task against a disposable guest emitted durable `RUN_STARTED`, `RUN_ERROR`,
  and terminal failure events when `codex app-server` was absent, proving the
  real failure path is clean. No production agentd service was installed or
  enabled.
- The real Hermes install in the long-running `hermes-a` KVM guest reports
  `Hermes Agent v0.18.2`, upstream `2ea39dae` (with one locally carried commit).
  A quiet tool-source turn and resume established the presentation behavior,
  but the adapter no longer depends on it. No provider credential was read or
  moved from that VM.
- A newly materialized `hermes-agent-plane` image and disposable
  `hermes-agent-plane-vm` joined both halves on real KVM/AF_VSOCK. Its boot
  report is ready/connected and advertises exactly one healthy adapter:
  `hermes`, CLI `0.18.2 (upstream 2ea39dae)`. A temporary no-HTTP agentd
  discovered it through hearthd's FD broker and selected Hermes automatically.
  The first full task reached the real Hermes CLI and failed terminally because
  the disposable VM initially had no inference provider configured. The user
  subsequently completed portal/model setup in that disposable VM. Direct ACP
  v1 verification then completed `initialize` → `session/new` →
  `session/prompt`, streamed four `agent_message_chunk` updates spelling
  `HERMES_ACP_OK`, and returned `stopReason: end_turn`. No credential or Hermes
  configuration file was inspected or copied. The old quiet-CLI guestd adapter
  still failed a full task with `Hermes quiet output contained no session_id`;
  that live failure is why the presentation parser was removed in favor of ACP.
- The ACP image was rebuilt from the pinned full source commit
  `2ea39daeb1f675d72e5c21c9400f2d58d7e6d71a`; its build-time `hermes acp
  --check` passed. It was imported separately as `hermes-agent-plane-acp`
  (qcow2 SHA-256
  `d5d060ef765f5c8a97dabbb8752ba7ea003d89db8563bafe1663d9d931e3afc4`),
  leaving the long-running VM and original image untouched. That fresh install's
  version banner labels the pinned checkout as `local 2ea39dae` rather than
  `upstream 2ea39dae`; the adapter records this banner but gates on ACP v1 and
  the Hermes agent identity, not the release or source revision. The imported qcow2 predates
  that final probe compatibility fix and must be rebuilt before it is used as a
  fresh-image acceptance artifact.
- A rebuilt host agentd and the existing disposable guest completed the
  production broker path for `agent ls`: restricted hearthd request,
  `SCM_RIGHTS` connected-FD handoff, in-band `CONNECT 1027`, mandatory
  protocol-v1 hello/ack, and the standard `agent-ls` verb. The guest advertised
  only `hermes`. The static ACP guestd was then installed in that disposable
  guest and its service restarted, preserving the user's provider setup without
  reading or copying it. A full task through the same production path completed
  as task `01KXQ8HJN0NP7ADPJS74YGCF5S`, with summary `HEARTH_ACP_OK`, Hermes
  `stopReason: end_turn`, and a durable finished run. This proves the host
  broker, strict hello, guest task engine, official ACP session, and real model
  turn as one vertical path. Its 37-record event log preserved ACP thought and
  usage updates as `RAW`, emitted the AG-UI `TEXT_MESSAGE_START` / `CONTENT` /
  `END` sequence that reconstructs `HEARTH_ACP_OK`, and ended with
  `RUN_FINISHED` plus the terminal Hearth state. A second real task used
  Hermes's terminal tool to run the harmless `sudo -n true` probe and completed
  with `HEARTH_PERMISSION_OK`; Hermes classified that command as safe, so it
  did not produce a permission interrupt.
- A real Codex `0.144.4` app-server schema audit found that the existing Codex
  adapter's modeled `0.1.0` protocol is obsolete (`thread/start`, `turn/start`,
  and same-connection approval responses replace its fake method set). A real
  Claude `2.1.210` quiet run confirmed the broad stream envelope but not the
  adapter's `1.0.0` pin. Neither adapter is enabled in the Hermes image.

### Fixes produced by the live pass

1. Offline image linting now detects absolute systemd enablement symlinks
   without resolving them against the host root.
2. `SCM_RIGHTS` control-message lengths compile on both glibc and musl libc
   layouts.
3. AF_VSOCK connect readiness is retained until real I/O consumes it.
4. `guest-bin` enforces a genuinely static guest artifact on Nix.
5. `/dev/kvm` is checked as a character device instead of a regular file.
6. Boot-config drift comparison tolerates systemd's resolved executable path
   and flattened whitespace representation while still requiring Hearth's
   generated argument remainder to match exactly.
7. Adapter registration is now explicit, so an authenticated Hermes image can
   advertise Hermes alone instead of publishing a broken codex default.
8. agentd chooses the target's first healthy boot-reported adapter; a
   Hermes-only guest no longer receives an internal `agent = "codex"` request.
9. Image listing skips malformed legacy qcow2 entries, returns structured
   warnings for them, and continues to expose valid manifest-backed images.
10. `spawn` no longer performs a fleet-wide image-list preflight when no
    Dockerfile was requested; the named `create` operation is authoritative.
11. Port 1027 now requires the §5.3 hello. agentd sends it after the brokered
    hybrid-vsock handshake, validates guestd's protocol ack, and guestd rejects
    missing, unauthorized-component, or version-skewed first frames.
12. The Hermes adapter now drives pinned ACP v1 instead of parsing quiet terminal
    output, registers the Hearth MCP shim per native session, maps structured
    message/tool updates, and preserves the live process across permission input.
13. Live host instructions explicitly build runnable release binaries; release
    tests alone can leave a stale top-level daemon executable on disk.

## Verified live on the NixOS module (2026-07-21)

The host now runs Hearth from the flake's NixOS module (`services.hearth` +
`services.hearth.agentPlane`), with `hearth.service` and `hearth-agentd.service`
as nix-store units. This pass closed remaining gaps 2 and 4 and found one new
production bug.

Gap 2 — fresh-image rebuild, authentication, and follow-up (**closed**):

- The rebuilt image `hermes-agent-plane-v2` bakes Hermes **0.19.0** (banner
  `Hermes Agent v0.19.0 (2026.7.20) · upstream 693d3909 (ACP v1)`) and the tip
  guestd (`0.0.1+5fadb94`). The `demo` VM boots it directly; provider
  configuration arrives declaratively as a provisioned
  `/home/agent/.hermes/.env` (`--provision-file`), superseding the interactive
  setup the earlier pass deferred. No credential was read or copied.
- After a clean boot (fresh boot report, `boot_config: current`,
  `static_lease: true`), task `01KY41MWA04S9DS848A837NJA9` ran through the
  production path — hearthd FD broker, in-band `CONNECT 1027`, protocol-v1
  hello, real ACP session — and completed with summary `HEARTH_V2_OK`,
  `stopReason: end_turn`, and the full durable AG-UI sequence.
- A same-thread `followup` on the completed task started run
  `01KY41NGSH5F17NHERA1079B2X`, which answered `FOLLOWUP:HEARTH_V2_OK`;
  Hermes's `cachedReadTokens: 15040` shows the prior conversation was actually
  reloaded, not restarted. The image is self-contained.

Gap 4 — live systemd hardening + `LoadCredential` (**closed**, one proof
delegated):

- The module declares the `hearth-agent` user (uid 992, group `hearth`) and the
  hardened unit: `User=hearth-agent`, `Group=hearth`,
  `LoadCredential=` for `http-token` and `ref-key` from root-only `0400` files
  under `/run/secrets`, `RuntimeDirectory=hearth-agentd` `0750`,
  `ProtectSystem=strict`, `NoNewPrivileges`, `MemoryDenyWriteExecute`, and the
  rest of the checked-in hardening block.
- `LoadCredential` startup is proven by the running daemon itself: it serves
  the AG-UI endpoint and mints signed task refs, both of which require the two
  credentials, and has run for days across restarts.
- `systemd-analyze security hearth-agentd` scores **6.7 MEDIUM**; the flagged
  `UMask=` is the deliberate `0007` that makes the control socket
  group-usable. (`hearth.service` scores 9.6 as expected for a root VM
  manager; hardening it is out of scope here.)
- `/run/hearth-agentd/agent.sock` is `0660 hearth-agent:hearth`, and a
  `hearth`-group operator (uid 1000) drove the entire `hearthctl agent`
  surface over it. The AG-UI HTTP endpoint returns 401 without or with a wrong
  bearer token.
- hearthd's audit log shows uid 992 exercising exactly the allowlisted
  broker/discovery verbs (`agent-endpoints`, `guest-connect`), and
  `/etc/hearth/verb-policy.toml` carries the explicit `hearth-agent` rule that
  omits every machine life-cycle verb. The denial itself is proven live too:
  `sudo -u hearth-agent hearthctl destroy no-such-vm` returned
  `verb.denied: peer is not authorized for verb destroy`, and hearthd's audit
  log recorded `caller_uid=992 verb=destroy allowed=false` — rejected at the
  policy gate before any target lookup.

New production bug found (fixed in-tree, deploy pending):

- **`RuntimeDirectory=hearth` wiped CHV's bound vsock sockets on every daemon
  restart.** systemd removes the runtime directory when the unit stops
  (`RuntimeDirectoryPreserve` defaults to `no`), but the Cloud Hypervisor
  processes outlive the daemon and cannot re-bind
  `/run/hearth/vsock/<id>.sock`. After a hearthd restart, guest→host channels
  (boot report, heartbeat, upcalls) recover — hearthd re-binds those listeners
  — while every **host→guest** `guest-connect` fails with `ENOENT` until the
  VM itself restarts. So `status` looked healthy while `agent ls` showed no
  adapters and every task start would have failed: exactly the half-broken
  state the earlier restart test (pre-module, no `RuntimeDirectory`) could not
  see. Fixed with `RuntimeDirectoryPreserve=yes` in both the packaged unit and
  the NixOS module; restarting the `demo` VM confirmed the diagnosis
  end-to-end. Three smaller fixes fell out of the same investigation:
  `hearthctl agent ls` rendered the NAME column from a nonexistent `name`
  field (agentd sends `hostname`); agentd's `list_agents` swallowed relay
  errors silently (now logged at WARN); and hearthd's wire errors dropped the
  underlying cause (`err.to_string()` → `{err:#}`), which had reduced the
  `guest-connect` failure to a context line with no errno.

### Gap 3 closed: live approvals from the web console (2026-07-21)

The `web/` operator console (official AG-UI `HttpAgent` over agentd's HTTP
leg, reverse-proxied with a bearer token) drove every approval scenario
against real Hermes 0.19.0 in `demo`:

- **Hermes decides most commands itself by default.** Its shipped default is
  `approvals.mode: smart`: a dangerous-pattern match goes to an auxiliary LLM
  ("Smart approval"), and the ACP client only sees a permission request on a
  smart DENY override. Live, `sudo touch /etc/...` and even `rm -rf` of a
  scratch directory were auto-approved (the latter after the auxiliary
  provider timed out twice and fell back). For deterministic owner prompts the
  `demo` VM's `~/.hermes/config.yaml` now sets `approvals.mode: manual` —
  worth considering as the baked default for agent-plane images, since the
  whole point of the Hearth approval loop is a human answering.
- **Found and fixed: the permission interrupt violated AG-UI event ordering**
  (high). Real Hermes raises `session/request_permission` while the gated
  tool call is still open. The adapter closed open message/reasoning streams
  before parking, but not open tool calls, so the run ended with an active
  `TOOL_CALL_START` and the official `HttpAgent` hard-errored
  (`Cannot send 'RUN_FINISHED' while tool calls are still active`) — the UI
  never even rendered the Allow/Deny controls. The in-process E2E missed it
  because fake_hermes completed its tool call before asking. Fixed
  (`Translation::close_tool_calls` on both the park and turn-end paths);
  fake_hermes now opens a tool call across the permission request and phase 6
  asserts no run end leaves a tool call open. Validated live: post-fix
  interrupts stream cleanly and the console renders and answers them.
  A pre-fix task's durable log still replays the invalid sequence; replay
  through the events API is unaffected — only live strict-client runs were.
- **Approve** — Allow in the console answered the parked ACP request on the
  still-live process; the command executed and the task completed with the
  report (task `01KY44N93DMQ`, 3 runs).
- **Deny** — Deny produced a blocked tool result (exit −1), Hermes explained
  the refusal without retrying, and the task completed (`01KY45678JDZ`).
- **Expiry (60s)** — Hermes's `approvals.timeout` self-denies the callback
  after 60 seconds (`Permission request timed out`) and continues its turn,
  while the parked adapter keeps the Hearth task in `awaiting_input`
  indefinitely — no run is lost or failed. The defined transition for a late
  answer: it cannot resolve the dead request, so it becomes an ordinary
  consent turn on the reloaded session, and Hermes re-raises a **fresh**
  permission request instead of executing on stale authority. Offline
  approvals therefore remain answerable forever at the Hearth layer; only the
  in-guest execution window is 60 seconds per ask.
- **guestd restart** — with an outstanding permission, restarting
  `hearth-guestd` (twice, including a binary upgrade via `hearthctl upgrade`)
  preserved `awaiting_input` durably. The parked ACP process is necessarily
  lost; a subsequent answer follows the same late-answer path above
  (`session/load`, consent turn, fresh permission request) and the task
  completed normally after a second Allow.

### Gap 5: live snapshot/restore incarnation rotation (2026-07-21)

Exercised on the live host against `demo`, after a baseline task
(`GAP5_BASELINE`, cursor at `<incarnation>.15`) and a deliberate
post-snapshot mutation (`GAP5_MUTATED`, seq 44):

- `hearthctl snapshot demo` completes in ~1.2s live: the daemon now pauses,
  snapshots, and resumes, with the resume guaranteed even when the snapshot
  fails. Getting there surfaced three real CHV 52 API contract bugs, each
  invisible until a live call: `vm.snapshot` refuses a running VM (hearthd
  never paused); CHV's bare action endpoints return HTTP 400 when the request
  carries any body — even `{}` — so `vm.pause`/`vm.resume` are now body-less
  PUTs (`vm.shutdown` tolerates a body; nothing else changed); and hearthd's
  HTTP client half-closed before reading, which CHV punishes on slow endpoints
  by dropping the response entirely (`Connection: close` is ignored) — the
  client now reads one Content-Length-framed response instead.
- `hearthctl restore demo` required `--kernel` in the relaunch argv (CHV
  demands a boot payload argument even under `--restore`) and then resumed
  the snapshot with running vCPUs.
- **The rotation contract holds end to end**: the pre-restore cursor is
  rejected with `cursor.stale: cursor predates a snapshot restore; re-sync
  via task.status`; `task.status` re-syncs (state, runs, and events intact)
  under a freshly rotated incarnation; and a cursor-less replay returns the
  full log under the new incarnation.
- **Honest limitation**: the resumed memory image met a rootfs that had
  advanced past the snapshot and wedged (CHV `Running`, guest dead on
  net/vsock). The pending-restore mark survived, so the rotation fired on the
  clean recovery boot — proving the wiring, not the resume. Disk-consistent
  capture is item 5 in the remaining gaps.
- Found alongside: `scripts/dev-restart.sh` wrote a drop-in whose
  `Environment=PATH` replaced the unit's PATH with FHS-only locations, which
  on NixOS left the dev daemon unable to spawn `ip`/`nft`/`systemctl` — it
  then judged every running VM inactive and tried to boot duplicates. The
  drop-in now appends `/run/current-system/sw/bin` and sets
  `RuntimeDirectoryPreserve=yes`, and a subsequent dev restart was observed
  leaving both running VMs' agent channels intact — the preserve fix
  validated live.

## Remaining gaps — and how to close each

These are genuine gaps, not hand-waves. Each says what is unproven and what
access would let a future session prove it.

1. **Codex and Claude are intentionally inactive.** The real-binary audit has
   now shown concrete skew rather than a hypothetical gap: the Codex adapter
   must be rewritten for the current app-server JSON-RPC schema and must retain
   the connection while answering server approval requests; Claude needs a
   deliberate pin/schema pass at `2.1.210` or whichever version is chosen.
   Authentication must then be provisioned for those CLIs. None of this blocks
   the Hermes-only vertical.

2. **Fresh-image rebuild, authentication, and follow-up.** **Closed
   2026-07-21** — see "Verified live on the NixOS module": the rebuilt
   `hermes-agent-plane-v2` (Hermes 0.19.0 ACP) booted self-contained with a
   provisioned `~/.hermes/.env`, completed a real task and a same-thread
   follow-up through the production broker path. The older
   `hermes-agent-plane`/`-acp` images are superseded. `hermes-a` remains
   running and unmodified.

3. **Hermes ACP approval expiry/restart.** **Closed 2026-07-21** — all four
   scenarios (approve, deny, 60-second expiry, guestd restart) were exercised
   live from the web console against the real Hermes 0.19.0 in the `demo` VM;
   see "Verified live on the NixOS module" for the full findings, including
   the AG-UI open-tool-call protocol bug this surfaced (fixed) and the defined
   expiry semantics: the Hearth task waits in `awaiting_input` indefinitely, a
   late answer becomes a consent turn on the reloaded session, and Hermes
   re-raises a fresh permission request rather than executing on stale
   authority.

4. **Live systemd hardening + `LoadCredential`.** The checked-in unit no longer
   shares hearthd's runtime directory: it owns `/run/hearth-agentd`, runs with
   primary group `hearth`, and creates its control socket as
   `0660 hearth-agent:hearth`. The install path also supplies an explicit
   user-matched verb policy, which takes priority over the broad `hearth` group
   rule and limits agentd to broker/discovery verbs. Portable and NixOS setup
   examples are in `docs/operations.md`.
   **Closed 2026-07-21** — see "Verified live on the NixOS module": the module
   declares the user and credentials, `LoadCredential` startup and broker
   access are proven on the running daemon, `systemd-analyze security` scores
   6.7 MEDIUM, a `hearth` operator drove the agent socket, and a live
   `destroy` attempt as the `hearth-agent` uid was denied at the policy gate
   (`verb.denied`, audited `allowed=false`). Nothing remains open here.

5. **VM snapshot/restore incarnation rotation, end to end.** **Closed
   2026-07-21** for the rotation contract — see "Gap 5: live snapshot/restore"
   below: a real CHV snapshot of a running agent VM, a post-snapshot task
   mutation, and the machine-plane `restore` produced `cursor.stale` on the
   pre-restore cursor and a clean `task.status` re-sync under a rotated
   incarnation. Three CHV API contract bugs were fixed to get there.
   *Remaining (new, narrower):* CHV's `vm.snapshot` captures memory/device
   state only, so resuming it against a rootfs that advanced after the
   snapshot wedged the guest (dead net/vsock; rotation completed on the
   recovery boot, which still carried the pending-restore mark).
   *Implemented 2026-07-23 against CHV ≥ 53 (pinned in Nix):* true memory
   resume. The journey mattered: a v52 memory restore via CLI `--restore`
   came back catatonic even with a byte-identical disk (`NO-CARRIER` tap,
   dead vsock, empty console, no CHV error anywhere — the CLI path swallows
   restore failures), and an interim cold-boot restore worked but broke
   transparency. The v52/v53 release audit then showed the platform had
   moved: v52 restores the KVM clock before resuming vCPUs and resets vsock
   connections across restore; v53 advances the guest clock by the elapsed
   wall-clock gap, re-syncs vCPU TSC offsets, and announces the restored
   guest on the network (GARP) instead of waiting out stale ARP. The final
   design: `snapshot` pauses, dumps memory/device state via `vm.snapshot`,
   captures the boot disk (`disk.qcow2`, reflink), and resumes — one ~1s
   quiesced window; `restore` copies the disk back, launches a **bare
   API-only VMM** (no clap payload requirement), and drives
   `vm.restore {source_url, resume:true}` over HTTP so contract errors
   surface through hearthd's error chain, with an explicit `vm.resume`
   fallback if the VM comes back paused. Snapshots missing their disk or
   state are refused (`snapshot.no_disk` / `snapshot.no_state`) before the
   running VM is touched; CHV's own version-binding of snapshots surfaces as
   the `vm.restore` error. A restored VM's transient unit runs the bare-VMM
   argv, so `boot_config` reports stale until its next plain reboot — that
   drift flag is truthful. If live verification still shows an unattached
   tap, the contingency is `net_fds` + `SCM_RIGHTS` fd-passing on the restore
   request. Also from the audit: `--disk` now declares `image_type=qcow2`
   explicitly (v52 deprecates autodetection), and v52's `backing_files=on`
   is noted as a future thin-disk opportunity. *Still to verify live under
   v53:* snapshot → mutate → restore with a healthy, reachable guest, the
   mutation rewound, and cursors rotated.

6. **Inter-guest bridge isolation.** Explicitly a non-goal of the proposal (§8,
   §14); no code claims to solve it and nothing here depends on it. Listed only
   so the boundary stays honest: guests can still reach each other over
   `hearth0` at the IP layer; the agent plane simply never uses that path.

## Host-environment issues observed during the live pass

- `cargo test --release` rebuilt release-mode test executables but left the
  top-level `target/release/hearth-agentd` stale. The strict guest correctly
  rejected its first `task.start` frame with `protocol.hello_required`.
  Explicit `cargo build --release --bin hearth-agentd --bin hearthctl` relinked
  the live executables; after restarting agentd, adapter discovery and the real
  task passed.
- The earlier locked dev shell omitted the musl Rust target and fell back to a
  garbage-collectable nixpkgs cross compiler that later failed to realize.
  `rust-toolchain.toml` now pins the Rust toolchain and musl standard library;
  `devenv.nix` supplies the musl linker without modifying user-global `rustup`
  state.

- `/etc/dnsmasq.d/hearth` is absent on this host. Hearth therefore cannot write
  its requested static lease drop-in and the live agent VM uses dnsmasq's
  dynamic lease instead. Connectivity works, but the declarative NixOS network
  integration should create/wire the drop-in before static addressing can be
  claimed. *Resolved by the NixOS module (2026-07-21): the module wires
  `HEARTH_DNSMASQ_DROPIN_DIR`, and the rebooted `demo` VM reports
  `static_lease: true`.*
- A legacy image at `/var/lib/hearth/images/debian-13-generic-amd64` has no
  `.hearth.toml`. It blocked the installed daemon's entire `image ls` and the
  old composite `spawn` preflight. Source now skips invalid entries with a
  structured warning, while `spawn` avoids listing altogether when it was
  given an already-imported image. The latter path created the disposable
  Hermes VM successfully against the still-installed older daemon. No legacy
  disk or image was deleted.
- The generated `example/vm-base/hearth-guestd` is a build-context artifact and
  is intentionally not source-controlled.

## Adversarial review pass

After the acceptance tests were green, a 4-dimension, 18-agent adversarial
review (engine concurrency, wake-up durability, the socket broker,
refs/HTTP/MCP) ran over the implementation. It confirmed **14 correctness bugs
the acceptance tests did not cover** — 13 now fixed (with regression tests
where the path is reachable in-process), 1 documented as a spec-conformant
design choice. The most serious two would have broken production outright.

Wake-up path (would silently break real delegation):

1. **Real delegation never populated `initiator_thread`** (high). The MCP shim's
   hello carried the calling agent's `thread_id`, but the MCP server discarded
   it, and the `delegate` tool only read an explicit (never-supplied)
   `initiator_thread` arg. Every real delegation thus recorded
   `initiator_thread: None`, and the completion wake-up was acked-and-dropped by
   the ledger's no-thread branch. The phase-4 test only passed because it passed
   the arg manually. **Fixed:** the `delegate` tool defaults `initiator_thread`
   to the shim's hello thread. The phase-4 test now delegates with *no* explicit
   arg, exercising the production path.
2. **Wake-ups were acked before the injection was durable** (high). `inject.turn`
   recorded the dedup id and enqueued the turn into an in-memory queue, then
   returned success (agentd acked, deleting the outbox entry) — a guestd crash
   before the run persisted anything lost the wake-up, and the dedup burned the
   retry. **Fixed:** a durable `inbox/` (fsync + atomic rename) persists the
   injection before the ack; `recover()` replays any inbox entry whose run did
   not durably start; `run_one` releases the entry once `RUN_STARTED` is logged.
   Regression: `store.rs::inbox_persists_pending_injections_across_reopen`.

Cancel / run lifecycle:

3. **Cancel didn't stop in-flight/queued work** (high) — a canceled task could
   resurrect `canceled → running → completed` with a second contradictory outbox
   delivery, and a `gc` race could corrupt the task dir. **Fixed:** `cancel`
   clears the queue; `run_one` refuses a `canceled` task; `set_terminal` + the
   event loop refuse to overwrite/append past a terminal state; `gc` skips a
   still-driving cell. Regression:
   `phase2_tasks.rs::cancel_is_terminal_even_against_an_in_flight_run`.
4. **A `run_one` error left followers hung and dropped the failure wake-up**
   (medium). **Fixed:** the `drive` error branch now finalizes through
   `set_terminal` (publish + durable `failed` delivery) like the adapter-error
   path.
5. **A double `task.respond` started two runs** (low). **Fixed:** `respond`
   reserves the state to `queued` under the lock before enqueuing, so the loser
   is rejected. Regression:
   `phase2_tasks.rs::a_second_respond_is_rejected_no_duplicate_run`.

Durability / ordering:

6. **Ledger grant written *after* `task.start`** (medium), inverting §7.1 —
   a fast task's first upcall could hit `no_grant` and be dropped. **Fixed:**
   agentd mints the task_id, ledgers the grant, *then* pins that id into
   `task.start`; a failed start revokes the grant.
7. **Ledger/outbox not fsynced** (low) — grants and pending wake-ups were not
   durable against host power loss. **Fixed:** `sync_all` on the ledger append
   and a `write_sync` (fsync-before-rename) for the outbox and inbox.
8. **A stuck delivery starved newer ones** (medium) — `deliver_outbox` broke on
   the first nack. **Fixed:** it continues past a non-ackable entry so one
   permanently-stuck wake-up never blocks the rest.
9. **Single-segment retention lost the truncation marker** (low, production
   uses 64 segments so unreachable there). **Fixed:** write-before-prune, and
   `max_segments` clamped to ≥ 1. Regression:
   `store.rs::single_segment_retention_marks_truncation`.

Security / identity:

10. **Wake-up delivery didn't check `grant.target == callee_vm`** (medium) — a
    VM that learned another delegation's task_id could inject attacker-chosen
    text into that delegation's initiator. **Fixed:** `deliver()` rejects a
    target mismatch (audited, not acked).
11. **`destroy` leaked agent-plane identity** (high) — the brokered
    `<vm>.sock_1026` was never unlinked, so a service recreated under the same
    name (even without `agent = true`) inherited the dead VM's agent-plane
    socket. **Fixed:** `drop_guest_channels` unlinks `_1026` too.
12. **The per-UID policy ignored supplementary groups** (medium) — a
    `usermod -aG hearth` operator was denied every verb. **Fixed:** `allows`
    resolves the peer's full group membership via `getgrouplist`. (This
    surfaced as two unit-test failures on the dev box precisely because the
    operator *is* in the `hearth` group — the fix now honors that.)
13. **(duplicate of #1)** — the refs/MCP reviewer independently flagged the same
    `initiator_thread` defect; fixed by #1.

Documented, not changed:

14. **AG-UI `threadId` continuity requires `forwardedProps.task_ref`** (low). A
    reused `threadId` with no `task_ref` creates a new task. This matches the
    spec (§4.2): agentd is content-stateless (D4), so resume is via the
    `task_ref` the endpoint hands back in the `hearth.task_ref` `CUSTOM` event,
    not via a server-held `threadId → task` map. Pure `threadId` continuity
    would reintroduce exactly the per-task host state D4 forbids; supporting it
    is a deliberate non-feature. Noted here so the boundary is explicit.

## Deliberate scope calls

- **No backward compatibility** (per the task directive): the `AF_VSOCK`
  listener was deleted outright rather than kept behind a flag; `wait --marker`
  became optional rather than dual-moded with a compatibility shim.
- **Example-image retirement deferred.** vm-base now installs guestd, but the
  `hermes-vm`/`agent-vm` example probes (`hermes-probe`, `netdiag`) and their
  acceptance scripts are left in place — removing them would break
  `scripts/test-hermes-vm.sh`, which needs a real VM to run anyway. The boot
  report supersedes them functionally; the cosmetic cleanup is a follow-up.
- **Concurrency: one active run per thread** (§14 open question) — day-1 stance,
  enforced by the engine's per-thread turn queue.
