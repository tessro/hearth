# Hearth Distribution Plan

## Scope

Support three ways to use Hearth:

1. Install and run it on NixOS through an in-repo flake and NixOS module.
2. Install it on other Linux systems through `.deb`, `.rpm`, or `.tar.gz`
   release files.
3. Build and restart local code with one short development command.

The first release target is `x86_64-linux`. Add AArch64 only after the guest
kernel, VM images, and live VM tests support it.

This work does not add state migration code, package hooks, activation steps,
or CLI migration commands. Hearth has no installed user base that needs them.
The one current host gets a short manual cutover note at the end of this file.

## Main choices

### One version

- Put the release version in `[workspace.package]` in `Cargo.toml`.
- Make each crate use `version.workspace = true`.
- Let the release workflow bump the chosen version component in a candidate
  commit, then create the matching `vX.Y.Z` tag only after that commit passes.
- Release builds report `X.Y.Z`.
- Local builds may report `X.Y.Z+<short-git-sha>`.
- Set the workspace repository URL to `https://github.com/tessro/hearth`.

### One staged install tree

Build one staged FHS tree for `.deb`, `.rpm`, and `.tar.gz` output. It contains
only Hearth files. Nix builds the same Hearth parts into store paths.

| Part | NixOS | `.deb` / `.rpm` | `.tar.gz` |
| --- | --- | --- | --- |
| `hearthd`, `hearthctl`, `hearth-agentd` | Nix store | `/usr/bin` | `bin/` under the archive prefix |
| Static `hearth-guestd` | Nix store | `/usr/lib/hearth/guest` | `lib/hearth/guest` |
| Guest kernel | Nix store | `/usr/lib/hearth/kernel` | `lib/hearth/kernel` |
| Cloud Hypervisor | Nix package dependency | Package dependency | Host prerequisite |
| systemd units | NixOS module/package | `/usr/lib/systemd/system` | `lib/systemd/system` |
| Public config | `/etc` links to store | `/etc/hearth` config files | Examples only |
| Mutable state | `/var/lib/hearth*` | `/var/lib/hearth*` | Host-chosen paths |
| Secrets | Runtime secret paths | Admin-created runtime files | Admin-created runtime files |
| Sockets | `/run` | `/run` | `/run` |

### Cloud Hypervisor comes from the host

Do not copy, download, or pin a Cloud Hypervisor binary in Hearth packages.

- The NixOS module adds `pkgs.cloud-hypervisor`, with an option to replace that
  package when needed.
- The Debian and RPM metadata declares `cloud-hypervisor` as a dependency.
- The install docs explain how to enable a repository that carries the package
  when the base distribution does not. Cloud Hypervisor publishes packages
  through the Open Build Service.
- The tarball and source-build docs list Cloud Hypervisor as a prerequisite.
- CI records and tests the Cloud Hypervisor version used by its live smoke test.
- `hearthctl host check` should show the found Cloud Hypervisor path and version,
  so a wrong or missing package has a clear result.

Do the same for other host tools where package names permit it: `qemu-img`,
`nft`, `dnsmasq`, `ip`, `socat`, and systemd. Keep build-only tools such as
Buildah, umoci, and `mkfs.ext4` out of the daemon's base runtime dependency set;
document them for `hearthctl image build` and use weak package dependencies when
the package format supports them.

## Phase 1: Set version and path rules

Change:

- `Cargo.toml`
- Each crate's `Cargo.toml`
- `crates/hearth-guestd/build.rs`
- `crates/hearth-guestd/src/lib.rs`
- `crates/hearthd/src/config.rs`

Tasks:

1. Use the workspace version in every shipped crate.
2. Add one release-build flag used by all binaries.
3. Keep the Git suffix only for non-release builds.
4. Move mutable defaults out of `/etc`:
   - services: `/var/lib/hearth/services`
   - allocations: `/var/lib/hearth/allocations.toml`
   - generated dnsmasq files: `/var/lib/hearth/dnsmasq.d`
