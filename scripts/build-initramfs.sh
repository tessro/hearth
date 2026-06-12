#!/usr/bin/env sh
set -eu

DATA_DIR="${HEARTH_DATA_DIR:-"$HOME/.local/share/hearth"}"
STAGING_DIR="${HEARTH_INITRAMFS_DIR:-"$DATA_DIR/initramfs"}"
OUTPUT="${HEARTH_INITRAMFS:-"$DATA_DIR/initramfs.cpio.gz"}"
BUSYBOX="${HEARTH_BUSYBOX:-}"
MODULES_DIR="${HEARTH_MODULES_DIR:-}"
RUNNER="${HEARTH_RUNNER:-}"

usage() {
    cat <<'EOF'
usage: scripts/build-initramfs.sh [options]

Options:
  --busybox PATH       busybox binary to copy into the initramfs
  --runner PATH        hearth-runner binary to copy into the initramfs
  --modules-dir DIR    module source dir; accepts flat or kernel-tree layout
  --data-dir DIR       Hearth data dir (default: $HOME/.local/share/hearth)
  --staging-dir DIR    unpacked initramfs staging dir
  --output PATH        packed initramfs path
  -h, --help           show this help

Environment:
  HEARTH_BUSYBOX, HEARTH_RUNNER, HEARTH_MODULES_DIR, HEARTH_DATA_DIR,
  HEARTH_INITRAMFS_DIR, HEARTH_INITRAMFS
EOF
}

die() {
    printf 'build-initramfs: %s\n' "$*" >&2
    exit 1
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --busybox)
            [ "$#" -ge 2 ] || die "--busybox requires a path"
            BUSYBOX="$2"
            shift 2
            ;;
        --runner)
            [ "$#" -ge 2 ] || die "--runner requires a path"
            RUNNER="$2"
            shift 2
            ;;
        --modules-dir)
            [ "$#" -ge 2 ] || die "--modules-dir requires a dir"
            MODULES_DIR="$2"
            shift 2
            ;;
        --data-dir)
            [ "$#" -ge 2 ] || die "--data-dir requires a dir"
            DATA_DIR="$2"
            STAGING_DIR="${HEARTH_INITRAMFS_DIR:-"$DATA_DIR/initramfs"}"
            OUTPUT="${HEARTH_INITRAMFS:-"$DATA_DIR/initramfs.cpio.gz"}"
            shift 2
            ;;
        --staging-dir)
            [ "$#" -ge 2 ] || die "--staging-dir requires a dir"
            STAGING_DIR="$2"
            shift 2
            ;;
        --output)
            [ "$#" -ge 2 ] || die "--output requires a path"
            OUTPUT="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "unknown argument: $1"
            ;;
    esac
done

if [ -z "$BUSYBOX" ]; then
    if command -v busybox >/dev/null 2>&1; then
        BUSYBOX="$(command -v busybox)"
    else
        die "busybox not found; pass --busybox PATH or set HEARTH_BUSYBOX"
    fi
fi
[ -f "$BUSYBOX" ] || die "busybox is not a file: $BUSYBOX"
if command -v file >/dev/null 2>&1 && file "$BUSYBOX" | grep -q "dynamically linked"; then
    die "busybox must be statically linked for the initramfs: $BUSYBOX"
fi

if [ -z "$RUNNER" ]; then
    if [ -f target/x86_64-unknown-linux-musl/release/hearth-runner ]; then
        RUNNER="target/x86_64-unknown-linux-musl/release/hearth-runner"
    elif [ -f target/release/hearth-runner ]; then
        RUNNER="target/release/hearth-runner"
    else
        die "hearth-runner not found; build it first with 'devenv shell build-hearth-runner', then pass --runner PATH or set HEARTH_RUNNER"
    fi
fi
[ -f "$RUNNER" ] || die "hearth-runner is not a file: $RUNNER"
if command -v file >/dev/null 2>&1 && file "$RUNNER" | grep -q "dynamically linked"; then
    die "hearth-runner must be statically linked for the initramfs: $RUNNER"
fi

if [ -z "$MODULES_DIR" ]; then
    release="$(uname -r)"
    MODULES_DIR="/run/booted-system/kernel-modules/lib/modules/$release"
