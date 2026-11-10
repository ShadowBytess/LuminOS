//! LuminOS PID 1 and service supervisor.
//!
//! Normal boot: init mounts the kernel pseudo-filesystems, loads one TOML file
//! per service, starts enabled services in dependency order, supervises them,
//! and accepts control commands over /run/luminos/init.sock.
//!
//! Host development: `init --dev --config ./rootfs-overlay/etc/luminos/services`
//! skips mounts and the PID-1 requirement. It still runs the same supervisor.

mod service;

use service::{Manager, ServiceConfig};
use std::env;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);
const DEFAULT_CONFIG_DIR: &str = "/etc/luminos/services";
const DEFAULT_SOCKET: &str = "/run/luminos/init.sock";

extern "C" fn handle_signal(_: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() {
    unsafe {
        libc::signal(libc::SIGTERM, handle_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, handle_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGQUIT, handle_signal as *const () as libc::sighandler_t);
    }
}

fn mount_fs(source: &str, target: &str, fstype: &str, flags: libc::c_ulong) {
    use std::ffi::CString;
    let (Ok(src), Ok(dst), Ok(kind)) = (
        CString::new(source), CString::new(target), CString::new(fstype),
    ) else { return };
    let rc = unsafe {
        libc::mount(src.as_ptr(), dst.as_ptr(), kind.as_ptr(), flags, std::ptr::null())
    };
    if rc != 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EBUSY) {
            eprintln!("luminos-init: mount {fstype} on {target}: {err}");
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

    // PID 1 in an initramfs may start before /dev/console exists.
    unsafe {
        let console = std::ffi::CString::new("/dev/console").unwrap();
        let fd = libc::open(console.as_ptr(), libc::O_RDWR);
        if fd >= 0 {
            libc::dup2(fd, 0);
            libc::dup2(fd, 1);
            libc::dup2(fd, 2);
            if fd > 2 { libc::close(fd); }
        }
    }
}

fn load_configs(dir: &Path) -> io::Result<Vec<ServiceConfig>> {
    let mut paths = fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect::<Vec<_>>();
    paths.sort();
    let mut configs = Vec::new();
    for path in paths {
        let contents = fs::read_to_string(&path)?;
        let config: ServiceConfig = toml::from_str(&contents).map_err(|err| {
            io::Error::new(io::ErrorKind::InvalidData, format!("{}: {err}", path.display()))
        })?;
        configs.push(config);
    }
    Ok(configs)
}

fn handle_client(mut stream: UnixStream, manager: &mut Manager) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut line = String::new();
    let read_result = BufReader::new(&stream).read_line(&mut line);
    let response = match read_result {
        Ok(_) => manager.handle_command(line.trim()),
        Err(err) => format!("ERR unable to read command: {err}"),
    };
    let _ = writeln!(stream, "{response}");
}

fn create_listener(socket_path: &Path) -> io::Result<UnixListener> {
    if let Some(parent) = socket_path.parent() { fs::create_dir_all(parent)?; }
    if socket_path.exists() { fs::remove_file(socket_path)?; }
    let listener = UnixListener::bind(socket_path)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let dev_mode = args.iter().any(|arg| arg == "--dev");
    let mut config_dir = PathBuf::from(DEFAULT_CONFIG_DIR);
    let mut socket_path = PathBuf::from(DEFAULT_SOCKET);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => {
                i += 1;
                config_dir = PathBuf::from(args.get(i).ok_or("--config requires a directory")?);
            }
            "--socket" => {
                i += 1;
                socket_path = PathBuf::from(args.get(i).ok_or("--socket requires a path")?);
            }
            "--dev" | "--help" | "-h" => {}
            other => return Err(format!("unknown argument: {other}").into()),
        }
        i += 1;
    }

    let pid = unsafe { libc::getpid() };
    if !dev_mode && pid != 1 {
        return Err(format!("must run as PID 1 (current PID {pid}); use --dev for host testing").into());
    }
    if !dev_mode { mount_pseudo_filesystems(); }
    install_signal_handlers();

    let configs = load_configs(&config_dir)?;
    let mut manager = Manager::new(configs, dev_mode)?;
    let listener = create_listener(&socket_path)?;
    println!("luminos-init: supervisor online (dev={dev_mode}), socket={}", socket_path.display());

    if let Err(err) = manager.start_enabled() {
        eprintln!("luminos-init: startup warning: {err}");
    }

    while !SHUTDOWN.load(Ordering::SeqCst) && !manager.shutdown_requested() {
        loop {
            match listener.accept() {
                Ok((stream, _)) => handle_client(stream, &mut manager),
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => eprintln!("luminos-init: socket accept: {err}"),
            }
        }
        manager.poll_children();
        thread::sleep(Duration::from_millis(100));
    }

    println!("luminos-init: shutting down services");
    manager.stop_all();
    let _ = fs::remove_file(&socket_path);

    if !dev_mode {
        unsafe {
            libc::sync();
            libc::reboot(libc::RB_POWER_OFF);
        }
        loop { thread::sleep(Duration::from_secs(3600)); }
    }
    Ok(())
}

fn main() {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if let Err(err) = run(&args) {
        eprintln!("luminos-init: {err}");
        eprintln!("usage: init [--dev] [--config DIR] [--socket PATH]");
        std::process::exit(1);
    }
}