5. Keep public SSH keys and the verb policy under `/etc/hearth`.
6. Keep agent tokens and keys outside the Nix store and out of release files.
7. Add no fallback reads from the old paths and no automatic file moves.

Checks:

- All shipped binaries print the same base version.
- Tagged release builds have no Git suffix.
- Hearth can run while `/etc` is read-only.
- Tests fail if mutable defaults move back under `/etc`.

## Phase 2: Build release parts and a staged tree

Add:

```text
packaging/
  nfpm.yaml
  systemd/
  sysusers.d/hearth.conf
  tmpfiles.d/hearth.conf
scripts/
  stage-release.sh
  verify-release.sh
```

Extend the Makefile with:

```text
make release-bins
make release-stage
make release-packages
make release-check
```

Tasks:

1. Build `hearthd`, `hearthctl`, `hearth-agentd`, and `hearth-guestd` for
   `x86_64-unknown-linux-musl`.
2. Fail if `readelf` finds a dynamic interpreter in any shipped Hearth binary.
3. Build the pinned Hearth guest kernel and include its contract file.
4. Stage the binaries, guest payload, guest kernel, units, sysusers rule,
   tmpfiles rule, public policy, docs, and license.
5. Make archive output stable: fixed file order, modes, owners, and timestamps
   from `SOURCE_DATE_EPOCH`.
6. Do not stage authorized keys, secret files, mutable registry files, VM disks,
   VM images, or Cloud Hypervisor.

The staged tree is the input to nFPM and the tarball builder. Nix may use the
same scripts where that keeps file names and modes in sync, but must not copy
non-Nix executables into its package.

Checks:

- Each binary runs `--version` on a clean Linux test host.
- `hearth-guestd` sits at the path that `hearthctl upgrade` derives from the
  `hearthctl` prefix.
- The guest kernel contract matches the kernel build input.
- Two builds from the same source and epoch produce the same archive checksum.

## Phase 3: Add the flake and NixOS module

Add:

```text
flake.nix
flake.lock
nix/
  package.nix
  guest-kernel.nix
  module.nix
  tests/module.nix
```

Export:

```text
packages.x86_64-linux.hearth
packages.x86_64-linux.guest-kernel
packages.x86_64-linux.default
nixosModules.default
checks.x86_64-linux.*
```

The module defines:

```nix
services.hearth = {
  enable;
  package;
  cloudHypervisorPackage;
  guestKernel;
  authorizedKeys;
  operatorUsers;

  agentPlane = {
    enable;
    httpTokenFile;
    refKeyFile;
  };

  networking = {
    manage;
    bridge;
    address;
    staticRange;
    dynamicRange;
    uplinkInterface;
  };
};
```

Tasks:

1. Declare the `hearth` group and `hearth-agent` system user.
2. Add named operator accounts to the group.
3. Install the Hearth package, Cloud Hypervisor package, and runtime tools.
4. Define both systemd units with store paths.
5. Use `StateDirectory`, `RuntimeDirectory`, and `LogsDirectory`.
6. Load `kvm` and `vhost_vsock`.
7. Write public policy and SSH key files through `environment.etc`.
8. Require secret source paths when the agent plane is on.
9. When `networking.manage = true`, declare `hearth0`, dnsmasq, forwarding, NAT,
   and separate static and dynamic DHCP ranges.
10. When `networking.manage = false`, leave all host network setup alone.

Checks:

- `nix flake check` evaluates every package and module output.
- A NixOS VM test starts `hearthd`, runs `hearthctl ping`, and checks its user,
  group, unit, state, log, config, and socket paths.
- A second test enables agentd with test credentials and checks its own state
  and runtime directories.
- A network test checks the managed bridge and dnsmasq config without needing
  nested KVM.
- The test confirms that mutable Hearth files do not appear under `/etc`.

## Phase 4: Build `.deb`, `.rpm`, and `.tar.gz` files

