# Dockerfile VM Images

Hearth-compatible Dockerfiles are VM root filesystem recipes. They are not
general container workload definitions.

The build contract is intentionally narrow:

- `hearthctl image build` builds the Dockerfile with `buildah`.
- The image is exported as an OCI layout and unpacked with `umoci`.
- Hearth reads the resolved OCI `ENTRYPOINT + CMD` from `config.json`.
- The resolved command becomes guest PID 1 through the kernel `init=` command
  line.
- The command must currently resolve to exactly one absolute path, such as
  `/usr/local/bin/init`.
- Arbitrary app Dockerfiles, shell forms, and commands with extra argv are not
  supported by the VM image path yet. Use Docker or Podman for those.

Build and import an image:

```sh
hearthctl image build \
  --name exeuntu \
  --dockerfile ./Dockerfile \
  --context . \
  --disk 40
```

The client builds locally, creates an ext4 qcow2 root disk, renders
`<name>.hearth.toml`, and asks `hearthd` to import both files. The daemon stores
them as:

```text
/var/lib/hearth/images/<name>.qcow2
/var/lib/hearth/images/<name>.hearth.toml
```

Then create and boot services normally:

```sh
hearthctl create dev --from exeuntu --disk 80 --mem 4096 --cpu 4
hearthctl start dev
hearthctl logs dev --follow
```

Images without a `.hearth.toml` sidecar are treated as traditional cloud qcow2
images and still use the firmware plus cloud-init seed boot path.

## Spawning multiple VMs from one template

`hearthctl spawn` collapses build (if the image is missing) → create → start
into one command, and provisions each VM independently at create time. The image
is immutable and shared; everything that makes a VM its own instance is applied
per spawn. Two spawns from the same `--image`:

```sh
# First VM: build the image on demand (it does not exist yet), then provision
# its own secret and boot it.
hearthctl spawn hermes-a \
  --image hermes-vm \
  --dockerfile example/hermes-vm/Dockerfile --context example/hermes-vm \
  --provision-file source=./a.env,dest=/home/agent/.hermes/.env,mode=0600,owner=1000:1000 \
  --cpu 4 --mem 4096 --disk 32

# Second VM: the image already exists, so the build is skipped. A different
# secret goes into an otherwise identical VM.
hearthctl spawn hermes-b \
  --image hermes-vm \
  --provision-file source=./b.env,dest=/home/agent/.hermes/.env,mode=0600,owner=1000:1000
```

`--provision-file` is repeatable and its `source=` is read on the client, so the
daemon only ever sees the file's literal content — never a CLI-relative path.
Pass `mode=0600` for anything secret (mode/owner default to `0644`/`0:0`). Fields
are comma-separated, so a `source` path may contain `=` but must not contain a
comma.

What differs per VM: the **name** (`hermes-a` vs `hermes-b`), the **hostname**
(defaults to the name; override with `--hostname`), the regenerated
**machine-id**, the regenerated **SSH host keys** (vm-base ships no baked keys,
so `ssh-hostkeys.service` mints a unique set on first boot; for a base that does
bake them, pass `--reset-ssh-hostkeys`), the **provisioned files** (each VM's own
`.env`), and the per-service **MAC**, **IP**, and **vsock CID** the daemon
allocates. What is shared: the immutable template **image** — the whole point.
Nothing baked into the image carries per-VM identity or secrets.

The address is a DHCP lease, so `spawn` may print a null `address` right after
boot; rerun `hearthctl status <name>` once the lease lands.

## Guest Kernel

docker-rootfs images boot a dedicated Hearth guest kernel directly through Cloud
Hypervisor's PVH entry point — no bootloader and **no initramfs**. The kernel is
a pinned vanilla kernel.org LTS build with the VM driver contract (virtio, ext4,
vsock, af_packet, overlayfs, cgroup v2, Docker-in-guest netfilter) compiled in;
the contract lives in `guest/kernel.config`.

