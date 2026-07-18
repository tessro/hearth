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
| KVM | `/dev/kvm`, `kvm` + `vhost_vsock` modules | kernel | kernel |

¹ `cloud-hypervisor` is distributed as a static release binary from
<https://github.com/cloud-hypervisor/cloud-hypervisor/releases>; drop it in
`/usr/local/bin`. Some distros also package it.

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

### SSH recovery keyring

Every VM create must resolve at least one authorized key. Install the host-wide
recovery keyring before creating VMs:

```sh
sudo install -d -m 0755 /etc/hearth
sudo install -m 0644 ~/.ssh/id_ed25519.pub /etc/hearth/authorized_keys
```

The file accepts bare OpenSSH public-key lines plus blank lines and comments.
AuthorizedKeys options are intentionally rejected. Override its location with
`HEARTH_AUTHORIZED_KEYS_FILE`; hearthd reads it on every create, so new keys do
not require a daemon restart. Per-VM `--ssh-key` and `--authorized-keys-file`
values are additive. If the host file and request are both empty, create fails
before allocating or forming a disk unless `--allow-no-ssh` is explicit.
`hearthctl ls` and `status` report `configured`, `intentionally-disabled`, or
`legacy-unknown`; manually migrated service records may show `legacy-unknown`
because Hearth cannot prove what their existing disks contain.

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
  `conf-dir=/etc/dnsmasq.d/hearth`. On `create` Hearth writes `<id>.conf`
  with a `dhcp-host=<mac>,<ip>,<hostname>` line and SIGHUPs dnsmasq; `rename`
  updates that DNS label, and `destroy` removes the file. If the dir is absent,
  static leases are skipped with a
  warning and VMs fall back to dynamic DHCP — they still boot.

**Static-range constraint.** Hearth allocates each VM a reserved address from
`HEARTH_DHCP_STATIC_START` for `HEARTH_DHCP_STATIC_COUNT` addresses — by default
`10.26.8.16`–`10.26.8.79`. This slice must sit inside the `hearth0` subnet and
**outside** dnsmasq's dynamic `dhcp-range`, or a reservation can collide with a
dynamically handed-out lease. If you move the bridge subnet, move the static
start with it.

## 3. Install

```sh
# 1. Build every installable artifact as the logged-in user, then copy them
#    into system paths as root. Install targets never invoke Cargo.
make build guest-bin
sudo make install
#    -> /usr/local/bin/{hearthd,hearthctl}
#    -> /usr/local/lib/hearth/guest/hearth-guestd
#    -> /etc/systemd/system/hearth.service
#    -> /usr/local/share/doc/hearth/operations.md

# 2. Install at least one host-wide SSH recovery key.
sudo install -d -m 0755 /etc/hearth
sudo install -m 0644 ~/.ssh/id_ed25519.pub /etc/hearth/authorized_keys

# 3. Build the dedicated guest kernel (vanilla kernel.org, no Nix). Installs
#    /var/lib/hearth/kernels/<version>/vmlinux and a `current` symlink, which is
#    the hearthd default. Rerun only to bump the pinned version, never for a host
#    kernel change.
sudo scripts/build-guest-kernel.sh

# 4. Start the daemon, then ask it to verify every host prerequisite.
sudo systemctl enable --now hearth.service
hearthctl ping                # "pong — hearthd <version> (pid <n>)"
hearthctl host check          # paths/commands/modules/keyring, ok=true each
```

### NixOS (or any declaratively-managed systemd)

`/etc/systemd/system` is read-only, so `make install`'s unit copy is skipped
(it installs the binaries and warns — it does not fail). Deploy the binaries and
run the unit from `/run` or your system config:

```sh
make build guest-bin
sudo make install-bin install-guest-payload # -> host binaries + guest updater payload
sudo cp systemd/hearth.service /run/systemd/system/   # runtime unit (survives until next boot)
sudo systemctl daemon-reload && sudo systemctl enable --now hearth.service
# For a persistent setup, add a systemd.services.hearth block to configuration.nix
# with ExecStart = "/usr/local/bin/hearthd" instead of the /run unit.
```

A code-only redeploy (e.g. after a daemon fix) builds unprivileged, then copies
and restarts privileged; the unit and paths do not change:

```sh
make build
sudo make install-bin
sudo systemctl restart hearth
```

### Optional agent-plane host service

`hearth-agentd` runs as its own user. It reads two secrets through systemd's
credential store and uses `/run/hearth-agentd/agent.sock` for `hearthctl agent`
commands. Install it only on hosts that need the agent plane:

```sh
# Build first as your normal user.
make build

# One-time host setup. The hearth group already owns /run/hearth.sock on a
# normal Hearth install; add operator accounts to it as needed.
sudo groupadd --system hearth 2>/dev/null || true
sudo useradd --system --gid hearth --home-dir /var/lib/hearth-agentd \
  --shell /usr/sbin/nologin hearth-agent
sudo install -d -m 0700 /etc/hearth/agent
sudo sh -c 'umask 077; openssl rand -hex 32 > /etc/hearth/agent/http-token'
sudo sh -c 'umask 077; openssl rand -hex 32 > /etc/hearth/agent/ref-key'

# This installs the unit and creates /etc/hearth/verb-policy.toml only when no
# policy exists. If you have a custom policy, merge the hearth-agent entry from
# systemd/hearth-agentd-verb-policy.toml yourself.
sudo make install-agentd
sudo systemctl restart hearth.service # reload the verb policy
sudo systemctl enable --now hearth-agentd.service
hearthctl agent ls
```

