# shellcheck shell=bash
# Shared helpers for the Hearth acceptance tests (test-agent-vm.sh,
# test-hermes-vm.sh, test-spawn-multi.sh). Source it, don't execute it:
#
#   HEARTH_LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
#   . "${HEARTH_LIB_DIR}/lib.sh"
#
# It only defines functions and default vars; it never runs a test on its own.
# All state lives in the caller. The caller is expected to `set -euo pipefail`.

HEARTH_LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="${REPO_ROOT:-$(cd "${HEARTH_LIB_DIR}/.." && pwd)}"

# Which CLI to drive. Defaults to the dev build; a packaged host can point this
# at the installed binary: HEARTHCTL=/usr/local/bin/hearthctl.
HEARTHCTL="${HEARTHCTL:-${REPO_ROOT}/target/debug/hearthctl}"
HEARTH_SOCKET="${HEARTH_SOCKET:-/run/hearth.sock}"

# Host locations hearthd writes to (must match the daemon's config). Overridable
# so the tests work against a daemon run with non-default dirs.
HEARTH_ALLOCATIONS="${HEARTH_ALLOCATIONS:-/etc/hearth/allocations.toml}"
HEARTH_DISKS_DIR="${HEARTH_DISKS_DIR:-/var/lib/hearth/disks}"
HEARTH_DNSMASQ_DROPIN_DIR="${HEARTH_DNSMASQ_DROPIN_DIR:-/etc/dnsmasq.d/hearth}"

# --- CLI wrappers -----------------------------------------------------------

# Human-readable hearthctl (tables, plain text).
ctl() {
  "${HEARTHCTL}" --socket "${HEARTH_SOCKET}" "$@"
}

# JSON hearthctl. Every response is one line of `{"id":...,"ok":...,"result":...}`,
# so field extraction goes through `.result` (see svc_field).
cjson() {
  "${HEARTHCTL}" --socket "${HEARTH_SOCKET}" --json "$@"
}

# --- assertions -------------------------------------------------------------
# Fail fast (the ground rule): the first failed assertion exits the script with a
# clear message. Passing checks print an "ok" line to stdout; failures go to
# stderr so a captured stdout stays clean.

_tests_run=0

pass() {
  _tests_run=$((_tests_run + 1))
  printf 'ok   %2d - %s\n' "${_tests_run}" "$*"
}

fail() {
  _tests_run=$((_tests_run + 1))
  printf 'FAIL %2d - %s\n' "${_tests_run}" "$*" >&2
  exit 1
}

# assert_eq <desc> <actual> <expected>
assert_eq() {
  if [ "$2" = "$3" ]; then
    pass "$1 ($2)"
  else
    fail "$1: expected '$3', got '$2'"
  fi
}

# assert_ne <desc> <a> <b>: the two values must differ.
assert_ne() {
  if [ "$2" != "$3" ]; then
    pass "$1 ('$2' != '$3')"
  else
    fail "$1: both values are '$2'"
  fi
}

# assert_nonempty <desc> <value>: value must be non-empty and not the string
# "null" (jq renders a JSON null as that).
assert_nonempty() {
  if [ -n "$2" ] && [ "$2" != "null" ]; then
    pass "$1 ($2)"
  else
    fail "$1: value is empty or null"
  fi
}

# assert_lt <desc> <value> <limit>: integer value must be < limit.
assert_lt() {
  if [ "$2" -lt "$3" ]; then
    pass "$1 ($2 < $3)"
  else
    fail "$1: $2 is not < $3"
  fi
}

# assert_cmd <desc> <cmd...>: the command must succeed.
assert_cmd() {
  local desc="$1"
  shift
  if "$@"; then
    pass "${desc}"
  else
    fail "${desc}"
  fi
}

# assert_absent <desc> <path>: the path must not exist.
assert_absent() {
  if [ ! -e "$2" ]; then
    pass "$1"
  else
    fail "$1: still present: $2"
  fi
}

# refute_service <desc> <name>: the service must not exist in the registry.
refute_service() {
  if ! ctl status "$2" >/dev/null 2>&1; then
    pass "$1"
  else
    fail "$1: service $2 still exists"
  fi
}

# --- prerequisites ----------------------------------------------------------

require_root() {
  if [ "$(id -u)" -ne 0 ]; then
    echo "error: run as root — image build preserves rootfs ownership and VM boot needs KVM/CHV/nft." >&2
    exit 1
  fi
}

# require_cmd <cmd...>: every named command must be on PATH.
require_cmd() {
  local c missing=0
  for c in "$@"; do
    if ! command -v "$c" >/dev/null 2>&1; then
      echo "error: required command not found: $c" >&2
      missing=1
    fi
  done
  [ "${missing}" -eq 0 ] || exit 1
}

require_hearthctl() {
  if [ ! -x "${HEARTHCTL}" ]; then
    echo "error: hearthctl not found or not executable: ${HEARTHCTL}" >&2
    echo "build it first (cargo build), or point HEARTHCTL at an installed binary." >&2
    exit 1
  fi
}

