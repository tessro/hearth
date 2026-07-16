# Agent Plane — Verification Report

Status: **implemented and self-verified, including a live KVM guest**
(2026-07-15). Companion to
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
  logs, incarnations, outbox, dedup), the codex and claude adapters, the
  task-verb server (port 1027), boot report + heartbeat + upcall loops, and the
  dumb MCP stdio↔vsock shim. Targets glibc by default and musl via
  `make guest-bin-musl`.
- `hearth-agentd` — the unprivileged host daemon: control socket, hearthd
  broker client (fd receipt), delegation ledger + signed refs, AG-UI HTTP + SSE,
  the MCP server, and durable outbox→ack→dedup wake-up delivery.
- `hearth-e2e` — the acceptance harness: boots hearthd + guestd(s) + agentd in
  one process, wired through the **real** hearthd socket broker, with fake
  `codex`/`claude` binaries speaking the pinned CLI contracts.

Changed: `hearth-proto` (new verbs, manifest `guestd` flag); `hearthd`
(per-VM hybrid vsock listeners replacing the `AF_VSOCK` listener, per-peer-UID
verb policy, socket broker, `wait`/`agent-endpoints` verbs, guest telemetry in
`status`, `agent` service flag); `hearthctl` (`agent` subcommands, boot-report
`wait`, `--agent` on create/spawn); vm-base (guestd install), image build
(stamps `guestd = true`), linter (guestd WARN), Makefile, ARCHITECTURE.md.

## Verified here (automated, on production code paths)

Run: `cargo test --release` (225 tests) and
`cargo clippy --release --all-targets -- -D warnings` (clean).

The `hearth-e2e` crate exercises the whole stack in-process. Because CHV's
hybrid vsock backend makes every host-side channel a plain unix socket, and the
harness runs guestd in a unix-socket emulation that plays CHV's role byte-for-
byte, everything below runs on the same code that runs in production **except**
the VM boot itself and the real CLIs:

| Phase | Test file | What it proves |
|---|---|---|
| 0 | `phase0_transport_auth.rs` | Agent-in-charge verb channel over the hybrid `<vm>.sock_1024`; broker verbs refused on the guest channel; per-UID policy allows the allowlist and denies `destroy`; `agent-endpoints` lists only agent VMs. **The full broker path (hearthd binds/​connects, `SCM_RIGHTS`-passes the fd, agentd adopts it) is real.** |
| 1 | `phase1_readiness.rs` | `wait` resolves on the guestd boot report with no marker; `status` surfaces guestd version, agents, `last_seen`; unknown service errors cleanly instead of hanging. |
| 2 | `phase2_tasks.rs` | Start → stream (RUN/text/tool-call AG-UI events) → complete; approval **interrupt → new run on the same thread** with both run outcomes recorded; cancel; **cursor staleness by incarnation**. |
| 3 | `phase3_agui_http.rs` | Real HTTP/SSE: task → interrupt → resume via `forwardedProps.task_ref`; lossless replay to two independent UIs; bearer auth required end-to-end; CORS origin echoed. |
| 4 | `phase4_delegation.rs` | Delegate over MCP; **crash agentd while the callee is `awaiting_input`**; restart; initiator woken **exactly once** (dedup by `delivery_id`); respond; collect result; grant + rejection both ledgered. |
| 5 | `phase5_claude.rs` | The claude adapter registered alongside codex; stream + complete; permission prompt → `awaiting_input` → resume, on the same engine and traces as codex. |

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
- `hearth-guestd` was cross-compiled with the locked nixpkgs musl toolchain via
  `make guest-bin-musl`; `file` reports `static-pie linked`, and the staged
  vm-base binary is byte-identical to the target artifact. The Makefile now
  forces `crt-static` because Nix's musl cross wrapper otherwise emits a
  dynamically linked binary whose interpreter lives in `/nix/store`.
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

### Fixes produced by the live pass

1. Offline image linting now detects absolute systemd enablement symlinks
   without resolving them against the host root.
2. `SCM_RIGHTS` control-message lengths compile on both glibc and musl libc
   layouts.
3. AF_VSOCK connect readiness is retained until real I/O consumes it.
4. `guest-bin-musl` enforces a genuinely static guest artifact on Nix.
5. `/dev/kvm` is checked as a character device instead of a regular file.
6. Boot-config drift comparison tolerates systemd's resolved executable path
   and flattened whitespace representation while still requiring Hearth's
   generated argument remainder to match exactly.

