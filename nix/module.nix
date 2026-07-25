{
  config,
  lib,
  pkgs,
  hearthPackage,
  hearthGuestKernel,
  hearthCloudHypervisor,
  hearthStalinPackage,
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
  egressToml = pkgs.formats.toml { };
  egressConfig = egressToml.generate "hearth-egress.toml" {
    proxy_url = "http://${cfg.egressProxy.guestAddress}:${toString cfg.egressProxy.port}";
    ca_cert = cfg.egressProxy.caCertificateFile;
    no_proxy = lib.concatStringsSep "," cfg.egressProxy.noProxy;
    environment = cfg.egressProxy.placeholderEnvironment;
  };
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
      default = hearthCloudHypervisor;
      description = ''
        cloud-hypervisor used to launch VMs. Defaults to the version Hearth
        pins and is tested against (see nix/cloud-hypervisor.nix); this does not
        touch the system-wide pkgs.cloud-hypervisor. Override to supply your own.
      '';
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
    egressProxy = {
      enable = mkEnableOption "one host-wide Stalin egress proxy for all Hearth VMs";
      package = mkOption {
        type = types.package;
        default = hearthStalinPackage;
        description = "Stalin package used by the host-wide egress service.";
      };
      stalinConfigFile = mkOption {
        type = types.str;
        default = "/etc/stalin/config.toml";
        description = "Runtime Stalin policy path. It may refer to systemd credentials.";
      };
      credentials = mkOption {
        type = types.attrsOf types.str;
        default = { };
        description = "Stalin systemd credential names mapped to runtime source paths.";
      };
      caCertificateFile = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = ''
          Public Stalin CA certificate path. Hearth copies this certificate,
          but never the CA key or provider keys, into each new VM.
        '';
      };
      guestAddress = mkOption {
        type = types.str;
        default = "10.26.8.1";
        description = "Host bridge address that guests use to reach Stalin.";
      };
      port = mkOption {
        type = types.port;
        default = 8080;
        description = "Stalin HTTP proxy port reachable from Hearth guests.";
      };
      noProxy = mkOption {
        type = types.listOf types.str;
        default = [
          "localhost"
          "127.0.0.1"
          "::1"
          "10.26.8.0/24"
        ];
        description = "Addresses that guest HTTP clients must not send through Stalin.";
      };
      placeholderEnvironment = mkOption {
        type = types.attrsOf types.str;
        default = { };
        example = {
          OPENAI_API_KEY = "stalin-managed";
          ANTHROPIC_API_KEY = "stalin-managed";
        };
        description = ''
          Public placeholder values for clients that require a provider
          variable before they send a request. Never put a real key here: Nix
          stores these values and Hearth writes them into VM disks.
        '';
      };
      blockDirectHttps = mkOption {
        type = types.bool;
        default = false;
        description = ''
          Reject direct guest traffic to port 443 (TCP and UDP) so HTTPS
          cannot bypass Stalin. All other guest traffic is unaffected. This
          requires networking.manage.
        '';
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
        {
          assertion = !cfg.egressProxy.enable || cfg.egressProxy.caCertificateFile != null;
          message = "services.hearth.egressProxy requires caCertificateFile";
        }
        {
          assertion =
            !cfg.egressProxy.enable
            || (cfg.egressProxy.caCertificateFile != null
              && lib.hasPrefix "/" cfg.egressProxy.stalinConfigFile
              && lib.hasPrefix "/" cfg.egressProxy.caCertificateFile);
          message = "services.hearth.egressProxy config and CA paths must be absolute";
        }
        {
          assertion = !cfg.egressProxy.blockDirectHttps || cfg.networking.manage;
          message = "services.hearth.egressProxy.blockDirectHttps requires networking.manage";
        }
        {
          assertion =
            lib.all
              (name: builtins.match "[A-Za-z_][A-Za-z0-9_]*" name != null)
              (builtins.attrNames cfg.egressProxy.placeholderEnvironment);
          message = "services.hearth.egressProxy.placeholderEnvironment keys must be valid variable names";
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
          pkgs.systemd
        ];
        serviceConfig = {
          Type = "notify";
          ExecStart = "${cfg.package}/bin/hearthd --guest-kernel ${cfg.guestKernel}/lib/hearth/kernel/vmlinux";
          Restart = "on-failure";
          RestartSec = 2;
          RuntimeDirectory = "hearth";
          RuntimeDirectoryMode = "0770";
          # Cloud Hypervisor binds each VM's hybrid-vsock socket under
          # /run/hearth and outlives daemon restarts; without preserve, a
          # restart deletes those bound sockets and every host->guest
          # agent-plane connect fails until the VM reboots.
          RuntimeDirectoryPreserve = "yes";
          StateDirectory = "hearth";
          LogsDirectory = "hearth";
          TimeoutStopSec = 120;
          Environment = [
            "HEARTH_BRIDGE=${cfg.networking.bridge}"
            "HEARTH_DHCP_STATIC_START=${staticStart}"
            "HEARTH_DHCP_STATIC_COUNT=${toString staticCount}"
            "HEARTH_DNSMASQ_DROPIN_DIR=/var/lib/hearth/dnsmasq.d"
          ] ++ lib.optional cfg.egressProxy.enable "HEARTH_EGRESS_CONFIG=${egressConfig}";
        };
      };
    }

    (mkIf cfg.egressProxy.enable {
      services.stalin = {
        enable = true;
        package = cfg.egressProxy.package;
        configFile = cfg.egressProxy.stalinConfigFile;
        credentials = cfg.egressProxy.credentials;
        healthAddress = "${cfg.egressProxy.guestAddress}:${toString cfg.egressProxy.port}";
      };
      systemd.services.hearth = {
        after = [ "stalin.service" ];
        requires = [ "stalin.service" ];
      };
    })

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
            ${lib.optionalString cfg.egressProxy.blockDirectHttps ''
              chain forward {
                type filter hook forward priority filter; policy accept;
                # Direct 443 from guests would bypass Stalin. UDP 443 covers
                # QUIC/HTTP-3, which an HTTP proxy cannot carry; blocking it
                # makes those clients fall back to TCP through the proxy.
                # Inbound connections published to a guest's port 443 arrive
                # from the uplink and are unaffected.
                iifname "${cfg.networking.bridge}" tcp dport 443 reject with icmp type admin-prohibited
                iifname "${cfg.networking.bridge}" udp dport 443 reject with icmp type admin-prohibited
              }
            ''}
          '';
        };
      };
    })
  ]);
}