require_daemon() {
  if ! ctl ping >/dev/null 2>&1; then
    echo "error: no hearthd answered on ${HEARTH_SOCKET}." >&2
    echo "start one (make dev, or 'systemctl start hearth.service') then retry." >&2
    exit 1
  fi
}

# --- registry / status helpers ---------------------------------------------

now_s() { date +%s; }

# service_exists <name>: 0 if the daemon knows the service, 1 otherwise.
service_exists() { ctl status "$1" >/dev/null 2>&1; }

# image_exists <name>: 0 if a built image with that name is registered.
image_exists() {
  cjson image ls | jq -e --arg n "$1" '.result.images[]? | select(.name == $n)' >/dev/null 2>&1
}

# svc_field <name> <jq-path>: read a field from `status`, e.g.
#   svc_field hermes-a .address
#   svc_field hermes-a .cloud_init.hostname
# Uses `// empty` so a missing/null field yields "" (never a hard jq error under
# `set -e`); assert against the result.
svc_field() {
  cjson status "$1" | jq -r ".result$2 // empty"
}

# alloc_mac <name>: the MAC recorded for a service in allocations.toml's [macs]
# table. Empty when the file or entry is absent.
alloc_mac() {
  local svc="$1"
  [ -f "${HEARTH_ALLOCATIONS}" ] || return 0
  awk -v svc="${svc}" '
    /^\[/            { section = $0; next }
    section == "[macs]" && $1 == svc { gsub(/"/, "", $3); print $3; exit }
  ' "${HEARTH_ALLOCATIONS}"
}

# disk_path <name> [ext=raw]: the per-VM disk path hearthd writes.
disk_path() {
  printf '%s/%s.%s\n' "${HEARTH_DISKS_DIR}" "$1" "${2:-raw}"
}

# dropin_path <name>: the dnsmasq static-lease drop-in path for a service.
dropin_path() {
  printf '%s/%s.conf\n' "${HEARTH_DNSMASQ_DROPIN_DIR}" "$1"
}

# --- readiness helpers ------------------------------------------------------

# await_marker <name> <marker> <timeout_s>: block on `hearthctl wait` (the
# first-class readiness signal that replaced wait_for_log). Emits one pass line
# folding in the matched log line, or fails the harness on timeout. hearthctl
# itself prints the last few console lines to stderr on timeout for context.
await_marker() {
  local name="$1" marker="$2" timeout_s="${3:-300}" line
  if line="$(ctl wait "${name}" --marker "${marker}" --timeout "${timeout_s}")"; then
    pass "marker seen for ${name}: ${line}"
  else
    fail "marker '${marker}' not seen for ${name} within ${timeout_s}s"
  fi
}

# wait_tcp <host> <port> <timeout_s>: retry a TCP connect until it succeeds. Used
# to prove a guest service (e.g. sshd on :22) actually answers from the host.
wait_tcp() {
  local host="$1" port="$2" timeout_s="${3:-30}" deadline
  deadline=$(( $(now_s) + timeout_s ))
  while [ "$(now_s)" -lt "${deadline}" ]; do
    if timeout 3 bash -c "exec 3<>/dev/tcp/${host}/${port}" 2>/dev/null; then
      return 0
    fi
    sleep 1
  done
  return 1
}

# http_probe <url> <timeout_s>: retry curl until the port answers HTTP, then
# print the status code (any 3-digit code — even 401 — proves it answered) and
# return 0. Returns 1 if it never got an HTTP response.
http_probe() {
  local url="$1" timeout_s="${2:-30}" deadline code
  deadline=$(( $(now_s) + timeout_s ))
  while [ "$(now_s)" -lt "${deadline}" ]; do
    code="$(curl -sS -o /dev/null -m 5 -w '%{http_code}' "${url}" 2>/dev/null || true)"
    if [ -n "${code}" ] && [ "${code}" != "000" ]; then
      printf '%s\n' "${code}"
      return 0
    fi
    sleep 2
  done
  return 1
}

# --- build helpers ----------------------------------------------------------

# ensure_vm_base: build the shared localhost/vm-base buildah base image if it is
# not already present. Workload Dockerfiles are `FROM localhost/vm-base`.
ensure_vm_base() {
  if buildah images --format '{{.Name}}' 2>/dev/null | grep -qx 'localhost/vm-base'; then
    echo "vm-base base image present; skipping build."
    return 0
  fi
  echo "building vm-base base image (make vm-base)..."
  ( cd "${REPO_ROOT}" && make vm-base )
}

# ensure_image <name> <dockerfile> <context> <disk_gib> [extra hearthctl args...]:
# build a Hearth image if it is not already registered. Extra args pass straight
# through (e.g. --build-arg HERMES_COMMIT=<sha>).
ensure_image() {
  local name="$1" dockerfile="$2" context="$3" disk="$4"
  shift 4
  if image_exists "${name}"; then
    echo "image ${name} present; skipping build."
    return 0
  fi
  echo "building image ${name} from ${dockerfile}..."
  ctl image build --name "${name}" --dockerfile "${dockerfile}" --context "${context}" --disk "${disk}" "$@"
}
