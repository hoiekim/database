//! A server at the small end: on a few CPUs, under a memory limit.
//!
//! ```text
//! BENCH_SERVER_CPUS=1 BENCH_SERVER_MEMORY_MB=256 ./target/release/bench
//! ```
//!
//! `BENCH_SERVER_CPUS=n` pins every server the benchmark starts to the
//! first `n` CPUs this process may use, and the benchmark's own client to
//! the rest, so that the client's work is not taken from the server's
//! share. `BENCH_SERVER_MEMORY_MB=m` starts every server in a memory
//! cgroup of its own with that limit, as a container with a memory limit
//! runs it: anonymous memory and the file cache both count, and a server
//! that goes over is killed.
//!
//! Both are Linux only. A limit that cannot be put in place stops the
//! run, except the memory limit where no cgroup can be made (a container
//! without the right to): the run then goes on without it and says so,
//! and the report's "most memory held" is to be read against the limit.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub struct Limits {
    /// The CPUs each server is pinned to, as `taskset -c` takes them.
    cpus: Option<String>,
    /// The CPUs the benchmark's own client runs on, if not the same.
    client_cpus: Option<String>,
    /// The memory each server may hold, in bytes.
    memory: Option<u64>,
    /// Whether the memory limit could be put in place, found out when the
    /// first server is started: how, or why not.
    enforced: OnceLock<Result<&'static str, String>>,
}

static LIMITS: OnceLock<Limits> = OnceLock::new();

/// The limits this run was asked for.
pub fn limits() -> &'static Limits {
    LIMITS.get().expect("the limits are read when the benchmark starts")
}

/// Reads `0-3,8,10-11`, the form the kernel and `taskset` share.
fn parse_cpu_list(text: &str) -> Option<Vec<usize>> {
    let mut cpus = Vec::new();
    for part in text.trim().split(',').filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((first, last)) => cpus.extend(first.trim().parse::<usize>().ok()?..=last.trim().parse::<usize>().ok()?),
            None => cpus.push(part.trim().parse().ok()?),
        }
    }
    (!cpus.is_empty()).then_some(cpus)
}

fn cpu_list(cpus: &[usize]) -> String {
    cpus.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(",")
}

/// The CPUs this process may run on.
fn allowed_cpus() -> Option<Vec<usize>> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    parse_cpu_list(status.lines().find_map(|l| l.strip_prefix("Cpus_allowed_list:"))?)
}

