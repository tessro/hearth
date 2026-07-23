# Hearth — Architecture

## Purpose

Hearth manages a small fleet of KVM virtual machines on a single host. Each VM
may run an autonomous agent that operates services inside that VM. Agents
coordinate task work through the unprivileged host `hearth-agentd`; they do not
receive direct access to hearthd's VM lifecycle API.

The host machine plane runs `hearthd`; operators use `hearthctl` to talk to it.
The optional agent plane adds `hearth-agentd` for AG-UI and MCP traffic. Hearth
owns VM lifecycle, networking, storage, and snapshots. Each guest owns its
containers and processes.

## Goals

- One root machine-plane process (`hearthd`) and one operator tool (`hearthctl`).
- Bounded, legible surface — future-you with no context should be able to read the configs and understand the system.
- VM lifecycle entirely owned by hearth: create, destroy, boot, shutdown, reboot, snapshot, restore, resize, image management.
- Agent task and delegation traffic carried by guestd and agentd, separate from
  the host-only VM lifecycle channel.
- Verb-level policy (allowlist) and a complete audit log enforced at the daemon, not the transport.

## Non-goals

- Multi-host clustering, live migration between hosts, HA.
- Libvirt-equivalent abstractions: virtual networks with NAT/DHCP, storage pools, MAC-address management.
- A web UI, REST API, or anything beyond the unix-socket JSON protocol —
  **scoped to hearthd**. The agent plane (`docs/agent-plane.md`) adds a
  sanctioned HTTP surface (AG-UI) in a *separate*, unprivileged daemon
  (`hearth-agentd`); hearthd itself stays unix-socket line-JSON only. See the
  agent-plane proposal for that boundary.
- Direct guest access to hearthd's lifecycle verbs. Guest agents use the
  agent-plane task and delegation APIs instead.
- General image-building pipelines. Hearth can build a narrow Dockerfile VM
  rootfs format, but arbitrary application Dockerfiles remain a container runtime
  concern.

## Topology

```
Host
├─ human ── hearthctl ── Unix JSON ──▶ hearthd ── systemd-run ──▶ cloud-hypervisor per VM
├─ UI / hearthctl agent ── AG-UI ──▶ hearth-agentd
└─ hearth-agentd ── restricted Unix JSON ──▶ hearthd socket broker
                                                    │
                                                    └── brokered hybrid vsock
                                                          │
Guest VM                                                  ▼
└─ agent CLI ◀── ACP / native protocol ──▶ hearth-guestd daemon / MCP shim
```

