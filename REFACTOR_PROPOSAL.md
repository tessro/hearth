# Refactor Proposal: From Working Prototype to Industrial Boot Path

## Context

The Hermes VM bring-up (2026-07-08/09) proved the docker-rootfs pipeline end to
end: `Dockerfile → buildah → umoci → ext4 qcow2 → image-import → direct-kernel
boot → systemd → workload reachable on the bridge`. Getting there required a
series of point fixes, each correct in isolation, several of which are
workarounds rather than designs. This document inventories them and proposes
the industrial-grade replacement for each.

Every item was hit in practice; none is speculative.

### Revisions (2026-07-09)

Two directives amend the original draft:

1. **No Nix dependency.** Nothing in the build, boot, or deployment path may
   require Nix. The guest kernel is a pinned *vanilla* kernel.org kernel built
   by a plain shell script with a checked-in config (§1). The deployment story
   is a systemd unit plus `make install`, with distro-generic tool hints (§6.3).
   `devenv.nix` stays as an optional dev-shell convenience only; no shipped
   artifact or documented operator flow may depend on it.
2. **Multi-VM spawn from one template.** It must be possible to spawn multiple
   VMs from the same template (Dockerfile) with different names and different
   provisioned files, each via a single CLI command. This lands as
   `hearthctl spawn` (§10), built on the §3 provisioning step.

### Workaround inventory

| # | Workaround | Where | Failure it papers over |
|---|---|---|---|
| 1 | `buildah bud --network host` | `crates/hearthctl/src/oci.rs` | netavark iptables race between consecutive `RUN` steps |
| 2 | switch_root initramfs built from *host* kernel modules | `scripts/build-vm-initramfs.sh` | host kernel has virtio/ext4 as `=m`; breaks silently on every host kernel bump |
| 3 | Module set discovered by boot-iteration (`af_packet`, earlier `virtio_net`) | same | no defined guest driver contract |
| 4 | `udev` + unit symlinks hand-wired into the image | `example/hermes-vm/Dockerfile` | `--no-install-recommends systemd` ships no udev → NIC never initialized |
| 5 | `serial-getty@ttyS0` masked per-image | same | 90s boot hang when getty races the kernel console |
| 6 | Secrets `COPY`'d into the shared image qcow2 | `hermes.env` → `/home/agent/.hermes/.env` | no per-service customization step exists |
| 7 | Guest IP discovered by grepping serial log (`netdiag`, probe `addr=`) | `example/hermes-vm/netdiag`, `hermes-probe` | Hearth has no view of guest addresses |
| 8 | Reachability via hand-typed nft DNAT / ssh tunnel | operator shell | no managed publish path from LAN/tailnet to a VM port |
| 9 | Stale-daemon failures surfaced as `protocol.invalid_json: unknown variant image-import` | `hearth-proto` dispatch | no client/server version handshake |
| 10 | Dev daemon run as `sudo ./target/debug/hearthd` beside a dead `hearth.service` pointing at a deleted `/usr/local/bin/hearthd` | host | no packaged deployment; reconcile adopts stale VMs booted with old flags |
| 11 | `mkfs.ext4` found via devenv PATH surgery (`sudo -E env "PATH=$PATH"`) | `devenv.nix`, operator shell | build-time host deps are implicit |
| 12 | Boot health = grep serial log for `HERMES_PROBE ok` | `scripts/test-hermes-vm.sh` | no first-class readiness signal |
| 13 | Hermes installed by piping a moving `install.sh\|bash` at build time | `example/hermes-vm/Dockerfile` | unpinned supply chain, launcher path fragility |
| 14 | Legacy OCI-process path still shipped (`docker_run.rs` 888 lines, `hearth-runner`, `build-initramfs.sh`) | `crates/`, `scripts/` | two boot models to reason about; plan doc already calls it dead |

Confirmed non-issue: guest MAC honors CHV `--net mac=` (verified
`52:54:00:8c:22:12` == allocation == dnsmasq lease). The mid-session "MAC
mismatch" was an observation across a destroy/recreate. Keep it a non-issue by
asserting it (see §5).

