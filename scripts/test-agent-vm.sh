#!/usr/bin/env bash
set -euo pipefail

# Hearth agent-VM acceptance test (REFACTOR_PROPOSAL.md §5): build the shared
# vm-base plus the example/agent-vm image, spawn one VM, and assert the whole
# contract that this class of bug hid behind — a reported address, real
# reachability, MAC == allocation, a boot-time budget, stop/start persistence,
# and a clean destroy.
#
# Runs as root on a prepared host (KVM, Cloud Hypervisor, a built guest kernel,
# and working hearth0 DHCP/NAT). It cannot run in CI without that host.

HEARTH_LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/lib.sh
. "${HEARTH_LIB_DIR}/lib.sh"

IMAGE_NAME="${IMAGE_NAME:-agent-vm}"
SERVICE_NAME="${SERVICE_NAME:-agent-probe}"
BUILD_DISK_GIB="${BUILD_DISK_GIB:-8}"
SERVICE_DISK_GIB="${SERVICE_DISK_GIB:-16}"
MEMORY_MIB="${MEMORY_MIB:-2048}"
CPUS="${CPUS:-2}"
# Boot-to-probe budget in seconds. The getty hang (inventory #5) was a silent
# ~90s regression; assert against a ceiling so it can't come back unnoticed.
BOOT_BUDGET_S="${BOOT_BUDGET_S:-120}"
# The port the image actually serves. agent-vm's probe only logs, but vm-base
# ships OpenSSH, so :22 is what this image provides to assert reachability on.
SSH_PORT="${SSH_PORT:-22}"
CLEAN="${CLEAN:-0}"

usage() {
  cat <<EOF
Run the Hearth agent-VM acceptance test.

Environment:
  HEARTHCTL=${HEARTHCTL}
  HEARTH_SOCKET=${HEARTH_SOCKET}
  IMAGE_NAME=${IMAGE_NAME}
  SERVICE_NAME=${SERVICE_NAME}
  BUILD_DISK_GIB=${BUILD_DISK_GIB}   SERVICE_DISK_GIB=${SERVICE_DISK_GIB}
  MEMORY_MIB=${MEMORY_MIB}   CPUS=${CPUS}
  BOOT_BUDGET_S=${BOOT_BUDGET_S}   SSH_PORT=${SSH_PORT}
  CLEAN=${CLEAN}   (set CLEAN=1 to destroy an existing test service first)

Expects a real root hearthd with KVM, Cloud Hypervisor, a built guest kernel,
and working hearth0 DHCP/NAT networking.
EOF
}

[ "${1:-}" = "--help" ] && { usage; exit 0; }

require_root
require_cmd jq buildah timeout
require_hearthctl
require_daemon

if [ "${CLEAN}" = "1" ]; then
  ctl destroy "${SERVICE_NAME}" >/dev/null 2>&1 || true
fi
if service_exists "${SERVICE_NAME}"; then
  echo "service ${SERVICE_NAME} already exists; re-run with CLEAN=1 to replace it." >&2
  exit 1
fi

# 1. Build vm-base, then the agent image (both skipped if already present).
ensure_vm_base
ensure_image "${IMAGE_NAME}" \
  "${REPO_ROOT}/example/agent-vm/Dockerfile" \
  "${REPO_ROOT}/example/agent-vm" \
  "${BUILD_DISK_GIB}"

# 2. Spawn (create + start) and time boot-to-probe.
start_s="$(now_s)"
ctl spawn "${SERVICE_NAME}" \
  --image "${IMAGE_NAME}" \
  --mem "${MEMORY_MIB}" --cpu "${CPUS}" --disk "${SERVICE_DISK_GIB}" >/dev/null
await_marker "${SERVICE_NAME}" "HEARTH_AGENT_PROBE ok boot_count=1" "${BOOT_BUDGET_S}"
elapsed=$(( $(now_s) - start_s ))
assert_lt "boot-to-probe under ${BOOT_BUDGET_S}s budget" "${elapsed}" "${BOOT_BUDGET_S}"

# 3. Address visibility (§4.1): status reports a hearth0 address.
addr="$(svc_field "${SERVICE_NAME}" .address)"
assert_nonempty "status reports an address" "${addr}"

# 4. MAC in status == allocations.toml MAC (locks in the verified non-issue). A
#    lease-sourced address additionally proves the guest presented that MAC to
#    dnsmasq, since the lease join is keyed on it.
status_mac="$(svc_field "${SERVICE_NAME}" .mac)"
alloc="$(alloc_mac "${SERVICE_NAME}")"
assert_nonempty "allocations.toml records a MAC" "${alloc}"
assert_eq "guest MAC matches allocation" "${status_mac}" "${alloc}"

# 5. Reachability: the image's sshd answers on the guest address from the host.
assert_cmd "sshd reachable on ${addr}:${SSH_PORT}" wait_tcp "${addr}" "${SSH_PORT}" 30

# 6. Stop/start persistence: the root disk survives, and the boot counter proves
#    it (a fresh disk would report boot_count=1 again).
ctl stop "${SERVICE_NAME}" >/dev/null
ctl start "${SERVICE_NAME}" >/dev/null
await_marker "${SERVICE_NAME}" "HEARTH_AGENT_PROBE ok boot_count=2" "${BOOT_BUDGET_S}"

# 7. Destroy cleans up: no service record, no root disk, no dnsmasq drop-in.
disk="$(disk_path "${SERVICE_NAME}")"
dropin="$(dropin_path "${SERVICE_NAME}")"
ctl destroy "${SERVICE_NAME}" >/dev/null
refute_service "service removed after destroy" "${SERVICE_NAME}"
assert_absent "root disk removed" "${disk}"
assert_absent "dnsmasq drop-in removed" "${dropin}"

echo
echo "agent-VM acceptance test passed (${_tests_run} checks)."
