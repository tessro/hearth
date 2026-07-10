#!/usr/bin/env bash
set -euo pipefail

# Hermes multi-VM acceptance test (REFACTOR_PROPOSAL.md §5 + §10): spawn TWO
# VMs — hermes-a and hermes-b — from the one hermes-vm image, each provisioned
# with its OWN secret env file, and assert they run simultaneously with distinct
# addresses, MACs, and hostnames, and that curl reaches each on :9119. This is
# the story §3/§10 exist for: two services from one immutable image sharing
# nothing but that image.
#
# The two env files are generated from hermes.env.example (below). The real
# example/hermes-vm/hermes.env is NEVER read or touched.
#
# Runs as root on a prepared host (KVM, Cloud Hypervisor, a built guest kernel,
# working hearth0 DHCP/NAT). Building the image needs outbound internet and a
# pinned Hermes commit; set HERMES_COMMIT=<sha> (skipped if the image exists).

HEARTH_LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib.sh
. "${HEARTH_LIB_DIR}/lib.sh"

IMAGE_NAME="${IMAGE_NAME:-hermes-vm}"
CONTEXT_DIR="${REPO_ROOT}/example/hermes-vm"
BUILD_DISK_GIB="${BUILD_DISK_GIB:-16}"
SERVICE_DISK_GIB="${SERVICE_DISK_GIB:-32}"
MEMORY_MIB="${MEMORY_MIB:-4096}"
CPUS="${CPUS:-4}"
BOOT_BUDGET_S="${BOOT_BUDGET_S:-300}"
HERMES_PORT="${HERMES_PORT:-9119}"
# The two VMs spawned from the one image.
NAME_A="${NAME_A:-hermes-a}"
NAME_B="${NAME_B:-hermes-b}"
CLEAN="${CLEAN:-0}"

usage() {
  cat <<EOF
Run the Hermes multi-VM (§10) acceptance test: two provisioned VMs from one image.

Environment:
  HEARTHCTL=${HEARTHCTL}
  HEARTH_SOCKET=${HEARTH_SOCKET}
  IMAGE_NAME=${IMAGE_NAME}
  NAME_A=${NAME_A}   NAME_B=${NAME_B}
  MEMORY_MIB=${MEMORY_MIB}   CPUS=${CPUS}   SERVICE_DISK_GIB=${SERVICE_DISK_GIB}
  BOOT_BUDGET_S=${BOOT_BUDGET_S}   HERMES_PORT=${HERMES_PORT}
  HERMES_COMMIT=${HERMES_COMMIT:-<unset>}   (required only if the image must be built)
  CLEAN=${CLEAN}   (set CLEAN=1 to destroy existing test VMs first)

Expects a real root hearthd with KVM, Cloud Hypervisor, a built guest kernel,
and working hearth0 DHCP/NAT. Two temp env files are generated from
hermes.env.example; the real hermes.env is never touched.
EOF
}

[ "${1:-}" = "--help" ] && { usage; exit 0; }

require_root
require_cmd jq curl buildah sed
require_hearthctl
require_daemon

EXAMPLE_ENV="${CONTEXT_DIR}/hermes.env.example"
if [ ! -f "${EXAMPLE_ENV}" ]; then
  echo "error: ${EXAMPLE_ENV} not found." >&2
  exit 1
fi

if [ "${CLEAN}" = "1" ]; then
  ctl destroy "${NAME_A}" >/dev/null 2>&1 || true
  ctl destroy "${NAME_B}" >/dev/null 2>&1 || true
fi
for name in "${NAME_A}" "${NAME_B}"; do
  if service_exists "${name}"; then
    echo "service ${name} already exists; re-run with CLEAN=1 to replace it." >&2
    exit 1
  fi
done

# Per-VM secret files, generated from the example so the two VMs get genuinely
# distinct dashboard credentials. Cleaned up on exit; the real hermes.env is
# never read.
TMPDIR_ENV="$(mktemp -d)"
trap 'rm -rf "${TMPDIR_ENV}"' EXIT

