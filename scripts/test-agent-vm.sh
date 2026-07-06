#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

HEARTHCTL="${HEARTHCTL:-${repo_root}/target/debug/hearthctl}"
HEARTH_SOCKET="${HEARTH_SOCKET:-/run/hearth.sock}"
IMAGE_NAME="${IMAGE_NAME:-agent-vm}"
SERVICE_NAME="${SERVICE_NAME:-agent-probe}"
BUILD_DISK_GIB="${BUILD_DISK_GIB:-8}"
SERVICE_DISK_GIB="${SERVICE_DISK_GIB:-16}"
MEMORY_MIB="${MEMORY_MIB:-2048}"
CPUS="${CPUS:-2}"
CLEAN="${CLEAN:-0}"

ctl() {
  "${HEARTHCTL}" --socket "${HEARTH_SOCKET}" "$@"
}

usage() {
  cat <<EOF
Run the Hearth agent VM acceptance test.

Environment:
  HEARTHCTL=${HEARTHCTL}
  HEARTH_SOCKET=${HEARTH_SOCKET}
  IMAGE_NAME=${IMAGE_NAME}
  SERVICE_NAME=${SERVICE_NAME}
  BUILD_DISK_GIB=${BUILD_DISK_GIB}
  SERVICE_DISK_GIB=${SERVICE_DISK_GIB}
  MEMORY_MIB=${MEMORY_MIB}
  CPUS=${CPUS}
  CLEAN=${CLEAN}

Set CLEAN=1 to destroy any existing test service and image first.
This test expects a real hearthd with KVM, Cloud Hypervisor, a guest kernel, and
working hearth0 DHCP/NAT networking.
EOF
}

wait_for_log() {
  local pattern="$1"
  local timeout_s="$2"
  local deadline=$((SECONDS + timeout_s))
  local log_file
  log_file="$(mktemp)"
  while [ "${SECONDS}" -lt "${deadline}" ]; do
    if ctl logs "${SERVICE_NAME}" >"${log_file}" 2>/dev/null; then
      if grep -Fq "${pattern}" "${log_file}"; then
        rm -f "${log_file}"
        return 0
      fi
    fi
    sleep 2
  done
  echo "Timed out waiting for serial log marker: ${pattern}" >&2
  echo "Last logs:" >&2
  cat "${log_file}" >&2 || true
  rm -f "${log_file}"
  return 1
}

if [ "${1:-}" = "--help" ]; then
  usage
  exit 0
fi

if [ "$(id -u)" -ne 0 ]; then
  echo "Run this test as root so image build preserves rootfs ownership and metadata." >&2
  exit 1
fi

if [ ! -x "${HEARTHCTL}" ]; then
  echo "hearthctl not found or not executable: ${HEARTHCTL}" >&2
  echo "Build it first with: devenv shell cargo build" >&2
  exit 1
fi

if [ "${CLEAN}" = "1" ]; then
  ctl destroy "${SERVICE_NAME}" >/dev/null 2>&1 || true
  ctl image rm "${IMAGE_NAME}" >/dev/null 2>&1 || true
fi

if ctl status "${SERVICE_NAME}" >/dev/null 2>&1; then
  echo "Service ${SERVICE_NAME} already exists. Re-run with CLEAN=1 to replace it." >&2
  exit 1
fi

if ctl image ls | grep -Eq "(^|[[:space:]])${IMAGE_NAME}([[:space:]]|$)"; then
  echo "Image ${IMAGE_NAME} already exists. Re-run with CLEAN=1 to replace it." >&2
  exit 1
fi

ctl image build \
  --name "${IMAGE_NAME}" \
  --dockerfile "${repo_root}/example/agent-vm/Dockerfile" \
  --context "${repo_root}/example/agent-vm" \
  --disk "${BUILD_DISK_GIB}"

ctl image ls

ctl create "${SERVICE_NAME}" \
  --from "${IMAGE_NAME}" \
  --disk "${SERVICE_DISK_GIB}" \
  --mem "${MEMORY_MIB}" \
  --cpu "${CPUS}"

ctl start "${SERVICE_NAME}"
wait_for_log "HEARTH_AGENT_PROBE ok boot_count=1" 180

ctl stop "${SERVICE_NAME}"
ctl start "${SERVICE_NAME}"
wait_for_log "HEARTH_AGENT_PROBE ok boot_count=2" 180

echo "Hearth agent VM acceptance test passed for service ${SERVICE_NAME}."
