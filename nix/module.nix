{
  config,
  lib,
  pkgs,
  hearthPackage,
  hearthGuestKernel,
  ...
}:

let
  cfg = config.services.hearth;
  inherit (lib)
    mkEnableOption
    mkIf
    mkMerge
    mkOption
    types
    ;
  policy = builtins.readFile ../systemd/hearth-agentd-verb-policy.toml;
  agentArgs = [
    "--token-file %d/http-token"
    "--ref-key-file %d/ref-key"
  ];
  octetsToInt =
    address: lib.foldl' (total: part: total * 256 + lib.toInt part) 0 (lib.splitString "." address);
  staticParts = lib.splitString "," cfg.networking.staticRange;
  staticStart = lib.elemAt staticParts 0;
  staticEnd = lib.elemAt staticParts 1;
  staticCount = octetsToInt staticEnd - octetsToInt staticStart + 1;
in
{
  options.services.hearth = {
    enable = mkEnableOption "Hearth VM management";
    package = mkOption {
      type = types.package;
      default = hearthPackage;
      description = "Hearth package built by Nix.";
    };
    cloudHypervisorPackage = mkOption {
      type = types.package;
      default = pkgs.cloud-hypervisor;
    };
    guestKernel = mkOption {
      type = types.package;
      default = hearthGuestKernel;
    };
    authorizedKeys = mkOption {
      type = types.listOf types.str;
      default = [ ];
    };
    operatorUsers = mkOption {
      type = types.listOf types.str;
      default = [ ];
    };
    agentPlane = {
      enable = mkEnableOption "the Hearth agent plane";
      httpTokenFile = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Runtime source path for the HTTP token; never copied to the Nix store.";
      };
      refKeyFile = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Runtime source path for the ref key; never copied to the Nix store.";
      };
    };
    networking = {
      manage = mkOption {
        type = types.bool;
        default = false;
      };
      bridge = mkOption {
        type = types.str;
        default = "hearth0";
      };
      address = mkOption {
        type = types.str;
        default = "10.26.8.1/24";
      };
      staticRange = mkOption {
        type = types.str;
        default = "10.26.8.16,10.26.8.79";
      };
      dynamicRange = mkOption {
        type = types.str;
        default = "10.26.8.128,10.26.8.254,12h";
      };
      uplinkInterface = mkOption {
        type = types.nullOr types.str;
        default = null;
      };
    };
  };

  config = mkIf cfg.enable (mkMerge [
    {
      assertions = [
        {
          assertion =
            !cfg.agentPlane.enable
            || (cfg.agentPlane.httpTokenFile != null && cfg.agentPlane.refKeyFile != null);
          message = "services.hearth.agentPlane requires httpTokenFile and refKeyFile";
        }
        {
          assertion = !cfg.networking.manage || cfg.networking.uplinkInterface != null;
          message = "services.hearth.networking.manage requires uplinkInterface";
        }
      ];

      users.groups.hearth = { };
      users.users = {
        hearth-agent = {
          isSystemUser = true;
          group = "hearth";
          home = "/var/lib/hearth-agentd";
        };
      }
      // lib.genAttrs cfg.operatorUsers (_: {
        extraGroups = [ "hearth" ];
      });

      environment.systemPackages = [
        cfg.package
        cfg.cloudHypervisorPackage
        pkgs.qemu
        pkgs.nftables
        pkgs.dnsmasq
        pkgs.iproute2
        pkgs.socat
      ];
      environment.etc."hearth/authorized_keys".text =
        lib.concatMapStringsSep "\n" (key: key) cfg.authorizedKeys + "\n";
      environment.etc."hearth/verb-policy.toml".text = policy;

      boot.kernelModules = [
        "kvm"
        "vhost_vsock"
      ];
      systemd.tmpfiles.rules = [
        "d /var/lib/hearth 0755 root root -"
        "d /var/lib/hearth/services 0755 root root -"
        "d /var/lib/hearth/dnsmasq.d 0755 root root -"
        "d /var/lib/hearth-agentd 0750 hearth-agent hearth -"
        "d /var/log/hearth 0755 root root -"
        "d /var/log/hearth-agentd 0750 hearth-agent hearth -"
      ];
      systemd.services.hearth = {
        description = "Hearth VM management daemon";
        after = [ "network-online.target" ];
        wants = [ "network-online.target" ];
        wantedBy = [ "multi-user.target" ];
        path = [
          cfg.cloudHypervisorPackage
          pkgs.qemu
          pkgs.nftables
          pkgs.dnsmasq
          pkgs.iproute2
          pkgs.socat
          pkgs.systemd
        ];
        serviceConfig = {
          Type = "notify";
          ExecStart = "${cfg.package}/bin/hearthd --guest-kernel ${cfg.guestKernel}/lib/hearth/kernel/vmlinux";
          Restart = "on-failure";
          RestartSec = 2;
          RuntimeDirectory = "hearth";
          RuntimeDirectoryMode = "0770";
          StateDirectory = "hearth";
          LogsDirectory = "hearth";
          TimeoutStopSec = 120;
          Environment = [
            "HEARTH_BRIDGE=${cfg.networking.bridge}"
            "HEARTH_DHCP_STATIC_START=${staticStart}"
            "HEARTH_DHCP_STATIC_COUNT=${toString staticCount}"
            "HEARTH_DNSMASQ_DROPIN_DIR=/var/lib/hearth/dnsmasq.d"
          ];
        };
      };
    }

    (mkIf cfg.agentPlane.enable {
      systemd.services.hearth-agentd = {
        description = "Hearth agent-plane host daemon";
        after = [
          "hearth.service"
          "network-online.target"
        ];
        wants = [
          "hearth.service"
          "network-online.target"
        ];
        wantedBy = [ "multi-user.target" ];
        path = [ cfg.package ];
        serviceConfig = {
          Type = "simple";
          User = "hearth-agent";
          Group = "hearth";
          UMask = "0007";
          LoadCredential = [
            "http-token:${cfg.agentPlane.httpTokenFile}"
            "ref-key:${cfg.agentPlane.refKeyFile}"
          ];
          ExecStart = "${cfg.package}/bin/hearth-agentd ${lib.concatStringsSep " " agentArgs}";
          Restart = "on-failure";
          StateDirectory = "hearth-agentd";
          StateDirectoryMode = "0750";
          RuntimeDirectory = "hearth-agentd";
          RuntimeDirectoryMode = "0750";
          LogsDirectory = "hearth-agentd";
          NoNewPrivileges = true;
          ProtectSystem = "strict";
          ProtectHome = true;
          PrivateTmp = true;
          ProtectKernelTunables = true;
          ProtectKernelModules = true;
          ProtectControlGroups = true;
          RestrictNamespaces = true;
          RestrictSUIDSGID = true;
          MemoryDenyWriteExecute = true;
          LockPersonality = true;
        };
      };
    })

    (mkIf cfg.networking.manage {
      networking.useNetworkd = true;
      systemd.network.netdevs."20-${cfg.networking.bridge}".netdevConfig = {
        Name = cfg.networking.bridge;
        Kind = "bridge";
      };
      systemd.network.networks."20-${cfg.networking.bridge}" = {
        matchConfig.Name = cfg.networking.bridge;
        linkConfig.RequiredForOnline = "routable";
        address = [ cfg.networking.address ];
        networkConfig.ConfigureWithoutCarrier = true;
      };
      services.dnsmasq = {
        enable = true;
        settings = {
          interface = cfg.networking.bridge;
          bind-dynamic = true;
          dhcp-range = cfg.networking.dynamicRange;
          conf-dir = "/var/lib/hearth/dnsmasq.d";
        };
      };
      systemd.services.dnsmasq = {
        wants = [ "network-online.target" ];
        after = [ "network-online.target" ];
      };
      systemd.services.hearth = {
        wants = [ "dnsmasq.service" ];
        after = [ "dnsmasq.service" ];
      };
      boot.kernel.sysctl."net.ipv4.ip_forward" = 1;
      networking.nftables = {
        enable = true;
        tables.hearth-host = {
          family = "ip";
          content = ''
            chain postrouting {
              type nat hook postrouting priority srcnat; policy accept;
              iifname "${cfg.networking.bridge}" oifname "${cfg.networking.uplinkInterface}" masquerade
            }
          '';
        };
      };
    })
  ]);
}
