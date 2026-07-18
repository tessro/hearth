# Hearth Operations

Everything an operator needs to install `hearthd` on one x86-64 Linux host and
keep it current. NixOS, Debian-family, Fedora-family, portable tarball, and
source installs use the same paths and service rules.

## 1. Host prerequisites

Hearth shells out to host tools rather than bundling them. Install these before
`hearthctl host check` will go green. Package names are distro-generic (`apt` =
Debian/Ubuntu, `dnf` = Fedora/RHEL); a few tools ship only as upstream release
binaries and are called out as such.

### Runtime — the daemon needs these to boot and network VMs

| Tool | Provides | apt | dnf |
| --- | --- | --- | --- |
| `cloud-hypervisor` | the VMM Hearth launches per VM | `cloud-hypervisor` | `cloud-hypervisor` |
| `qemu-img` | per-VM disk create/convert | `qemu-utils` | `qemu-img` |
| `nft` | the `hearth_nat` publish table | `nftables` | `nftables` |
| `dnsmasq` | guest DHCP + leases on the bridge | `dnsmasq` | `dnsmasq` |
| `ip` | tap/bridge wiring | `iproute2` | `iproute` |
| `socat` | agent-in-charge vsock proxy | `socat` | `socat` |
| KVM | `/dev/kvm`, `kvm` + `vhost_vsock` modules | kernel | kernel |

If the base distribution does not carry `cloud-hypervisor`, enable the Cloud
Hypervisor Open Build Service repository for that distribution before installing
Hearth. Hearth packages depend on the host package and never bundle or download
the hypervisor.

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
- **A drop-in dir Hearth can write**, default `/var/lib/hearth/dnsmasq.d`
  (override `HEARTH_DNSMASQ_DROPIN_DIR`). Point dnsmasq at it, e.g.
  `conf-dir=/var/lib/hearth/dnsmasq.d`. On `create` Hearth writes `<id>.conf`
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

### NixOS

Add the flake input and module. Nix builds Hearth and the pinned guest kernel;
the module installs Cloud Hypervisor and the other runtime tools.

```nix
{
  inputs.hearth.url = "github:tessro/hearth";
  outputs = { nixpkgs, hearth, ... }: {
    nixosConfigurations.host = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        hearth.nixosModules.default
        {
          services.hearth = {
            enable = true;
            authorizedKeys = [ "ssh-ed25519 AAAA... operator" ];
            operatorUsers = [ "operator" ];
            networking = {
              manage = true;
              uplinkInterface = "enp1s0";
            };
          };
        }
      ];
    };
  };
}
```

Set `networking.manage = false` to leave the bridge, dnsmasq, forwarding, and
NAT unchanged. You can replace `package`, `cloudHypervisorPackage`, and
`guestKernel` through module options.

For the agent plane, secret options name runtime source paths. Do not use Nix
path literals, since those copy data to the store:

```nix
services.hearth.agentPlane = {
  enable = true;
  httpTokenFile = "/run/agenix/hearth-http-token";
  refKeyFile = "/run/agenix/hearth-ref-key";
};
```

### Debian and RPM packages