fi
[ -d "$MODULES_DIR" ] || die "modules dir not found: $MODULES_DIR"

WORK_DIR="$DATA_DIR/initramfs.work.$$"
OUT_TMP="$OUTPUT.tmp.$$"

cleanup() {
    rm -rf "$WORK_DIR" "$OUT_TMP"
}
trap cleanup EXIT INT TERM

rm -rf "$WORK_DIR"
mkdir -p \
    "$WORK_DIR/bin" \
    "$WORK_DIR/lib/modules" \
    "$WORK_DIR/proc" \
    "$WORK_DIR/sys" \
    "$WORK_DIR/dev" \
    "$WORK_DIR/newroot" \
    "$(dirname "$OUTPUT")"

cp "$BUSYBOX" "$WORK_DIR/bin/busybox"
chmod 0755 "$WORK_DIR/bin/busybox"
cp "$RUNNER" "$WORK_DIR/hearth-runner"
chmod 0755 "$WORK_DIR/hearth-runner"
for applet in sh mount mkdir insmod chroot poweroff reboot; do
    ln -s busybox "$WORK_DIR/bin/$applet"
done

copy_module() {
    rel="$1"
    dest="$2"
    if [ -f "$MODULES_DIR/$dest" ]; then
        src="$MODULES_DIR/$dest"
    elif [ -f "$MODULES_DIR/$rel" ]; then
        src="$MODULES_DIR/$rel"
    else
        die "missing module $dest; looked for $MODULES_DIR/$dest and $MODULES_DIR/$rel"
    fi
    cp "$src" "$WORK_DIR/lib/modules/$dest"
}

copy_module kernel/drivers/virtio/virtio_ring.ko.xz virtio_ring.ko.xz
copy_module kernel/drivers/virtio/virtio.ko.xz virtio.ko.xz
copy_module kernel/drivers/virtio/virtio_pci_modern_dev.ko.xz virtio_pci_modern_dev.ko.xz
copy_module kernel/drivers/virtio/virtio_pci_legacy_dev.ko.xz virtio_pci_legacy_dev.ko.xz
copy_module kernel/drivers/virtio/virtio_pci.ko.xz virtio_pci.ko.xz
copy_module kernel/fs/fuse/fuse.ko.xz fuse.ko.xz
copy_module kernel/fs/fuse/virtiofs.ko.xz virtiofs.ko.xz

cat > "$WORK_DIR/init" <<'EOF'
#!/bin/sh
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev || true

echo "hearth initramfs: loading modules"
insmod /lib/modules/virtio_ring.ko.xz
insmod /lib/modules/virtio.ko.xz
insmod /lib/modules/virtio_pci_modern_dev.ko.xz 2>/dev/null || true
insmod /lib/modules/virtio_pci_legacy_dev.ko.xz 2>/dev/null || true
insmod /lib/modules/virtio_pci.ko.xz
insmod /lib/modules/fuse.ko.xz
insmod /lib/modules/virtiofs.ko.xz

echo "hearth initramfs: mounting virtiofs root"
mount -t virtiofs root /newroot || exec sh

mkdir -p /newroot/proc /newroot/sys /newroot/dev
mount -t proc proc /newroot/proc 2>/dev/null || true
mount -t sysfs sysfs /newroot/sys 2>/dev/null || true
mount -t devtmpfs devtmpfs /newroot/dev 2>/dev/null || true

echo "hearth initramfs: running OCI process"
/hearth-runner /newroot /newroot/.hearth/run.json
status="$?"
if [ ! -f /newroot/.hearth/exit-status ]; then
    echo "$status" > /newroot/.hearth/exit-status
fi

echo "hearth initramfs: OCI process exited with status $status; powering off"
poweroff -f || reboot -f || echo o > /proc/sysrq-trigger

exec sh
EOF
chmod 0755 "$WORK_DIR/init"

(
    cd "$WORK_DIR"
    find . | cpio -o -H newc | gzip -9 > "$OUT_TMP"
)

rm -rf "$STAGING_DIR"
mv "$WORK_DIR" "$STAGING_DIR"
mv "$OUT_TMP" "$OUTPUT"
trap - EXIT INT TERM

printf 'wrote %s\n' "$OUTPUT"
