# Hearth — Implementation Plan

Phased plan. Each phase ends with something you can actually run and inspect. Earlier phases are scoped tight enough that scope creep is the main risk; later phases get vaguer because they depend on what we learn.

## Prerequisite — Host prep

Host setup (NixOS module for bridge, directories, user, packages, plus by-hand verification that Cloud Hypervisor can boot a Debian cloud image and round-trip vsock) is out of scope for this plan. It lives separately. Hearth assumes:

- `cloud-hypervisor`, `rust-hypervisor-firmware`, `cloud-image-utils`, `qemu-utils`, `socat` available on the host.
- `br0` bridge exists with upstream connectivity; VMs get DHCP from upstream.
- `/dev/kvm` and `vhost_vsock` available.
- `hearth` system user/group exist.
- Directory tree exists with correct ownership: `/etc/hearth/services`, `/var/lib/hearth/{images,disks,seeds,snapshots}`, `/var/log/hearth`, `/run/hearth/{vms,vsock}`.
- A Debian cloud image is present at `/var/lib/hearth/images/debian-12-cloud-amd64.qcow2`.
- `rust-hypervisor-firmware`'s blob path is known (hearth's config will point at it).

If any of the above is unmet, hearth's Phase 1 cannot start. `hearthctl host check` (Phase 10) will verify these properties from inside hearth.

## Phase 1 — hearthd skeleton

Goal: a daemon that accepts connections on `/run/hearth.sock`, parses line-JSON, answers `ping`. The protocol scaffold.

- Rust workspace. Crates: `hearth-proto` (shared types), `hearthd` (daemon), `hearthctl` (CLI).
- `hearth-proto`: `Request`, `Response`, `Error`, verb enum. `serde` for JSON.
- `hearthd`: `tokio` runtime. Listen on `/run/hearth.sock` with `0660` perms, owner `root:hearth`. Per-connection task reads newline-delimited frames, dispatches to a handler trait, writes response.
- Handler trait: `async fn handle(&self, req: Request) -> Response`. Day-1 implementation answers `ping` and rejects everything else with `verb.not_allowed`.
- Audit log: every request emitted as a structured journald log via `tracing` + `tracing-journald`. Fields: `id`, `verb`, `caller_uid`, `result`, `duration_ms`.
- systemd unit `hearth.service`: `Type=notify`, `After=network-online.target`, `WantedBy=multi-user.target`. `sd_notify` ready after socket is bound.

**Done when**: `systemctl start hearth`, then `echo '{"id":"1","verb":"ping","args":{}}' | socat - UNIX:/run/hearth.sock` returns `{"id":"1","ok":true,"result":{"pong":true}}`. `journalctl -u hearth` shows the audit line.

## Phase 2 — hearthctl skeleton

Goal: a CLI that connects, sends JSON, prints results.

- `clap` for argument parsing. Subcommand structure mirrors verbs.
- Connect to `/run/hearth.sock`. Send one request, read one response (or stream for `logs --follow`).
- Two output modes: pretty (default) and `--json`. Pretty mode uses `comfy-table` or similar for `ls`/`status`.
- Initial subcommands: `ping`, `version`. Both exist mostly to exercise the plumbing.

**Done when**: `hearthctl ping` returns `pong`. `hearthctl --json ping` returns the raw JSON.

## Phase 3 — Service registry

Goal: hearthd knows what services exist. No VM management yet.

- `serde` parser for the TOML schema in ARCHITECTURE.md.
- On startup, scan `/etc/hearth/services/*.toml`. Build in-memory registry. Watch for changes via `notify` (optional — restart-to-reload is fine day 1).
- Allocations file `/etc/hearth/allocations.toml` tracks claimed vsock CIDs and MACs.
- Verbs: `ls` (returns all services with `enabled` flag and *static* config, no runtime status yet), `status <name>` (returns the static config for one service).
- hearthctl `ls` renders a table; `status` renders details.

**Done when**: with two hand-written TOML files in `/etc/hearth/services/`, `hearthctl ls` shows both. No VMs are running. Cargo of the daemon and CLI now feels real.

## Phase 4 — Boot, stop, reboot

