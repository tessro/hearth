#!/usr/bin/env bash
# Build the dedicated Hearth guest kernel from vanilla kernel.org sources.
#
# Hearth VMs boot a raw `vmlinux` ELF directly through Cloud
# Hypervisor's PVH entry point: no bootloader, no bzImage, no initramfs. That
# only works if every driver the guest needs (virtio_blk for root, ext4,
# virtio_net, vsock, af_packet, ...) is compiled *in*, so this script pins an
# LTS kernel version, verifies its sha256, applies the checked-in contract in
# guest/kernel.config, and builds `make vmlinux`.
#
# This replaces the old host-kernel-coupled initramfs (scripts/build-vm-initramfs.sh,
# deleted): the artifact here is versioned and self-contained, so a host kernel
# bump can never silently break guest boots.
#
# No Nix required. Build deps are ordinary host packages (see preflight below).
set -euo pipefail

# --- pinned kernel (bump deliberately, in a commit) -----------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck source=../guest/kernel-version.env
source "$REPO_DIR/guest/kernel-version.env"

CONFIG_FRAGMENT="${HEARTH_KERNEL_CONFIG:-$REPO_DIR/guest/kernel.config}"
CACHE_DIR="${HEARTH_KERNEL_CACHE:-$HOME/.cache/hearth/kernel}"
INSTALL_DIR="${HEARTH_KERNEL_INSTALL_DIR:-/var/lib/hearth/kernels}"
JOBS="$(nproc 2>/dev/null || echo 1)"

usage() {
  cat <<EOF
usage: scripts/build-guest-kernel.sh [options]

Downloads, verifies, configures, and builds the pinned Hearth guest kernel
(vanilla linux-${KERNEL_VERSION}) with the VM contract in guest/kernel.config
compiled in, then installs the resulting vmlinux.

Options:
  --install-dir DIR   install root (default: ${INSTALL_DIR})
                      writes <DIR>/${KERNEL_VERSION}/vmlinux, a <DIR>/${KERNEL_VERSION}/contract
                      file, and a <DIR>/current -> ${KERNEL_VERSION} symlink.
                      Use a user-writable dir (e.g. ~/.local/share/hearth/kernels)
                      to build without root.
  --cache-dir DIR     download/build cache (default: ${CACHE_DIR})
  --config FILE       kernel config fragment (default: ${CONFIG_FRAGMENT})
  --jobs N            parallel make jobs (default: nproc = ${JOBS})
  -h, --help          show this help

Environment overrides: HEARTH_KERNEL_INSTALL_DIR, HEARTH_KERNEL_CACHE,
HEARTH_KERNEL_CONFIG.
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --install-dir) INSTALL_DIR="$2"; shift 2 ;;
    --cache-dir) CACHE_DIR="$2"; shift 2 ;;
    --config) CONFIG_FRAGMENT="$2"; shift 2 ;;
    --jobs) JOBS="$2"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

# --- preflight: fail up front listing ALL missing build deps ---------------
# Distro-generic install hints (apt = Debian/Ubuntu, dnf = Fedora/RHEL).
missing=()

have() { command -v "$1" >/dev/null 2>&1; }

# A downloader: curl or wget (either is fine).
if ! have curl && ! have wget; then
  missing+=("curl or wget  (apt: curl | dnf: curl)")
fi
have gcc   || missing+=("gcc          (apt: gcc / build-essential | dnf: gcc)")
have make  || missing+=("make         (apt: make / build-essential | dnf: make)")
have flex  || missing+=("flex         (apt: flex | dnf: flex)")
have bison || missing+=("bison        (apt: bison | dnf: bison)")
have bc    || missing+=("bc           (apt: bc | dnf: bc)")
have perl  || missing+=("perl         (apt: perl | dnf: perl)")
have xz    || missing+=("xz           (apt: xz-utils | dnf: xz)")
have sha256sum || missing+=("sha256sum    (apt: coreutils | dnf: coreutils)")

# libelf headers are required by objtool (x86_64 builds enable CONFIG_OBJTOOL).
if have gcc; then
  if ! printf '#include <libelf.h>\nint main(void){return 0;}\n' \
       | gcc -x c - -o /dev/null -lelf >/dev/null 2>&1; then
    missing+=("libelf-dev   (apt: libelf-dev | dnf: elfutils-libelf-devel)")
  fi
fi

if [ "${#missing[@]}" -gt 0 ]; then
  echo "error: missing required build tools:" >&2
  for m in "${missing[@]}"; do
    echo "  - $m" >&2
  done
  exit 1
fi

