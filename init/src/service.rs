//! Service definitions, dependency resolution, and the supervisor.
//!
//! Model: every service has `want` (should it be up?) and a `State` (what is it
//! doing?). `reconcile()` repeatedly takes whatever single step moves each
//! service toward `want`, honouring `depends_on`, until nothing changes. Events
//! (child exits, control commands, timers) only change `want` or `State`; they
//! never start or stop processes directly.

use crate::{reset_sigmask, PowerAction};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const STABLE_AFTER: Duration = Duration::from_secs(10);
const LOG_ROTATE_BYTES: u64 = 1 << 20;
const LOG_TAIL_BYTES: u64 = 256 * 1024;
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

// ---- configuration --------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    Never,
    #[default]
    OnFailure,
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceType {
    /// Long-running. Dependents may start as soon as it has been spawned.
    #[default]
    Simple,
    /// Runs to completion. Dependents wait until it has exited successfully.
    Oneshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub restart: RestartPolicy,
    #[serde(default = "default_restart_delay")]
    pub restart_delay_secs: u64,
    #[serde(default = "default_max_restart_delay")]
    pub max_restart_delay_secs: u64,
    /// Share init's stdin/stdout/stderr instead of logging to a file.
    #[serde(default)]
    pub inherit_stdio: bool,
    #[serde(default, rename = "type")]
    pub kind: ServiceType,
    /// SIGTERM, then SIGKILL after this many seconds.
    #[serde(default = "default_stop_timeout")]
    pub stop_timeout_secs: u64,
    /// Run in its own session with this terminal (e.g. "/dev/console") as the
    /// controlling terminal, so job control and ctrl-c work in a shell.
    #[serde(default)]
    pub tty: Option<String>,
}
fn default_true() -> bool {
    true
}
fn default_restart_delay() -> u64 {
    1
}
fn default_max_restart_delay() -> u64 {
    30
}
fn default_stop_timeout() -> u64 {
    5
}

fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && !n.starts_with('.')
        && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '@'))
}

impl ServiceConfig {
    fn validate(&self) -> Result<(), String> {
        // The name becomes a log file name, so it must not be able to escape the log directory.
        if !valid_name(&self.name) {
            return Err(format!("invalid service name '{}'", self.name));
        }
        if self.command.trim().is_empty() {
            return Err("command must not be empty".into());
        }
        for d in &self.depends_on {
            if d == &self.name {
                return Err("a service cannot depend on itself".into());
            }
            if !valid_name(d) {
                return Err(format!("invalid dependency name '{d}'"));
            }
        }
        if self.tty.as_deref() == Some("") {
            return Err("tty must not be empty".into());
        }
        Ok(())
    }
}

pub struct Loaded {
    /// Dependencies before dependents.
    pub configs: Vec<ServiceConfig>,
    pub warnings: Vec<String>,
}