Goal: hearth can run a VM that was *already provisioned by hand* in Phase 0.

- Verb `start <name>`:
  - Read service config.
  - Build the `cloud-hypervisor` argv (long; codify in a builder).
  - Invoke `systemd-run --unit=hearth-vm-<name> --collect --property=Restart=<policy> --property=TimeoutStopSec=30s -- cloud-hypervisor ...`.
  - Poll `/run/hearth/vms/<name>.sock` for readiness with a timeout.
  - Mark service `enabled = true` in registry.
  - Return current status.
- CHV HTTP-over-unix client in hearthd. Probably `hyper` + `hyperlocal`. Just enough for `vm.info`, `vm.shutdown`, `vm.reboot`, `vm.power-off`.
- Verb `stop <name>`: call `vm.shutdown`, wait, escalate to `vm.power-off` then `systemctl stop` on timeout. Mark `enabled = false`.
- Verb `reboot <name>`: call `vm.reboot`. Idempotent.
- Verb `restart <name>`: stop + start.
- Verb `status <name>`: now includes runtime fields (running, uptime, cpu_count, memory_mib actual).
- Reconciliation on hearthd startup: for each `enabled = true`, ensure the systemd unit is active; for each active `hearth-vm-*` unit, ensure it's in the registry (log + leave alone if not).

**Done when**: starting from a clean reboot, `hearthctl start mail` brings up the VM hand-provisioned in Phase 0. `hearthctl ls` shows it running. `hearthctl stop mail` shuts it down gracefully. `hearthctl restart mail` works. Audit log shows everything.

## Phase 5 — Create and destroy

Goal: hearth provisions new VMs from a base image.

- Verb `create <name> [--from <image>] [--cpu N] [--mem M] [--disk G]`:
  - Validate name (kebab-case, not already present).
  - Allocate vsock CID and MAC from `allocations.toml`.
  - Allocate disk: `qemu-img create -f qcow2 -F qcow2 -b <image> <disk_path> <size>`.
  - Generate cloud-init user-data and meta-data (templated); `cloud-localds <seed_iso> <user-data> <meta-data>`.
  - Write `/etc/hearth/services/<name>.toml` with `enabled = false`.
  - Do *not* boot. `create` is provisioning only.
