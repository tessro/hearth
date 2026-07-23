# Hearth

Hearth manages a single-host fleet of Cloud Hypervisor/KVM virtual machines.
Its Rust workspace contains the machine-plane daemon and CLI plus an optional
agent plane with durable in-guest sessions and a web operator console.

## Development

The checked-in devenv and `rust-toolchain.toml` provide the complete Rust
toolchain, including the static musl guest target. Run commands from the
repository root through that environment:

```sh
devenv shell -- make check
devenv shell -- make build
```

`make check` runs formatting, Clippy, and the release test suite. `make build`
rebuilds the host release binaries; tests alone do not guarantee those
top-level executables are fresh.

When changing `hearth-agentd`, `hearth-guestd`, or their shared protocol, build
the deployable pair explicitly:

```sh
devenv shell -- make agent-plane-artifacts
```

This builds host `hearth-agentd`, builds `hearth-guestd` as a static musl
binary, verifies it has no dynamic interpreter, and stages it at
`example/vm-base/hearth-guestd`.

For the normal edit-build-restart loop on a configured host, run:

```sh
make dev-restart
# Include the static guest payload, but do not change running VMs:
make dev-restart-agent-plane
# Remove only the runtime overrides and copied dev binaries:
make dev-reset
```

## Install and release files

Hearth supports NixOS through its flake and module, Debian-family systems with
the `.deb`, Fedora-family systems with the `.rpm`, and other x86-64 Linux hosts
with the portable tarball. The guest daemon is always static. Tarball host tools
are static too; native packages use the target system's C library. Nix builds
all of its own programs.

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
            operatorUsers = [ "tess" ];
          };
        }
      ];
    };
  };
}
```

Release files install no secrets, keys, VM images, or Cloud Hypervisor binary.
See [`docs/operations.md`](docs/operations.md) for package prerequisites,
network setup, agent credentials, checksums, and release steps.

Install targets are copy-only. Build the native stage as your normal user, then
run `sudo make install`; this keeps Cargo from leaving root-owned files. The
stage includes the pinned guest kernel, so packages or the NixOS module are the
faster choice for most hosts.

## Web console

```sh
devenv shell -- pnpm --dir web install
devenv shell -- pnpm --dir web dev
```

See [`web/README.md`](web/README.md) for proxy, authentication, CORS, and
production-build details.

## Build a VM image

```sh
devenv shell -- make vm-base
devenv shell -- target/release/hearthctl image build --name exeuntu --dockerfile ./Dockerfile --context . --disk 40
devenv shell -- target/release/hearthctl create dev --from exeuntu --disk 80 --mem 4096 --cpu 4
devenv shell -- target/release/hearthctl start dev
```

## Documentation

- [`docs/operations.md`](docs/operations.md): host prerequisites, installation,
  networking, development, and upgrades.
- [`docs/dockerfile-images.md`](docs/dockerfile-images.md): image contracts and
  rootfs construction.
- [`docs/agent-plane.md`](docs/agent-plane.md): agent-plane architecture and
  protocols.
