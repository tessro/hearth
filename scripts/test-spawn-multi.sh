#!/usr/bin/env bash
set -euo pipefail

# Generic multi-VM spawn test (REFACTOR_PROPOSAL.md §5 + §10). Spawns TWO VMs
# from the one example/agent-vm image and asserts they run simultaneously with
# distinct addresses, MACs, and hostnames, and that each is reachable. Unlike
# test-hermes-vm.sh this needs no secrets and no external supply chain, so it is
# the fast `hearthctl spawn` smoke for the N-from-one-template contract.
#
# Runs as root on a prepared host (KVM, Cloud Hypervisor, a built guest kernel,
# working hearth0 DHCP/NAT).

HEARTH_LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib.sh
. "${HEARTH_LIB_DIR}/lib.sh"

IMAGE_NAME="${IMAGE_NAME:-agent-vm}"
NAME_A="${NAME_A:-agent-a}"
NAME_B="${NAME_B:-agent-b}"
BUILD_DISK_GIB="${BUILD_DISK_GIB:-8}"
SERVICE_DISK_GIB="${SERVICE_DISK_GIB:-16}"
MEMORY_MIB="${MEMORY_MIB:-2048}"
CPUS="${CPUS:-2}"
BOOT_BUDGET_S="${BOOT_BUDGET_S:-120}"
SSH_PORT="${SSH_PORT:-22}"
CLEAN="${CLEAN:-0}"

usage() {
  cat <<EOF
Run the generic multi-VM spawn test: two VMs from one agent-vm image.

Environment:
  HEARTHCTL=${HEARTHCTL}
  HEARTH_SOCKET=${HEARTH_SOCKET}
  IMAGE_NAME=${IMAGE_NAME}
  NAME_A=${NAME_A}   NAME_B=${NAME_B}
  MEMORY_MIB=${MEMORY_MIB}   CPUS=${CPUS}   SERVICE_DISK_GIB=${SERVICE_DISK_GIB}
  BOOT_BUDGET_S=${BOOT_BUDGET_S}   SSH_PORT=${SSH_PORT}
  CLEAN=${CLEAN}   (set CLEAN=1 to destroy existing test VMs first)

Expects a real root hearthd with KVM, Cloud Hypervisor, a built guest kernel,
and working hearth0 DHCP/NAT networking.
EOF
}

[ "${1:-}" = "--help" ] && { usage; exit 0; }

require_root
require_cmd jq buildah timeout
require_hearthctl
require_daemon

if [ "${NAME_A}" = "${NAME_B}" ]; then
  echo "error: NAME_A and NAME_B must differ (got '${NAME_A}')." >&2
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

# 1. Build vm-base, then the agent image (both skipped if already present).
ensure_vm_base
ensure_image "${IMAGE_NAME}" \
  "${REPO_ROOT}/example/agent-vm/Dockerfile" \
  "${REPO_ROOT}/example/agent-vm" \
  "${BUILD_DISK_GIB}"

# 2. Spawn both VMs from the one image, overriding the hostname per VM.
spawn_agent() {  # spawn_agent <name>
  ctl spawn "$1" \
    --image "${IMAGE_NAME}" \
    --hostname "$1" \
    --mem "${MEMORY_MIB}" --cpu "${CPUS}" --disk "${SERVICE_DISK_GIB}" >/dev/null
}

spawn_agent "${NAME_A}"
spawn_agent "${NAME_B}"

# 3. Both reach the readiness probe (they boot concurrently).
await_marker "${NAME_A}" "HEARTH_AGENT_PROBE ok boot_count=1" "${BOOT_BUDGET_S}"
await_marker "${NAME_B}" "HEARTH_AGENT_PROBE ok boot_count=1" "${BOOT_BUDGET_S}"
# vm-base contract: each VM proves the agent user's session stack (logind,
# session bus, XDG_RUNTIME_DIR, lingering user@1000) is live at boot.
await_marker "${NAME_A}" "HEARTH_USERSESSION ok" "${BOOT_BUDGET_S}"
await_marker "${NAME_B}" "HEARTH_USERSESSION ok" "${BOOT_BUDGET_S}"

# 4. Read each VM's identity from status.
addr_a="$(svc_field "${NAME_A}" .address)"
addr_b="$(svc_field "${NAME_B}" .address)"
mac_a="$(svc_field "${NAME_A}" .mac)"
mac_b="$(svc_field "${NAME_B}" .mac)"
host_a="$(svc_field "${NAME_A}" .cloud_init.hostname)"
host_b="$(svc_field "${NAME_B}" .cloud_init.hostname)"

assert_nonempty "${NAME_A} has an address" "${addr_a}"
assert_nonempty "${NAME_B} has an address" "${addr_b}"

# 5. Both run simultaneously, and everything that must differ, differs.
assert_eq "${NAME_A} is running" "$(svc_field "${NAME_A}" .running)" "true"
assert_eq "${NAME_B} is running" "$(svc_field "${NAME_B}" .running)" "true"
assert_ne "distinct addresses" "${addr_a}" "${addr_b}"
assert_ne "distinct MACs" "${mac_a}" "${mac_b}"
assert_ne "distinct hostnames" "${host_a}" "${host_b}"
assert_eq "${NAME_A} hostname honored" "${host_a}" "${NAME_A}"
assert_eq "${NAME_B} hostname honored" "${host_b}" "${NAME_B}"

# 6. Each guest's sshd answers on its own address from the host.
assert_cmd "${NAME_A} sshd reachable on ${addr_a}:${SSH_PORT}" wait_tcp "${addr_a}" "${SSH_PORT}" 30
assert_cmd "${NAME_B} sshd reachable on ${addr_b}:${SSH_PORT}" wait_tcp "${addr_b}" "${SSH_PORT}" 30

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
echo "multi-VM spawn test passed (${_tests_run} checks)."