/// Load every `*.toml` in `dir`. A bad file, a missing dependency or a cycle
/// drops the affected services with a warning; it never aborts loading, because
/// PID 1 must keep running no matter what the configuration looks like.
pub fn load_configs(dir: &Path) -> io::Result<Loaded> {
    let mut paths = fs::read_dir(dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect::<Vec<_>>();
    paths.sort();
    let mut by_name = BTreeMap::new();
    let mut warnings = Vec::new();
    for path in paths {
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                warnings.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        let config: ServiceConfig = match toml::from_str(&text) {
            Ok(c) => c,
            Err(e) => {
                warnings.push(format!("{}: {}", path.display(), e.message()));
                continue;
            }
        };
        if let Err(e) = config.validate() {
            warnings.push(format!("{}: {e}", path.display()));
            continue;
        }
        if by_name.contains_key(&config.name) {
            warnings.push(format!("{}: duplicate service name '{}', ignored", path.display(), config.name));
            continue;
        }
        by_name.insert(config.name.clone(), config);
    }
    Ok(resolve(by_name, warnings))
}

pub fn resolve(mut map: BTreeMap<String, ServiceConfig>, mut warnings: Vec<String>) -> Loaded {
    // Dropping a service can orphan its dependents, so repeat until stable.
    // Every pass removes at least one service, so this terminates.
    let order = loop {
        let mut removed = false;
        let names: BTreeSet<String> = map.keys().cloned().collect();
        let missing: Vec<(String, String)> = map
            .values()
            .filter_map(|c| c.depends_on.iter().find(|d| !names.contains(*d)).map(|d| (c.name.clone(), d.clone())))
            .collect();
        for (svc, dep) in missing {
            map.remove(&svc);
            warnings.push(format!("{svc}: depends on unknown service '{dep}', not loaded"));
            removed = true;
        }
        if removed {
            continue;
        }
        let (order, stuck) = topo(&map);
        if stuck.is_empty() {
            break order;
        }
        for svc in stuck {
            map.remove(&svc);
            warnings.push(format!("{svc}: in or behind a dependency cycle, not loaded"));
        }
    };
    let configs = order.iter().filter_map(|n| map.remove(n)).collect();
    Loaded { configs, warnings }
}

/// Kahn's algorithm. Returns (start order, services that could not be ordered).
fn topo(map: &BTreeMap<String, ServiceConfig>) -> (Vec<String>, Vec<String>) {
    let mut indegree: BTreeMap<&str, usize> = map
        .iter()
        .map(|(n, c)| (n.as_str(), c.depends_on.iter().collect::<BTreeSet<_>>().len()))
        .collect();
    let mut ready: BTreeSet<&str> = indegree.iter().filter(|(_, d)| **d == 0).map(|(n, _)| *n).collect();
    let mut order = Vec::new();
    while let Some(n) = ready.iter().next().copied() {
        ready.remove(n);
        order.push(n.to_string());
        for (m, c) in map {
            if c.depends_on.iter().any(|d| d == n) {
                if let Some(e) = indegree.get_mut(m.as_str()) {
                    *e = e.saturating_sub(1);
                    if *e == 0 {
                        ready.insert(m.as_str());
                    }
                }
            }
        }
    }
    let stuck = map.keys().filter(|n| !order.contains(n)).cloned().collect();
    (order, stuck)
}

// ---- runtime state --------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exit {
    Code(i32),
    Signal(i32),
}

impl fmt::Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Exit::Code(c) => write!(f, "exit {c}"),
            Exit::Signal(s) => write!(f, "signal {s}"),
        }
    }
}

#[derive(Debug)]
enum State {
    Inactive,
    /// `group`: the process leads its own process group, so signals go to the group.
    Running { pid: i32, since: Instant, group: bool },
    Stopping { pid: i32, group: bool, kill_at: Instant, killed: bool },
    Backoff { until: Instant },
    /// Exited successfully.
    Done,
    Failed(String),
}

struct Service {
    config: ServiceConfig,
    state: State,
    want: bool,
    /// Consecutive failures, for exponential backoff; reset after a stable run.
    failures: u32,
    last_exit: Option<Exit>,
}

impl Service {
    /// Can dependents start? A running oneshot is not ready until it finishes.
    fn is_ready(&self) -> bool {
        match self.state {
            State::Running { .. } => self.config.kind == ServiceType::Simple,
            State::Done => true,
            _ => false,
        }
    }
}

enum DepCheck {
    Ready,
    Wait(Vec<String>),
    Failed(String),
}

enum Act {
    None,
    Start,
    Stop,
    Inactive,
    Fail(String),
}

struct Shutdown {
    action: PowerAction,
    hard_deadline: Instant,
}

pub struct Manager {
    services: Vec<Service>,
    log_dir: PathBuf,
    pid1: bool,
    shutdown: Option<Shutdown>,
}

fn kill_pid(pid: i32, group: bool, sig: i32) {
    if pid > 1 {
        unsafe {
            libc::kill(if group { -pid } else { pid }, sig);
        }
    }
}

fn fmt_secs(d: Duration) -> String {
    format!("{:.1}s", d.as_secs_f64())
}

impl Manager {
    /// `configs` must be dependency-ordered (as returned by `load_configs`).
    pub fn new(configs: Vec<ServiceConfig>, log_dir: PathBuf, pid1: bool) -> Self {
        if let Err(e) = fs::create_dir_all(&log_dir) {
            llog!("cannot create log directory {}: {e}", log_dir.display());
        }
        let services = configs
            .into_iter()
            .map(|config| Service { config, state: State::Inactive, want: false, failures: 0, last_exit: None })
            .collect();
        Manager { services, log_dir, pid1, shutdown: None }
    }

