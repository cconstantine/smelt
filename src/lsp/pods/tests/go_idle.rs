//! The go watchdog (SME-456), run in the pod script on this host: a fake
//! `go` on `PATH`, the script's files in a directory of the test's own,
//! a received-bytes table that never grows, and a 3 s bound for 120 s.
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

/// Runs the pod script for `install`, with `go` on `PATH` the shell
/// script `go_body` and `env` set, and waits up to `wait` for its record.
fn run_pod_script(name: &str, go_body: &str, install: &str, env: &[(&str, &str)], wait: Duration) -> Ran {
    let dir = std::env::temp_dir().join(format!("sme-456-go-idle-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("bin")).expect("the test's directory");
    let go = dir.join("bin/go");
    std::fs::write(&go, format!("#!/bin/sh\n{go_body}\n")).expect("the fake go");
    std::fs::set_permissions(&go, std::os::unix::fs::PermissionsExt::from_mode(0o755)).expect("chmod");
    // Never grows: only the log counts as progress.
    let net_dev = dir.join("net-dev");
    std::fs::write(&net_dev, "Inter-|   Receive\n face |bytes\n    lo:    1000 10 0 0 0 0 0 0 1000 10 0 0 0 0 0 0\n")
        .expect("net-dev");
    let tmp = dir.to_str().expect("a UTF-8 path");
    let at = ScriptAt { tmp, net_dev: net_dev.to_str().expect("a UTF-8 path"), go_idle: Duration::from_secs(3) };
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

/// (b) A go that keeps writing to the log is never stopped, however long
/// it takes past the bound.
#[test]
fn test_a_go_that_keeps_logging_is_left_alone() {
    let go = "i=0; while [ $i -lt 8 ]; do echo step $i; sleep 1; i=$((i+1)); done";
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
