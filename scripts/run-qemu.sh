#!/usr/bin/env bash
# Boot the kernel + initramfs in QEMU with the serial console on your terminal.
# Quit QEMU with Ctrl-A then X (or just `poweroff` inside the guest).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD="$ROOT/build"

[[ -f "$BUILD/bzImage" ]] || { echo "Missing $BUILD/bzImage; run scripts/build-kernel.sh" >&2; exit 1; }
[[ -f "$BUILD/initramfs.cpio.gz" ]] || { echo "Missing initramfs; run scripts/build-initramfs.sh" >&2; exit 1; }

ACCEL=()
if [[ -w /dev/kvm ]]; then
    ACCEL=(-enable-kvm -cpu host)
else
    echo ">> /dev/kvm not usable, falling back to software emulation (slow)." >&2
    echo ">> If this is a permissions problem on Arch: sudo usermod -aG kvm \$USER, then log out and in." >&2
    # The default qemu64 CPU lacks instructions that modern glibc builds use
    # (SIGILL in ld.so), so expose everything TCG can emulate.
    ACCEL=(-cpu "${CPU_MODEL:-max}")
fi

exec qemu-system-x86_64 \
    "${ACCEL[@]}" \
    -m "${MEM:-1G}" \
    -smp "${CPUS:-2}" \
    -kernel "$BUILD/bzImage" \
    -initrd "$BUILD/initramfs.cpio.gz" \
    -append "console=ttyS0 rdinit=/init quiet" \
    -nographic \
    -no-reboot
