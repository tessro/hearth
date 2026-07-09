# Hearth Operations

Everything an operator needs to stand up `hearthd` on a single host and keep it
current. No Nix anywhere: Hearth is a pair of static Rust binaries plus a systemd
unit, and every host dependency is an ordinary distro package or an upstream
release binary.

## 1. Host prerequisites

Hearth shells out to host tools rather than bundling them. Install these before
`hearthctl host check` will go green. Package names are distro-generic (`apt` =
Debian/Ubuntu, `dnf` = Fedora/RHEL); a few tools ship only as upstream release
binaries and are called out as such.

### Runtime — the daemon needs these to boot and network VMs

| Tool | Provides | apt | dnf |
| --- | --- | --- | --- |
| `cloud-hypervisor` | the VMM Hearth launches per VM | upstream release binary¹ | upstream release binary¹ |
| `qemu-img` | per-VM disk create/convert | `qemu-utils` | `qemu-img` |
| `nft` | the `hearth_nat` publish table | `nftables` | `nftables` |
| `dnsmasq` | guest DHCP + leases on the bridge | `dnsmasq` | `dnsmasq` |
| `ip` | tap/bridge wiring | `iproute2` | `iproute` |
| `socat` | agent-in-charge vsock proxy | `socat` | `socat` |
| `cloud-localds` | seed ISO for cloud-image services² | `cloud-image-utils` | `cloud-utils` |
| KVM | `/dev/kvm`, `kvm` + `vhost_vsock` modules | kernel | kernel |
| CHV firmware | PVH firmware for cloud images² | `make firmware` (downloads `CLOUDHV.fd`) | same |

¹ `cloud-hypervisor` is distributed as a static release binary from
<https://github.com/cloud-hypervisor/cloud-hypervisor/releases>; drop it in
`/usr/local/bin`. Some distros also package it.
² Only for cloud-image services. Pure docker-rootfs VMs (the primary path) boot
the dedicated guest kernel directly and need neither `cloud-localds` nor firmware.

### Build-time — `hearthctl image build` needs these

| Tool | Provides | apt | dnf |
| --- | --- | --- | --- |
| `buildah` | builds the Dockerfile | `buildah` | `buildah` |
| `umoci` | unpacks the OCI layout | `umoci`³ | `umoci`³ |
| `mkfs.ext4` | the ext4 root filesystem | `e2fsprogs` | `e2fsprogs` |
| `qemu-img` | wraps the rootfs as a disk | `qemu-utils` | `qemu-img` |

³ `umoci` is packaged on recent distros; otherwise use the release binary from
<https://github.com/opencontainers/umoci/releases>. `hearthctl image build`
preflights these and fails with the exact `install <pkg>` hint if one is missing,
before the ~10-minute build, not after.

### Guest kernel — `scripts/build-guest-kernel.sh` needs these

Ordinary kernel build tools. The script preflights them and prints the same
distro-generic hints if any are absent:

| Tool | apt | dnf |
| --- | --- | --- |
| `gcc`, `make` | `build-essential` | `gcc`, `make` |
| `flex`, `bison`, `bc`, `perl` | `flex bison bc perl` | `flex bison bc perl` |
| `xz` | `xz-utils` | `xz` |
| libelf headers | `libelf-dev` | `elfutils-libelf-devel` |
| `curl` or `wget` | `curl` | `curl` |

## 2. Bridge + dnsmasq expectations

Hearth does **not** create the bridge or run dnsmasq — it assumes the host owns
that layer (see `ARCHITECTURE.md`). It attaches each VM's tap to the bridge,
reads the lease file to report addresses, and writes static-lease drop-ins. The
operator provides:

- **A bridge named `hearth0`** (override with `HEARTH_BRIDGE`) with a gateway
  address, default `10.26.8.1/24`, and outbound NAT/masquerade to the uplink.
  That masquerade table is the host's own; Hearth's `hearth_nat` table only holds
  publish DNAT rules and is rewritten wholesale, never touching yours.
- **`dnsmasq` serving DHCP on `hearth0`**, writing leases to
  `/var/lib/dnsmasq/dnsmasq.leases` (override `HEARTH_LEASE_FILE`). Its dynamic
  `dhcp-range` MUST NOT overlap Hearth's static slice.
- **A drop-in dir Hearth can write**, default `/etc/dnsmasq.d/hearth`
  (override `HEARTH_DNSMASQ_DROPIN_DIR`). Point dnsmasq at it, e.g.
  `conf-dir=/etc/dnsmasq.d/hearth`. On `create` Hearth writes `<service>.conf`
  with a `dhcp-host=<mac>,<ip>` line and SIGHUPs dnsmasq; on `destroy` it removes
  it and SIGHUPs again. If the dir is absent, static leases are skipped with a
  warning and VMs fall back to dynamic DHCP — they still boot.