    /// Want every enabled service (and what it depends on).
    pub fn boot(&mut self) {
        let names: Vec<String> = self.services.iter().filter(|s| s.config.enabled).map(|s| s.config.name.clone()).collect();
        for n in names {
            let _ = self.start_closure(&n);
        }
    }

    fn idx(&self, name: &str) -> Option<usize> {
        self.services.iter().position(|s| s.config.name == name)
    }

    fn log_path(&self, name: &str) -> PathBuf {
        self.log_dir.join(format!("{name}.log"))
    }

    // ---- desired state ----

    /// Want `name` and everything it depends on. Failed services in that set
    /// get another chance; a finished target runs again.
    fn start_closure(&mut self, name: &str) -> Result<(), String> {
        let root = self.idx(name).ok_or_else(|| format!("unknown service: {name}"))?;
        let mut seen = vec![false; self.services.len()];
        let mut stack = vec![root];
        while let Some(i) = stack.pop() {
            if std::mem::replace(&mut seen[i], true) {
                continue;
            }
            for d in self.services[i].config.depends_on.clone() {
                if let Some(j) = self.idx(&d) {
                    stack.push(j);
                }
            }
            let s = &mut self.services[i];
            s.want = true;
            if matches!(s.state, State::Failed(_)) || (i == root && matches!(s.state, State::Done)) {
                s.state = State::Inactive;
                s.failures = 0;
            }
        }
        Ok(())
    }

    /// Stop `name` and everything that depends on it. Returns the names affected.
    fn stop_closure(&mut self, name: &str) -> Result<Vec<String>, String> {
        let root = self.idx(name).ok_or_else(|| format!("unknown service: {name}"))?;
        let mut affected = Vec::new();
        for i in std::iter::once(root).chain(self.dependents_of(root)) {
            let s = &mut self.services[i];
            s.want = false;
            // A finished oneshot is only "done" while what it set up is still
            // there; stopping it, or what it needs, means it must run again.
            if matches!(s.state, State::Done) {
                s.state = State::Inactive;
            }
            affected.push(s.config.name.clone());
        }
        Ok(affected)
    }

    /// Everything that depends on service `i`, directly or through a chain. The
    /// chain matters: with app -> prep -> base, where prep is a finished
    /// oneshot, app still needs base even though prep is not running.
    fn dependents_of(&self, i: usize) -> Vec<usize> {
        let mut seen = vec![false; self.services.len()];
        let mut stack = vec![i];
        let mut out = Vec::new();
        while let Some(k) = stack.pop() {
            if std::mem::replace(&mut seen[k], true) {
                continue;
            }
            if k != i {
                out.push(k);
            }
            let me = &self.services[k].config.name;
            for (j, s) in self.services.iter().enumerate() {
                if s.config.depends_on.contains(me) {
                    stack.push(j);
                }
            }
        }
        out
    }

    fn has_live_dependent(&self, i: usize) -> bool {
        self.dependents_of(i)
            .into_iter()
            .any(|j| matches!(self.services[j].state, State::Running { .. } | State::Stopping { .. }))
    }

    // ---- reconcile ----

    fn reconcile(&mut self) {
        for _ in 0..64 {
            let mut changed = false;
            for i in 0..self.services.len() {
                changed |= self.step(i);
            }
            if !changed {
                break;
            }
        }
    }

    fn step(&mut self, i: usize) -> bool {
        let now = Instant::now();
        let want = self.services[i].want;
        let act = match &self.services[i].state {
            State::Inactive if want => self.deps_action(i),
            State::Backoff { until } if want => {
                if now >= *until {
                    self.deps_action(i)
                } else {
                    Act::None
                }
            }
            State::Backoff { .. } => Act::Inactive,
            // Dependents go first: wait until nothing that needs us is running.
            State::Running { .. } if !want => {
                if self.has_live_dependent(i) {
                    Act::None
                } else {
                    Act::Stop
                }
            }
            _ => Act::None,
        };
        match act {
            Act::None => false,
            Act::Start => {
                self.spawn_service(i);
                true
            }
            Act::Stop => {
                self.begin_stop(i);
                true
            }
            Act::Inactive => {
                self.services[i].state = State::Inactive;
                true
            }
            Act::Fail(msg) => {
                llog!("{} failed: {msg}", self.services[i].config.name);
                self.services[i].state = State::Failed(msg);
                true
            }
        }
    }