---

## 1. Boot chain: replace the host-coupled initramfs

**Now.** `build-vm-initramfs.sh` copies `.ko` files out of
`/run/booted-system/kernel-modules` for whatever kernel the host happens to
run. The artifact is invisibly pinned: a `nixos-rebuild` that bumps the kernel
makes every docker-rootfs VM drop to a busybox shell on next boot, and nothing
detects the skew until then. The module list itself was discovered empirically
(virtio_blk → virtio_net → af_packet, one panic/failure at a time).

**Target.** A dedicated, versioned guest kernel with the VM contract compiled
in, no initramfs at all:

```
CONFIG_VIRTIO_PCI=y  CONFIG_VIRTIO_BLK=y  CONFIG_VIRTIO_NET=y
CONFIG_VSOCKETS=y    CONFIG_VIRTIO_VSOCKETS=y
CONFIG_EXT4_FS=y     CONFIG_PACKET=y      CONFIG_FUSE_FS/VIRTIO_FS=y
+ overlayfs, cgroup v2, netfilter set for Docker-in-guest (plan doc list)
```

- Build it from *vanilla* kernel.org sources with a plain script — no Nix.
  `scripts/build-guest-kernel.sh` downloads a pinned LTS tarball (exact version
  and sha256 constants in the script, bumped deliberately in a commit), applies
  the checked-in config `guest/kernel.config`, runs `make vmlinux`, and
  installs the artifact at `/var/lib/hearth/kernels/<version>/vmlinux` plus a
  `/var/lib/hearth/kernels/current` symlink (CHV direct-boots vmlinux; no
  bootloader, no compression constraints). Build deps are ordinary host
  packages (gcc, make, flex, bison, bc, libelf), checked up front by the
  script with distro-generic install hints.
- `guest_kernel` config default moves from `/run/booted-system/kernel` (a
  NixOS-ism) to `/var/lib/hearth/kernels/current/vmlinux`; `hearthd
  host-check` verifies presence.
- Image manifests (`<name>.hearth.toml`) gain `min_kernel_contract = 1` so a
  kernel that predates a required feature refuses at start, not at panic.

The dedicated kernel is now the *primary* deliverable, not the endgame: with
virtio/ext4 built in, docker-rootfs VMs boot with **no initramfs at all**, and
`scripts/build-vm-initramfs.sh` is deleted along with the host-kernel coupling.

**Bridge (only until the vanilla kernel lands).** If the initramfs path must
survive an interim period, make it self-healing instead of documenting the
coupling:

- Embed the kernel version in the initramfs filename:
  `vm-initramfs-<kver>.cpio.gz`. `hearthd` derives the expected name from
  `guest_kernel`'s version and **fails `start` with a one-line remedy** ("run
  scripts/build-vm-initramfs.sh") when missing — a clear error at start beats a
  busybox shell on serial.
- `hearthd start` already knows the manifest kind; this is ~30 lines in
  `docker_rootfs_argv` + config.
- The module list stays a constant in the script, commented with which failure
  each entry prevents (`af_packet`: networkd DHCPv4 raw sockets; etc.). This is
  the guest driver contract until the kernel makes it moot.

**Effort.** Bridge: small (1–2h). Kernel derivation: medium (a day incl. boot
matrix run). Removes inventory #2, #3.

## 2. Base-image contract: validate at build, not at boot

**Now.** Three of the session's four boot failures were image-content bugs that
only manifested at runtime, one boot each: missing `udev` (NIC `pending`
forever), missing `xz-utils` (installer dies mid-build), getty hang. The
"contract" for a bootable image lives in an example Dockerfile and people's
heads.

**Target.** Two pieces:

1. **A published base layer.** `example/vm-base/Dockerfile` (or an OCI image
   `hearth/vm-base` built in CI) that owns the boilerplate every VM image
   repeats: systemd + udev + networkd enablement symlinks, dhcp `.network`,
   `/usr/local/bin/init` mount shim, fstab with `x-systemd.growfs`,
   `serial-getty@ttyS0` mask, `STOPSIGNAL SIGRTMIN+3`. `hermes-vm` becomes
   `FROM hearth/vm-base` + Hermes install + units — the 60%-identical
   `agent-vm`/`hermes-vm` Dockerfiles stop drifting.