The **agent plane** (`docs/agent-plane.md`) layers on top without changing any
of the above: one `hearth-guestd` inside every agent VM (boot report, task
registry, adapters) and one unprivileged `hearth-agentd` on the host (AG-UI
over HTTP, an MCP server for agent-to-agent delegation, relaying to guestds
through hearthd's socket broker). hearthd's role is unchanged — machine plane
only; it never learns what a task is.

## Components

### hearthd

Long-running Rust daemon. Itself a systemd unit (`hearth.service`). Responsibilities:

- Owns the service registry (`/etc/hearth/services/*.toml`). Each VM has a fixed,
  generated id for host resources and a mutable hostname for commands and DNS.
- Listens on the host-only `/run/hearth.sock` control socket and on each
  running VM's boot-report channel.
- Accepts line-delimited JSON requests; validates against a verb allowlist; dispatches to:
  - `systemd-run` for VM process lifecycle (start/stop the CHV process itself).
  - The per-VM Cloud Hypervisor HTTP API over its unix socket for runtime ops (`vm.boot`, `vm.shutdown`, `vm.reboot`, `vm.info`, `vm.snapshot`, `vm.restore`, `vm.resize`).
  - The host filesystem for image import and per-VM disk provisioning.
- Writes a structured audit log to journald: every request, who, when, args, result.
- On startup, reconciles desired state (services marked `enabled = true`) with runtime state (which CHV processes are running).

### hearthctl

Rust CLI. Connects to `/run/hearth.sock` and speaks line-JSON. Two output modes:

- Human (default): pretty-printed tables/status for terminals.
- `--json`: machine-readable, one JSON object per response, line-delimited.

Verbs (initial set):

```
hearthctl ls                              # list services + state
hearthctl status <hostname>               # detailed status of one VM
hearthctl create <hostname> --from <image> # provision new VM and generate its id
hearthctl rename <old> <new>              # change hostname; retain fixed id
hearthctl destroy <hostname>              # stop and remove VM, disk, config
hearthctl start <hostname>                # boot VM (idempotent)
hearthctl stop <hostname>                 # graceful shutdown
hearthctl restart <hostname>              # graceful restart
hearthctl reboot <hostname>               # ACPI reboot inside guest
hearthctl snapshot <hostname> [--tag t]   # CHV memory+disk snapshot
hearthctl restore <hostname> [--tag t]    # restore from snapshot
hearthctl resize <hostname> [--cpu N] [--mem M] # live resize via CHV API
hearthctl logs <hostname> [--follow]      # serial-console output
hearthctl image ls                        # list base images
hearthctl image build --name n --dockerfile ./Dockerfile --context . --disk 40
hearthctl image rm <name>                 # remove base image
```

### Per-VM Cloud Hypervisor processes

One `cloud-hypervisor` process per VM, supervised by systemd as a transient unit
named `hearth-vm-<id>.service`. Each has:

- An API socket at `/run/hearth/vms/<id>.sock`.
- A vsock CID assigned by hearth.
- A standalone qcow2 disk provisioned from the base image at create time.
- Serial console redirected to `/var/log/hearth/<id>.console` (this is what
  `hearthctl logs` tails).
- Direct-kernel boot with the shared Hearth guest kernel, `root=/dev/vda`, and
  `init=<resolved OCI command>` from the image manifest.

## Protocol

Line-delimited JSON over a unix domain socket. One request per line, one response per line, request/response correlation by client-assigned `id`.

**Request:**

```json
{"id": "01HXYZ...", "verb": "restart", "args": {"name": "mail"}}
```

**Response (success):**

```json
{"id": "01HXYZ...", "ok": true, "result": {"state": "running", "uptime_seconds": 0}}
```

**Response (failure):**

```json
{"id": "01HXYZ...", "ok": false, "error": {"code": "service.not_found", "message": "no service with hostname mail"}}
```

**Streaming responses** (used by `logs --follow`) emit multiple lines tagged with the same `id`, terminated by a `{"id": ..., "ok": true, "stream": "end"}` line.

Why line-JSON not HTTP/gRPC: any Unix-socket client can inspect it, no schema
compiler sits in the path, no version negotiation is needed, and newlines mark
frames. CHV's HTTP API is *internal* to hearthd; clients cannot reach it.

## Service model

A service is a VM. Its record lives at `/etc/hearth/services/<id>.toml`:

```toml
id          = "vm-0123456789abcdef0123456789abcdef"
hostname    = "mail"
enabled     = true
image       = "mail-vm"                   # image + manifest live in /var/lib/hearth/images/
cpu         = 2
memory_mib  = 2048
disk_gib    = 20
vsock_cid   = 100                          # assigned by hearth on create; preserved across reboots
mac         = "52:54:00:12:34:56"          # generated on create; preserved

[provision]
hostname         = "mail"
reset_machine_id = true
authorized_keys  = ["ssh-ed25519 AAAA... operator"]

[restart]
policy      = "on-failure"
max_retries = 5
backoff_sec = 10
```

The id never changes. It keys service files, allocations, disks, sockets, units,
logs, snapshots, agent task refs, and delegation records. The hostname may
change. It keys operator commands and the dnsmasq DNS record. The registry is
the source of truth for what VMs exist; runtime state is derived.

## Lifecycle

### Create

1. Validate the hostname (one DNS label, not already in the registry), generate
   a fixed id, merge the host recovery
   keyring with request keys, and reject an empty or malformed effective set
   unless `allow_no_ssh` is explicit.
2. Allocate vsock CID (next free integer ≥ 100) and MAC (locally administered range).
3. Convert `/var/lib/hearth/images/<image>.qcow2` to a sized raw scratch disk.
4. Apply the per-service provisioning plan (hostname, machine-id, managed
   `/home/agent/.ssh/authorized_keys`, optional files), verify SSH file
   contents/mode/ownership,
   then convert the scratch to `/var/lib/hearth/disks/<id>.qcow2`.
5. Write `/etc/hearth/services/<id>.toml` with `enabled = false`.
6. Return; do not boot. `hearthctl start <hostname>` is a separate step.

### Boot (start)

1. Read service config.
2. Create the tap, transient unit, CHV API socket, vsock socket, and console log
   from the fixed id, then start Cloud Hypervisor with the recorded disk, MAC,
   CID, CPU, and memory settings.
3. Wait for CHV API socket to be ready (poll with timeout).
4. Mark `enabled = true` in registry (so reboot survives host restart).
5. Return current status.

### Stop

1. `PUT /api/v1/vm.shutdown` on the per-VM API socket (ACPI signal to guest).
2. Wait up to `TimeoutStopSec` for the systemd unit to go inactive (i.e., CHV process to exit).
3. If timeout, escalate: `PUT /api/v1/vm.power-off` (hard stop), then
   `systemctl stop hearth-vm-<id>` if still up.
4. Mark `enabled = false` in registry.

### Reboot

`PUT /api/v1/vm.reboot` — ACPI reboot inside the guest. CHV stays up.

### Snapshot / Restore

CHV's `vm.snapshot` produces a directory with memory state + disk metadata.
Hearth stores it under `/var/lib/hearth/snapshots/<id>/<tag>/`. Restore is
`vm.restore` against that directory.

### Resize

`PUT /api/v1/vm.resize` — CHV supports live CPU and memory resize. Resize updates persist in the service config so they survive next boot.

### Destroy

1. Stop if running.
2. Remove disk, snapshot directory, console log.
3. Remove `/etc/hearth/services/<id>.toml`.
4. Free vsock CID and MAC in the registry's allocation map.

## Networking

One host bridge `hearth0` carries all VM traffic. Hearth does not manage the bridge — it expects it to exist on the host, declared in the NixOS module that ships alongside hearth (which also stands up dnsmasq for DHCP + DNS and the nftables rules for NAT to upstream).

Each VM gets a tap derived from its fixed id, created by hearthd at start time
and attached to `hearth0`.

Guest network configuration is part of the image contract; the standard base
uses systemd-networkd with DHCP. The dnsmasq instance on `hearth0` answers.

## Storage

```
/etc/hearth/
  services/
    mail.toml
    web.toml
  allocations.toml          # vsock CID + MAC allocations
/var/lib/hearth/
  images/                   # immutable image disks plus required manifests
    mail-vm.qcow2
    mail-vm.hearth.toml
  disks/                    # standalone per-VM qcow2 disks
    mail.qcow2
  snapshots/
    mail/
      <tag>/                # CHV snapshot directory
/var/log/hearth/
  mail.console              # serial output, captured by CHV
/run/hearth.sock            # hearthctl ↔ hearthd
/run/hearth/
  vms/mail.sock             # CHV API socket
  vsock/mail.sock            # host-side vsock unix socket (CHV's vsock backend)
```

## Host ↔ guest channel

CHV's vsock device is the Firecracker-style **hybrid** model: there is no
host-side `AF_VSOCK`. A guest connecting to CID 2 port *P* lands on whoever
listens on the host unix socket `/run/hearth/vsock/<vm>.sock_P`; a host→guest
connection dials `/run/hearth/vsock/<vm>.sock` and sends `CONNECT <port>`.
hearthd binds a per-VM `<vm>.sock_1025` listener for readiness reports and
heartbeats. The agent plane uses guest-to-host port 1026 and host-to-guest port
1027 through hearthd's FD-passing socket broker (see `docs/agent-plane.md` §6).
**Identity is the socket path** — whichever VM's socket a connection arrives on
is the caller, VMM-attested, with no guest token.

> Historical note: an earlier build bound a host-side `AF_VSOCK` listener
> (`VMADDR_CID_ANY` + peer-CID filter). That is the vhost-vsock model and never
> saw CHV's hybrid-backend connections. Hearth now uses per-VM hybrid Unix
> sockets for all host-side guest channels.

## Authorization

Day 1 model is intentionally minimal:

- **Host-local socket** (`/run/hearth.sock`): protected by Unix filesystem permissions (`0660`, owned by `root:hearth`). Any human user in the `hearth` group can issue any verb.
- **Verb allowlist + per-peer-UID policy**: enforced at hearthd. A config-driven map (`/etc/hearth/verb-policy.toml`) grants specific uids/gids a restricted verb set; the built-in default keeps root and the `hearth` group at full access. This is what lets `hearth-agentd` run as an unprivileged `hearth-agent` user with exactly `ping`/`version`/`ls`/`status`/`rename`/`agent-endpoints`/`guest-listener`/`guest-connect` and nothing else — the socket broker (`guest-listener`/`guest-connect`) passes brokered fds via `SCM_RIGHTS` so agentd never opens the root-owned vsock directory itself.

Audit log: every request includes the Unix peer uid, verb, args, result code,
and duration in structured journald fields. Guest channel logs identify the
fixed VM id from the socket path. `journalctl -u hearth.service` is the
canonical audit view.

## Supervision model

VM processes are not their own systemd units on disk. hearthd issues
`systemd-run --unit=hearth-vm-<id> --collect ...` per VM. systemd handles:

- cgroup isolation,
- restart policy (configurable per-service),
- stdout/stderr → journald,
- clean shutdown on host reboot (units stopped in dependency order),
- `SIGCHLD` reaping.

hearthd's own systemd unit (`hearth.service`) declares `After=network-online.target` and `WantedBy=multi-user.target`. On host boot:

1. Network and bridge come up.
2. hearthd starts.
3. hearthd reads the registry, for each `enabled = true` service issues `systemd-run`.
4. VMs come up in parallel; hearthd polls for readiness.

This is the only systemd config that lives on disk for hearth-related VM management. Adding or removing a service does not touch `/etc/systemd/`.

## Failure modes

| Failure | Detection | Recovery |
|---|---|---|
| CHV process crashes | systemd transient unit exits non-zero | `Restart=on-failure` brings it back per policy; hearthd notes in audit log |
| CHV API socket unresponsive | `vm.shutdown` request times out | hearthd escalates to `systemctl stop hearth-vm-<name>`; on next start, treat as cold boot |
| hearthd crashes | systemd restarts `hearth.service` | On restart, reconcile: each `enabled = true` service that has no live unit is started; each live unit not in registry is logged but left alone |
| Host reboot | normal systemd boot sequence | hearthd starts, reconciles, all `enabled = true` VMs come up |
| Agent VM compromised | agentd records denied task and MCP calls | Revoke its fixed id from the delegator allowlist; rebuild or restore the VM |
| Disk corruption | qcow2 read errors surface to guest | Out of scope; restore from snapshot or recreate from base image |

## Open questions

- **Snapshot retention**: explicit only, or auto-prune by count/age?
- **`hearthctl exec`**: a verb for running commands inside guests was deferred. Likely belongs in a per-guest agent, not hearthd. *(Resolved: this is what `hearth-guestd` and the agent plane provide — see `docs/agent-plane.md`.)*
- **Resource limits beyond CHV's**: guest RAM (`--memory size=`) and the host
  cgroup footprint of the VMM are different quantities, so Hearth must not
  derive a tight systemd `MemoryMax` from guest RAM. A future host limit should
  be an explicit operator budget with separate accounting and admission
  control; guest workload limits belong inside the guest.
- **Host package surface**: keep libvirt/qemu/virt-manager installed as a debugging escape hatch, or strip to just cloud-hypervisor + virtiofsd? Default to keep for now; revisit after hearth is solid.
- **Bridge management**: hearth currently *expects* `hearth0` to exist, declared in NixOS alongside dnsmasq + NAT. `hearthctl host check` validates its presence.

## Known follow-ups

Work items retired here from the (removed) agent-plane verification report;
history for each lives in that file's git log.

- **Codex and Claude adapters are deliberately inactive.** The live audit
  showed the Codex adapter models an obsolete app-server schema (the current
  one uses `thread/start`/`turn/start` and same-connection approval
  responses) and the Claude adapter's version pin predates the audited CLI.
  Both need a rewrite against a freshly pinned real binary plus provisioned
  authentication before any image advertises them.
- **`hearthctl image build --rootless` is broken**: it flattens root-owned
  guest files to the invoking uid, producing an invalid sudo installation.
  Workaround: run `hearthctl image build` inside `buildah unshare` without
  the flag.
- **Example-image probes await retirement.** `hermes-probe`/`netdiag` (and
  `scripts/test-hermes-vm.sh`, which depends on them) are functionally
  superseded by the guestd boot report.
- **Guest images ship no NTP client**, so a VM restored from a snapshot keeps
  a wall clock behind by the stopped window until one is added (in progress).