impl Limits {
    /// Reads the limits from the environment and moves this process off
    /// the servers' CPUs. Call it before any other thread is started:
    /// threads started later inherit where this one may run.
    pub fn init() -> Result<&'static Limits, String> {
        let mut limits = Limits { cpus: None, client_cpus: None, memory: None, enforced: OnceLock::new() };
        if let Ok(text) = std::env::var("BENCH_SERVER_CPUS") {
            let n: usize = text.trim().parse().ok().filter(|n| *n > 0).ok_or("BENCH_SERVER_CPUS must be a positive number")?;
            let allowed = allowed_cpus().ok_or("BENCH_SERVER_CPUS: cannot tell which CPUs this process may use (no /proc/self/status: not Linux?)")?;
            if n > allowed.len() {
                return Err(format!("BENCH_SERVER_CPUS={n}, but this process may use only {} CPUs", allowed.len()));
            }
            let (server, client) = allowed.split_at(n);
            let taskset = |args: &[&str]| Command::new("taskset").args(args).stdout(std::process::Stdio::null()).status().map(|s| s.success()).unwrap_or(false);
            if !taskset(&["-c", &cpu_list(server), "true"]) {
                return Err("BENCH_SERVER_CPUS needs `taskset` (util-linux), which is missing or was refused".into());
            }
            if !client.is_empty() {
                if !taskset(&["-a", "-cp", &cpu_list(client), &std::process::id().to_string()]) {
                    return Err("BENCH_SERVER_CPUS: could not move the benchmark's client off the servers' CPUs".into());
                }
                limits.client_cpus = Some(cpu_list(client));
            }
            limits.cpus = Some(cpu_list(server));
            eprintln!("limits: {}", limits.placed().unwrap_or_default());
        }
        if let Ok(text) = std::env::var("BENCH_SERVER_MEMORY_MB") {
            let mb: u64 = text.trim().parse().ok().filter(|n| *n > 0).ok_or("BENCH_SERVER_MEMORY_MB must be a positive number")?;
            limits.memory = Some(mb * 1024 * 1024);
        }
        Ok(LIMITS.get_or_init(|| limits))
    }

    /// A memory cgroup for one server, if a memory limit was asked for
    /// and can be put in place here.
    pub fn cage(&self, name: &str) -> Option<Cage> {
        let bytes = self.memory?;
        match Cage::new(name, bytes) {
            Ok(cage) => {
                let how = if cage.v2 { "a cgroup (v2, memory.max)" } else { "a cgroup (v1, memory.limit_in_bytes)" };
                self.enforced.get_or_init(|| Ok(how));
                Some(cage)
            }
            Err(why) => {
                if self.enforced.set(Err(why.clone())).is_ok() {
                    eprintln!("limits: the memory limit of {} MB is NOT enforced: {why}", bytes / 1_048_576);
                    eprintln!("limits: the report says how much each server held at most, to be read against it");
                }
                None
            }
        }
    }

    /// The command that runs `program` under the limits: on the servers'
    /// CPUs, and in `cage` if there is one. Arguments added to it are
    /// the program's.
    pub fn command(&self, cage: Option<&Cage>, program: &Path) -> Command {
        let mut argv: Vec<OsString> = Vec::new();
        if let Some(cage) = cage {
            // The shell moves itself into the cgroup and then becomes the
            // program, so that all the program ever holds is counted.
            argv.extend(["sh".into(), "-c".into(), r#"echo $$ > "$0/cgroup.procs" && exec "$@""#.into(), cage.dir.clone().into_os_string()]);
        }
        if let Some(cpus) = &self.cpus {
            argv.extend(["taskset".into(), "-c".into(), cpus.into()]);
        }
        argv.push(program.into());
        let mut command = Command::new(&argv[0]);
        command.args(&argv[1..]);
        command
    }

    /// Where the servers and the client run, if the servers are pinned.
    fn placed(&self) -> Option<String> {
        let cpus = self.cpus.as_ref()?;
        Some(match &self.client_cpus {
            Some(client) => format!("Each server on CPU {cpus}, the benchmark's client on CPU {client}."),
            None => format!("Each server on CPU {cpus}, which is every CPU the benchmark may use: its client shares them."),
        })
    }

    /// The conditions of the run, a line each, for the report.
    pub fn describe(&self) -> Vec<String> {
        let mut lines: Vec<String> = self.placed().into_iter().collect();
        if let Some(bytes) = self.memory {
            let mb = bytes / 1_048_576;
            lines.push(match self.enforced.get() {
                Some(Ok(how)) => format!("Each server ran under a memory limit of {mb} MB, enforced by {how}: file cache counts towards it, and a server that goes over is killed."),
                Some(Err(why)) => format!("A memory limit of {mb} MB was asked for and NOT enforced ({why}): read \"most memory held\" against it."),
                None => format!("A memory limit of {mb} MB was asked for, and no server was started under it."),
            });
        }
        lines
    }

    /// The memory limit, in bytes, if one was asked for.
    pub fn memory(&self) -> Option<u64> {
        self.memory
    }
}

/// A memory cgroup made for one server and removed after it.
pub struct Cage {
    dir: PathBuf,
    v2: bool,
}

/// Where a cgroup hierarchy is mounted, and this process's own group in
/// it. `controller` is `None` for the unified hierarchy (v2) and the
/// controller's name for a hierarchy of its own (v1).
fn own_cgroup(controller: Option<&str>) -> Option<(PathBuf, PathBuf)> {
    let mounts = std::fs::read_to_string("/proc/self/mounts").ok()?;
    let mount = mounts.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let (_, at, fstype, options) = (fields.next()?, fields.next()?, fields.next()?, fields.next()?);
        match controller {
            None => (fstype == "cgroup2").then_some(at),
            Some(name) => (fstype == "cgroup" && options.split(',').any(|o| o == name)).then_some(at),
        }
    })?;
    let groups = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let own = groups.lines().find_map(|line| {
        let mut fields = line.splitn(3, ':');
        let (_, controllers, path) = (fields.next()?, fields.next()?, fields.next()?);
        match controller {
            None => controllers.is_empty().then_some(path),
            Some(name) => controllers.split(',').any(|c| c == name).then_some(path),
        }
    })?;
    Some((PathBuf::from(mount), Path::new(mount).join(own.trim_start_matches('/'))))
}

impl Cage {
    fn new(name: &str, bytes: u64) -> Result<Cage, String> {
        let leaf = format!("database-bench-{}-{name}", std::process::id());
        let limit = bytes.to_string();
        let mut tried = Vec::new();
        // The unified hierarchy first: under this process's own group
        // or, where that group may not hand the memory controller down
        // (one that holds processes itself may not), at the top.
        if let Some((mount, own)) = own_cgroup(None) {
            let mut parents = vec![own];
            if parents[0] != mount {
                parents.push(mount);
            }
            for parent in parents {
                let dir = parent.join(&leaf);
                if std::fs::create_dir(&dir).is_err() {
                    tried.push(format!("cannot create {}", dir.display()));
                    continue;
                }
                if std::fs::write(dir.join("memory.max"), &limit).is_ok() {
                    // No swap, so that the limit is on memory alone.
                    let _ = std::fs::write(dir.join("memory.swap.max"), "0");
                    return Ok(Cage { dir, v2: true });
                }
                let _ = std::fs::remove_dir(&dir);
                tried.push(format!("no memory controller for groups under {}", parent.display()));
            }
        }
        // Then a memory hierarchy of its own (v1).
        if let Some((_, own)) = own_cgroup(Some("memory")) {
            let dir = own.join(&leaf);
            if std::fs::create_dir(&dir).is_ok() {
                if std::fs::write(dir.join("memory.limit_in_bytes"), &limit).is_ok() {
                    // Memory and swap together, where swap is counted.
                    let _ = std::fs::write(dir.join("memory.memsw.limit_in_bytes"), &limit);
                    return Ok(Cage { dir, v2: false });
                }
                let _ = std::fs::remove_dir(&dir);
                tried.push(format!("cannot set a limit in {}", dir.display()));
            } else {
                tried.push(format!("cannot create {}", dir.display()));
            }
        }
        if tried.is_empty() {
            tried.push("no cgroup hierarchy is mounted here".into());
        }
        Err(tried.join("; "))
    }