2. **A build-time linter.** After `umoci unpack`, before `mkfs.ext4`,
   `image_build.rs` walks the rootfs and rejects/warns:
   - reject: resolved OCI init not present/executable in rootfs; no
     `/etc/fstab` root entry; `init` is shell-form.
   - warn: no `systemd-udevd` enabled (NIC will stay unmanaged); no
     `.network` matching `en*`; no `sshd`; `serial-getty@ttyS0` unmasked.
   Each check exists because its absence cost a full build+boot cycle
   (~10 min) to diagnose from serial output. Table-driven, pure functions over
   the unpacked tree — unit-testable without KVM.

**Effort.** Base layer: small. Linter: medium (~200 lines + tests). Removes
inventory #4, #5, and the class behind #3.

## 3. Secrets and per-service customization: stop baking credentials into images

**Now.** `hermes.env` (dashboard password, signing key, model API keys) and
`authorized_keys` are `COPY`'d into the image. Consequences: secrets live
world-readable-to-root in `/var/lib/hearth/images/*.qcow2` and in the buildah
layer store; rotating a key means rebuilding the image; two services from one
image share credentials; `machine-id` and SSH host keys are cloned across every
VM from the image.

**Target.** The plan doc's deferred "offline customization" step, made real as
an explicit create/start-time phase in `hearthd` (it already owns the
disk-copy step in `create`):

- Service TOML grows:

  ```toml
  [provision]
  files = [
    { source = "/etc/hearth/secrets/hermes.env", dest = "/home/agent/.hermes/.env", mode = "0600", owner = "1000:1000" },
    { dest = "/home/agent/.ssh/authorized_keys", from_literal = "ssh-ed25519 ...", mode = "0600", owner = "1000:1000" },
  ]
  reset_machine_id  = true   # truncate /etc/machine-id
  reset_ssh_hostkeys = true  # rm /etc/ssh/ssh_host_*; ssh-keygen -A unit regenerates
  hostname = "hermes"
  ```

- Implementation (revised — zero new host deps, no libguestfs): per-service
  docker-rootfs disks switch from qcow2 to **sparse raw** (`qemu-img convert
  -O raw` at create; the base image in the store stays qcow2). The rootfs is a
  bare whole-device ext4 — no partition table — so provisioning is a plain
  `mount -o loop` of the per-VM disk by hearthd (which already runs as root):
  copy/write files, `chown`/`chmod`, truncate `/etc/machine-id`, remove SSH
  host keys, write `/etc/hostname`, unmount. One-shot at create time; the
  image stays generic. Raw also sidesteps CHV's qcow2 limitations (its reader
  already rejects backing chains, which is why create does a full-copy
  convert today) and costs nothing on a sparse file.
- Per-VM disk filenames must not lie about their format (no `.qcow2` suffix on
  a raw disk); cloud-image services keep qcow2 + seed ISO untouched.
- `source =` paths are read by hearthd (absolute, e.g.
  `/etc/hearth/secrets/…`); `from_literal` carries content inline in the
  service TOML. `hearthctl` conveniences (`spawn --provision-file`, §10) read
  local files client-side and send content as literals, so the CLI's cwd
  never matters to the daemon.
- This intentionally replaces cloud-init for docker-rootfs images (plan doc
  already rules cloud-init out) and is Hearth-native VM customization, not
  Docker emulation.
- `hostname` defaults to the service name, so N services from one image get
  distinct identities without configuration.

**Effort.** Medium (guestfish path ~1 day incl. tests against a fixture qcow2).
Removes inventory #6; also fixes cloned machine-id/host keys, which today make
two services from one image collide in dnsmasq (duplicate DUIDs) and SSH
known-hosts.

## 4. Networking: managed addresses and managed publish