    fn deps_action(&self, i: usize) -> Act {
        match self.check_deps(i) {
            DepCheck::Ready => Act::Start,
            DepCheck::Wait(_) => Act::None,
            DepCheck::Failed(msg) => Act::Fail(msg),
        }
    }

    fn check_deps(&self, i: usize) -> DepCheck {
        let mut waiting = Vec::new();
        for d in &self.services[i].config.depends_on {
            match self.idx(d).map(|j| &self.services[j]) {
                None => return DepCheck::Failed(format!("depends on missing service '{d}'")),
                Some(dep) => {
                    if matches!(dep.state, State::Failed(_)) {
                        return DepCheck::Failed(format!("dependency '{d}' failed"));
                    }
                    if !dep.is_ready() {
                        waiting.push(d.clone());
                    }
                }
            }
        }
        if waiting.is_empty() {
            DepCheck::Ready
        } else {
            DepCheck::Wait(waiting)
        }
    }

    // ---- processes ----

    fn spawn_service(&mut self, i: usize) {
        let config = self.services[i].config.clone();
        let log = self.log_path(&config.name);
        match spawn_process(&config, &log, self.pid1) {
            Ok((pid, group)) => {
                llog!("started {} (pid {pid})", config.name);
                self.marker(i, &format!("started (pid {pid})"));
                self.services[i].state = State::Running { pid, since: Instant::now(), group };
            }
            Err(e) => {
                llog!("failed to start {}: {e}", config.name);
                self.marker(i, &format!("failed to start: {e}"));
                self.ended(i, Duration::ZERO, Some(format!("cannot start: {e}")), Exit::Code(127));
            }
        }
    }

    fn begin_stop(&mut self, i: usize) {
        let s = &mut self.services[i];
        if let State::Running { pid, group, .. } = s.state {
            kill_pid(pid, group, libc::SIGTERM);
            let kill_at = Instant::now() + Duration::from_secs(s.config.stop_timeout_secs);
            s.state = State::Stopping { pid, group, kill_at, killed: false };
        }
    }

    /// Append a line to the service's log, so crashes and restarts show up next
    /// to the service's own output.
    fn marker(&self, i: usize, msg: &str) {
        let s = &self.services[i];
        if s.config.inherit_stdio || s.config.tty.is_some() {
            return;
        }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(self.log_path(&s.config.name)) {
            let _ = writeln!(f, "[init] {msg}");
        }
    }

    fn reap(&mut self) {
        loop {
            let mut status: libc::c_int = 0;
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid <= 0 {
                break;
            }
            let exit = if libc::WIFEXITED(status) {
                Exit::Code(libc::WEXITSTATUS(status))
            } else if libc::WIFSIGNALED(status) {
                Exit::Signal(libc::WTERMSIG(status))
            } else {
                continue;
            };
            self.on_exit(pid, exit);
        }
    }

    fn on_exit(&mut self, pid: i32, exit: Exit) {
        // Anything else is an orphan re-parented to us; reaping it was all that was needed.
        let Some(i) = self.services.iter().position(|s| match s.state {
            State::Running { pid: p, .. } | State::Stopping { pid: p, .. } => p == pid,
            _ => false,
        }) else {
            return;
        };
        let name = self.services[i].config.name.clone();
        self.services[i].last_exit = Some(exit);
        let old = std::mem::replace(&mut self.services[i].state, State::Inactive);
        match old {
            State::Stopping { .. } => {
                llog!("{name} stopped ({exit})");
                self.marker(i, &format!("stopped ({exit})"));
                self.services[i].failures = 0;
            }
            State::Running { since, .. } => {
                llog!("{name} exited ({exit})");
                self.marker(i, &format!("exited ({exit})"));
                self.ended(i, since.elapsed(), None, exit);
            }
            _ => {}
        }
    }

