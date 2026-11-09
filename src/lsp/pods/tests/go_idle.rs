//! The go watchdog (SME-456), run in the pod script on this host: a fake
//! `go` on `PATH`, the script's files in a directory of the test's own,
//! and a 3 s bound for 120 s.
//! Needs `sh` and `/proc`, as the test container and CI have; no cluster.

use super::super::script::{ScriptAt, pod_script_at};
use std::os::unix::process::CommandExt;
use std::time::{Duration, Instant};

/// What the pod script did with one install.
struct Ran {
    /// The install's record, or `None` if none came within the wait.
    rc: Option<String>,
    log: String,
    took: Duration,
    /// Watchdog processes still running once the record was written.
    watchdogs: Vec<String>,
}

/// The command lines of the processes running `script` (`sh <script> …`,
/// the watchdog and its subshells; not the pod script, whose own command
/// line holds the watchdog's path too).
fn running(script: &str) -> Vec<String> {
    let running = format!("sh {script} ");
    let Ok(dirs) = std::fs::read_dir("/proc") else { return vec![] };
    dirs.flatten()
        .filter(|d| d.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|d| std::fs::read(d.path().join("cmdline")).ok())
        .map(|c| String::from_utf8_lossy(&c).replace('\0', " "))
        .filter(|c| c.starts_with(&running))
        .collect()
}

/// Traffic the install never asked for, while it runs: UDP to a socket of
/// the test's own on loopback, about 50 KB/s, as a busy node's bridge
/// sends every pod on it (SME-456's full-gate red: 1–2.5 KB/s on `eth0`).
struct Traffic(std::sync::Arc<std::sync::atomic::AtomicBool>, Option<std::thread::JoinHandle<()>>);