Build and install it once with a plain script (ordinary host packages only — no
Nix):

```sh
scripts/build-guest-kernel.sh
```

That downloads the pinned source, verifies its sha256, applies
`guest/kernel.config`, builds `vmlinux`, and installs it to
`/var/lib/hearth/kernels/<version>/vmlinux` with a `current` symlink. `hearthd`'s
`guest_kernel` default (`/var/lib/hearth/kernels/current/vmlinux`) points there.
Run with `--install-dir ~/.local/share/hearth/kernels` to build without root, or
`--help` for all options.

If the guest kernel is missing, `hearthd` refuses to `start` a docker-rootfs
service with a `kernel.not_found` error naming this script, rather than panicking
on the serial console. Each image can also declare `min_kernel_contract` in its
`.hearth.toml`; a kernel older than an image requires is rejected at start with
`kernel.contract_too_old`. `hearthctl host check` reports guest-kernel presence.

Rebuilding the kernel is only needed to bump the pinned version (edit the
`KERNEL_VERSION`/`KERNEL_SHA256` constants in the script) — never for a host
kernel change, since the guest kernel is fully self-contained.

## User sessions

Hearth VMs run autonomous agents as the `agent` user, so the guest contract
includes a working systemd **user** session for that user — over SSH and at boot,
before any login. Every image built on `example/vm-base` gets it: `systemctl
--user`, `loginctl`, the per-user session bus, and `XDG_RUNTIME_DIR`
(`/run/user/1000`) all work, and lingering is enabled for `agent` so
`user@1000.service` (with its delegated cgroup subtree) is up at boot with no
login. The pieces — `dbus-user-session`, `libpam-systemd`, an explicitly enabled
system bus + logind, and the `agent` linger stamp — and why each is present live
in `example/vm-base/README.md`.

The boot-time proof is a single marker on the serial console:
`HEARTH_USERSESSION ok` (or `HEARTH_USERSESSION fail <reason>`), emitted by
`hearth-usersession.service` once the manager is active, `/run/user/1000` exists,
and the agent's session bus answers. The acceptance tests below gate on it with
`hearthctl wait`, and `hearthctl image build`'s linter warns at build time if
dbus is not enabled or `pam_systemd.so` is missing from the rootfs.

## Agent VM Acceptance Test

The repository includes a stripped-down agent VM fixture at
`example/agent-vm`. It is intentionally exeuntu-shaped without exeuntu-specific
CLI, browser, agent-product, or site assets.

The fixture proves:

- Ubuntu rootfs boots through `/usr/local/bin/init`.
- systemd becomes PID 1.
- `/` is an ext4 root disk.
- systemd-networkd gets a default route through Hearth networking.
- the `agent` user exists and has passwordless sudo.
- the `agent` user has a working systemd user session at boot — logind, the
  session bus, `XDG_RUNTIME_DIR`, and lingering `user@1000` — proven by the
  `HEARTH_USERSESSION ok` marker on the serial console.
- common agent tools are present: `git`, `curl`, `jq`, and `python3`.
- guest state persists across stop/start through a boot counter on the root
  disk.

Run it against a real root `hearthd` with KVM, Cloud Hypervisor, guest kernel,
and `hearth0` DHCP/NAT configured:

```sh
cargo build
sudo scripts/test-agent-vm.sh
```

The harness builds `example/agent-vm` (on the shared `vm-base` layer), spawns the
VM, blocks on the readiness marker with `hearthctl wait`, and asserts the address,
reachability, MAC-vs-allocation, a boot-time budget, stop/start persistence, and a
clean destroy. Use `CLEAN=1` to replace a previous test service and image:

```sh
sudo CLEAN=1 scripts/test-agent-vm.sh
```

`hearthctl image build` defaults to a rootful `umoci unpack` for this path so
ownership and filesystem metadata have a chance to survive into the VM root
disk. The `--rootless` flag remains available for lightweight smoke builds, but
it is not suitable for proving an agent-ready VM rootfs.