## Remaining gaps — and how to close each

These are genuine gaps, not hand-waves. Each says what is unproven and what
access would let a future session prove it.

1. **Real `codex app-server` / `claude` CLIs.** The adapters drive a *pinned
   contract* (`PINNED_APP_SERVER_VERSION` / `PINNED_CLI_VERSION`) modelled on
   the real stream shapes but **not validated against the real binaries** — the
   exact method names (`newThread`/`sendUserTurn`/`execApproval`; claude's
   `stream-json` event/`control_request` shapes) may differ.
   *To verify:* install the pinned codex/claude versions in an image; capture a
   real `codex app-server` and `claude -p --output-format stream-json` session;
   diff against `crates/hearth-guestd/src/adapter/{codex,claude}.rs`; adjust the
   `translate_*` functions and bump the pinned version. The version-pin refusal
   path (`fake_codex` with `HEARTH_FAKE_CODEX_VERSION`) already tests that a
   mismatch is a loud, clean error, so the *contract mechanism* is proven — only
   the *specific schema* needs a real CLI.

2. **Unmodified AG-UI `HttpAgent` conformance.** Phase 3 drives the endpoint
   with a raw HTTP/SSE client that mirrors what `HttpAgent` does on the wire,
   not the TypeScript SDK itself.
   *To verify:* a Node environment with `@ag-ui/client`; point a real
   `HttpAgent` at `POST /v1/agents/<name>/agui` with the bearer token and assert
   it drives a run, an interrupt, and a resume. The event field casing
   (`SCREAMING_SNAKE` type, camelCase fields) is already the AG-UI shape
   (`crates/hearth-agent-proto/src/events.rs`), so this is a conformance check,
   not new development.

3. **systemd unit hardening + `LoadCredential` + real `hearth-agent` user.**
   `systemd/hearth-agentd.service` declares the hardening and credential wiring
   §4 requires, but it must not be enabled as written. Both `hearth.service`
   and `hearth-agentd.service` claim `RuntimeDirectory=hearth`, so the
   unprivileged unit could change ownership of the shared broker directory.
   Further, agentd chmods its control socket to `0660` but does not assign the
   documented `root:hearth` ownership; when run as `hearth-agent`, operators in
   the `hearth` group would not own the socket through that primary group.
   *To verify after fixing the unit/socket ownership:* declare the
   `hearth-agent` user and credentials in NixOS, launch the unit, assert it can
   reach hearthd through its supplementary `hearth` group, inspect
   `systemd-analyze security hearth-agentd`, and confirm an unrelated uid is
   denied machine-plane lifecycle verbs.

4. **VM snapshot/restore incarnation rotation, end to end.** Incarnation
   rotation is unit-tested (`store.rs`) and the restore→`ReportAck{restored:
   true}`→`rotate_incarnation` wiring is in place (`hearthd` marks pending
   restore; guestd rotates on the ack), but the *machine-plane* `restore` path
   that triggers it needs a real CHV snapshot.
   *To verify:* on the CHV host from (1), `snapshot` a running agent VM, mutate
   a task, `restore`, and assert outstanding cursors return `cursor.stale` and a
   fresh `task.status` re-syncs. The guest-side half is already proven by
   `phase2_tasks.rs::stale_cursor_is_rejected_by_incarnation`.

5. **Inter-guest bridge isolation.** Explicitly a non-goal of the proposal (§8,
   §14); no code claims to solve it and nothing here depends on it. Listed only
   so the boundary stays honest: guests can still reach each other over
   `hearth0` at the IP layer; the agent plane simply never uses that path.

## Host-environment issues observed during the live pass

- `/etc/dnsmasq.d/hearth` is absent on this host. Hearth therefore cannot write
  its requested static lease drop-in and the live agent VM uses dnsmasq's
  dynamic lease instead. Connectivity works, but the declarative NixOS network
  integration should create/wire the drop-in before static addressing can be
  claimed.
- A legacy image at `/var/lib/hearth/images/debian-13-generic-amd64` has no
  `.hearth.toml`; one malformed legacy entry currently aborts the entire
  `image ls`, which also blocks the composite `spawn` path. The live pass used
  explicit image build/import, `create`, and `start` instead. No legacy disk or
  image was deleted.
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
