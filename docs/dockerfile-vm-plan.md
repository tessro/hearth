# Dockerfile-Defined VM Plan

## Goal

Make Hearth a VM manager whose primary image format is a Hearth-compatible
Dockerfile.

The contract is intentionally narrow:

- A Dockerfile defines a VM root filesystem, not a container workload.
- Hearth resolves the OCI `ENTRYPOINT + CMD` after build/unpack.
- The resolved command is booted as PID 1 in the guest.
- If that command is not a usable init process, the VM is broken and the serial
  log should make that obvious.
- Hearth does not try to support arbitrary application Dockerfiles. Those should
  use Docker or Podman.

This means an image such as exeuntu is a good target because its Dockerfile ends
with:

```dockerfile
CMD ["/usr/local/bin/init"]
```

and that script is designed to run as PID 1 before execing systemd.

## Current Pieces We Can Reuse

Hearth already has most of the skeleton:

- `hearthctl run` builds Dockerfiles with `buildah`.
- `hearthctl run` exports an OCI image layout and unpacks it with `umoci`.
- `hearthctl run` already reads the resolved OCI process from `config.json`.
- The daemon already knows how to manage Cloud Hypervisor VM lifecycle.
- The daemon already owns tap networking, vsock, serial logs, per-service disks,
  snapshots, start/stop/reboot, and the service registry.

The main thing to delete or bypass is the current OCI process-runner behavior:
`hearth-runner`, `.hearth/run.json`, virtiofs root mounting as the main rootfs,
and exit-status collection. Those are useful for container-like smoke tests, but
they are not the VM contract.

## Target Model

The target flow is:

```text
Dockerfile
  -> buildah bud
  -> OCI image layout
  -> umoci unpack
  -> validate resolved OCI command
  -> materialize rootfs as ext4 qcow2
  -> register image manifest
  -> hearthctl create/start boots it as a normal Hearth VM
```

At VM boot:

```text
Cloud Hypervisor
  -> guest kernel
  -> /dev/vda ext4 root disk
  -> init=<resolved OCI command>
  -> guest init becomes PID 1
```

For exeuntu, that means booting with an init equivalent to:

```text
init=/usr/local/bin/init
```

The existing per-service disk model still works: copy the base qcow2 into
`/var/lib/hearth/disks/<service>.qcow2`, resize it, and boot that copy.

## Image Layout

Keep compatibility with the current image store by continuing to place the root
disk at:

```text
/var/lib/hearth/images/<name>.qcow2
```

Add a sidecar manifest:

```text
/var/lib/hearth/images/<name>.hearth.toml
```

Initial manifest shape:

```toml
version = 1
kind = "docker-rootfs"
root_device = "/dev/vda"
root_fstype = "ext4"
init = "/usr/local/bin/init"

[oci]
args = ["/usr/local/bin/init"]
cwd = "/home/exedev"
env = ["EXEUNTU=1"]
```

Images without a sidecar manifest are treated as today's cloud-image qcow2
format during the transition.

## CLI Shape

Add a build/import command:

```sh
hearthctl image build \
  --name exeuntu \
  --dockerfile ./Dockerfile \
  --context . \
  --disk 40
```

The first implementation should build locally in `hearthctl`, then ask
`hearthd` to import the finished qcow2 plus manifest. That reuses the existing
client-side build code and avoids making the daemon run arbitrary Docker builds.

Add a daemon verb for the final import step:

```text
image-import { name, qcow2_path, manifest_path }
```

The daemon should validate the name, refuse overwrite unless a future `--force`
is added, copy the files into `images_dir`, and return the same shape as
`image ls`.

Then normal service creation stays familiar:

```sh
hearthctl create dev --from exeuntu --disk 80 --mem 4096 --cpu 4
hearthctl start dev
hearthctl logs dev --follow
```

## Boot Changes

Add image metadata loading in `hearthd`:

- If the image has no sidecar manifest, boot as today's cloud image.
- If `kind = "docker-rootfs"`, use direct-kernel boot.

The direct-kernel Cloud Hypervisor args should look conceptually like:

```text
--kernel <guest-kernel>
--disk path=<service-disk.qcow2>
--cmdline "console=ttyS0 root=/dev/vda rw init=/usr/local/bin/init"
```

Continue to add the existing network, vsock, serial, CPU, and memory arguments.

For the first milestone, require `oci.args[0]` to be an absolute path and use
that as `init=...`. Extra args can be rejected at first. If we later need args,
we can inject a tiny static `hearth-init-exec` adapter into the rootfs that
immediately `exec`s the full resolved argv, preserving the target process as PID
1.

## Kernel And Initramfs

The clean end state is no Hearth semantic boot shim. The guest kernel mounts the
root disk and executes the image init directly.

That requires a guest kernel with the needed drivers built in:

- virtio block
- virtio net
- virtio vsock
- ext4
- overlayfs
- cgroup v2
- namespaces and netfilter features needed by Docker inside the guest

For the first working prototype, it is acceptable to keep a tiny generic
initramfs if the available kernel does not have root-disk drivers built in. That
initramfs must only be boot plumbing: load/mount/switch_root. It should not
interpret OCI metadata, run the workload, write exit status, or act like a
container supervisor.

### Status (interim initramfs is what we ship today)

We took the initramfs route, not the built-in-driver kernel route. The stock
NixOS host kernel (`/run/booted-system/kernel`, the `guest_kernel` default) has
`virtio_blk`, `ext4`, `virtio_net`, and `vsock` as modules (`=m`), not built in.
Direct-booting it against a bare ext4 rootfs panics with `Unable to mount root
fs` because nothing loads `virtio_blk` before `root=/dev/vda` is opened, and the
Ubuntu guest rootfs has no matching `/lib/modules` to modprobe from afterward.