**Now.** The guest's IP is discovered by grepping the serial log for a probe
line; reachability from a Mac was an ssh `-L` tunnel or a hand-typed
`nft add rule ... dnat`. Neither survives an IP change, host reboot (runtime
nft rule), or a second service. Meanwhile Hearth already owns the two halves of
the answer: it allocates the MAC (`allocations.toml`), and the NixOS module
owns dnsmasq + nftables.

**Target.** Three layers, all inside Hearth's existing "opinionated about the
VM surface" scope (this is VM port-forwarding, not Docker `-p` emulation — no
per-container semantics, no healthcheck coupling):

1. **Address visibility.** `hearthd` reads the dnsmasq lease file (path in
   config, default `/var/lib/dnsmasq/dnsmasq.leases`), joins on the service
   MAC, and `status`/`ls` gain an `address` field. ~50 lines, no new deps,
   kills the `netdiag`-over-serial hack (inventory #7). The probe script's
   `addr=` line stays as a boot-time debug nicety only.
2. **Address stability.** `create` appends `dhcp-host=<mac>,<ip>` to a
   dnsmasq drop-in dir (`/etc/dnsmasq.d/hearth/` or the NixOS-managed
   equivalent) allocating from a reserved slice of the range, and `destroy`
   removes it + `SIGHUP`s dnsmasq. IPs become part of the allocation map next
   to CID and MAC, where they belong.
3. **Publish.** Service TOML:

   ```toml
   [[publish]]
   host_port  = 9119
   guest_port = 9119
   protocol   = "tcp"
   bind       = "100.121.19.41"   # optional; default all host addrs
   ```

   `hearthd` owns an nftables table (`table ip hearth_nat`) it fully rewrites
   from the registry on start/stop/reconcile — same pattern as tap setup,
   idempotent, survives daemon restarts, leaves the NixOS-owned NAT table
   untouched. `status` shows publishes next to the address.

**Effort.** (1) small; (2) small-medium; (3) medium. Removes inventory #7, #8.

## 5. Acceptance tests: assert the contract that bit us

**Now.** `test-agent-vm.sh` / `test-hermes-vm.sh` grep serial logs and stop at
"probe ok". Everything this session actually broke on — no lease, wrong MAC
assumptions, unreachable port — was outside the asserted surface.

**Target.** Extend the harness (still bash + `hearthctl --json` + `jq`, no
framework):

- assert `status` reports an address (once §4.1 lands) and that
  `curl http://<address>:<port>` answers from the host;
- assert guest MAC == allocated MAC (locks in the verified non-issue);
- assert boot-to-probe wall time under a budget (the getty hang was a silent
  90s regression that only eyeballs caught);
- keep the stop/start persistence check; add a host-kernel-skew test for the
  §1 bridge (point `HEARTH_GUEST_INITRAMFS` at a mismatched artifact, expect a
  clean `start` error, not a hung VM).
- a `hearthctl wait <name> --marker 'HEARTH_PROBE ok' --timeout 300`
  (client-side tail of the existing `logs` stream) to replace the three copies
  of `wait_for_log()`.

**Effort.** Small, incremental with §4.

## 6. Daemon/ops hygiene: version handshake and one deployment story

**Now.** The single most confusing failure of the session was
`protocol.invalid_json: unknown variant image-import` — a *serde* error
standing in for "your daemon is 3 weeks older than your CLI". Separately, a
systemd `hearth.service` was running a deleted `/usr/local/bin/hearthd` while
the real daemon was `sudo ./target/debug/hearthd` in a terminal, and the new
daemon silently adopted a VM booted by the old one with pre-initramfs flags
(reconcile treats "unit exists" as "mine, correctly configured").

**Target.**

1. **Handshake.** `hearthctl` sends `version` first on every connection (it's
   one line-JSON round trip, negligible); on any subsequent
   `protocol.invalid_json` / unknown-verb error it reports:
   `daemon 0.1.0 (started 2026-05-28) does not support 'image-import'; hearthctl is 0.2.0 — restart hearthd`.
   Alternatively (cheaper): `version_result` grows a `verbs: [...]` list and
   the CLI pre-checks. Either way the serde error never reaches a human.
2. **Boot-config fingerprint in reconcile.** The transient unit already embeds
   the full CHV argv in its `Description`. On startup reconcile, `hearthd`
   compares the running unit's argv against what it *would* launch now
   (kernel, initramfs, cmdline) and flags drift in `status`
   (`boot_config: stale`) instead of adopting silently. Restart remains the
   operator's call; invisibility is the bug.
3. **One deployment story (no Nix).** `make install` copies release binaries
   to `/usr/local/bin` and installs `systemd/hearth.service` (kept current in
   this repo: correct ExecStart, kernel/env paths, restart policy). An
   `docs/operations.md` section lists the host prerequisites — `buildah`,
   `umoci`, `qemu-img`, `e2fsprogs`, `cloud-hypervisor`, dnsmasq + bridge —
   with distro-generic package hints, ending the `sudo -E env "PATH=$PATH"`
   era (inventory #11). Dev loop keeps `target/debug/hearthd`, but README
   documents it as *the* alternative, and `hearthctl ping` prints the
   daemon's version+pid so you always know which one answered.
4. **Build preflight.** `hearthctl image build` checks its external tools
   up front and fails with a distro-generic package hint (`mkfs.ext4 not
   found — install e2fsprogs`) instead of `spawn mkfs.ext4 ... No such file
   or directory` after a 10-minute build. ~20 lines.

**Effort.** (1)+(4) small; (2) small-medium; (3) medium. Removes inventory #9,
#10, #11.

## 7. Image build isolation: make `--network host` a choice, not a global

**Now.** `--network host` was applied unconditionally in `buildah_bud_args` to
dodge the netavark chain race. Right call for this host, but it's a behavioral
global: every `RUN` step in every build now shares the host netns (can reach
`hearth0`, the daemon socket's network namespace, link-local metadata, etc.).
For a single-operator homelab this is acceptable; it shouldn't be invisible.

**Target.**

- `hearthctl image build --build-network {host|netavark}` defaulting to `host`
  (documented: VM-rootfs builds are trusted, netavark is broken on this
  host-config as of buildah 1.43/netavark) — the flag exists so the day
  netavark is fixed or a multi-user host appears, the default can flip without
  an API change.
- Pass through `--build-arg` while touching this surface: it is the natural
  fix for half of §3's "secrets in context files" (non-secret parameters like
  `HERMES_BRANCH`), and `image_build.rs` already forwards a Vec of args.
- Enable `--layers` for the buildah invocation: the session paid the full
  ~6-minute Hermes reinstall for a one-line apt change *three times*. Layer
  caching is buildah-native and free.

**Effort.** Small. Contains inventory #1, improves #13's iteration cost.

## 8. Example hardening: pin the Hermes supply chain

**Now.** The image runs `curl … install.sh | bash` at build time: unpinned
branch, unverified script, launcher path (`~/.local/bin/hermes`) asserted by a
README note. Rebuilds are non-reproducible by construction.

**Target.**

- Pin: `install.sh … --branch main --commit <sha>` (the installer already
  supports `--commit`); record the sha in a build arg with the tested default.
  Bump deliberately, in a commit.
- Verify: `ExecStart=/usr/bin/env hermes serve …` is *not* the fix (PATH
  games); instead the Dockerfile ends with
  `RUN test -x /home/agent/.local/bin/hermes && su - agent -c 'hermes --version'`
  so a moved launcher fails the build, not the first boot (§2's linter can
  carry this as an image-local check).
- The probe asserting `:9119` answers stays; it caught real breakage twice
  this session.

**Effort.** Small. Removes inventory #13.

## 9. Delete the legacy OCI-process path

**Now.** 1,273 lines across `docker_run.rs`, `hearth-runner`, and
`build-initramfs.sh` implement the pre-VM model: virtiofs root, `run.json`,
chroot runner, exit-status files, poweroff-on-exit. The plan doc's open
question ("keep `hearthctl run` as a legacy smoke tool?") has an empirical
answer from this session: it was never once useful, and its initramfs
(virtiofs, no virtio_blk) actively confused the boot-path diagnosis — two
same-named artifacts (`initramfs.cpio.gz` vs `vm-initramfs.cpio.gz`) with
opposite contracts.

**Target.** Remove `hearthctl run`, `hearth-runner`, `scripts/build-initramfs.sh`,
and the `docker_run.rs` module; keep the shared buildah/umoci helpers that
`image_build.rs` already imports from `oci.rs` (move the two arg-builder tests
there). Smoke-testing a Dockerfile becomes `image build` + `create` + `start`
against a throwaway service — the thing we actually do now.

**Effort.** Small-medium (mostly deletion + test moves). Removes inventory #14.

## 10. `hearthctl spawn`: N VMs from one template, one command each

**Now.** Standing up a second Hermes VM means: rebuild (or reuse) the image,
`create`, hand-edit secrets *into the image* (they're `COPY`'d — see §3), and
`start` — four steps, and the two VMs would share credentials, machine-id, and
host keys anyway.

**Target.** One command that goes from a built template (or straight from a
Dockerfile) to a running, individually-provisioned VM:

```
hearthctl spawn hermes-a \
  --image hermes-vm \
  --dockerfile example/hermes-vm/Dockerfile --context example/hermes-vm \
  --provision-file source=./a.env,dest=/home/agent/.hermes/.env,mode=0600,owner=1000:1000 \
  --cpu 4 --mem 4096 --disk 32
hearthctl spawn hermes-b \
  --image hermes-vm \
  --provision-file source=./b.env,dest=/home/agent/.hermes/.env,mode=0600,owner=1000:1000
```

- `spawn` = (build image locally iff `--dockerfile` given and the image does
  not exist yet) → `create` with provisioning args → `start`. Pure CLI-side
  composition of existing verbs; the registry format from §3 remains the
  contract.
- `--provision-file` is repeatable; `source=` is read client-side and sent as
  literal content, `dest`/`mode`/`owner` map straight onto §3's `[provision]`
  entries. `--hostname` overrides the default (the service name).
- Two spawns from the same image differ in name, hostname, machine-id, SSH
  host keys, and provisioned files — nothing is shared but the immutable
  image, which is the point.
- Depends on §3 (provisioning) and composes with §7 (`--build-arg`,
  `--layers` make the build-if-missing path cheap).

**Effort.** Small once §3 lands (CLI composition + arg parsing + tests).

---

## Sequencing

Ordered by (risk removed ÷ effort), respecting dependencies:

1. **§9** — legacy deletion first: shrinks the surface every later step
   touches (`oci.rs`, `main.rs`), and nothing depends on it.
2. **§1 vanilla kernel** — `build-guest-kernel.sh` + checked-in config +
   start-time validation. Defuses the one landmine guaranteed to fire (next
   host kernel bump) by removing the host coupling entirely; the initramfs
   bridge is only kept if an interim period demands it.
3. **§6.1/6.4 + §7** — version handshake, build preflight, `--layers`,
   `--build-network`/`--build-arg` flags. All small; each converts a
   session-scale confusion into a one-line error or a 6-minute save.
4. **§3** — provisioning step (raw per-VM disks + loop-mount); secrets leave
   the image. Unblocks §10 and key rotation.
5. **§2 + §8** — vm-base layer + build-time linter + pinned Hermes example.
   Prevents the next image-content class bug from costing a boot cycle.
6. **§4** — address in `status`, static leases, managed publish. Ends the
   ssh-tunnel/DNAT hand-work.
7. **§10** — `hearthctl spawn`. Small once §3 lands.
8. **§5 + §6.3** — acceptance tests asserting the new contract, plus the
   `make install` deployment story and ops docs.

## Non-goals (unchanged from ARCHITECTURE.md, restated against these changes)

- No Docker port/volume/healthcheck emulation — `[[publish]]` is VM
  port-forwarding owned by the registry, nothing more.
- No general image-build service — the linter narrows what Hearth accepts; it
  does not widen what it runs.
- No cloud-init for docker-rootfs images — §3 is Hearth-native provisioning
  with an explicit, inspectable file list.
- No multi-host anything.