    /// How many times the kernel killed a process of the group for going
    /// over the limit.
    pub fn kills(&self) -> u64 {
        let file = if self.v2 { "memory.events" } else { "memory.oom_control" };
        std::fs::read_to_string(self.dir.join(file))
            .ok()
            .and_then(|text| text.lines().find_map(|l| l.strip_prefix("oom_kill ")?.trim().parse().ok()))
            .unwrap_or(0)
    }
}

impl Drop for Cage {
    fn drop(&mut self) {
        // The group can be removed once its last process is gone.
        for _ in 0..100 {
            if std::fs::remove_dir(&self.dir).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        eprintln!("limits: could not remove {}", self.dir.display());
    }
}

/// The most memory a server held while it was watched.
///
/// A server that is one process is asked for its own high-water mark
/// (`VmHWM`), which misses nothing. One that is several (Postgres) is
/// sampled twice a second, counting shared pages once (PSS): a peak
/// between two samples is missed.
pub struct Peak {
    root: u32,
    most: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    sampler: Option<std::thread::JoinHandle<()>>,
}

fn high_water(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let kb: u64 = status.lines().find_map(|l| l.strip_prefix("VmHWM:"))?.split_whitespace().next()?.parse().ok()?;
    Some(kb * 1024)
}

fn sample(root: u32) -> u64 {
    let pids = crate::pids_of(root);
    if pids.len() == 1 {
        high_water(root).unwrap_or(0)
    } else {
        crate::memory(root)
    }
}

impl Peak {
    pub fn watch(root: u32) -> Peak {
        let (most, stop) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicBool::new(false)));
        let (seen, stopped) = (most.clone(), stop.clone());
        let sampler = std::thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                seen.fetch_max(sample(root), Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(500));
            }
        });
        Peak { root, most, stop, sampler: Some(sampler) }
    }

    /// Stops watching. Call it while the server still runs.
    pub fn most(mut self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(sampler) = self.sampler.take() {
            let _ = sampler.join();
        }
        self.most.fetch_max(sample(self.root), Ordering::Relaxed);
        self.most.load(Ordering::Relaxed)
    }
}

impl Drop for Peak {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_list_of_cpus() {
        assert_eq!(parse_cpu_list("0-3"), Some(vec![0, 1, 2, 3]));
        assert_eq!(parse_cpu_list("\t0-1,8,10-11\n"), Some(vec![0, 1, 8, 10, 11]));
        assert_eq!(parse_cpu_list("5"), Some(vec![5]));
        assert_eq!(parse_cpu_list(""), None);
        assert_eq!(parse_cpu_list("a-b"), None);
        assert_eq!(cpu_list(&[0, 1, 8]), "0,1,8");
    }

    #[test]
    fn a_command_under_no_limits_is_the_program_itself() {
        let limits = Limits { cpus: None, client_cpus: None, memory: None, enforced: OnceLock::new() };
        let mut command = limits.command(None, Path::new("/bin/server"));
        command.arg("serve");
        assert_eq!(command.get_program(), "/bin/server");
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["serve"]);
    }

    #[test]
    fn a_command_under_limits_is_wrapped_and_keeps_its_arguments() {
        let limits = Limits { cpus: Some("0".into()), client_cpus: Some("1,2,3".into()), memory: Some(1), enforced: OnceLock::new() };
        assert_eq!(limits.describe()[0], "Each server on CPU 0, the benchmark's client on CPU 1,2,3.");
        let cage = Cage { dir: PathBuf::from("/sys/fs/cgroup/x"), v2: true };
        let mut command = limits.command(Some(&cage), Path::new("/bin/server"));
        command.arg("serve");
        assert_eq!(command.get_program(), "sh");
        let args: Vec<_> = command.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(&args[2..], ["/sys/fs/cgroup/x", "taskset", "-c", "0", "/bin/server", "serve"]);
        // Nothing was made, so there is nothing to remove.
        std::mem::forget(cage);
    }
}