The unit uses `Group=hearth` so operators in that group can open the agent
socket. The explicit `hearth-agent` user rule in the verb policy takes priority
over the broad operator-group rule and denies machine life-cycle verbs such as
`create`, `start`, and `destroy`.

On NixOS, declare the same user, policy, credentials, and unit. Keep the secret
files out of the Nix store; the example assumes another secret manager creates
the two `/etc/hearth/agent/*` source files:

```nix
users.groups.hearth = {};
users.users.hearth-agent = {
  isSystemUser = true;
  group = "hearth";
  home = "/var/lib/hearth-agentd";
};

environment.etc."hearth/verb-policy.toml".source =
  /path/to/hearth/systemd/hearth-agentd-verb-policy.toml;

systemd.services.hearth-agentd = {
  description = "Hearth agent-plane host daemon";
  after = [ "hearth.service" "network-online.target" ];
  wants = [ "hearth.service" ];
  wantedBy = [ "multi-user.target" ];
  serviceConfig = {
    Type = "simple";
    User = "hearth-agent";
    Group = "hearth";
    UMask = "0007";
    LoadCredential = [
      "http-token:/etc/hearth/agent/http-token"
      "ref-key:/etc/hearth/agent/ref-key"
    ];
    ExecStart = "/usr/local/bin/hearth-agentd --token-file %d/http-token --ref-key-file %d/ref-key";
    Restart = "on-failure";
    RestartSec = 2;
    NoNewPrivileges = true;
    ProtectSystem = "strict";
    ProtectHome = true;
    PrivateTmp = true;
    ProtectKernelTunables = true;
    ProtectKernelModules = true;
    ProtectControlGroups = true;
    RestrictNamespaces = true;
    RestrictSUIDSGID = true;
    MemoryDenyWriteExecute = true;
    LockPersonality = true;
    StateDirectory = "hearth-agentd";
    StateDirectoryMode = "0750";
    RuntimeDirectory = "hearth-agentd";
    RuntimeDirectoryMode = "0750";
    ReadWritePaths = [ "/var/lib/hearth-agentd" ];
  };
};
```

`hearthctl host check` reports each directory, command, the guest
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
#   == sudo HEARTH_BRIDGE=hearth0 ./target/release/hearthd
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
make build guest-bin
sudo make install
sudo systemctl restart hearth.service
hearthctl ping        # confirm the new version answered
```

Running VMs are transient systemd units and survive a daemon restart;
`hearthd` reconciles them on start (see the `boot_config` note in §4). A guest
kernel bump (rare) means rebuilding with `scripts/build-guest-kernel.sh` and
restarting the affected VMs so they boot the new `current` kernel.

### Updating hearth-guestd in existing VMs

`make install` also installs the static musl guest payload outside the host's
`PATH`, under `PREFIX/lib/hearth/guest/hearth-guestd`. Upgrade one running VM,
or every eligible running VM, over the logged-in operator's normal OpenSSH
connection:

```sh
hearthctl upgrade hermes-a
hearthctl upgrade
```

The command connects as `agent`, so the matching key should already be
available through `ssh-agent` (a forwarded laptop agent is fine) and the VM's
host key should already be in the operator's normal `known_hosts`. SSH runs in
batch mode: it never falls back to a password prompt. The guest's passwordless
`sudo` installs the candidate atomically at `/usr/local/bin/hearth-guestd`,
restarts the existing `hearth-guestd.service`, and leaves the unit and its
drop-ins untouched. Hearth waits for a fresh boot report with the expected
version. A failed start or health check restores
`/usr/local/bin/hearth-guestd.previous`.

Development builds append their source commit to the package version, for
example `0.1.0+8c14e42`. The guest reports that same version, so upgrade output
identifies both the old and new guestd commits.

With no VM hostname, stopped VMs, guestd-less VMs, VMs without a resolved address,
and VMs with a running/queued agent task are informational skips. Managed-SSH
registry metadata is advisory: for every otherwise eligible VM, the command
attempts the operator's ordinary SSH access. An explicitly named VM that is not
eligible is an error. The active-task check reads guestd's durable task state
over that SSH connection, so it does not require access to the host's agentd
socket. Use `--force` to permit restarting guestd while a task is active; that
task will be recorded as `failed(guestd_restart)`. Awaiting-input and terminal
tasks do not block an upgrade.

For a development build or a payload not installed under the same `PREFIX` as
`hearthctl`, override the source explicitly:

```sh
hearthctl upgrade hermes-a \
  --from ./target/x86_64-unknown-linux-musl/release/hearth-guestd
```

This command updates only VMs that already run and report a guestd. It does not
install a unit, retrofit guestd-less images, start stopped VMs, or alter image
manifests.
