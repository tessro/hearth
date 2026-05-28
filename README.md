# Hearth

Hearth manages a small single-host fleet of Cloud Hypervisor/KVM VMs through
one daemon (`hearthd`) and one CLI (`hearthctl`).

The implementation is a Rust workspace:

- `hearth-proto`: shared line-delimited JSON protocol types.
- `hearthd`: Unix-socket daemon, service registry, lifecycle dispatcher, CHV API client.
- `hearthctl`: CLI with human and `--json` output modes.

Build and test:

```sh
cargo test
cargo build
```

Local smoke test without touching `/etc` or `/var`:

```sh
mkdir -p /tmp/hearth-smoke/{services,images,disks,seeds,snapshots,run,log}
target/debug/hearthd \
  --socket /tmp/hearth-smoke/hearth.sock \
  --services-dir /tmp/hearth-smoke/services \
  --allocations /tmp/hearth-smoke/allocations.toml \
  --images-dir /tmp/hearth-smoke/images \
  --disks-dir /tmp/hearth-smoke/disks \
  --seeds-dir /tmp/hearth-smoke/seeds \
  --snapshots-dir /tmp/hearth-smoke/snapshots \
  --run-dir /tmp/hearth-smoke/run \
  --log-dir /tmp/hearth-smoke/log \
  --firmware /tmp/hearth-smoke/CLOUDHV.fd \
  --disable-vsock

target/debug/hearthctl --socket /tmp/hearth-smoke/hearth.sock ping
target/debug/hearthctl --socket /tmp/hearth-smoke/hearth.sock ls
```

Production defaults follow `ARCHITECTURE.md`: `/run/hearth.sock`,
`/etc/hearth/services`, `/etc/hearth/allocations.toml`,
`/var/lib/hearth/{images,disks,seeds,snapshots}`, `/run/hearth/{vms,vsock}`,
and `/var/log/hearth`.

When a service is marked `is_agent_in_charge = true`, `hearthd` also starts a
host vsock listener on `HEARTH_VSOCK_PORT`/`--vsock-port` and accepts only that
service's CID. The guest-side proxy units in `systemd/hearth-proxy.*` expose
`/run/hearth.sock` inside the VM and forward it to host CID 2.
