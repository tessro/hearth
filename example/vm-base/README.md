# hearth/vm-base

The base layer every bootable Hearth VM image shares. It owns the boot
boilerplate that `hermes-vm` and `agent-vm` used to each copy verbatim (they
were ~60% identical and drifting): systemd + udev enablement, `systemd-networkd`
with a DHCP `.network` matching `en*`, the `/usr/local/bin/init` mount shim,
the `/etc/fstab` root entry with `x-systemd.growfs`, the `serial-getty@ttyS0`
mask, `STOPSIGNAL SIGRTMIN+3`, `openssh-server`, and a one-shot unit that runs
`ssh-keygen -A` before sshd when host keys are absent.

Every line exists because its absence cost a full build+boot cycle to diagnose
from the serial console during the 2026-07 Hermes bring-up.

## Build it

`vm-base` is a plain local buildah image — there is no registry and no Hearth
`image build` step (it is not a bootable image on its own, just a base layer):

```sh
buildah bud --network host --layers -t vm-base -f example/vm-base/Dockerfile example/vm-base
```

`--network host` runs the `RUN` steps in the host network namespace. Without it,
netavark races its own iptables chains between consecutive `RUN` steps and the
build dies with `netavark: iptables: Chain already exists` — the same reason
`hearthctl image build` defaults to host. VM-rootfs builds only need outbound
network, so this is safe on a single-operator host.

or, from the repo root:

```sh
make vm-base
```

`--layers` caches each `RUN` step so rebuilding a workload image on top does not
re-run the base's apt install.

## Use it

A workload image bases on it by tag and adds only its own bits:

```dockerfile
FROM localhost/vm-base
# ... install the workload, drop in its units, enable them ...
```

See `example/hermes-vm/Dockerfile` and `example/agent-vm/Dockerfile`.

## User sessions

Hearth VMs run autonomous agents as the `agent` user, so a full systemd **user**
session must work for that user — both over SSH and at boot, before any login.
`vm-base` bakes in the whole stack:

- **`systemctl --user`, `loginctl`, the session bus, `XDG_RUNTIME_DIR`** all work
  because the image installs `dbus-user-session` (the per-user session bus units)
  and `libpam-systemd` (`pam_systemd.so`, which its `pam-auth-update` trigger
  wires into `/etc/pam.d/common-session` so a login registers a logind session
  and gets `/run/user/1000`), then explicitly enables `dbus.socket`,
  `dbus.service`, and `systemd-logind.service` at boot — a `--no-install-recommends`
  image runs no systemctl preset, so nothing enables them otherwise.
- **Lingering is on for `agent`** (`/var/lib/systemd/linger/agent`), so
  `user@1000.service` starts at boot with no login. Its cgroup subtree is
  delegated (`user@.service` ships `Delegate=pids memory cpu` on Ubuntu 24.04's
  systemd 255) — that delegation is the piece a container/VM without user-session
  infrastructure lacks.
- **Boot-time proof.** `hearth-usersession.service` runs after
  `user@1000.service` and prints exactly one line to the serial console:
  `HEARTH_USERSESSION ok` when the manager is active, `/run/user/1000` exists, and
  the agent's session bus answers — or `HEARTH_USERSESSION fail <reason>`. The
  acceptance tests gate on that marker with `hearthctl wait`.

## What a workload image still owns

`vm-base` deliberately does **not** bake in:

- **Secrets or per-VM files.** Provision them at create time with
  `spawn --provision-file` / the service TOML `[provision]` block (§3), not
  `COPY`. That keeps credentials out of the shared image and gives each VM its
  own machine-id and SSH host keys.
- **SSH authorized keys.** Hearth installs its host recovery keyring plus any
  per-VM `--ssh-key` / `--authorized-keys-file` values during disk provisioning.
  Workload images must not bake `/home/agent/.ssh/authorized_keys`.
- **Workload packages and units.** Anything specific to the service — the
  Hermes install, probes, extra apt packages — lives in the workload Dockerfile.