Use nFPM to form `.deb` and `.rpm` files from the staged tree. Use a small,
checked-in archive script for `.tar.gz`.

Package rules:

1. Depend on Cloud Hypervisor and the normal runtime tools by their package
   names for each format.
2. Use weak dependencies for image-build tools when possible.
3. Install both units but enable neither one during package installation.
4. Run `systemd-sysusers` and `systemd-tmpfiles` from package hooks.
5. Reload systemd after install, upgrade, or removal.
6. Preserve `/etc/hearth/verb-policy.toml` as admin-owned package config.
7. Never generate tokens or SSH keys.
8. Never change the host uplink, firewall, bridge, or dnsmasq config from a
   package hook.
9. Never inspect or move files from old Hearth paths.

Tests:

- Install the `.deb` on a chosen Debian or Ubuntu CI image.
- Install the `.rpm` on a chosen Fedora-family CI image.
- Let the package manager resolve Cloud Hypervisor and all base dependencies.
- Check file owners, modes, config handling, units, sysusers, and tmpfiles.
- Run `systemd-analyze verify` on both units.
- Run each Hearth binary's `--version`.
- Extract the tarball under a temporary prefix and run the same file and
  version checks.

A release `.deb` supports `apt install ./hearth_<version>_amd64.deb` after its
dependencies can be resolved. A public `apt install hearth` or
`dnf install hearth` service needs a signed package repository. Add that only
after choosing its host and signing-key process; it does not block release
files on GitHub.

## Phase 5: Add the local build-and-restart flow

Add:

```text
scripts/dev-restart.sh
make dev-restart
make dev-restart-agent-plane
make dev-reset
```

`make dev-restart` must:

1. Build as the current user through the pinned development environment.
2. Copy host binaries to `/run/hearth-dev/<git-sha>/`.
3. Write runtime-only systemd drop-ins under `/run/systemd/system`.
4. Override only `ExecStart` and the command `PATH`.
5. Restart `hearthd`.
6. Restart agentd only when it was active before the build.
7. Run the newly built `hearthctl ping`.
8. Print the version and PID that answered.

It must not write to `/usr`, `/etc`, or `/nix/store`. A reboot or
`make dev-reset` restores the installed units. `make dev-reset` removes only
the known Hearth runtime drop-ins and `/run/hearth-dev` files.

`make dev-restart-agent-plane` also builds the static guest payload. It should
print the exact `hearthctl upgrade --from <path>` command, but it must not
upgrade running VMs without an explicit second command from the developer.

Tests:

- Test the helper with a fake `systemctl` and a temporary root.
- Check that a failed build leaves the running daemons untouched.
- Check that a failed restart reports unit status and recent logs.
- Run one live build-and-restart smoke test on the NixOS host.

## Phase 6: Add CI and tag releases

Add:

```text
.github/workflows/ci.yml
.github/workflows/release.yml
```

### Pull request and main checks

Run:

1. `devenv shell -- make check`
2. `nix flake check`
3. All Nix package builds
4. `.deb`, `.rpm`, and `.tar.gz` builds
5. Package install checks
6. NixOS VM tests
7. Stable-archive comparison

Use the same release targets in pull-request CI and release-candidate CI. Do not
keep a second set of release-only build commands.

### Stable release workflow

Trigger by hand with a `major`, `minor`, or `patch` choice and a dry-run
checkbox. Build an exact candidate commit before changing `main`. A dry run
uploads the release bundle and stops. A real run pushes the tested commit and
matching `vX.Y.Z` tag only after every build passes.

Release gates:

1. The tag version equals the Cargo workspace version.
2. Cargo builds with `--locked`.
3. Rust, Nix, package, and NixOS VM tests pass.
4. Every package and binary contains the tag version.
5. Static-link checks pass.
6. Package dependency checks find Cloud Hypervisor through the package manager.
7. Generate `SHA256SUMS` and an SPDX SBOM.
8. Add GitHub build-provenance attestations.
9. Create a non-draft GitHub Release with generated notes.
10. Attach `.deb`, `.rpm`, `.tar.gz`, checksums, and SBOM files.

