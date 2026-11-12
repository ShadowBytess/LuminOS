//! LuminOS PID 1 and service supervisor.
//!
//! Normal boot: init mounts the kernel pseudo-filesystems, loads one TOML file
//! per service, starts enabled services in dependency order, supervises them,
//! and accepts control commands over /run/luminos/init.sock.
//!
//! Host development: `init --dev --config ./rootfs-overlay/etc/luminos/services`
//! skips mounts and the PID-1 requirement. It runs the same supervisor.
//!
//! Rule for this file and service.rs: PID 1 must never exit or panic, because
//! the kernel panics when init dies. Problems are logged and worked around.

/// Log to the console. Never panics: the release profile uses panic=abort, so a
/// failed write to a closed console through `println!` would take the machine down.
macro_rules! llog {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), "luminos-init: {}", format_args!($($arg)*));
    }};
}

mod service;

use service::{Manager, RestartPolicy, ServiceConfig, ServiceType};
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::ptr;
use std::thread::sleep;
use std::time::Duration;

const DEFAULT_CONFIG_DIR: &str = "/etc/luminos/services";
const DEFAULT_SOCKET: &str = "/run/luminos/init.sock";
const PID1_LOG_DIR: &str = "/run/luminos/logs";
const DEV_LOG_DIR: &str = "./build/logs";
const USAGE: &str = "usage: init --dev [--config DIR] [--socket PATH]\n(without --dev it must run as PID 1)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    Poweroff,
    Reboot,
    Halt,
}

/// Children inherit the signal mask across exec, so they must clear it.
pub fn reset_sigmask() {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigprocmask(libc::SIG_SETMASK, &set, ptr::null_mut());
    }
}

/// Block the signals init cares about and receive them through a file
/// descriptor instead of async handlers.
fn setup_signals() -> io::Result<OwnedFd> {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for sig in [libc::SIGCHLD, libc::SIGINT, libc::SIGTERM, libc::SIGQUIT, libc::SIGUSR1, libc::SIGUSR2] {
            libc::sigaddset(&mut set, sig);
        }
        if libc::sigprocmask(libc::SIG_BLOCK, &set, ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = libc::signalfd(-1, &set, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(OwnedFd::from_raw_fd(fd))
    }
}

fn mount_fs(source: &str, target: &str, fstype: &str, flags: libc::c_ulong) {
    let (Ok(src), Ok(dst), Ok(kind)) = (CString::new(source), CString::new(target), CString::new(fstype)) else {
        return;
    };
    let rc = unsafe { libc::mount(src.as_ptr(), dst.as_ptr(), kind.as_ptr(), flags, ptr::null()) };
    if rc != 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EBUSY) {
            llog!("mount {fstype} on {target}: {err}");
        }
    }
}

