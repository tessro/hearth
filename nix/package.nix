{
  lib,
  runCommand,
  rustPlatform,
  pkgsStatic,
  pkg-config,
  binutils,
}:

let
  version = (lib.importTOML ../Cargo.toml).workspace.package.version;
  source = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates
    ];
  };
  common = {
    pname = "hearth";
    inherit version;
    src = source;
    cargoLock.lockFile = ../Cargo.lock;
    HEARTH_RELEASE = "1";
    doCheck = false;
  };
  host = rustPlatform.buildRustPackage (
    common
    // {
      pname = "hearth-host";
      cargoBuildFlags = [
        "-p"
        "hearthd"
        "-p"
        "hearthctl"
        "-p"
        "hearth-agentd"
      ];
      installPhase = ''
        runHook preInstall
        mkdir -p $out/bin
        for binary in hearthd hearthctl hearth-agentd; do
          path=$(find target -type f -path "*/release/$binary" -print -quit)
          test -n "$path"
          install -m755 "$path" "$out/bin/$binary"
        done
        runHook postInstall
      '';
    }
  );
  guest = pkgsStatic.rustPlatform.buildRustPackage (
    common
    // {
      pname = "hearth-guest";
      cargoBuildFlags = [
        "-p"
        "hearth-guestd"
      ];
      nativeBuildInputs = [
        binutils
        pkg-config
      ];
      CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER = "${pkgsStatic.stdenv.cc}/bin/${pkgsStatic.stdenv.cc.targetPrefix}cc";
      installPhase = ''
        runHook preInstall
        mkdir -p $out/bin
        guest_bin=$(find target -type f -path '*/release/hearth-guestd' -print -quit)
        test -n "$guest_bin"
        install -m755 "$guest_bin" $out/bin/hearth-guestd
        if readelf -lW $out/bin/hearth-guestd | grep -q ' INTERP '; then
          echo "hearth-guestd is not static" >&2
          exit 1
        fi
        runHook postInstall
      '';
    }
  );
in
runCommand "hearth-${version}" { meta.mainProgram = "hearthctl"; } ''
  mkdir -p $out/bin $out/lib/hearth/guest $out/share/hearth \
    $out/share/doc/hearth $out/share/licenses/hearth
  cp ${host}/bin/* $out/bin/
  cp ${guest}/bin/hearth-guestd $out/lib/hearth/guest/
  cp ${../systemd/hearth-agentd-verb-policy.toml} $out/share/hearth/verb-policy.toml
  cp ${../README.md} ${../docs/operations.md} ${../docs/agent-plane.md} $out/share/doc/hearth/
  cp ${../LICENSE} $out/share/licenses/hearth/LICENSE
''