    /// A run ended on its own: decide between done, failed and restart.
    fn ended(&mut self, i: usize, ran_for: Duration, spawn_err: Option<String>, exit: Exit) {
        let s = &mut self.services[i];
        if ran_for >= STABLE_AFTER {
            s.failures = 0;
        }
        let success = spawn_err.is_none() && exit == Exit::Code(0);
        let restart = s.want
            && match s.config.restart {
                RestartPolicy::Always => true,
                RestartPolicy::OnFailure => !success,
                RestartPolicy::Never => false,
            };
        if restart {
            let base = s.config.restart_delay_secs.max(1);
            let max = s.config.max_restart_delay_secs.max(base);
            let delay = base.saturating_mul(1u64 << s.failures.min(20)).min(max);
            s.failures = s.failures.saturating_add(1);
            llog!("{} will restart in {delay}s", s.config.name);
            s.state = State::Backoff { until: Instant::now() + Duration::from_secs(delay) };
        } else if success {
            s.state = State::Done;
        } else {
            s.state = State::Failed(spawn_err.unwrap_or_else(|| exit.to_string()));
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        for s in &mut self.services {
            if let State::Stopping { pid, group, kill_at, killed } = &mut s.state {
                if !*killed && now >= *kill_at {
                    llog!("{} did not stop in time, killing it", s.config.name);
                    kill_pid(*pid, *group, libc::SIGKILL);
                    *killed = true;
                }
            }
        }
    }

    // ---- shutdown ----

    fn begin_shutdown(&mut self, action: PowerAction) {
        if self.shutdown.is_some() {
            return;
        }
        llog!("shutting down ({action:?})");
        let mut longest = 0;
        for s in &mut self.services {
            s.want = false;
            longest = longest.max(s.config.stop_timeout_secs);
        }
        self.shutdown = Some(Shutdown { action, hard_deadline: Instant::now() + Duration::from_secs(longest + 2) });
    }

    fn all_stopped(&self) -> bool {
        !self.services.iter().any(|s| matches!(s.state, State::Running { .. } | State::Stopping { .. }))
    }

    // ---- event loop ----

    fn next_timeout(&self) -> i32 {
        let now = Instant::now();
        let mut next: Option<Instant> = None;
        let mut consider = |t: Instant| next = Some(next.map_or(t, |n| n.min(t)));
        for s in &self.services {
            match &s.state {
                // A backoff that is already due is waiting on a dependency, which
                // will arrive as an event; don't spin on it.
                State::Backoff { until } if *until > now => consider(*until),
                State::Stopping { kill_at, killed: false, .. } if *kill_at > now => consider(*kill_at),
                _ => {}
            }
        }
        if let Some(sd) = &self.shutdown {
            consider(sd.hard_deadline);
        }
        match next {
            None => -1,
            Some(t) => (t.saturating_duration_since(now).as_millis() as i64 + 1).min(i32::MAX as i64) as i32,
        }
    }

    /// Run until a shutdown has completed; the caller performs the power action.
    pub fn run(&mut self, listener: Option<&UnixListener>, sigfd: RawFd) -> PowerAction {
        let lfd = listener.map_or(-1, |l| l.as_raw_fd());
        loop {
            self.reap();
            self.reconcile();
            self.tick();
            if let Some(sd) = &self.shutdown {
                if self.all_stopped() || Instant::now() >= sd.hard_deadline {
                    return sd.action;
                }
            }
            let mut fds = [
                libc::pollfd { fd: sigfd, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: lfd, events: libc::POLLIN, revents: 0 },
            ];
            let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, self.next_timeout()) };
            if rc < 0 {
                if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    std::thread::sleep(Duration::from_millis(10));
                }
                continue;
            }
            if fds[0].revents != 0 {
                self.drain_signals(sigfd);
            }
            if fds[1].revents & libc::POLLIN != 0 {
                if let Some(l) = listener {
                    self.accept_clients(l);
                }
            }
        }
    }

    fn drain_signals(&mut self, sigfd: RawFd) {
        loop {
            let mut info: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
            let want = std::mem::size_of::<libc::signalfd_siginfo>();
            let n = unsafe { libc::read(sigfd, (&mut info as *mut libc::signalfd_siginfo).cast(), want) };
            if n != want as isize {
                break;
            }
            // Same convention as sysvinit and busybox: INT (ctrl-alt-del) and
            // TERM reboot, USR1 halts, USR2 powers off. This is how busybox's
            // `poweroff` and `reboot` talk to us.
            match info.ssi_signo as i32 {
                libc::SIGINT | libc::SIGTERM => self.begin_shutdown(PowerAction::Reboot),
                libc::SIGUSR1 => self.begin_shutdown(PowerAction::Halt),
                libc::SIGUSR2 => self.begin_shutdown(PowerAction::Poweroff),
                _ => {}
            }
        }
    }

    // ---- control socket ----

    fn accept_clients(&mut self, listener: &UnixListener) {
        loop {
            match listener.accept() {
                Ok((stream, _)) => self.serve(stream),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break, // WouldBlock: nothing more pending
            }
        }
    }

    /// Clients are handled synchronously, so the timeouts keep a stuck client
    /// from stalling init for long. The socket is root-only.
    fn serve(&mut self, stream: UnixStream) {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
        let mut line = String::new();
        let response = match BufReader::new((&stream).take(1024)).read_line(&mut line) {
            Ok(_) => self.handle_command(line.trim()),
            Err(e) => format!("ERR unable to read command: {e}"),
        };
        let _ = writeln!(&stream, "{response}");
    }

    pub fn handle_command(&mut self, command: &str) -> String {
        let words: Vec<&str> = command.split_whitespace().collect();
        let readonly = matches!(words.first(), Some(&("status" | "log" | "help")));
        if self.shutdown.is_some() && !readonly {
            return "ERR system is shutting down".into();
        }
        match words.as_slice() {
            ["status"] => {
                let lines: Vec<String> = (0..self.services.len())
                    .map(|i| format!("{}: {}", self.services[i].config.name, self.state_text(i)))
                    .collect();
                format!("OK\n{}", lines.join("\n"))
            }
            ["status", name] => match self.idx(name) {
                Some(i) => format!("OK {name}: {}", self.state_text(i)),
                None => format!("ERR unknown service: {name}"),
            },
            ["start", name] => match self.start_closure(name) {
                Ok(()) => {
                    self.reconcile();
                    self.summary("OK", name)
                }
                Err(e) => format!("ERR {e}"),
            },
            ["stop", name] => match self.stop_closure(name) {
                Ok(affected) => {
                    self.reconcile();
                    format!("OK stopping {}", affected.join(", "))
                }
                Err(e) => format!("ERR {e}"),
            },
            // Restarts just this service; whatever depends on it keeps running.
            ["restart", name] => match self.restart(name) {
                Ok(()) => {
                    self.reconcile();
                    self.summary("OK", name)
                }
                Err(e) => format!("ERR {e}"),
            },
            ["log", name] => self.log_tail(name, 50),
            ["log", name, n] => match n.parse() {
                Ok(n) => self.log_tail(name, n),
                Err(_) => format!("ERR '{n}' is not a number of lines"),
            },
            ["shutdown" | "poweroff"] => self.power(PowerAction::Poweroff, "powering off"),
            ["reboot"] => self.power(PowerAction::Reboot, "rebooting"),
            ["halt"] => self.power(PowerAction::Halt, "halting"),
            ["help"] => "OK commands: status [SERVICE], start SERVICE, stop SERVICE, restart SERVICE, \
                         log SERVICE [LINES], shutdown|poweroff, reboot, halt"
                .into(),
            [] => "ERR empty command; try help".into(),
            ["start" | "stop" | "restart" | "log", ..] => "ERR usage: start|stop|restart|log SERVICE".into(),
            _ => "ERR unknown command; try help".into(),
        }
    }

    fn power(&mut self, action: PowerAction, msg: &str) -> String {
        self.begin_shutdown(action);
        format!("OK {msg}")
    }

    fn summary(&self, prefix: &str, name: &str) -> String {
        match self.idx(name) {
            Some(i) => format!("{prefix} {name}: {}", self.state_text(i)),
            None => format!("{prefix} {name}"),
        }
    }

    fn restart(&mut self, name: &str) -> Result<(), String> {
        self.start_closure(name)?;
        let Some(i) = self.idx(name) else { return Ok(()) };
        match self.services[i].state {
            // It comes back by itself: `want` is set, so once the process has
            // exited reconcile starts it again.
            State::Running { .. } => self.begin_stop(i),
            State::Backoff { .. } => self.services[i].state = State::Inactive,
            _ => {}
        }
        Ok(())
    }

    fn state_text(&self, i: usize) -> String {
        let s = &self.services[i];
        match &s.state {
            State::Inactive if s.want => match self.check_deps(i) {
                DepCheck::Wait(w) => format!("waiting for {}", w.join(", ")),
                _ => "waiting".into(),
            },
            State::Inactive => match s.last_exit {
                Some(e) => format!("stopped ({e})"),
                None => "stopped".into(),
            },
            State::Running { pid, .. } => format!("running (pid {pid})"),
            State::Stopping { pid, .. } => format!("stopping (pid {pid})"),
            State::Backoff { until } => {
                format!("waiting to restart ({})", fmt_secs(until.saturating_duration_since(Instant::now())))
            }
            State::Done => "done".into(),
            State::Failed(msg) => format!("failed ({msg})"),
        }
    }

    fn log_tail(&self, name: &str, lines: usize) -> String {
        let Some(i) = self.idx(name) else {
            return format!("ERR unknown service: {name}");
        };
        let c = &self.services[i].config;
        if c.inherit_stdio || c.tty.is_some() {
            return format!("OK {name} runs on the console and has no log file");
        }
        match tail_lines(&self.log_path(name), lines) {
            Ok(t) => format!("OK\n{}", t.trim_end_matches('\n')),
            Err(e) if e.kind() == io::ErrorKind::NotFound => "OK".into(),
            Err(e) => format!("ERR cannot read log: {e}"),
        }
    }
}

