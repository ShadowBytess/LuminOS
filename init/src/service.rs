use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    Never,
    OnFailure,
    Always,
}
impl Default for RestartPolicy { fn default() -> Self { Self::OnFailure } }

#[derive(Debug, Clone, Deserialize)]
pub struct ServiceConfig {
    pub name: String,
    pub command: String,
    #[serde(default)] pub args: Vec<String>,
    #[serde(default)] pub env: BTreeMap<String, String>,
    #[serde(default)] pub depends_on: Vec<String>,
    #[serde(default = "default_true")] pub enabled: bool,
    #[serde(default)] pub restart: RestartPolicy,
    #[serde(default = "default_restart_delay")] pub restart_delay_secs: u64,
    #[serde(default = "default_max_restart_delay")] pub max_restart_delay_secs: u64,
    #[serde(default)] pub inherit_stdio: bool,
}
fn default_true() -> bool { true }
fn default_restart_delay() -> u64 { 1 }
fn default_max_restart_delay() -> u64 { 30 }

struct RuntimeService {
    config: ServiceConfig,
    child: Option<Child>,
    last_exit: Option<String>,
    restart_count: u32,
    restart_at: Option<Instant>,
    manually_stopped: bool,
}

pub struct Manager {
    services: BTreeMap<String, RuntimeService>,
    log_dir: PathBuf,
    shutdown: bool,
}

