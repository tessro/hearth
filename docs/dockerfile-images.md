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
- common agent tools are present: `git`, `curl`, `jq`, and `python3`.
- guest state persists across stop/start through a boot counter on the root
  disk.

Run it against a real root `hearthd` with KVM, Cloud Hypervisor, guest kernel,
and `hearth0` DHCP/NAT configured:

```sh
devenv shell cargo build
sudo -E scripts/test-agent-vm.sh
```

Use `CLEAN=1` to replace a previous test service and image:

```sh
sudo -E CLEAN=1 scripts/test-agent-vm.sh
```

`hearthctl image build` defaults to a rootful `umoci unpack` for this path so
ownership and filesystem metadata have a chance to survive into the VM root
disk. The `--rootless` flag remains available for lightweight smoke builds, but
it is not suitable for proving an agent-ready VM rootfs.
