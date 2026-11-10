#!/usr/bin/env bash
# Build the initramfs containing Redforge PID 1, myosctl, BusyBox and services.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD="$ROOT/build"
RFS="$BUILD/rootfs"
INIT_BIN="$ROOT/target/release/init"
CTL_BIN="$ROOT/target/release/myosctl"

[[ -x "$INIT_BIN" && -x "$CTL_BIN" ]] || {
    echo "Build both binaries first: cargo build --release --workspace" >&2
    exit 1
}
BUSYBOX="${BUSYBOX:-$(command -v busybox || true)}"
[[ -n "$BUSYBOX" ]] || { echo "busybox not found; install it (e.g. busybox-static) or set BUSYBOX=/path" >&2; exit 1; }

rm -rf "$RFS"
mkdir -p "$RFS"/{bin,sbin,dev,proc,sys,run,tmp,etc,root,usr/bin,usr/sbin,lib,lib64}

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
install_with_libs "$CTL_BIN" /sbin/myosctl
install_with_libs "$BUSYBOX" /bin/busybox
"$BUSYBOX" --list-full 2>/dev/null | while read -r applet; do
    [[ "$applet" == "bin/busybox" ]] && continue
    mkdir -p "$RFS/$(dirname "$applet")"
    ln -sf /bin/busybox "$RFS/$applet"
done

install -D -m 0644 "$ROOT"/rootfs-overlay/etc/redforge/services/*.toml "$RFS/etc/redforge/services/"
echo "root:x:0:0:root:/root:/bin/sh" > "$RFS/etc/passwd"
echo "root:x:0:" > "$RFS/etc/group"

( cd "$RFS" && find . -print0 | cpio --null -o -H newc --quiet | gzip -9 ) > "$BUILD/initramfs.cpio.gz"
echo ">> Done: $BUILD/initramfs.cpio.gz ($(du -h "$BUILD/initramfs.cpio.gz" | cut -f1))"