- User-data template: sets hostname, installs SSH keys, creates the `agent` user, enables docker (we'll get to compose in Phase 8).
- Verb `destroy <name>`:
  - Stop if running.
  - Remove disk, seed, snapshot dir, console log, service TOML, allocations.

**Done when**: `hearthctl create web --from debian-12-cloud-amd64 && hearthctl start web` produces a working VM you can SSH into. `hearthctl destroy web` cleans up completely.

## Phase 6 — Snapshot, restore, resize, logs

Goal: the operational verbs that aren't lifecycle.

- Verb `snapshot <name> [--tag <tag>]`: call `vm.snapshot` with destination path under `/var/lib/hearth/snapshots/<name>/<tag>/`. Default tag is a timestamp.
- Verb `restore <name> [--tag <tag>]`: stop if running, call `vm.restore` from the snapshot dir, mark running.
- Verb `resize <name> [--cpu N] [--mem M]`: call `vm.resize`; persist new values to the service TOML so future boots reflect them.
- Verb `logs <name> [--follow]`: tail `/var/log/hearth/<name>.console`. `--follow` is the only streaming response in the protocol so far; nail down the streaming framing now.

**Done when**: snapshot/restore round-trips an in-VM state change. Live CPU resize works. `hearthctl logs mail --follow` streams kernel + cloud-init output during boot.

## Phase 7 — Image management

Goal: `hearthctl image` subcommands.

- Verb `image ls`: list files in `/var/lib/hearth/images/` with size + sha256.
- Verb `image pull <url> [--name <name>]`: download to images dir, sha256 the result, name it.
- Verb `image rm <name>`: refuse if any service references it; otherwise delete.

**Done when**: a fresh host can be brought to a working state with: `hearthctl image pull <debian-cloud-url>`, then `hearthctl create ... && hearthctl start ...`.

## Phase 8 — Agent-in-charge VM and vsock

Goal: the symmetric channel that motivated this whole design.

- Add `is_agent_in_charge = true` (boolean, exactly one VM may have it) to the service schema. Hearth verifies uniqueness.
- hearthd binds a vsock listener (host CID 2, fixed port — pick `1024`). Accepts connections only from the agent-in-charge VM's CID. Any other CID: log and drop.
- Same line-JSON protocol on vsock as on the unix socket; no transport-level differences.
- Inside the agent-in-charge VM: a NixOS module (or generic systemd snippet via cloud-init) installs a socket-activated `hearth-proxy.service`:
  - `hearth-proxy.socket`: `ListenStream=/run/hearth.sock`
  - `hearth-proxy.service`: `ExecStart=/usr/bin/socat - UNIX-LISTEN:/run/hearth.sock,fork VSOCK-CONNECT:2:1024`
- hearthctl built statically; placed in the agent-in-charge VM at `/usr/local/bin/hearthctl`. Same binary as the host.

**Done when**: `hearthctl ls` works *inside the agent-in-charge VM*, with no transport awareness in the client. The agent-in-charge can `hearthctl restart <peer>`. Audit log shows the vsock CID as caller.

## Phase 9 — Docker compose inside guests

Goal: the actual service-running layer the agents will use.

- Update the create-time cloud-init template: install docker-ce + docker-compose-plugin via the official apt repo.
- Create the `agent` user with docker group membership.
- Confirm a peer VM can run `docker compose up -d` against a checked-in compose file and serve traffic.
- Decide where compose files live in the guest — probably `/etc/compose/` or `~agent/compose/`. Out of scope for hearth itself; this is the agent's territory.

**Done when**: a peer VM created by hearth runs a nontrivial `docker compose` stack (e.g., a tiny web service), reachable on the bridge network.

## Phase 10 — Hardening pass

Goal: things that have been deferred but now matter.

- Allocations: serialize allocations file writes atomically (tempfile + rename).
- Concurrent request handling: per-service mutex in hearthd so two `restart mail` calls don't interleave.
- Reconciliation on hearthd startup: well-tested behavior for partial states (CHV unit exists but API socket gone; service marked enabled but unit missing; etc.).
- `MemoryMax` on transient units as a belt-and-suspenders bound beyond CHV's own limits.
- `hearthctl host check` verb: verifies bridge exists, directories exist with correct perms, host packages present, kernel modules loaded (vhost_vsock, kvm).
- Audit log review: structured fields stable enough to query with `journalctl -u hearth -o json | jq`.
- Verify graceful shutdown on host reboot: hearthd's systemd unit stops VMs in order, with a generous timeout.

**Done when**: yanking the power cord and bringing the host back up produces an identical running set of VMs without intervention.

## Out of scope (explicit non-plan)

- Multi-host, clustering, migration.
- Web UI, REST API.
- Per-caller capability model (deferred; current model is "agent-in-charge has everything").
- `hearthctl exec` inside guests — that's the guest agent's job, not hearth's.
- Image building (Packer-style). Hearth consumes images; agents or humans produce them.
- Backup/restore of whole hosts. Snapshots are per-VM, not a backup strategy.

## Risks and known unknowns

- **CHV API stability**: CHV's HTTP API does still evolve. Pin to the version shipped in the NixOS module; upgrade deliberately. Document the pinned version in `hearth-proto`.
- **rust-hypervisor-firmware boot quirks**: occasional reports of slow first boot or specific kernel-version incompatibilities. Mitigation: test boot with each new base image before pulling it into circulation.
- **vsock CID collisions** if two hearth instances ever run on the same kernel (e.g., during testing): not a real risk for production but a foot-gun during development. Use a CID range high enough (≥100) to leave room.
- **Snapshot footprint**: memory snapshots are large (one full memory image per snapshot). Easy to fill the disk. Phase 10 should add a soft warning + `image ls`-style accounting.
- **The agent-in-charge is fully trusted day 1.** A future compromise of that VM compromises the whole fleet. Per-caller capabilities, signed audit log, or a separate confirmation channel for destructive verbs are all things to think about *after* the system is working — not before.
