# LuminOS (WIP name)

A custom Linux-kernel OS with its own Rust userspace. Stage 1 boots a Linux kernel and Rust PID 1 to a BusyBox shell in QEMU. Stage 2 adds a TOML-configured service supervisor and `luminosctl`.

## Layout

```
init/                       Rust PID 1 and service manager
luminosctl/                    Unix-socket control client
rootfs-overlay/etc/luminos/services/  One TOML file per boot service
scripts/                    Kernel, initramfs and QEMU build scripts
kernel/myos.config          Saved kernel config
 tests/                     Host-side development service definitions
build/                      Generated output (ignored by git)
```

## Host requirements

Install the kernel build tools, BusyBox, QEMU and Rust as described below (package names vary by distribution):

```
sudo pacman -S --needed base-devel flex bison openssl libelf bc curl xz cpio gzip busybox qemu-system-x86 rustup
```

Make sure a stable Rust toolchain is installed with `rustup default stable`.

## Build and boot

```
cargo build --release --workspace
make kernel        # first time only; downloads/builds the kernel
make run           # rebuilds initramfs and boots QEMU
```

Inside the guest:

```
/sbin/luminosctl status
/sbin/luminosctl restart heartbeat
/sbin/luminosctl stop heartbeat
/sbin/luminosctl start heartbeat
```

Type `poweroff` in the console shell to ask BusyBox to power off the guest. The init process also accepts `shutdown` through `luminosctl`.

## Service definitions

Put one `.toml` file per service in `/etc/luminos/services/` (source files live in `rootfs-overlay/etc/luminos/services/`). Example:

```toml
name = "example"
command = "/bin/busybox"
args = ["sh", "-c", "echo hello; sleep 60"]
depends_on = []
enabled = true
restart = "on-failure" # never, on-failure, always
restart_delay_secs = 1
max_restart_delay_secs = 30
inherit_stdio = false

[env]
MODE = "production"
```

Dependencies start before dependents; shutdown stops them in reverse order. Dependency cycles and missing dependencies are rejected. Crashed services follow their restart policy with exponential backoff capped by `max_restart_delay_secs`. Non-console service output is appended to `/run/luminos/logs/SERVICE.log`.

`inherit_stdio = true` is intended for interactive console services. Other services receive stdin from `/dev/null` and have stdout/stderr appended to their log.

## Host development mode (no QEMU required)

This runs the exact same supervisor without mounting filesystems or requiring PID 1:

```
cargo run --release -p init -- --dev --config ./tests --socket /tmp/luminos-init.sock
```

In another terminal:

```
cargo run --release -p luminosctl -- --socket /tmp/luminos-init.sock status
cargo run --release -p luminosctl -- --socket /tmp/luminos-init.sock restart worker
cargo run --release -p luminosctl -- --socket /tmp/luminos-init.sock shutdown
```

Host-mode logs go to `./build/logs/`. Stop the dev supervisor with Ctrl-C or the `shutdown` command.

## Notes / current scope

- This is an early service manager, not yet a complete replacement for systemd/OpenRC.
- `stop` currently terminates the service's direct process; process-group management and graceful per-service stop timeouts are future work.
- The initramfs still uses BusyBox and the existing debug console. The disk-backed root filesystem remains the next stage.

## Roadmap

1. Boot kernel + Rust init in QEMU (completed)
2. Service manager, dependency ordering, supervision, logs and control socket (implemented; needs local build/runtime testing)
3. Persistent disk image and fuller root filesystem
4. Package manager and build recipes
5. Graphics stack (Mesa, libinput, seatd, Wayland), then Hyprland
6. Thunar, Rofi, bar, notification daemon
7. Installer and ISO