impl Manager {
    pub fn new(configs: Vec<ServiceConfig>, dev_mode: bool) -> io::Result<Self> {
        let mut services = BTreeMap::new();
        for config in configs {
            if config.name.trim().is_empty() || config.command.trim().is_empty() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "service name and command must not be empty"));
            }
            let name = config.name.clone();
            if services.insert(name.clone(), RuntimeService {
                config, child: None, last_exit: None, restart_count: 0,
                restart_at: None, manually_stopped: false,
            }).is_some() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("duplicate service name: {name}")));
            }
        }
        let log_dir = if dev_mode { PathBuf::from("./build/logs") } else { PathBuf::from("/run/luminos/logs") };
        fs::create_dir_all(&log_dir)?;
        let manager = Self { services, log_dir, shutdown: false };
        manager.validate_dependencies()?;
        Ok(manager)
    }

    fn validate_dependencies(&self) -> io::Result<()> {
        for (name, service) in &self.services {
            for dependency in &service.config.depends_on {
                if !self.services.contains_key(dependency) {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, format!("service {name} depends on unknown service {dependency}")));
                }
            }
        }
        let mut visiting = HashSet::new();
        let mut visited = HashSet::new();
        fn visit(name: &str, services: &BTreeMap<String, RuntimeService>, visiting: &mut HashSet<String>, visited: &mut HashSet<String>) -> io::Result<()> {
            if visited.contains(name) { return Ok(()); }
            if !visiting.insert(name.to_string()) {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("dependency cycle involving {name}")));
            }
            if let Some(service) = services.get(name) {
                for dep in &service.config.depends_on { visit(dep, services, visiting, visited)?; }
            }
            visiting.remove(name);
            visited.insert(name.to_string());
            Ok(())
        }
        for name in self.services.keys() { visit(name, &self.services, &mut visiting, &mut visited)?; }
        Ok(())
    }

    fn startup_order(&self) -> Vec<String> {
        fn visit(name: &str, services: &BTreeMap<String, RuntimeService>, seen: &mut HashSet<String>, out: &mut Vec<String>) {
            if !seen.insert(name.to_string()) { return; }
            if let Some(svc) = services.get(name) {
                for dep in &svc.config.depends_on { visit(dep, services, seen, out); }
            }
            out.push(name.to_string());
        }
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for (name, svc) in &self.services { if svc.config.enabled { visit(name, &self.services, &mut seen, &mut out); } }
        out
    }

    pub fn start_enabled(&mut self) -> io::Result<()> {
        for name in self.startup_order() { self.start_service(&name)?; }
        Ok(())
    }

    fn start_service(&mut self, name: &str) -> io::Result<()> {
        let dependencies = self.services.get(name)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("unknown service: {name}")))?
            .config.depends_on.clone();
        for dep in dependencies {
            let running = self.services.get(&dep).is_some_and(|s| s.child.is_some());
            if !running { self.start_service(&dep)?; }
        }
        let log_dir = self.log_dir.clone();
        let svc = self.services.get_mut(name).unwrap();
        if svc.child.is_some() { return Ok(()); }
        svc.manually_stopped = false;
        svc.restart_at = None;

        let mut command = Command::new(&svc.config.command);
        command.args(&svc.config.args).envs(&svc.config.env);
        if svc.config.inherit_stdio {
            command.stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::inherit());
        } else {
            let log_path = log_dir.join(format!("{name}.log"));
            let log = OpenOptions::new().create(true).append(true).open(log_path)?;
            let stderr = log.try_clone()?;
            command.stdin(Stdio::null()).stdout(Stdio::from(log)).stderr(Stdio::from(stderr));
        }
        match command.spawn() {
            Ok(child) => {
                println!("luminos-init: started {name} (pid {})", child.id());
                svc.child = Some(child);
                svc.last_exit = None;
                Ok(())
            }
            Err(err) => {
                svc.last_exit = Some(format!("spawn failed: {err}"));
                Err(io::Error::new(err.kind(), format!("starting {name}: {err}")))
            }
        }
    }

    fn stop_service(&mut self, name: &str) -> io::Result<()> {
        let svc = self.services.get_mut(name)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("unknown service: {name}")))?;
        svc.manually_stopped = true;
        svc.restart_at = None;
        if let Some(mut child) = svc.child.take() {
            let _ = child.kill();
            let _ = child.wait();
            println!("luminos-init: stopped {name}");
        }
        Ok(())
    }

    fn stop_dependents_first(&mut self, name: &str) {
        let mut dependents = self.services.iter()
            .filter(|(_, svc)| svc.config.depends_on.iter().any(|dep| dep == name))
            .map(|(name, _)| name.clone()).collect::<Vec<_>>();
        dependents.sort();
        for dependent in dependents { self.stop_dependents_first(&dependent); let _ = self.stop_service(&dependent); }
    }

    pub fn stop_all(&mut self) {
        let mut order = self.startup_order();
        order.reverse();
        for name in order { let _ = self.stop_service(&name); }
        // Also reap any children not included in the enabled startup list.
        for svc in self.services.values_mut() {
            if let Some(mut child) = svc.child.take() { let _ = child.kill(); let _ = child.wait(); }
        }
    }

    pub fn poll_children(&mut self) {
        // PID 1 must reap *all* children, including grandchildren orphaned by
        // supervised services. Reap with waitpid and match known service PIDs.
        loop {
            let mut raw_status: libc::c_int = 0;
            let pid = unsafe { libc::waitpid(-1, &mut raw_status, libc::WNOHANG) };
            if pid <= 0 { break; }
            let name = self.services.iter().find_map(|(name, svc)| {
                svc.child.as_ref().filter(|child| child.id() == pid as u32).map(|_| name.clone())
            });
            if let Some(name) = name {
                if let Some(svc) = self.services.get_mut(&name) { svc.child = None; }
                use std::os::unix::process::ExitStatusExt;
                self.record_exit(&name, ExitStatus::from_raw(raw_status));
            } else {
                println!("luminos-init: reaped orphan child {pid}");
            }
        }

        let due = self.services.iter().filter_map(|(name, svc)| {
            svc.restart_at.filter(|when| Instant::now() >= *when).map(|_| name.clone())
        }).collect::<Vec<_>>();
        for name in due {
            if let Err(err) = self.start_service(&name) { eprintln!("luminos-init: restart {name}: {err}"); }
        }
    }

    fn record_exit(&mut self, name: &str, status: ExitStatus) {
        if let Some(svc) = self.services.get_mut(name) {
            svc.child = None;
            svc.last_exit = Some(status.to_string());
            let success = status.success();
            let should_restart = !svc.manually_stopped && match svc.config.restart {
                RestartPolicy::Never => false,
                RestartPolicy::OnFailure => !success,
                RestartPolicy::Always => true,
            };
            if should_restart {
                let base = svc.config.restart_delay_secs.max(1);
                let max = svc.config.max_restart_delay_secs.max(base);
                let shift = svc.restart_count.min(20);
                let delay = base.saturating_mul(1u64 << shift).min(max);
                svc.restart_count = svc.restart_count.saturating_add(1);
                svc.restart_at = Some(Instant::now() + Duration::from_secs(delay));
                eprintln!("luminos-init: {name} exited ({status}); restart in {delay}s");
            } else {
                svc.restart_at = None;
                println!("luminos-init: {name} exited ({status})");
            }
        }
    }

    pub fn handle_command(&mut self, command: &str) -> String {
        let mut parts = command.split_whitespace();
        let action = parts.next().unwrap_or("");
        let name = parts.next();
        if parts.next().is_some() { return "ERR too many arguments".into(); }
        match action {
            "status" => {
                if let Some(name) = name {
                    match self.services.get(name) {
                        Some(svc) => format!("OK {}: {}", name, service_state(svc)),
                        None => format!("ERR unknown service: {name}"),
                    }
                } else {
                    let lines = self.services.iter().map(|(name, svc)| format!("{name}: {}", service_state(svc))).collect::<Vec<_>>();
                    format!("OK\n{}", lines.join("\n"))
                }
            }
            "start" => match name {
                Some(name) => match self.start_service(name) { Ok(()) => format!("OK started {name}"), Err(e) => format!("ERR {e}") },
                None => "ERR usage: start SERVICE".into(),
            },
            "stop" => match name {
                Some(name) => { self.stop_dependents_first(name); match self.stop_service(name) { Ok(()) => format!("OK stopped {name}"), Err(e) => format!("ERR {e}") } },
                None => "ERR usage: stop SERVICE".into(),
            },
            "restart" => match name {
                Some(name) => { self.stop_dependents_first(name); match self.stop_service(name).and_then(|_| self.start_service(name)) { Ok(()) => format!("OK restarted {name}"), Err(e) => format!("ERR {e}") } },
                None => "ERR usage: restart SERVICE".into(),
            },
            "shutdown" => { self.shutdown = true; "OK shutting down".into() }
            "help" => "OK commands: status [SERVICE], start SERVICE, stop SERVICE, restart SERVICE, shutdown".into(),
            _ => "ERR unknown command; try help".into(),
        }
    }

    pub fn shutdown_requested(&self) -> bool { self.shutdown }
}

fn service_state(service: &RuntimeService) -> String {
    if let Some(child) = &service.child { format!("running (pid {})", child.id()) }
    else if let Some(when) = service.restart_at { format!("waiting to restart ({:.1}s)", when.saturating_duration_since(Instant::now()).as_secs_f64()) }
    else if let Some(exit) = &service.last_exit { format!("stopped ({exit})") }
    else { "stopped".into() }
}