impl Traffic {
    fn start() -> Traffic {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let receiver = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind a receiver");
        let to = receiver.local_addr().expect("its address");
        let sender = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind a sender");
        let stopping = stop.clone();
        let thread = std::thread::spawn(move || {
            let _receiver = receiver;
            while !stopping.load(std::sync::atomic::Ordering::SeqCst) {
                let _ = sender.send_to(&[0u8; 1000], to);
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        Traffic(stop, Some(thread))
    }
}

impl Drop for Traffic {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread) = self.1.take() {
            let _ = thread.join();
        }
    }
}

/// Runs the pod script for `install`, with `go` on `PATH` the shell
/// script `go_body` and `env` set, and waits up to `wait` for its record.
fn run_pod_script(name: &str, go_body: &str, install: &str, env: &[(&str, &str)], wait: Duration) -> Ran {
    run_pod_script_with(name, go_body, install, env, wait, false)
}

/// `run_pod_script`, with `Traffic` on this host's network while it runs
/// when `traffic`.
fn run_pod_script_with(
    name: &str,
    go_body: &str,
    install: &str,
    env: &[(&str, &str)],
    wait: Duration,
    traffic: bool,
) -> Ran {
    let dir = std::env::temp_dir().join(format!("sme-456-go-idle-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("bin")).expect("the test's directory");
    let go = dir.join("bin/go");
    std::fs::write(&go, format!("#!/bin/sh\n{go_body}\n")).expect("the fake go");
    std::fs::set_permissions(&go, std::os::unix::fs::PermissionsExt::from_mode(0o755)).expect("chmod");
    let _traffic = traffic.then(Traffic::start);
    let tmp = dir.to_str().expect("a UTF-8 path");
    let at = ScriptAt { tmp, go_idle: Duration::from_secs(3) };
    let script = pod_script_at(install, &at);
    let path = format!("{}:{}", dir.join("bin").display(), std::env::var("PATH").unwrap_or_default());
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(&script)
        .env("HOME", dir.join("home"))
        .env("PATH", path)
        .env_remove("GOFLAGS")
        .envs(env.iter().copied())
        .process_group(0)
        .spawn()
        .expect("sh runs");
    let started = Instant::now();
    let record = dir.join("install.rc");
    let rc = loop {
        if let Ok(rc) = std::fs::read_to_string(&record) {
            break Some(rc.trim().to_string());
        }
        if started.elapsed() > wait {
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let took = started.elapsed();
    let watchdogs = running(&format!("{tmp}/smelt-go-idle.sh"));
    // The script idles once its record is written; end it and anything
    // left of the install, the fake go's sleeps included.
    let _ = std::process::Command::new("kill").args(["-KILL", "--", &format!("-{}", child.id())]).status();
    let _ = child.wait();
    let log = std::fs::read_to_string(dir.join("install.log")).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);
    Ran { rc, log, took, watchdogs }
}

const STOPPED: &str = "smelt: go wrote nothing and received under 1 KB/s for 3 s, so it was stopped";

/// (a) A go that hangs silently, as on a module proxy that never answers,
/// is stopped and says why, and the install fails with its own status.
/// The install writes after it's stopped (here the fake go itself), and
/// the watchdog's line survives that: the log is opened for append.
#[test]
fn test_a_silent_go_is_stopped_and_says_why() {
    let go = "trap 'kill $! 2>/dev/null; echo go: interrupted; exit 143' TERM; sleep 600 & wait";
    let ran = run_pod_script("silent", go, "go install x@v1", &[], Duration::from_secs(15));
    let rc = ran.rc.unwrap_or_else(|| panic!("no record within 15 s; the log: {:?}", ran.log));
    assert_ne!(rc, "0", "{}", ran.log);
    assert!(ran.log.ends_with(&format!("{STOPPED}\ngo: interrupted\n")), "{:?}", ran.log);
    assert!(ran.watchdogs.is_empty(), "the watchdog outlived the install: {:?}", ran.watchdogs);
    eprintln!("stopped after {} s, rc {rc}", ran.took.as_secs());
}

/// (d) SME-456's full-gate red: a silent go is stopped even while the pod
/// receives traffic that isn't the install's (on a busy node, the
/// bridge's broadcasts reach every pod). Only what go itself reads
/// counts.
#[test]
fn test_a_silent_go_is_stopped_while_other_traffic_arrives() {
    let go = "trap 'kill $! 2>/dev/null; exit 143' TERM; sleep 600 & wait";
    let ran = run_pod_script_with("traffic", go, "go install x@v1", &[], Duration::from_secs(15), true);
    let rc = ran.rc.unwrap_or_else(|| panic!("no record within 15 s; the log: {:?}", ran.log));
    assert_ne!(rc, "0", "{}", ran.log);
    assert!(ran.log.trim_end().ends_with(STOPPED), "{:?}", ran.log);
    assert!(ran.watchdogs.is_empty(), "the watchdog outlived the install: {:?}", ran.watchdogs);
}

/// (e) A go that writes nothing but keeps reading (a download between
/// `-x` lines) is never stopped: 4 KB a second is over the bound's 1 KB.
#[test]
fn test_a_silent_go_that_keeps_reading_is_left_alone() {
    let go = "printf '%4096s\\n' x > \"$HOME/line\"; i=0; while [ $i -lt 10 ]; do read -r l < \"$HOME/line\"; sleep 1; i=$((i+1)); done";
    let ran = run_pod_script("reading", go, "go install x@v1", &[], Duration::from_secs(25));
    assert_eq!(ran.rc.as_deref(), Some("0"), "{:?}", ran.log);
    assert!(!ran.log.contains("smelt: go"), "{:?}", ran.log);
    assert!(ran.watchdogs.is_empty(), "the watchdog outlived the install: {:?}", ran.watchdogs);
}

/// (b) A go that keeps writing to the log is never stopped, however long
/// it takes past the bound. It logs with builtins alone, one `sleep` for
/// the whole 8 s: a child that exits adds its reads to its parent's
/// `rchar`, so a `sleep` per line would read as progress by itself and
/// the test wouldn't show that log growth counts.
#[test]
fn test_a_go_that_keeps_logging_is_left_alone() {
    let go = "sleep 8 & s=$!; i=0; while kill -0 $s 2>/dev/null; do i=$((i+1)); \
              [ $((i % 50000)) -ne 0 ] || echo step $i; done";
    let ran = run_pod_script("logging", go, "go install x@v1", &[], Duration::from_secs(20));
    assert_eq!(ran.rc.as_deref(), Some("0"), "{:?}", ran.log);
    assert!(!ran.log.contains("smelt: go"), "{:?}", ran.log);
    assert!(ran.watchdogs.is_empty(), "the watchdog outlived the install: {:?}", ran.watchdogs);
}

/// (c) A silent install that isn't go (a compile, a sleep) is never
/// stopped: the watchdog watches go alone (the Driver's Q4).
#[test]
fn test_a_silent_install_that_isnt_go_is_left_alone() {
    let ran = run_pod_script("notgo", "exit 0", "sleep 8", &[], Duration::from_secs(20));
    assert_eq!(ran.rc.as_deref(), Some("0"), "{:?}", ran.log);
    assert!(!ran.log.contains("smelt: go"), "{:?}", ran.log);
    assert!(ran.watchdogs.is_empty(), "the watchdog outlived the install: {:?}", ran.watchdogs);
}

/// The config's own `GOFLAGS` are kept, with `-x` after them (the
/// Driver's Q3).
#[test]
fn test_a_go_install_keeps_the_configs_goflags() {
    let ran = run_pod_script("goflags", "echo \"flags: $GOFLAGS\"", "go install x@v1", &[("GOFLAGS", "-mod=mod")], Duration::from_secs(15));
    assert_eq!(ran.rc.as_deref(), Some("0"), "{:?}", ran.log);
    assert!(ran.log.contains("flags: -mod=mod -x\n"), "{:?}", ran.log);
}
