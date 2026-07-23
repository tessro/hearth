# Pin cloud-hypervisor ahead of the version the flake's nixpkgs provides.
# hearthd is written against the CHV 53 HTTP API contract (body-less action
# PUTs, single Content-Length-framed responses); see the chv_request helpers
# in crates/hearthd/src/host.rs. Bump `version`, the src `hash`, and the
# vendor `hash` together when moving to a newer release. Snapshots are
# CHV-version-bound: after a bump, existing snapshots stop being restorable
# (docs/operations.md §6).
{
  cloud-hypervisor,
  fetchFromGitHub,
  rustPlatform,
}:

cloud-hypervisor.overrideAttrs (
  old: rec {
    version = "53.0";

    src = fetchFromGitHub {
      owner = "cloud-hypervisor";
      repo = "cloud-hypervisor";
      rev = "v${version}";
      hash = "sha256-fPTGf8bAITDA8QwllWbbGXA7tJ6p/SxRDfcBQVRvCTI=";
    };

    cargoDeps = rustPlatform.fetchCargoVendor {
      inherit src;
      name = "cloud-hypervisor-${version}-vendor";
      hash = "sha256-+RbW/9ap/69MyODUk/bHBlH6ZuqYYIyKaarYSMQ2G7w=";
    };
  }
)
