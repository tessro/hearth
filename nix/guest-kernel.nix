{
  lib,
  stdenv,
  fetchurl,
  gnumake,
  flex,
  bison,
  bc,
  perl,
  elfutils,
  openssl,
  pkg-config,
}:

stdenv.mkDerivation rec {
  pname = "hearth-guest-kernel";
  version = "6.12.95";

  src = fetchurl {
    url = "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-${version}.tar.xz";
    hash = "sha256-qejFH8seaV0dNd3liGy6V5y28pyWRsWInznWOEHUufY=";
  };
  config = ../guest/kernel.config;
  contract = "1";

  nativeBuildInputs = [
    gnumake
    flex
    bison
    bc
    perl
    elfutils
    openssl
    pkg-config
  ];
  hardeningDisable = [ "all" ];

  configurePhase = ''
    runHook preConfigure
    patchShebangs scripts
    make defconfig
    scripts/kconfig/merge_config.sh -m -O . .config ${config}
    make olddefconfig
    grep -q '^CONFIG_PVH=y' .config
    runHook postConfigure
  '';
  buildPhase = ''
    runHook preBuild
    make -j$NIX_BUILD_CORES vmlinux
    runHook postBuild
  '';
  installPhase = ''
    runHook preInstall
    mkdir -p $out/lib/hearth/kernel
    install -m644 vmlinux $out/lib/hearth/kernel/vmlinux
    printf '%s\n' ${contract} > $out/lib/hearth/kernel/contract
    runHook postInstall
  '';

  meta = {
    description = "Pinned direct-boot kernel for Hearth guests";
    license = lib.licenses.gpl2Only;
    platforms = [ "x86_64-linux" ];
  };
}
