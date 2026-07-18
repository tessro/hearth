{ pkgs, hearthModule }:

let
  fakeKernel = pkgs.runCommand "hearth-test-kernel" { } ''
    mkdir -p $out/lib/hearth/kernel
    touch $out/lib/hearth/kernel/vmlinux
    echo 1 > $out/lib/hearth/kernel/contract
  '';
  fakeCloud = pkgs.writeShellScriptBin "cloud-hypervisor" ''
    if [ "''${1:-}" = --version ]; then echo "cloud-hypervisor test"; exit 0; fi
    exit 1
  '';
  common = { ... }: {
    imports = [ hearthModule ];
    services.hearth = {
      enable = true;
      guestKernel = fakeKernel;
      cloudHypervisorPackage = fakeCloud;
      authorizedKeys = [
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPEVBr+XtUOuloYyDWGTcKPPHbVwpSIATl/mJ6RE7gdN nix-test"
      ];
      operatorUsers = [ "operator" ];
    };
    users.users.operator.isNormalUser = true;
  };
in
{
  module-basic = pkgs.testers.runNixOSTest {
    name = "hearth-module-basic";
    nodes.machine = common;
    testScript = ''
      machine.wait_for_unit("hearth.service")
      machine.succeed("hearthctl ping | grep 'hearthd 0.1.0'")
      machine.succeed("getent group hearth | grep operator")
      machine.succeed("test -S /run/hearth.sock")
      machine.succeed("test -d /var/lib/hearth/services")
      machine.succeed("test -d /var/lib/hearth/dnsmasq.d")
      machine.succeed("test -d /var/log/hearth")
      machine.succeed("test -f /etc/hearth/authorized_keys")
      machine.succeed("test -f /etc/hearth/verb-policy.toml")
      machine.succeed("test ! -e /etc/hearth/services")
      machine.succeed("test ! -e /etc/hearth/allocations.toml")
      machine.succeed("systemctl show hearth -p User --value | grep '^$'")
    '';
  };

  module-agent = pkgs.testers.runNixOSTest {
    name = "hearth-module-agent";
    nodes.machine = { pkgs, ... }: {
      imports = [ common ];
      services.hearth.agentPlane = {
        enable = true;
        httpTokenFile = toString (pkgs.writeText "http-token" "0123456789abcdef0123456789abcdef");
        refKeyFile = toString (pkgs.writeText "ref-key" "0123456789abcdef0123456789abcdef");
      };
    };
    testScript = ''
      machine.wait_for_unit("hearth.service")
      machine.wait_for_unit("hearth-agentd.service")
      machine.succeed("test -S /run/hearth-agentd/agent.sock")
      machine.succeed("test -d /var/lib/hearth-agentd")
      machine.succeed("test -d /var/log/hearth-agentd")
      machine.succeed("test $(stat -c %U /var/lib/hearth-agentd) = hearth-agent")
    '';
  };

  module-network = pkgs.testers.runNixOSTest {
    name = "hearth-module-network";
    nodes.machine = { ... }: {
      imports = [ common ];
      services.hearth.networking = {
        manage = true;
        uplinkInterface = "eth1";
      };
    };
    testScript = ''
      machine.wait_for_unit("systemd-networkd.service")
      machine.wait_until_succeeds("ip link show hearth0")
      machine.wait_for_unit("dnsmasq.service")
      machine.succeed("ip address show hearth0 | grep 10.26.8.1/24")
      machine.succeed("grep -R '/var/lib/hearth/dnsmasq.d' /etc/systemd /nix/store/*-dnsmasq.conf 2>/dev/null")
      machine.succeed("nft list table ip hearth-host | grep masquerade")
    '';
  };
}
