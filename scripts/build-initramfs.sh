#!/usr/bin/env bash
# Build a minimal initramfs containing our Rust init (as /init), a static-ish
# BusyBox for a debug shell, and the glibc runtime libraries both need.
# Output: build/initramfs.cpio.gz
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD="$ROOT/build"
RFS="$BUILD/rootfs"
INIT_BIN="$ROOT/target/release/init"

[[ -x "$INIT_BIN" ]] || { echo "Build the init first: cargo build --release" >&2; exit 1; }

BUSYBOX="${BUSYBOX:-$(command -v busybox || true)}"
[[ -n "$BUSYBOX" ]] || { echo "busybox not found; install it (e.g. apt install busybox-static) or set BUSYBOX=/path" >&2; exit 1; }

rm -rf "$RFS"
mkdir -p "$RFS"/{bin,sbin,dev,proc,sys,run,tmp,etc,root,usr/bin,usr/sbin,lib,lib64}

# Copy a binary plus every shared library it links against, preserving paths
# (so the glibc dynamic loader is found at its expected location).
install_with_libs() {
    local src="$1" dest="$2"
    install -D -m 0755 "$src" "$RFS$dest"
    local libs
    libs="$(ldd "$src" 2>/dev/null | grep -oE '/[^ ]+' || true)"
    for lib in $libs; do
        [[ -e "$lib" ]] || continue
        mkdir -p "$RFS$(dirname "$lib")"
        cp -L "$lib" "$RFS$lib"
    done
}

install_with_libs "$INIT_BIN" /init
install_with_libs "$BUSYBOX" /bin/busybox

# Busybox applet symlinks so the debug shell has ls, cat, mount, etc.
"$BUSYBOX" --list-full 2>/dev/null | while read -r applet; do
    [[ "$applet" == "bin/busybox" ]] && continue
    mkdir -p "$RFS/$(dirname "$applet")"
    ln -sf /bin/busybox "$RFS/$applet"
done

echo "root:x:0:0:root:/root:/bin/sh" > "$RFS/etc/passwd"
echo "root:x:0:" > "$RFS/etc/group"

( cd "$RFS" && find . -print0 | cpio --null -o -H newc --quiet | gzip -9 ) > "$BUILD/initramfs.cpio.gz"
echo ">> Done: $BUILD/initramfs.cpio.gz ($(du -h "$BUILD/initramfs.cpio.gz" | cut -f1))"