Keep default workflow permissions read-only. Grant `contents: write`,
`attestations: write`, and `id-token: write` only to the final jobs that need
them. Pin third-party actions to full commit IDs.

Expected first-release files:

```text
hearth-0.1.0-x86_64-linux.tar.gz
hearth_0.1.0_amd64.deb
hearth-0.1.0-1.x86_64.rpm
hearth-0.1.0.spdx.json
SHA256SUMS
```

## Phase 7: Update operator and development docs

Update:

- `README.md`
- `docs/operations.md`
- `AGENTS.md`

Document:

1. Nix flake input and module use.
2. Debian and RPM repository prerequisites, including Cloud Hypervisor.
3. `.deb`, `.rpm`, and tarball install steps.
4. Source-build prerequisites.
5. Network setup for NixOS, NetworkManager, and systemd-networkd hosts.
6. Authorized keys and agent-plane secret setup.
7. `make dev-restart`, agent-plane payload builds, and reset behavior.
8. Tag creation, release checks, checksums, SBOMs, and attestations.
9. The supported CPU architecture and tested host distributions.

Remove the current NixOS advice that copies mutable binaries into `/usr/local`
or a unit into `/run`. Keep `make install` for source installs on normal Linux
systems, but route it through the same staged file layout used by packages.

## Current-host cutover note

This is a one-time operator note for the sole current Hearth host. Do not turn
it into code, a script, a package hook, a daemon fallback, or general user
documentation.

Before deploying the build that changes the default paths:

1. Record `hearthctl ls`, `hearthctl status`, and the current daemon version.
2. Stop agentd, then stop Hearth so the registry cannot change during the copy.
3. Create `/var/lib/hearth/services` and `/var/lib/hearth/dnsmasq.d`, owned by
   root with directory mode `0755`.
4. Copy the contents of `/etc/hearth/services` to
   `/var/lib/hearth/services`, preserving service file names and mode `0600`.
5. Copy `/etc/hearth/allocations.toml` to
   `/var/lib/hearth/allocations.toml` with mode `0600`.
6. If `/etc/dnsmasq.d/hearth` exists, copy its generated `*.conf` files to
   `/var/lib/hearth/dnsmasq.d` with mode `0644`.
7. Point dnsmasq's `conf-dir` at `/var/lib/hearth/dnsmasq.d` before restarting
   Hearth.
8. Deploy the new package or NixOS module, then start Hearth and agentd.
9. Run `hearthctl ping`, `hearthctl host check`, `hearthctl ls`, and
   `hearthctl status`; compare the VM IDs, allocations, MAC addresses, and IPs
   with the recorded output.
10. Keep the old `/etc/hearth` state files untouched until the host has run and
    restarted cleanly. Remove only the old service, allocation, and generated
    dnsmasq files after that check. Keep authorized keys, policy, and agent
    secrets under `/etc/hearth`.

If the new daemon does not load the copied registry, stop it, restore the old
unit or package, and start it with the old path settings. Do not let both copies
of the registry receive writes.

## Completion criteria

The work is complete when:

1. A clean NixOS VM can import the module, start Hearth, and pass its module
   tests without mutable `/etc` files.
2. A clean Debian-family host can install the `.deb` with package-managed Cloud
   Hypervisor and pass the package checks.
3. A clean Fedora-family host can install the `.rpm` with package-managed Cloud
   Hypervisor and pass the package checks.
4. A tarball user can install the listed host prerequisites and run all Hearth
   binaries from the extracted prefix.
5. A developer can change Rust code and run one build-and-restart command
   without changing the installed system package.
6. A non-dry release run bumps, builds, tests, pushes, tags, attests, and
   publishes all release files, while any failed gate leaves `main` unchanged.
