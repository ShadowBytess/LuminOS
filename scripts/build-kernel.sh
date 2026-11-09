#!/usr/bin/env bash
# Download, configure and build the Linux kernel (x86_64).
# Output: build/bzImage
#
# Usage:
#   scripts/build-kernel.sh                  # latest stable from kernel.org
#   KERNEL_VERSION=6.12.30 scripts/build-kernel.sh
#   scripts/build-kernel.sh menuconfig       # tweak the config interactively
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD="$ROOT/build"
SRC_DIR="$BUILD/kernel-src"
mkdir -p "$BUILD"

# ---- host tool check --------------------------------------------------------
missing=()
for tool in make gcc flex bison bc perl curl python3 tar xz cpio gzip; do
    command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
done
if (( ${#missing[@]} )); then
    echo "Missing host tools: ${missing[*]}" >&2
    echo "  Debian/Ubuntu: sudo apt install build-essential flex bison bc libssl-dev libelf-dev curl xz-utils cpio python3 perl" >&2
    echo "  Arch:          sudo pacman -S base-devel bc cpio xz python perl libelf openssl" >&2
    echo "  Fedora:        sudo dnf install gcc make flex bison bc openssl-devel elfutils-libelf-devel cpio xz python3 perl" >&2
    exit 1
fi

# ---- pick a version ---------------------------------------------------------
if [[ -z "${KERNEL_VERSION:-}" ]]; then
    echo ">> Looking up the latest stable kernel on kernel.org"
    KERNEL_VERSION="$(curl -fsSL https://www.kernel.org/releases.json \
        | python3 -I -c 'import json,sys; print(json.load(sys.stdin)["latest_stable"]["version"])')"
fi
echo ">> Kernel version: $KERNEL_VERSION"

MAJOR="${KERNEL_VERSION%%.*}"
TARBALL="linux-$KERNEL_VERSION.tar.xz"
URL="https://cdn.kernel.org/pub/linux/kernel/v${MAJOR}.x/$TARBALL"

# ---- fetch + unpack ---------------------------------------------------------
if [[ ! -d "$SRC_DIR/linux-$KERNEL_VERSION" ]]; then
    mkdir -p "$SRC_DIR"
    if [[ ! -f "$BUILD/$TARBALL" ]]; then
        echo ">> Downloading $URL"
        curl -fL --output "$BUILD/$TARBALL" "$URL"
    fi
    echo ">> Unpacking"
    tar -xf "$BUILD/$TARBALL" -C "$SRC_DIR"
fi

cd "$SRC_DIR/linux-$KERNEL_VERSION"

# ---- configure --------------------------------------------------------------
CONFIG_OUT="$ROOT/kernel/myos.config"
mkdir -p "$ROOT/kernel"

if [[ -f "$CONFIG_OUT" ]]; then
    echo ">> Using saved config $CONFIG_OUT"
    cp "$CONFIG_OUT" .config
    make olddefconfig
else
    echo ">> Generating config from x86_64 defconfig + KVM guest options"
    make defconfig
    make kvm_guest.config
    # Things our init and the desktop stack will need.
    ./scripts/config \
        --enable BLK_DEV_INITRD \
        --enable DEVTMPFS \
        --enable TMPFS \
        --enable PROC_FS \
        --enable SYSFS \
        --enable UNIX \
        --enable INOTIFY_USER \
        --enable SIGNALFD \
        --enable TIMERFD \
        --enable EPOLL \
        --enable EVENTFD \
        --enable SERIAL_8250 \
        --enable SERIAL_8250_CONSOLE \
        --enable EFI \
        --enable EFI_STUB \
        --enable DRM \
        --enable DRM_VIRTIO_GPU \
        --enable DRM_SIMPLEDRM \
        --enable INPUT_EVDEV
    make olddefconfig
fi

if [[ "${1:-}" == "menuconfig" ]]; then
    make menuconfig
    cp .config "$CONFIG_OUT"
    echo ">> Saved config to $CONFIG_OUT"
    exit 0
fi

cp .config "$CONFIG_OUT"

# ---- build ------------------------------------------------------------------
echo ">> Building (using $(nproc) jobs)"
make -j"$(nproc)" bzImage

cp arch/x86/boot/bzImage "$BUILD/bzImage"
echo ">> Done: $BUILD/bzImage"