Enable a distribution or Open Build Service repository that provides
`cloud-hypervisor`, then install the local release file. These commands use the
OBS repositories tested by CI for Debian 13 and Fedora 42; select the matching
directory from the [Cloud Hypervisor OBS
project](https://download.opensuse.org/repositories/home:/cloud-hypervisor/)
for another supported release.

```sh
# Debian 13
curl -fsSL https://download.opensuse.org/repositories/home:/cloud-hypervisor/Debian_13/Release.key \
  | sudo gpg --dearmor -o /usr/share/keyrings/cloud-hypervisor.gpg
echo 'deb [signed-by=/usr/share/keyrings/cloud-hypervisor.gpg] https://download.opensuse.org/repositories/home:/cloud-hypervisor/Debian_13/ /' \
  | sudo tee /etc/apt/sources.list.d/cloud-hypervisor.list
sudo apt update

# Fedora 42
sudo curl -fsSL \
  https://download.opensuse.org/repositories/home:/cloud-hypervisor/Fedora_42/home:cloud-hypervisor.repo \
  -o /etc/yum.repos.d/cloud-hypervisor.repo
```

The package manager resolves `qemu-img`, nftables, dnsmasq, iproute, socat,
systemd, and the host C library. Units stay disabled after install.

```sh
sudo apt install ./hearth_0.1.0_amd64.deb
# or
sudo dnf install ./hearth-0.1.0-1.x86_64.rpm

sudo install -m 0644 ~/.ssh/id_ed25519.pub /etc/hearth/authorized_keys
sudo systemctl enable --now hearth.service
hearthctl ping
hearthctl host check
```

The package keeps `/etc/hearth/verb-policy.toml` as admin-owned config. It does
not create keys or tokens and does not change the uplink, firewall, bridge, or
dnsmasq setup.

### Portable tarball

Install the runtime tools in §1, extract the archive under a prefix, and copy or
adapt its systemd units. All Hearth programs in the tarball are musl-static.

```sh
tar -xzf hearth-0.1.0-x86_64-linux.tar.gz
sudo cp -a hearth-0.1.0/{bin,lib,share} /usr/local/
sudo cp hearth-0.1.0/etc/hearth/verb-policy.toml /etc/hearth/
```

The included units use `/usr` paths. If you keep the files under `/usr/local`,
change only `ExecStart` and `PATH` in local unit drop-ins. The tarball does not
include Cloud Hypervisor, host network setup, secrets, authorized keys, state,
images, or VM disks.

### Source install

Source builds need Rust 1.94.1, a musl linker for `hearth-guestd`, the kernel
build tools in §1, `readelf`, and the runtime tools. The default install prefix
is `/usr` and uses the staged native layout:

```sh
devenv shell -- make release-stage
sudo make install
sudo install -d -m 0755 /etc/hearth
sudo install -m 0644 ~/.ssh/id_ed25519.pub /etc/hearth/authorized_keys
sudo systemctl enable --now hearth.service
hearthctl ping
hearthctl host check
```

### Optional agent-plane host service

`hearth-agentd` runs as its own user. It reads two secrets through systemd's
credential store and uses `/run/hearth-agentd/agent.sock` for `hearthctl agent`
commands. Install it only on hosts that need the agent plane:

```sh
sudo install -d -m 0700 /etc/hearth/agent
sudo sh -c 'umask 077; openssl rand -hex 32 > /etc/hearth/agent/http-token'
sudo sh -c 'umask 077; openssl rand -hex 32 > /etc/hearth/agent/ref-key'
sudo systemctl restart hearth.service # reload the verb policy
sudo systemctl enable --now hearth-agentd.service
hearthctl agent ls
```

Packages, source installs, and the NixOS module create the `hearth` group and
`hearth-agent` user and install both units. They do not create these two secret
files or enable agentd. If you have a custom policy, merge the `hearth-agent`
rule from the installed default before restarting hearthd.

The unit uses `Group=hearth` so operators in that group can open the agent
socket. The explicit `hearth-agent` user rule in the verb policy takes priority
over the broad operator-group rule and denies machine life-cycle verbs such as
`create`, `start`, and `destroy`.


`hearthctl host check` reports each directory, command, the guest
kernel, `/dev/kvm`, the bridge, and the `kvm`/`vhost_vsock` modules. Green there
means `create`/`start` will not fail on a missing prerequisite. `hearthctl image
build` and `scripts/build-guest-kernel.sh` additionally preflight their own build
tools, so a missing `mkfs.ext4` or `libelf-dev` fails up front with an install
hint rather than mid-build.

`make uninstall` removes the binaries, unit, and doc (and daemon-reloads).

## 4. Development restart loop

Build as the current user, copy the exact build under `/run`, add runtime-only
unit drop-ins, restart, and ping the new daemon with one command:

```sh
make dev-restart
make dev-restart-agent-plane # also builds guestd and prints an upgrade command
make dev-reset               # remove only Hearth's runtime files and drop-ins
```

The helper restarts agentd only if it was active before the build. A failed
build changes no unit. A failed restart prints unit status and recent logs.
It never writes to `/usr`, `/etc`, or `/nix/store`. The agent-plane form never
changes a running VM without the separate printed `hearthctl upgrade` command.

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

## 6. Network setup by host type

The NixOS module can own `hearth0`, dnsmasq, forwarding, and NAT. On a
NetworkManager host, create a persistent bridge connection with the address in
§2, attach no physical interface to it, run dnsmasq on that bridge, and add a
masquerade rule for the uplink. On a systemd-networkd host, use a bridge
`.netdev` and matching `.network` file, then give dnsmasq the same dynamic range
and `/var/lib/hearth/dnsmasq.d` config dir. In both cases keep the static and
dynamic ranges separate.

## 7. Releases

CI reads Nix build results from the public `tessro` Cachix cache. To let
trusted main-branch and release runs add new results, create a write token for
that cache and store it as the `CACHIX_AUTH_TOKEN` GitHub Actions repository
secret. Pull requests use the cache read-only.

The checked-in Cargo version is the last stable release. Run the Release
workflow with a `major`, `minor`, or `patch` bump. It makes a candidate version
commit and builds that exact commit across the Rust, Nix, package, install, VM,
unit, static-link, and stable-archive jobs. CI builds `.deb` on Debian-family
Linux, `.rpm` on Fedora-family Linux, the portable tarball with static host
tools, and Nix packages in Nix.

Keep `dry_run` on to test the full build without changing the repository:

```sh
gh workflow run release.yml --ref main -f bump=minor -f dry_run=true
gh run watch
# After it finishes, use the run ID shown by `gh run list`:
gh run download RUN_ID -n release-bundle
```

A dry run uploads `release-bundle` with the packages, archive, checksums, and
SBOM, then discards the candidate commit. For a release, run the same command
with `dry_run=false`. After every build passes, the protected publish job checks
that `main` has not moved, atomically pushes the tested version commit and its
`vX.Y.Z` tag, attests the files, and publishes the GitHub Release. The first
release starts from the checked-in `0.0.0`, so use `bump=minor` for `v0.1.0`.
Repository rules must let GitHub Actions update `main` and create release tags.
Use the `release` environment to require approval before the publish job.

The release contains the three package files, `SHA256SUMS`, an SPDX JSON SBOM,
and GitHub build attestations. Verify a file with `sha256sum -c SHA256SUMS` and
an attestation with `gh attestation verify <file> --repo tessro/hearth`.
AArch64 is not supported yet.