**Static-range constraint.** Hearth allocates each VM a reserved address from
`HEARTH_DHCP_STATIC_START` for `HEARTH_DHCP_STATIC_COUNT` addresses — by default
`10.26.8.16`–`10.26.8.79`. This slice must sit inside the `hearth0` subnet and
**outside** dnsmasq's dynamic `dhcp-range`, or a reservation can collide with a
dynamically handed-out lease. If you move the bridge subnet, move the static
start with it.

## 3. Install

```sh
# 1. Build and install the binaries, the systemd unit, and this doc.
sudo make install
#    -> /usr/local/bin/{hearthd,hearthctl}
#    -> /etc/systemd/system/hearth.service
#    -> /usr/local/share/doc/hearth/operations.md

# 2. Build the dedicated guest kernel (vanilla kernel.org, no Nix). Installs
#    /var/lib/hearth/kernels/<version>/vmlinux and a `current` symlink, which is
#    the hearthd default. Rerun only to bump the pinned version, never for a host
#    kernel change.
sudo scripts/build-guest-kernel.sh

# 3. (cloud-image services only) fetch the CHV firmware.
sudo make firmware

# 4. Verify every prerequisite before starting anything.
hearthctl host check          # table of paths/commands/modules, ok=true each

# 5. Start the daemon.
sudo systemctl enable --now hearth.service
hearthctl ping                # "pong — hearthd <version> (pid <n>)"
```

`hearthctl host check` reports each directory, command, the firmware, the guest
kernel, `/dev/kvm`, the bridge, and the `kvm`/`vhost_vsock` modules. Green there
means `create`/`start` will not fail on a missing prerequisite. `hearthctl image
build` and `scripts/build-guest-kernel.sh` additionally preflight their own build
tools, so a missing `mkfs.ext4` or `libelf-dev` fails up front with an install
hint rather than mid-build.

`make uninstall` removes the binaries, unit, and doc (and daemon-reloads).

## 4. Dev loop — the alternative to the packaged daemon

For development, run the daemon straight out of the build tree instead of the
installed one. This is **the** supported alternative, not a second deployment:

```sh
make dev
#   == sudo HEARTH_FIRMWARE=... HEARTH_BRIDGE=hearth0 ./target/release/hearthd
# or the debug binary directly:
sudo ./target/debug/hearthd
```

Run at most one daemon on `/run/hearth.sock` at a time. The failure mode that
cost real time was a stale `hearth.service` pointing at a deleted
`/usr/local/bin/hearthd` while a `sudo ./target/debug/hearthd` ran in a terminal
— two daemons, unclear which answered. Two guards make that visible now:

- `hearthctl ping` prints the responding daemon's **version and pid**, so you
  always know which one you reached. Stop the other (`systemctl stop
  hearth.service`, or kill the terminal daemon) if it is not the one you meant.
- On startup `hearthd` reconciles running VMs and flags any whose recorded boot
  argv differs from what it would launch now as `boot_config: stale` in `status`
  — instead of silently adopting a VM booted by an older daemon with different
  flags. Restarting that VM is your call; the point is it is no longer invisible.

The local smoke setup in `README.md` runs a throwaway daemon under `/tmp` with
`--disable-vsock` and no root-owned dirs, for exercising the CLI/registry without
touching `/etc` or `/var`.

## 5. Upgrades

`hearthctl` and `hearthd` share one line-JSON protocol, and the CLI can outrun a
long-running daemon. Two things keep that from turning into a cryptic serde
error:

- Every `hearthctl` connection performs a version handshake, and `version`
  exposes the daemon's verb list. If you send a verb the running daemon predates,
  the CLI reports it plainly:
  `daemon 0.1.0 does not support 'image-import'; hearthctl is 0.2.0 — restart hearthd` —
  never the raw `protocol.invalid_json: unknown variant` it used to. (The named
  verb is always a wire verb the daemon actually received; composite CLI commands
  like `spawn` never appear here, since they are built from existing verbs.)
- **So the upgrade rule is one line: after upgrading the binaries, restart the
  daemon.**

```sh
sudo make install
sudo systemctl restart hearth.service
hearthctl ping        # confirm the new version answered
```

Running VMs are transient systemd units and survive a daemon restart;
`hearthd` reconciles them on start (see the `boot_config` note in §4). A guest
kernel bump (rare) means rebuilding with `scripts/build-guest-kernel.sh` and
restarting the affected VMs so they boot the new `current` kernel.
