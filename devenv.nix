{ pkgs, lib, config, inputs, ... }:

{
  # https://devenv.sh/basics/
  env.GREET = "devenv";

  # https://devenv.sh/packages/
  packages = with pkgs; [
    git
    jq

    cargo
    clippy
    rustc
    rustfmt
    rust-analyzer
    pkgsStatic.busybox
  ];

  # https://devenv.sh/languages/
  # languages.rust.enable = true;

  # https://devenv.sh/processes/
  # processes.dev.exec = "${lib.getExe pkgs.watchexec} -n -- ls -la";

  # https://devenv.sh/services/
  # services.postgres.enable = true;

  # https://devenv.sh/scripts/
  scripts.hello.exec = ''
    echo hello from $GREET
  '';

  scripts.build-hearth-runner.exec = ''
    LIBRARY_PATH="${pkgs.glibc.static}/lib''${LIBRARY_PATH:+:$LIBRARY_PATH}" \
      cargo rustc -p hearth-runner --release -- -C target-feature=+crt-static
  '';

  scripts.build-hearth-initramfs.exec = ''
    scripts/build-initramfs.sh \
      --busybox ${pkgs.pkgsStatic.busybox}/bin/busybox \
      --runner target/release/hearth-runner \
      "$@"
  '';

  # https://devenv.sh/basics/
  enterShell = ''
    hello         # Run scripts directly
    git --version # Use packages
  '';

  # https://devenv.sh/tasks/
  # tasks = {
  #   "myproj:setup".exec = "mytool build";
  #   "devenv:enterShell".after = [ "myproj:setup" ];
  # };

  # https://devenv.sh/tests/
  enterTest = ''
    echo "Running tests"
    git --version | grep --color=auto "${pkgs.git.version}"
  '';

  # https://devenv.sh/git-hooks/
  # git-hooks.hooks.shellcheck.enable = true;

  # See full reference at https://devenv.sh/reference/options/
}
