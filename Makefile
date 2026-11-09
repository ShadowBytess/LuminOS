.PHONY: all init kernel initramfs run clean

all: kernel initramfs

init:
	cargo build --release

kernel:
	scripts/build-kernel.sh

initramfs: init
	scripts/build-initramfs.sh

run: initramfs
	scripts/run-qemu.sh

clean:
	cargo clean
	rm -rf build/rootfs build/initramfs.cpio.gz
