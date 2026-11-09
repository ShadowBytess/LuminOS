# myos

A custom Linux-kernel OS with its own userspace. Stage 1: a Rust PID 1 that boots to a BusyBox debug shell in QEMU.

## Layout

```
init/                  Rust PID 1 (mounts /proc /sys /dev, starts shell, reaps children)
scripts/build-kernel.sh      download + configure + build the Linux kernel
scripts/build-initramfs.sh   pack init + busybox + glibc libs into an initramfs
scripts/run-qemu.sh          boot it
kernel/myos.config           saved kernel config (created on first build)
build/                       all generated output
```

## Host requirements (Debian/Ubuntu names)

```
sudo apt install build-essential flex bison libssl-dev libelf-dev bc \
                 curl xz-utils cpio gzip busybox-static qemu-system-x86 python3
curl https://sh.rustup.rs -sSf | sh
```

## Build and boot

```
make kernel        # first time only, takes a while
make run           # builds init, packs initramfs, boots QEMU
```

Inside the guest you get a shell. Type `poweroff` or `exit` to shut down.
Customise the kernel with `scripts/build-kernel.sh menuconfig`.

## Roadmap

1. Boot kernel + Rust init in QEMU (this stage)
2. Turn init into a service manager (dependencies, supervision, logging)
3. Base rootfs on a real disk image, glibc + coreutils + udev
4. Package manager and build recipes
5. Graphics stack (Mesa, libinput, seatd, Wayland), then Hyprland
6. Thunar, Rofi, bar, notification daemon
7. Installer and ISO
