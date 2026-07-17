# Hearth

Hearth manages a small single-host fleet of Cloud Hypervisor/KVM VMs through
one daemon (`hearthd`) and one CLI (`hearthctl`).

The implementation is a Rust workspace:

- `hearth-proto`: shared line-delimited JSON protocol types.
- `hearthd`: Unix-socket daemon, service registry, lifecycle dispatcher, CHV API client.
- `hearthctl`: CLI with human and `--json` output modes.

Build and test:

```sh
cargo test
cargo build
```

Web operator console (React 19 + TypeScript 7 + Vite 8.1):

```sh
devenv shell
cd web
pnpm install
pnpm dev
```

See [`web/README.md`](web/README.md) for the agentd proxy, authentication, and
production CORS setup.

Install the daemon, CLI, and systemd unit; then build the dedicated guest kernel
(vanilla kernel.org sources, no Nix) that Hearth VMs boot directly:

```sh
sudo make install                     # /usr/local/bin + hearth.service
sudo scripts/build-guest-kernel.sh    # /var/lib/hearth/kernels/current/vmlinux
sudo install -d -m 0755 /etc/hearth
sudo install -m 0644 ~/.ssh/id_ed25519.pub /etc/hearth/authorized_keys
sudo systemctl enable --now hearth.service
hearthctl host check                  # verify every host prerequisite
```

Host prerequisites, the bridge/dnsmasq contract, and the upgrade rule live in
`docs/operations.md`.

Dockerfile VM images:

```sh
make vm-base                          # the shared FROM localhost/vm-base base layer
hearthctl image build --name exeuntu --dockerfile ./Dockerfile --context . --disk 40
hearthctl create dev --from exeuntu --disk 80 --mem 4096 --cpu 4
hearthctl start dev
```

Dockerfile images are VM root filesystem recipes: the resolved OCI
`ENTRYPOINT + CMD` must be an init-like absolute path and becomes guest PID 1.
`image build` runs a build-time linter over the unpacked rootfs (rejecting a
missing init, fstab root entry, agent account, or usable sshd) so an
image-content bug fails the build instead of a boot. See
`docs/dockerfile-images.md`.

Every new VM must have managed SSH recovery access. Hearth merges the host
keyring at `/etc/hearth/authorized_keys` (override with
`HEARTH_AUTHORIZED_KEYS_FILE`) with per-VM `--ssh-key` and
`--authorized-keys-file` values, then installs the result for `agent` while the
scratch disk is mounted. A keyless create is rejected unless the operator uses
the deliberately noisy `--allow-no-ssh` escape hatch.

Spawn N VMs from one template, each individually provisioned and reachable, in a
single command each:

```sh
hearthctl spawn web-a --image exeuntu \
  --provision-file source=./a.env,dest=/etc/app.env,mode=0600,owner=1000:1000 \
  --publish 8080:80 --mem 2048 --cpu 2
hearthctl spawn web-b --image exeuntu \
  --provision-file source=./b.env,dest=/etc/app.env,mode=0600,owner=1000:1000 \
  --publish 8081:80
hearthctl wait web-a --marker 'HEARTH_PROBE ok'   # block on a readiness marker
```

`spawn` = build-if-missing → `create` (with per-VM `[provision]` files and
`[[publish]]` port forwards) → `start`. Two VMs from one image share nothing but
the immutable image — distinct name, hostname, MAC, address, machine-id, and SSH
host keys.

Acceptance tests (run as root on a prepared host — KVM, Cloud Hypervisor, a built
guest kernel, and `hearth0` DHCP/NAT):

```sh
cargo build
sudo scripts/test-agent-vm.sh      # one VM: authenticated SSH, address, MAC==alloc, budget, persistence, cleanup
sudo scripts/test-spawn-multi.sh   # two VMs from one image: distinct address/MAC/hostname, reachable
sudo scripts/test-hermes-vm.sh     # the §10 multi-VM story (needs HERMES_COMMIT to build the image)
```

Each asserts through `hearthctl --json` + `jq` and uses `hearthctl wait` for the
guest readiness marker. Run `scripts/test-agent-vm.sh --help` for the tunable
environment.

Local smoke test without touching `/etc` or `/var`:

```sh
mkdir -p /tmp/hearth-smoke/{services,images,disks,snapshots,run,log}
target/debug/hearthd \
  --socket /tmp/hearth-smoke/hearth.sock \
  --services-dir /tmp/hearth-smoke/services \
  --allocations /tmp/hearth-smoke/allocations.toml \
  --images-dir /tmp/hearth-smoke/images \
  --disks-dir /tmp/hearth-smoke/disks \
  --snapshots-dir /tmp/hearth-smoke/snapshots \
  --run-dir /tmp/hearth-smoke/run \
  --log-dir /tmp/hearth-smoke/log \
  --disable-vsock

target/debug/hearthctl --socket /tmp/hearth-smoke/hearth.sock ping
target/debug/hearthctl --socket /tmp/hearth-smoke/hearth.sock ls
```

Production defaults follow `ARCHITECTURE.md`: `/run/hearth.sock`,
`/etc/hearth/services`, `/etc/hearth/allocations.toml`,
`/var/lib/hearth/{images,disks,snapshots}`, `/run/hearth/{vms,vsock}`,
and `/var/log/hearth`.

When a service is marked `is_agent_in_charge = true`, `hearthd` also starts a
host vsock listener on `HEARTH_VSOCK_PORT`/`--vsock-port` and accepts only that
service's CID. The guest-side proxy units in `systemd/hearth-proxy.*` expose
`/run/hearth.sock` inside the VM and forward it to host CID 2.
