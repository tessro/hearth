# hearth/vm-base

The base layer every bootable Hearth VM image shares. It owns the boot
boilerplate that `hermes-vm` and `agent-vm` used to each copy verbatim (they
were ~60% identical and drifting): systemd + udev enablement, `systemd-networkd`
with a DHCP `.network` matching `en*`, the `/usr/local/bin/init` mount shim,
the `/etc/fstab` root entry with `x-systemd.growfs`, the `serial-getty@ttyS0`
mask, `STOPSIGNAL SIGRTMIN+3`, `openssh-server`, and a one-shot unit that runs
`ssh-keygen -A` before sshd when host keys are absent.

Every line exists because its absence cost a full build+boot cycle to diagnose
from the serial console during the 2026-07 Hermes bring-up. See
`REFACTOR_PROPOSAL.md` §2.

## Build it

`vm-base` is a plain local buildah image — there is no registry and no Hearth
`image build` step (it is not a bootable image on its own, just a base layer):

```sh
buildah bud --layers -t vm-base -f example/vm-base/Dockerfile example/vm-base
```

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

## What a workload image still owns

`vm-base` deliberately does **not** bake in:

- **Secrets or per-VM files.** Provision them at create time with
  `spawn --provision-file` / the service TOML `[provision]` block (§3), not
  `COPY`. That keeps credentials out of the shared image and gives each VM its
  own machine-id and SSH host keys.
- **Workload packages and units.** Anything specific to the service — the
  Hermes install, probes, extra apt packages — lives in the workload Dockerfile.