`scripts/build-vm-initramfs.sh` builds the boot-plumbing initramfs: static
busybox + the `modprobe --show-depends` closure for those modules (copied out of
the host module tree, decompressed) + an `init` that loads them in dependency
order, then `mount`s `root=`/`rootfstype=`/`init=` from the kernel cmdline and
`switch_root`s into the image init. It interprets no OCI metadata. hearthd
passes it only when `guest_initramfs` / `HEARTH_GUEST_INITRAMFS` is set.

**Tracked fragility — host-kernel coupling.** The staged `.ko` files are built
for one exact kernel version. If a host `nixos-rebuild` bumps the kernel, the
modules stop matching (`insmod` fails, boots drop to the initramfs shell) until
`build-vm-initramfs.sh` is re-run against the new `/run/booted-system`. This is
the price of reusing the host kernel. The clean end state above (a dedicated
guest kernel with `CONFIG_VIRTIO_BLK=y` etc., no initramfs) removes the coupling;
until then, treat "rebuild the VM initramfs after a host kernel change" as a
known operational step.

## Rootfs Materialization

The importer must preserve enough filesystem metadata for VM boot:

- uid/gid ownership
- symlinks
- executable bits
- file capabilities and xattrs where possible
- device nodes if the image contains any required ones

Possible implementation options:

1. Use `virt-make-fs` or another libguestfs tool to create ext4/qcow2 directly
   from the unpacked rootfs.
2. Use `mkfs.ext4 -d` to populate an ext4 image, then `qemu-img convert` to
   qcow2, if metadata preservation is good enough.
3. Use a rootful loop-mount path only as a fallback because it raises the host
   privilege burden.

The exeuntu target strongly prefers an ext4 block root rather than virtiofs
because it installs Docker inside the guest and writes `/etc/fstab` for:

```text
/dev/vda / ext4 defaults,x-systemd.growfs 0 1
```

## First-Boot Policy

Do not depend on cloud-init for Dockerfile-defined VMs.

For the first implementation:

- Hearth should boot the image exactly as built.
- Hostname, users, SSH keys, and service-specific customization are the image
  author's responsibility.
- Existing `cloud_init` fields can remain for cloud-image services.

Later, add an offline customization step for Dockerfile images if needed:

- hostname injection
- authorized keys
- empty or regenerate `/etc/machine-id`
- SSH host key cleanup
- optional files dropped into the rootfs before first boot

That should be explicit Hearth VM customization, not hidden Docker compatibility.

## Milestones

### 1. Document And Preserve The Contract

- Add docs that say Dockerfile images are VM rootfs recipes.
- Say resolved `ENTRYPOINT + CMD` is guest PID 1.
- Say arbitrary app Dockerfiles are unsupported.
- Update examples so the primary example has an init-like command, not a
  one-shot app command.

### 2. Build Dockerfile Rootfs Into qcow2

- Extract the reusable buildah/umoci pieces out of `docker_run.rs`.
- Add `hearthctl image build`.
- Read and validate the resolved OCI process.
- Produce `<name>.qcow2`.
- Produce `<name>.hearth.toml`.
- Add unit tests for OCI process validation and manifest rendering.

### 3. Import Built Images Into hearthd

- Add a protocol verb for image import.
- Copy qcow2 and manifest into `images_dir` atomically.
- Extend `image ls` to show image kind.
- Keep current `.qcow2` listing behavior for cloud-image compatibility.

### 4. Boot Dockerfile Images As Managed VMs

- Add manifest lookup to service start.
- Add direct-kernel Cloud Hypervisor argv generation.
- Add config for the guest kernel path.
- Build and wire the boot-plumbing initramfs (`scripts/build-vm-initramfs.sh`);
  set `HEARTH_GUEST_INITRAMFS`. Done, with the host-kernel coupling caveat above.
- Preserve existing network, vsock, serial logs, CPU, memory, stop, reboot, and
  snapshot behavior.
- Make serial logs clear when PID 1 exits or cannot be executed.

### 5. Smoke Tests

- Add a tiny VM-init Dockerfile fixture.
- Add a heavier Ubuntu/systemd fixture that resembles exeuntu but avoids
  downloading the world.
- Test build/import/create/start/logs in an environment with KVM.
- Keep unit tests runnable without KVM.

### 6. Exeuntu Validation

- Build exeuntu with `hearthctl image build`.
- Create a service from the image with a larger disk.
- Confirm `/usr/local/bin/init` runs as PID 1.
- Confirm systemd reaches `multi-user.target`.
- Confirm root grows to the service disk size.
- Confirm Docker inside the guest can start and use its storage driver.
- Confirm graceful shutdown works through `hearthctl stop`.

## Non-Goals

- Running arbitrary Dockerfiles as containers.
- Emulating Docker port publishing, volumes, healthchecks, restart policy, or
  container exit-code semantics.
- Supporting Dockerfile `CMD` that is not a VM init.
- Streaming build contexts through the line-JSON protocol in the first version.
- Making cloud-init a requirement for Dockerfile-defined VMs.

## Open Questions

- Do we keep `hearthctl run` as a legacy ephemeral OCI-process smoke tool, or
  replace it with a VM-init smoke path?
- Do we standardize on a Hearth-provided guest kernel, or allow host-kernel
  direct boot for development only?
- Should extra OCI args be rejected initially, or should the first version
  include a tiny `exec` adapter?
- Which rootfs-to-qcow2 tool gives the best balance of metadata fidelity and
  low host privilege requirements?
