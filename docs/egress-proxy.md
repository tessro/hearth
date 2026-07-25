# Host-managed VM egress

Hearth can send HTTP and HTTPS from every new VM through one Stalin service on
the host. Provider keys stay in Stalin's systemd credentials. VMs receive only:

- the HTTP proxy address;
- Stalin's public CA certificate;
- optional public placeholder values for clients that require a key variable.

No VM or app gets a real provider key. Stalin inspects only the exact TLS
destinations in its policy and tunnels other HTTPS traffic.

## NixOS setup

Hearth's flake pins Stalin and imports its NixOS module. A host needs one Stalin
policy, one CA pair, and one set of provider credentials.

Generate the CA once. Keep the private key in your secret manager:

```sh
openssl genrsa -out stalin-ca-key.pem 4096
openssl req -x509 -new -nodes \
  -key stalin-ca-key.pem \
  -sha256 -days 365 \
  -subj "/CN=stalin local MITM CA" \
  -addext "basicConstraints=critical,CA:TRUE" \
  -addext "keyUsage=critical,keyCertSign,cRLSign,digitalSignature" \
  -out stalin-ca.pem
```

Use an exact Stalin policy. This example covers OpenAI and OpenRouter:

```toml
listen = "10.26.8.1:8080"

[mitm]
mode = "selective"
ca_cert = "/etc/stalin/ca.pem"
ca_key = "/run/credentials/stalin.service/mitm_ca_key"

[destinations.openai]
scheme = "https"
host = "api.openai.com"
port = 443
tls = "inspect"

[destinations.openrouter]
scheme = "https"
host = "openrouter.ai"
port = 443
tls = "inspect"

[secrets.openai_api_key]
source = "systemd_credential"
name = "openai_api_key"

[secrets.openrouter_api_key]
source = "systemd_credential"
name = "openrouter_api_key"

[[rules]]
name = "openai-auth"
destination = "openai"
audit = true

[rules.request_headers.set.authorization]
secret = "openai_api_key"
format = "Bearer {value}"

[[rules]]
name = "openrouter-auth"
destination = "openrouter"
audit = true

[rules.request_headers.set.authorization]
secret = "openrouter_api_key"
format = "Bearer {value}"
```

Then connect the policy and runtime secret paths to Hearth:

```nix
{
  environment.etc = {
    "stalin/config.toml".source = ./stalin.toml;
    # This certificate is public. It is safe in the Nix store.
    "stalin/ca.pem".source = ./stalin-ca.pem;
  };

  services.hearth = {
    enable = true;
    networking = {
      manage = true;
      uplinkInterface = "enp1s0";
    };

    egressProxy = {
      enable = true;
      stalinConfigFile = "/etc/stalin/config.toml";
      caCertificateFile = "/etc/stalin/ca.pem";

      # Runtime paths from agenix, sops-nix, or another secret manager.
      credentials = {
        mitm_ca_key = "/run/agenix/stalin-ca-key";
        openai_api_key = "/run/agenix/openai-api-key";
        openrouter_api_key = "/run/agenix/openrouter-api-key";
      };

      # Public dummy values for SDKs that check these before making a request.
      # Hermes, for example, only offers a provider whose variable is set.
      placeholderEnvironment = {
        OPENAI_API_KEY = "stalin-managed";
        OPENROUTER_API_KEY = "stalin-managed";
      };

      # Block direct guest port-443 traffic so HTTPS cannot bypass Stalin.
      blockDirectHttps = true;
    };
  };
}
```

`nixos-rebuild switch` starts Stalin, waits for its health check, then starts
Hearth. Run:

```sh
systemctl status stalin.service
hearthctl host check
```

The Hearth check adds the egress config, public CA, and proxy socket when this
feature is on.

## What a VM gets

On `hearthctl create` or `spawn`, Hearth writes the public policy into the new
disk. The current `vm-base`:

- adds the CA to the system trust bundle before normal services start;
- sets lower- and upper-case HTTP proxy variables for PID 1 services;
- sets the same values for systemd user services and login shells;
- gives `hearth-guestd` the same environment;
- passes the fixed proxy/CA set and configured placeholder names through
  Hermes's cleared environment.

This covers `curl`, common HTTP SDKs, normal system services, user services, and
Hermes without per-app proxy setup. Existing VMs do not change in place. Rebuild
their image on the current `vm-base`, then recreate them.

The optional `EnvironmentFile=-/etc/hearth/egress.env` line in the guestd unit
is internal wiring, not an operator step. Hearth writes that file into a new
disk. The same values also come from PID 1's global environment; the direct
guestd read keeps the agent path intact if a workload overrides that global
setting.

When egress is on, Hearth rejects an image that lacks the early CA unit before
it allocates or writes a disk. `hearthctl image build` records this support in
the image manifest.

## Limits

Stalin is an explicit HTTP proxy. Programs that honor `http_proxy`,
`https_proxy`, `HTTP_PROXY`, or `HTTPS_PROXY` use it. Raw TCP, UDP, SSH, and
programs that ignore these variables cannot use an HTTP proxy without their own
support.

`blockDirectHttps = true` rejects direct guest traffic to port 443 at the
host, TCP and UDP both — UDP 443 covers QUIC/HTTP-3, which an HTTP proxy
cannot carry, so those clients fall back to TCP through Stalin. Everything
else — DNS, NTP, SSH, raw TCP, plain HTTP — is not filtered. An HTTPS client
that ignores the proxy variables fails visibly instead of bypassing
inspection; Stalin only ever sees traffic that programs send to it.

The proxy variables and CA are global for the VM. Selective TLS inspection means
only exact Stalin destinations use the private CA; other HTTPS requests remain
tunnels and use their normal public server certificates.

## Non-NixOS hosts

Run one Stalin service on the Hearth bridge, then create a non-secret host file:

```toml
proxy_url = "http://10.26.8.1:8080"
ca_cert = "/etc/stalin/ca.pem"
no_proxy = "localhost,127.0.0.1,::1,10.26.8.0/24"

[environment]
OPENAI_API_KEY = "stalin-managed"
```

Point the host daemon at it with `HEARTH_EGRESS_CONFIG=/etc/hearth/egress.toml`
or `hearthd --egress-config /etc/hearth/egress.toml`. This variable configures
the host daemon once. It is not an app or per-VM proxy setting.

The non-NixOS operator must also order `hearth.service` after the Stalin service
and, if direct-HTTPS blocking is wanted, add an equivalent firewall rule
rejecting guest port-443 forwards.
