{ pkgs, ... }:

{
  packages = with pkgs; [
    binutils
    file
    gcc
    git
    jq
    nodejs_24
    pnpm

    # mkfs.ext4 for `hearthctl image build` rootfs materialization.
    e2fsprogs

    # Guest-kernel build toolchain for scripts/build-guest-kernel.sh (optional
    # dev convenience; the script itself only needs ordinary host packages).
    flex
    bison
    bc
    elfutils
    openssl

    # Expose only the linker executable. Adding musl.dev directly would inject
    # musl headers and libraries into native build-script compilation.
    (writeShellScriptBin "musl-gcc" ''
      export REALGCC=${stdenv.cc.cc}/bin/gcc
      exec ${musl.dev}/bin/musl-gcc "$@"
    '')
  ];

  languages.rust = {
    enable = true;
    toolchainFile = ./rust-toolchain.toml;
  };
}
