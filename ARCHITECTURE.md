# Hearth — Architecture

## Purpose

Hearth manages a small fleet of KVM virtual machines on a single host. Each VM runs an autonomous agent that operates the services inside that VM. One VM — the *agent-in-charge* — is privileged to manage its peers by issuing VM-lifecycle and operational commands back to the host through a constrained channel.

The host runs a single daemon, `hearthd`. The CLI, `hearthctl`, is the only interface anyone (human or agent) uses to talk to hearthd. Hearth is opinionated about the VM surface — lifecycle, networking, storage, snapshots — and incurious about what those VMs run. How a guest organises its own services (containers, raw processes, an init system) is the guest agent's business.

## Goals

- One process to think about on the host (`hearthd`); one tool to operate it (`hearthctl`).
- Bounded, legible surface — future-you with no context should be able to read the configs and understand the system.
- VM lifecycle entirely owned by hearth: create, destroy, boot, shutdown, reboot, snapshot, restore, resize, image management.
- A single privileged channel from the agent-in-charge VM to hearthd, structurally identical to the host-side local channel.
- Verb-level policy (allowlist) and a complete audit log enforced at the daemon, not the transport.

## Non-goals

- Multi-host clustering, live migration between hosts, HA.
- Libvirt-equivalent abstractions: virtual networks with NAT/DHCP, storage pools, MAC-address management.
- A web UI, REST API, or anything beyond the unix-socket JSON protocol.
- Per-caller capability models. Day 1 is "the privileged channel can do everything; nobody else has the channel."
- General image-building pipelines. Hearth can build a narrow Dockerfile VM
  rootfs format, but arbitrary application Dockerfiles remain a container runtime
  concern.

## Topology

```
                                                  Host
+---------------------------------------------------------------------------------------+
|                                                                                       |
|  human         hearthctl ─────unix/JSON──┐                                            |
|                                          │                                            |
|                                          ▼                                            |
|                                       hearthd ──systemd-run──▶ cloud-hypervisor (mail)|
|                                          │       (transient    cloud-hypervisor (web) |
|                                          │       units)        cloud-hypervisor (ai)  |
|                                          │                              │             |
|                                          │       per-VM API socket      │             |
|                                          ├──HTTP/unix──▶ /run/hearth/vms/mail.sock    |
|                                          │                                            |
|                                          └──vsock listener (CID 2, port N)            |
|                                                       ▲                               |
|                                                       │ virtio-vsock                  |
|  +----------------------+   +----------------------+  │                               |
|  │ VM: agent-in-charge  │   │ VM: mail (peer)      │  │                               |
|  │                      │   │                      │  │                               |
|  │ hearthctl ─unix──▶ socat ─vsock──────────────────┘                                 |
|  │ (guest agent)        │   │ (guest agent)        │                                  |
|  +----------------------+   +----------------------+                                  |
+---------------------------------------------------------------------------------------+
```

## Components

### hearthd

Long-running Rust daemon. Itself a systemd unit (`hearth.service`). Responsibilities:

- Owns the service registry (`/etc/hearth/services/*.toml`) and the runtime mapping `service-name → { systemd unit, CHV API socket path, vsock CID, disk path, ... }`.
- Listens on `/run/hearth.sock` (host) and on a vsock port (host CID 2, fixed port) for the agent-in-charge.
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
hearthctl status <name>                   # detailed status of one service
hearthctl create <name> --from <image>    # provision new VM
hearthctl destroy <name>                  # stop and remove VM, disk, config
hearthctl start <name>                    # boot VM (idempotent)
hearthctl stop <name>                     # graceful shutdown
hearthctl restart <name>                  # graceful restart
hearthctl reboot <name>                   # ACPI reboot inside guest
hearthctl snapshot <name> [--tag t]       # CHV memory+disk snapshot
hearthctl restore <name> [--tag t]        # restore from snapshot
hearthctl resize <name> [--cpu N] [--mem M]  # live resize via CHV API
hearthctl logs <name> [--follow]          # serial-console output
hearthctl image ls                        # list base images
hearthctl image build --name n --dockerfile ./Dockerfile --context . --disk 40
hearthctl image rm <name>                 # remove base image
```

### Per-VM Cloud Hypervisor processes

One `cloud-hypervisor` process per VM, supervised by systemd as a transient unit named `hearth-vm-<name>.service`. Each has:

- An API socket at `/run/hearth/vms/<name>.sock`.
- A vsock CID assigned by hearth.
- A standalone qcow2 disk provisioned from the base image at create time.
- Serial console redirected to a file at `/var/log/hearth/<name>.console` (this is what `hearthctl logs` tails).
- Direct-kernel boot with the shared Hearth guest kernel, `root=/dev/vda`, and
  `init=<resolved OCI command>` from the image manifest.

### Agent-in-charge vsock proxy

Inside the agent-in-charge VM, a tiny systemd socket-activated service uses `socat` (or equivalent) to present `/run/hearth.sock` at the same path it would have on the host:

```
[Socket] /run/hearth.sock     LISTEN
[Service] socat - UNIX-LISTEN:/run/hearth.sock,fork VSOCK-CONNECT:2:<HEARTH_PORT>
```

This means hearthctl in the agent-in-charge VM is byte-identical to hearthctl on the host. The transport is invisible to the client.

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
{"id": "01HXYZ...", "ok": false, "error": {"code": "service.not_found", "message": "no service named mail"}}
```

**Streaming responses** (used by `logs --follow`) emit multiple lines tagged with the same `id`, terminated by a `{"id": ..., "ok": true, "stream": "end"}` line.

Why line-JSON not HTTP/gRPC: trivial to debug with `socat - UNIX:/run/hearth.sock`, no schema compiler in the path, no version negotiation, framing is just newlines. CHV's HTTP API is *internal* to hearthd; it is not exposed to clients.

## Service model

A service is a VM. Defined by a TOML file in `/etc/hearth/services/<name>.toml`:

```toml
name        = "mail"
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

The registry is the source of truth for "what VMs exist." Runtime state (PID, current API socket, last-known status) is derived, not stored.

## Lifecycle

### Create

1. Validate name (kebab-case, not already in registry), merge the host recovery
   keyring with request keys, and reject an empty or malformed effective set
   unless `allow_no_ssh` is explicit.
2. Allocate vsock CID (next free integer ≥ 100) and MAC (locally administered range).
3. Convert `/var/lib/hearth/images/<image>.qcow2` to a sized raw scratch disk.
4. Apply the per-service provisioning plan (hostname, machine-id, managed
   `/home/agent/.ssh/authorized_keys`, optional files), verify SSH file
   contents/mode/ownership,
   then convert the scratch to `/var/lib/hearth/disks/<name>.qcow2`.
5. Write `/etc/hearth/services/<name>.toml` with `enabled = false`.
6. Return; do not boot. `hearthctl start <name>` is a separate step.

### Boot (start)

1. Read service config.
2. Pre-create the per-VM tap (`ip tuntap add dev hrt-<name> mode tap`, then attach to `hearth0` and set up), then `systemd-run --unit=hearth-vm-<name> --collect --property=Restart=<policy> --property=TimeoutStopSec=30s cloud-hypervisor --api-socket /run/hearth/vms/<name>.sock --kernel /var/lib/hearth/kernels/current/vmlinux --disk path=<disk>.qcow2 --cmdline "console=ttyS0 root=/dev/vda rootfstype=ext4 rw init=<manifest-init>" --net tap=hrt-<name>,mac=<mac> --vsock cid=<cid>,socket=/run/hearth/vsock/<name>.sock --serial file=/var/log/hearth/<name>.console --console off --cpus boot=<cpu> --memory size=<mem>M --balloon size=0,free_page_reporting=on`.
3. Wait for CHV API socket to be ready (poll with timeout).
4. Mark `enabled = true` in registry (so reboot survives host restart).
5. Return current status.

### Stop

1. `PUT /api/v1/vm.shutdown` on the per-VM API socket (ACPI signal to guest).
2. Wait up to `TimeoutStopSec` for the systemd unit to go inactive (i.e., CHV process to exit).
3. If timeout, escalate: `PUT /api/v1/vm.power-off` (hard stop), then `systemctl stop hearth-vm-<name>` if still up.
4. Mark `enabled = false` in registry.

### Reboot

`PUT /api/v1/vm.reboot` — ACPI reboot inside the guest. CHV stays up.

### Snapshot / Restore

CHV's `vm.snapshot` produces a directory with memory state + disk metadata. Hearth stores under `/var/lib/hearth/snapshots/<name>/<tag>/`. Restore is `vm.restore` against that directory. Note: this is *memory-snapshot* style (pause → snapshot → resume), not qcow2-layered.

### Resize

`PUT /api/v1/vm.resize` — CHV supports live CPU and memory resize. Resize updates persist in the service config so they survive next boot.

### Destroy

1. Stop if running.
2. Remove disk, snapshot directory, console log.
3. Remove `/etc/hearth/services/<name>.toml`.
4. Free vsock CID and MAC in the registry's allocation map.

## Networking

One host bridge `hearth0` carries all VM traffic. Hearth does not manage the bridge — it expects it to exist on the host, declared in the NixOS module that ships alongside hearth (which also stands up dnsmasq for DHCP + DNS and the nftables rules for NAT to upstream).

Each VM gets a persistent tap named `hrt-<service>`, created by hearthd at start time and attached to `hearth0`. The tap is named explicitly rather than letting CHV pick — CHV's `--net` doesn't accept `bridge=`, so the bridge attachment happens outside CHV before launch.

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

The agent-in-charge VM is configured at create time with a known vsock CID and a flag `is_agent_in_charge = true`. On boot, hearthd listens on vsock (host CID 2, fixed port) and accepts connections *only* from the registered agent-in-charge CID. Any other CID connecting is logged and dropped.

Inside the agent-in-charge VM, a socket-activated systemd unit forwards `/run/hearth.sock` to vsock. The result: `hearthctl` in either location is identical code, identical config, identical socket path.

## Authorization

Day 1 model is intentionally minimal:

- **Host-local socket** (`/run/hearth.sock`): protected by Unix filesystem permissions (`0660`, owned by `root:hearth`). Any human user in the `hearth` group can issue any verb.
- **Vsock channel**: accepts connections only from the registered agent-in-charge CID. That channel can issue any verb in the allowlist, including lifecycle.
- **Verb allowlist**: enforced at hearthd. The allowlist is static, in code. Future versions may add per-caller capability models, but not day 1.

Audit log: every request, including caller identity (uid for unix-socket, CID for vsock), verb, args, result code, and duration, written to journald with structured fields. `journalctl -u hearth.service` is the canonical audit view.

## Supervision model

VM processes are not their own systemd units on disk. hearthd issues `systemd-run --unit=hearth-vm-<name> --collect ...` per VM. systemd handles:

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
| Agent-in-charge VM compromised | n/a (treated as trusted) | Out of scope day 1; sketched as a future per-caller capability model |
| Disk corruption | qcow2 read errors surface to guest | Out of scope; restore from snapshot or recreate from base image |

## Open questions

- **Snapshot retention**: explicit only, or auto-prune by count/age?
- **`hearthctl exec`**: a verb for running commands inside guests was deferred. Likely belongs in a per-guest agent, not hearthd.
- **Resource limits beyond CHV's**: guest RAM (`--memory size=`) and the host
  cgroup footprint of the VMM are different quantities, so Hearth must not
  derive a tight systemd `MemoryMax` from guest RAM. A future host limit should
  be an explicit operator budget with separate accounting and admission
  control; guest workload limits belong inside the guest.
- **Host package surface**: keep libvirt/qemu/virt-manager installed as a debugging escape hatch, or strip to just cloud-hypervisor + virtiofsd? Default to keep for now; revisit after hearth is solid.
- **Bridge management**: hearth currently *expects* `hearth0` to exist, declared in NixOS alongside dnsmasq + NAT. `hearthctl host check` validates its presence.
