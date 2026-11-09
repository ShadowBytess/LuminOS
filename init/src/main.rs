//! Minimal PID 1 for Redforge.
//!
//! Stage 1 responsibilities:
//!   1. Mount the pseudo-filesystems (/proc, /sys, /dev, /run, /tmp)
//!   2. Start a debug shell
//!   3. Reap orphaned children forever (that's PID 1's job)
//!   4. Power off cleanly when the shell exits
//!
//! Later this grows into a real service manager.

use std::ffi::CString;
use std::io;
use std::process::Command;
use std::ptr;
use std::thread::sleep;
use std::time::Duration;

fn mount(source: &str, target: &str, fstype: &str, flags: libc::c_ulong) {
    let src = CString::new(source).unwrap();
    let tgt = CString::new(target).unwrap();
    let fst = CString::new(fstype).unwrap();
    let rc = unsafe { libc::mount(src.as_ptr(), tgt.as_ptr(), fst.as_ptr(), flags, ptr::null()) };
    if rc != 0 {
        let err = io::Error::last_os_error();
        // EBUSY means it is already mounted, which is fine.
        if err.raw_os_error() != Some(libc::EBUSY) {
            eprintln!("init: mounting {fstype} on {target} failed: {err}");
        }
    }
}

fn poweroff() -> ! {
    unsafe {
        libc::sync();
        libc::reboot(libc::RB_POWER_OFF);
    }
    // Only reached if the reboot syscall failed.
    loop {
        sleep(Duration::from_secs(3600));
    }
}

fn main() {
    if unsafe { libc::getpid() } != 1 {
        eprintln!("init: must be run as PID 1");
        std::process::exit(1);
    }

    for dir in ["/proc", "/sys", "/dev", "/run", "/tmp"] {
        let _ = std::fs::create_dir_all(dir);
    }

    let strict = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
    mount("proc", "/proc", "proc", strict);
    mount("sysfs", "/sys", "sysfs", strict);
    mount("devtmpfs", "/dev", "devtmpfs", libc::MS_NOSUID);
    mount("tmpfs", "/run", "tmpfs", libc::MS_NOSUID | libc::MS_NODEV);
    mount("tmpfs", "/tmp", "tmpfs", libc::MS_NOSUID | libc::MS_NODEV);

    // An initramfs has no /dev/console until devtmpfs is mounted, so PID 1
    // starts with no stdio. Attach it now or every message is silently lost.
    unsafe {
        let console = CString::new("/dev/console").unwrap();
        let fd = libc::open(console.as_ptr(), libc::O_RDWR);
        if fd >= 0 {
            libc::dup2(fd, 0);
            libc::dup2(fd, 1);
            libc::dup2(fd, 2);
            if fd > 2 {
                libc::close(fd);
            }
        }
    }

    println!("\n=== Redforge: init is up (PID 1) ===\n");

    // `setsid cttyhack` gives the shell a controlling terminal so job control works.
    let shell = Command::new("/bin/busybox")
        .args(["setsid", "cttyhack", "/bin/sh"])
        .spawn();

    let shell_pid = match shell {
        Ok(child) => child.id() as libc::pid_t,
        Err(e) => {
            eprintln!("init: failed to start shell: {e}");
            poweroff();
        }
    };

    // Reap every child that exits; power off when the shell does.
    loop {
        let mut status: libc::c_int = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
        if pid == shell_pid {
            println!("init: shell exited (status {status}), powering off");
            poweroff();
        }
        if pid == -1 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ECHILD) {
                println!("init: no children left, powering off");
                poweroff();
            }
        }
    }
}