rand_hex() { openssl rand -hex 16 2>/dev/null || head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n'; }

make_env() {  # make_env <out> <username> <secret>
  sed \
    -e "s|^HERMES_DASHBOARD_BASIC_AUTH_USERNAME=.*|HERMES_DASHBOARD_BASIC_AUTH_USERNAME=$2|" \
    -e "s|^HERMES_DASHBOARD_BASIC_AUTH_PASSWORD=.*|HERMES_DASHBOARD_BASIC_AUTH_PASSWORD=$3|" \
    -e "s|^HERMES_DASHBOARD_BASIC_AUTH_SECRET=.*|HERMES_DASHBOARD_BASIC_AUTH_SECRET=$3|" \
    "${EXAMPLE_ENV}" >"$1"
}

ENV_A="${TMPDIR_ENV}/${NAME_A}.env"
ENV_B="${TMPDIR_ENV}/${NAME_B}.env"
make_env "${ENV_A}" "${NAME_A}" "$(rand_hex)"
make_env "${ENV_B}" "${NAME_B}" "$(rand_hex)"

# 1. Build vm-base, then the hermes image if missing (needs a pinned commit).
ensure_vm_base
if ! image_exists "${IMAGE_NAME}"; then
  if [ -z "${HERMES_COMMIT:-}" ]; then
    echo "error: image ${IMAGE_NAME} is not built and HERMES_COMMIT is unset." >&2
    echo "the Hermes install is pinned by commit (§8). Build it with, e.g.:" >&2
    echo "  HERMES_COMMIT=<sha> $0" >&2
    exit 1
  fi
  ensure_image "${IMAGE_NAME}" "${CONTEXT_DIR}/Dockerfile" "${CONTEXT_DIR}" \
    "${BUILD_DISK_GIB}" --build-arg "HERMES_COMMIT=${HERMES_COMMIT}"
fi

# 2. Spawn both VMs from the one image, each with its own secret env file.
spawn_hermes() {  # spawn_hermes <name> <env-file>
  ctl spawn "$1" \
    --image "${IMAGE_NAME}" \
    --provision-file "source=$2,dest=/home/agent/.hermes/.env,mode=0600,owner=1000:1000" \
    --mem "${MEMORY_MIB}" --cpu "${CPUS}" --disk "${SERVICE_DISK_GIB}" >/dev/null
}

spawn_hermes "${NAME_A}" "${ENV_A}"
spawn_hermes "${NAME_B}" "${ENV_B}"

# 3. Both reach the readiness probe (they boot concurrently).
await_marker "${NAME_A}" "HERMES_PROBE ok" "${BOOT_BUDGET_S}"
await_marker "${NAME_B}" "HERMES_PROBE ok" "${BOOT_BUDGET_S}"

# 4. Read each VM's identity from status.
addr_a="$(svc_field "${NAME_A}" .address)"
addr_b="$(svc_field "${NAME_B}" .address)"
mac_a="$(svc_field "${NAME_A}" .mac)"
mac_b="$(svc_field "${NAME_B}" .mac)"
host_a="$(svc_field "${NAME_A}" .provision.hostname)"
host_b="$(svc_field "${NAME_B}" .provision.hostname)"

assert_nonempty "${NAME_A} has an address" "${addr_a}"
assert_nonempty "${NAME_B} has an address" "${addr_b}"

# 5. Both run simultaneously, and everything that must differ, differs.
assert_eq "${NAME_A} is running" "$(svc_field "${NAME_A}" .running)" "true"
assert_eq "${NAME_B} is running" "$(svc_field "${NAME_B}" .running)" "true"
assert_ne "distinct addresses" "${addr_a}" "${addr_b}"
assert_ne "distinct MACs" "${mac_a}" "${mac_b}"
assert_ne "distinct hostnames" "${host_a}" "${host_b}"
# Hostname defaults to the service name (§3/§10), so N VMs get distinct identities.
assert_eq "${NAME_A} hostname is its service name" "${host_a}" "${NAME_A}"
assert_eq "${NAME_B} hostname is its service name" "${host_b}" "${NAME_B}"

# 6. curl reaches each VM's gateway on :9119 (any HTTP code proves it answered;
#    hermes serve returns 401 to unauthenticated non-loopback callers).
if code_a="$(http_probe "http://${addr_a}:${HERMES_PORT}/" 90)"; then
  pass "${NAME_A} serves HTTP on :${HERMES_PORT} (HTTP ${code_a})"
else
  fail "curl could not reach ${NAME_A} on ${addr_a}:${HERMES_PORT}"
fi
if code_b="$(http_probe "http://${addr_b}:${HERMES_PORT}/" 90)"; then
  pass "${NAME_B} serves HTTP on :${HERMES_PORT} (HTTP ${code_b})"
else
  fail "curl could not reach ${NAME_B} on ${addr_b}:${HERMES_PORT}"
fi

# 7. Tear both down and confirm cleanup.
for name in "${NAME_A}" "${NAME_B}"; do
  disk="$(disk_path "${name}")"
  dropin="$(dropin_path "${name}")"
  ctl destroy "${name}" >/dev/null
  refute_service "service ${name} removed after destroy" "${name}"
  assert_absent "${name} root disk removed" "${disk}"
  assert_absent "${name} dnsmasq drop-in removed" "${dropin}"
done

echo
echo "Hermes multi-VM acceptance test passed (${_tests_run} checks)."