fn tail_lines(path: &Path, n: usize) -> io::Result<String> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(LOG_TAIL_BYTES);
    f.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    f.take(LOG_TAIL_BYTES).read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    let mut all: Vec<&str> = text.lines().collect();
    if start > 0 && !all.is_empty() {
        all.remove(0); // the first line is probably cut in half
    }
    let from = all.len().saturating_sub(n);
    Ok(all[from..].join("\n"))
}

/// Start the process. Returns (pid, leads_its_own_process_group).
fn spawn_process(config: &ServiceConfig, log: &Path, pid1: bool) -> io::Result<(i32, bool)> {
    let mut cmd = Command::new(&config.command);
    cmd.args(&config.args).envs(&config.env);
    if std::env::var_os("PATH").is_none() {
        cmd.env("PATH", DEFAULT_PATH);
    }
    let mut group = true;

    match (&config.tty, pid1) {
        (Some(tty), true) => {
            // Own session with the terminal as controlling tty, so job control
            // and ctrl-c work in an interactive shell.
            let tty = std::ffi::CString::new(tty.as_str()).map_err(io::Error::other)?;
            cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
            unsafe {
                cmd.pre_exec(move || {
                    reset_sigmask();
                    libc::setsid();
                    let fd = libc::open(tty.as_ptr(), libc::O_RDWR);
                    if fd < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    libc::ioctl(fd, libc::TIOCSCTTY as _, 0);
                    libc::dup2(fd, 0);
                    libc::dup2(fd, 1);
                    libc::dup2(fd, 2);
                    if fd > 2 {
                        libc::close(fd);
                    }
                    Ok(())
                });
            }
        }
        (tty, _) if tty.is_some() || config.inherit_stdio => {
            // Dev mode has no console to take over, and `inherit_stdio` keeps its
            // old meaning: share init's stdio, no new process group (so tools like
            // `busybox setsid` keep working).
            cmd.stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
            group = false;
            unsafe {
                cmd.pre_exec(|| {
                    reset_sigmask();
                    Ok(())
                });
            }
        }
        _ => {
            // Keep /run (a tmpfs) from filling up with a chatty service's output.
            if fs::metadata(log).map(|m| m.len() > LOG_ROTATE_BYTES).unwrap_or(false) {
                let _ = fs::rename(log, log.with_extension("log.1"));
            }
            let file = OpenOptions::new().create(true).append(true).open(log)?;
            cmd.stdin(Stdio::null()).stdout(Stdio::from(file.try_clone()?)).stderr(Stdio::from(file));
            cmd.process_group(0);
            unsafe {
                cmd.pre_exec(|| {
                    reset_sigmask();
                    Ok(())
                });
            }
        }
    }

    let child = cmd.spawn()?;
    Ok((child.id() as i32, group))
}
