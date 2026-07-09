# Hermes Agent VM

A Hearth Dockerfile-VM that boots Ubuntu 24.04 under systemd and runs the
[Hermes Agent](https://hermes-agent.nousresearch.com) gateway
(`hermes serve`) so the Hermes macOS desktop app can connect to it as a remote
backend.

It is `FROM localhost/vm-base` (all the boot boilerplate — systemd, udev,
networkd, the init shim, fstab, the getty mask, sshd, the `agent` user) plus:

- Hermes Agent installed for the `agent` user, pinned to a commit.
- A `hermes.service` unit that runs `hermes serve --host 0.0.0.0 --port 9119`.
- A serial-log probe that prints `HERMES_PROBE ok ... addr=<guest-ip>` once the
  gateway is listening.

## 1. Provide the two per-VM files

Both are gitignored. They are **no longer baked into the image** — §3/§10
provision them into each VM at create time, so two VMs from one image get
independent credentials instead of a shared, world-readable-to-root copy in the
image qcow2.

```sh
cd example/hermes-vm
cp hermes.env.example hermes.env
cp authorized_keys.example authorized_keys   # optional; may stay empty
```

Edit `hermes.env`:

- Set `HERMES_DASHBOARD_BASIC_AUTH_USERNAME` / `_PASSWORD` — the macOS app signs
  in with these. `hermes serve` refuses unauthenticated non-loopback access.
- Set `HERMES_DASHBOARD_BASIC_AUTH_SECRET` to a stable random value
  (`openssl rand -hex 32`) so the app's session survives VM restarts.
- Set at least one model-provider key (e.g. `OPENROUTER_API_KEY`), **or** leave
  them blank and run `hermes setup --portal` over SSH after first boot.

Put your Mac's SSH public key in `authorized_keys` if you want to log in.

## 2. Build the base, then the image

Build `vm-base` once (see `example/vm-base/README.md`), then the Hermes image.
The Hermes install is pinned by commit; there is no default, so a reproducible
build needs an explicit `--build-arg HERMES_COMMIT=<sha>`:

```sh
make vm-base
hearthctl image build --name hermes-vm --dockerfile example/hermes-vm/Dockerfile \
  --context example/hermes-vm --disk 16 --build-arg HERMES_COMMIT=<sha>
```

> **Tested commit.** Record the sha you validated here and bump it deliberately
> in a commit (the build refuses without one). At the time of writing the pin
> had not been captured — the environment could not reach the Hermes git remote
> — so pass the sha you install. The build ends with
> `test -x /home/agent/.local/bin/hermes && hermes --version`, so a moved
> launcher fails the build here, not on first boot.

`image build` runs a build-time linter over the unpacked rootfs before it makes
the disk (§2.2): it rejects an image whose init is missing or whose fstab has no
root entry, and warns about a missing udev/`.network`/sshd or an unmasked
serial getty. Pass `--skip-lint` only for an image that boots something other
than systemd.

## 3. Boot and provision

`spawn` builds (if needed), provisions the per-VM files, and starts the VM in
one command — this is what replaces the old `COPY hermes.env ...` in the
Dockerfile:

```sh
hearthctl spawn hermes \
  --image hermes-vm \
  --provision-file source=./hermes.env,dest=/home/agent/.hermes/.env,mode=0600,owner=1000:1000 \
  --provision-file source=./authorized_keys,dest=/home/agent/.ssh/authorized_keys,mode=0600,owner=1000:1000 \
  --mem 4096 --cpu 4 --disk 32
hearthctl logs hermes --follow      # watch for HERMES_PROBE ok
```

Or do the steps by hand — `create` (with the same `[provision]` files in the
service TOML) then `start`. The probe line includes the guest's `hearth0` IP;
note it for the next step.

## 4. Reach the VM from your Mac

The VM sits on the NAT'd `hearth0` bridge, so it is not reachable from your Mac
by default. Give it a stable address (its MAC is fixed in
`/etc/hearth/allocations.toml`) and forward the gateway port to it — see §4 of
the refactor proposal for the managed `[[publish]]` path that replaces the old
hand-typed nftables DNAT.

Then in the macOS app: **Settings → Gateway → Remote gateway**, set the URL to
`http://<hearth-host>:9119` and sign in with the dashboard username/password
from `hermes.env`.

Hermes' own docs warn against exposing a password-protected backend to the open
internet — keep this on a trusted LAN/VPN, or install Tailscale in the guest and
connect over the tailnet instead.

## Notes

- `hermes serve` needs outbound internet for model APIs at runtime; Hearth's
  NAT already provides that.
- The Dockerfile installs Hermes with `--skip-browser`; drop that flag and
  rebuild if you want Hermes' Playwright/Chromium browser tools in the VM.