# openssl headers are not required with module signing disabled (see
# guest/kernel.config), but warn if absent in case a future config bump needs
# them for certificate generation.
if have gcc && ! printf '#include <openssl/opensslv.h>\nint main(void){return 0;}\n' \
     | gcc -x c - -o /dev/null >/dev/null 2>&1; then
  echo "note: openssl headers not found (apt: libssl-dev | dnf: openssl-devel)." >&2
  echo "      Not required for the current config, but a future bump may need them." >&2
fi

if [ ! -f "$CONFIG_FRAGMENT" ]; then
  echo "error: kernel config fragment not found: $CONFIG_FRAGMENT" >&2
  exit 1
fi

# --- download + verify -----------------------------------------------------
TARBALL="linux-${KERNEL_VERSION}.tar.xz"
URL="https://cdn.kernel.org/pub/linux/kernel/v6.x/${TARBALL}"
mkdir -p "$CACHE_DIR"
TARBALL_PATH="$CACHE_DIR/$TARBALL"

if [ ! -f "$TARBALL_PATH" ]; then
  echo "downloading $URL"
  if have curl; then
    curl -fSL --retry 3 -o "$TARBALL_PATH.tmp" "$URL"
  else
    wget -O "$TARBALL_PATH.tmp" "$URL"
  fi
  mv "$TARBALL_PATH.tmp" "$TARBALL_PATH"
else
  echo "using cached tarball $TARBALL_PATH"
fi

echo "verifying sha256"
if ! echo "${KERNEL_SHA256}  ${TARBALL_PATH}" | sha256sum -c - >/dev/null 2>&1; then
  echo "error: sha256 mismatch for $TARBALL_PATH" >&2
  echo "  expected: $KERNEL_SHA256" >&2
  echo "  actual:   $(sha256sum "$TARBALL_PATH" | cut -d' ' -f1)" >&2
  echo "  (removed the bad file; re-run to re-download)" >&2
  rm -f "$TARBALL_PATH"
  exit 1
fi

# --- extract ---------------------------------------------------------------
SRC_DIR="$CACHE_DIR/linux-${KERNEL_VERSION}"
if [ ! -d "$SRC_DIR" ]; then
  echo "extracting $TARBALL"
  tar -C "$CACHE_DIR" -xf "$TARBALL_PATH"
else
  echo "using extracted source $SRC_DIR"
fi

# --- configure: defconfig + merged fragment + olddefconfig -----------------
echo "configuring kernel (defconfig + guest/kernel.config)"
make -C "$SRC_DIR" defconfig
# merge_config.sh -m merges the fragment over the base .config without
# regenerating; olddefconfig then fills defaults for anything the merge pulled in.
"$SRC_DIR/scripts/kconfig/merge_config.sh" -m -O "$SRC_DIR" \
  "$SRC_DIR/.config" "$CONFIG_FRAGMENT"
make -C "$SRC_DIR" olddefconfig

# Sanity check: PVH is mandatory for CHV direct boot; catch a silent drop.
if ! grep -q '^CONFIG_PVH=y' "$SRC_DIR/.config"; then
  echo "error: CONFIG_PVH did not survive olddefconfig; CHV cannot boot this kernel" >&2
  exit 1
fi

# --- build -----------------------------------------------------------------
echo "building vmlinux with make -j${JOBS} (this takes a few minutes)"
make -C "$SRC_DIR" -j"$JOBS" vmlinux

VMLINUX="$SRC_DIR/vmlinux"
[ -f "$VMLINUX" ] || { echo "error: build produced no $VMLINUX" >&2; exit 1; }

# --- install ---------------------------------------------------------------
DEST_DIR="$INSTALL_DIR/$KERNEL_VERSION"
mkdir -p "$DEST_DIR"
install -m 0644 "$VMLINUX" "$DEST_DIR/vmlinux"
printf '%s\n' "$KERNEL_CONTRACT" > "$DEST_DIR/contract"
# `current` is a directory symlink so <install-dir>/current/vmlinux and
# <install-dir>/current/contract both resolve (matches hearthd's guest_kernel
# default of <install-dir>/current/vmlinux). Relative target keeps the tree
# relocatable.
ln -sfn "$KERNEL_VERSION" "$INSTALL_DIR/current"

echo
echo "installed guest kernel:"
echo "  $DEST_DIR/vmlinux   (contract $KERNEL_CONTRACT)"
echo "  $INSTALL_DIR/current -> $KERNEL_VERSION"
echo
echo "Point hearthd at it (this is the default if --install-dir is /var/lib/hearth/kernels):"
echo "  HEARTH_GUEST_KERNEL=$INSTALL_DIR/current/vmlinux"