fn mount_pseudo_filesystems() {
    for dir in ["/proc", "/sys", "/dev", "/run", "/tmp"] {
        let _ = fs::create_dir_all(dir);
    }
    let strict = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
    mount_fs("proc", "/proc", "proc", strict);
    mount_fs("sysfs", "/sys", "sysfs", strict);
    mount_fs("devtmpfs", "/dev", "devtmpfs", libc::MS_NOSUID);
    mount_fs("tmpfs", "/run", "tmpfs", libc::MS_NOSUID | libc::MS_NODEV);
    mount_fs("tmpfs", "/tmp", "tmpfs", libc::MS_NOSUID | libc::MS_NODEV);

    // PID 1 in an initramfs starts with no stdio and /dev/console does not
    // exist until devtmpfs is mounted. Attach it now or every message is lost.
    unsafe {
        let Ok(console) = CString::new("/dev/console") else { return };
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
}

fn create_listener(socket_path: &Path) -> io::Result<UnixListener> {
    if let Some(parent) = socket_path.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::remove_file(socket_path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(socket_path)?;
    fs::set_permissions(socket_path, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// Used when no usable service definitions exist, so a broken or missing
/// services directory still leaves a way to fix things.
fn emergency_shell() -> ServiceConfig {
    ServiceConfig {
        name: "emergency-shell".into(),
        command: "/bin/sh".into(),
        args: vec![],
        env: Default::default(),
        depends_on: vec![],
        enabled: true,
        restart: RestartPolicy::Always,
        restart_delay_secs: 1,
        max_restart_delay_secs: 5,
        inherit_stdio: false,
        kind: ServiceType::Simple,
        stop_timeout_secs: 1,
        tty: Some("/dev/console".into()),
    }
}

struct Options {
    config_dir: PathBuf,
    socket: PathBuf,
}

fn parse_args(args: &[String]) -> Result<Option<Options>, String> {
    let mut dev = false;
    let mut opts = Options { config_dir: DEFAULT_CONFIG_DIR.into(), socket: DEFAULT_SOCKET.into() };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dev" => dev = true,
            "--config" => opts.config_dir = it.next().ok_or("--config requires a directory")?.into(),
            "--socket" => opts.socket = it.next().ok_or("--socket requires a path")?.into(),
            "--help" | "-h" => return Ok(None),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    if !dev {
        return Err("not running as PID 1; pass --dev to run as a normal process".into());
    }
    Ok(Some(opts))
}

/// Last step of shutdown. In dev mode this just exits.
fn finish(action: PowerAction, pid1: bool) -> ! {
    if !pid1 {
        llog!("dev mode: would {action:?}, exiting instead");
        std::process::exit(0);
    }
    unsafe {
        // Whatever is left (orphans, daemons that ignored us) gets a last chance.
        libc::kill(-1, libc::SIGTERM);
    }
    sleep(Duration::from_millis(300));
    unsafe {
        libc::kill(-1, libc::SIGKILL);
        libc::sync();
        libc::reboot(match action {
            PowerAction::Poweroff => libc::RB_POWER_OFF,
            PowerAction::Reboot => libc::RB_AUTOBOOT,
            PowerAction::Halt => libc::RB_HALT_SYSTEM,
        });
    }
    loop {
        sleep(Duration::from_secs(3600));
    }
}

fn main() {
    let pid1 = unsafe { libc::getpid() } == 1;

    // As PID 1, the kernel passes unrecognised command-line words to init as
    // arguments, so they are ignored here rather than treated as errors.
    let opts = if pid1 {
        Options { config_dir: DEFAULT_CONFIG_DIR.into(), socket: DEFAULT_SOCKET.into() }
    } else {
        let args: Vec<String> = std::env::args().skip(1).collect();
        match parse_args(&args) {
            Ok(Some(o)) => o,
            Ok(None) => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("luminos-init: {e}\n{USAGE}");
                std::process::exit(2);
            }
        }
    };

    if pid1 {
        mount_pseudo_filesystems();
    }
    llog!("starting ({})", if pid1 { "PID 1" } else { "dev mode" });

    let sigfd = match setup_signals() {
        Ok(fd) => fd,
        Err(e) => {
            llog!("fatal: cannot set up signal handling: {e}");
            if pid1 {
                loop {
                    sleep(Duration::from_secs(3600));
                }
            }
            std::process::exit(1);
        }
    };

    let mut configs = match service::load_configs(&opts.config_dir) {
        Ok(loaded) => {
            for w in &loaded.warnings {
                llog!("warning: {w}");
            }
            loaded.configs
        }
        Err(e) => {
            llog!("cannot read {}: {e}", opts.config_dir.display());
            Vec::new()
        }
    };
    if configs.is_empty() {
        if pid1 {
            llog!("no services loaded, starting an emergency shell");
            configs.push(emergency_shell());
        } else {
            llog!("no services loaded");
        }
    }

    let listener = match create_listener(&opts.socket) {
        Ok(l) => Some(l),
        Err(e) => {
            llog!("cannot create control socket {}: {e} (luminosctl will not work)", opts.socket.display());
            None
        }
    };
    if listener.is_some() {
        llog!("control socket: {}", opts.socket.display());
    }

    let log_dir = if pid1 { PathBuf::from(PID1_LOG_DIR) } else { PathBuf::from(DEV_LOG_DIR) };
    let mut manager = Manager::new(configs, log_dir, pid1);
    manager.boot();
    let action = manager.run(listener.as_ref(), sigfd.as_raw_fd());
    let _ = fs::remove_file(&opts.socket);
    finish(action, pid1);
}
