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

Install targets are copy-only. Build as your normal user first, then elevate
only the filesystem installation step so Cargo never leaves root-owned build
artifacts:

```sh
devenv shell -- make build guest-bin
devenv shell -- sudo make install
```

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
- [`docs/agent-plane-verification.md`](docs/agent-plane-verification.md): test
  coverage, live verification, and known gaps.
