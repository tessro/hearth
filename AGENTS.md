# Repository guidance

- Run repository commands through the pinned environment from the repository root: `devenv shell -- <command>`. Do not install Rust toolchains or targets into user-global state.
- Run `devenv shell -- make check` after Rust changes. It checks formatting, runs Clippy with warnings denied, and runs the release test suite.
- Run `devenv shell -- make build` to rebuild host release binaries. Tests do not guarantee that top-level release executables are fresh.
- Run `devenv shell -- make agent-plane-artifacts` when work affects `hearth-agentd`, `hearth-guestd`, or their shared protocol. This rebuilds host `agentd`, builds the static musl guest binary, and stages the latter under `example/vm-base/`.
- Run `devenv shell -- make release-archive release-check` after release layout or packaging changes. The tarball uses static host binaries. Native `.deb` and `.rpm` files build in their target-family CI matrix jobs with `make release-packages PACKAGE_FORMAT=deb|rpm`.
- Run `nix flake check` after flake or NixOS module changes. Nix builds its own host and guest binaries and must not copy files from `target/`.
- `hearth-guestd` is deployed only as a static musl artifact. Never stage `target/release/hearth-guestd`, which is a host-libc build.
- Unix-socket and file-descriptor-passing tests may require a scoped sandbox escalation. Retry the same test command with approval rather than changing the test or host configuration.
- Preserve unrelated working-tree changes and generated artifacts unless the task explicitly includes them.
