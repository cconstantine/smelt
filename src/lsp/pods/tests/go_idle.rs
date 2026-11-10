//! The pod script run on this host (SME-456): the go watchdog, and
//! where the install's curl reads its config. A fake `go` on `PATH`, the
//! script's files in a directory of the test's own, and a 3 s bound for
//! 120 s unless a test says otherwise.
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

/// The directory a run named `name` keeps its files in; `$HOME` is its
/// `home`.
fn test_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("sme-456-go-idle-{name}-{}", std::process::id()))
}

/// How to run the pod script on this host.
struct Run<'a> {
    name: &'a str,
    /// The fake `go`'s shell script.
    go_body: &'a str,
    install: &'a str,
    env: &'a [(&'a str, &'a str)],
    /// Files written under `$HOME` before the script starts.
    home_files: &'a [(&'a str, &'a str)],
    /// How long to wait for the install's record.
    wait: Duration,
    /// `Traffic` on this host's network while it runs.
    traffic: bool,
    go_idle: Duration,
}

impl Default for Run<'_> {
    fn default() -> Self {
        Run {
            name: "",
            go_body: "exit 0",
            install: "go install x@v1",
            env: &[],
            home_files: &[],
            wait: Duration::from_secs(15),
            traffic: false,
            go_idle: Duration::from_secs(3),
        }
    }
}

/// Runs the pod script for `install`, with `go` on `PATH` the shell
/// script `go_body` and `env` set, and waits up to `wait` for its record.
fn run_pod_script(name: &str, go_body: &str, install: &str, env: &[(&str, &str)], wait: Duration) -> Ran {
    run(Run { name, go_body, install, env, wait, ..Default::default() })
}

/// Runs the pod script as `run` says.
fn run(run: Run) -> Ran {
    let Run { name, go_body, install, env, home_files, wait, traffic, go_idle } = run;
    let dir = test_dir(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("bin")).expect("the test's directory");
    for (path, content) in home_files {
        let path = dir.join("home").join(path);
        std::fs::create_dir_all(path.parent().expect("a file in a directory")).expect("its directory");
        std::fs::write(&path, content).expect("a file in $HOME");
    }
    let go = dir.join("bin/go");
    std::fs::write(&go, format!("#!/bin/sh\n{go_body}\n")).expect("the fake go");
    std::fs::set_permissions(&go, std::os::unix::fs::PermissionsExt::from_mode(0o755)).expect("chmod");
    let _traffic = traffic.then(Traffic::start);
    let tmp = dir.to_str().expect("a UTF-8 path");
    let at = ScriptAt { tmp, go_idle };
    let script = pod_script_at(install, &at);
    let path = format!("{}:{}", dir.join("bin").display(), std::env::var("PATH").unwrap_or_default());
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(&script)
        .env("HOME", dir.join("home"))
        .env("PATH", path)
        .env_remove("GOFLAGS")
        .env_remove("CURL_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .envs(env.iter().copied())
        .process_group(0)
        .spawn()
        .expect("sh runs");
    let started = Instant::now();
    let record = dir.join("install.rc");
    let rc = read_record(&record, wait);
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

/// The install's record, or `None` if none came within `wait`.
fn read_record(record: &std::path::Path, wait: Duration) -> Option<String> {
    let started = Instant::now();
    loop {
        if let Ok(rc) = std::fs::read_to_string(record) {
            break Some(rc.trim().to_string());
        }
        if started.elapsed() > wait {
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
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
    let ran = run(Run { name: "traffic", go_body: go, traffic: true, ..Default::default() });
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

/// (b) An install whose log keeps growing is never stopped, however long
/// go takes past the bound. The log is written beside go, outside its
/// process tree, and go itself only waits on one `sleep`: so go reads
/// nothing until it ends (a child's reads join its parent's `rchar` only
/// when it's reaped), and only log growth can be the progress that keeps
/// it running (SME-456 review 1).
#[test]
fn test_a_go_that_keeps_logging_is_left_alone() {
    let writer = "(i=0; while [ $i -lt 8 ]; do echo step $i; sleep 1; i=$((i+1)); done) &";
    // go isn't the install's last command, so the shell doesn't exec it
    // and the writer isn't its child.
    let install = format!("{writer} go install x@v1; r=$?; wait; exit $r");
    let ran = run_pod_script("logging", "sleep 8 & wait", &install, &[], Duration::from_secs(20));
    assert_eq!(ran.rc.as_deref(), Some("0"), "{:?}", ran.log);
    assert!(!ran.log.contains("smelt: go"), "{:?}", ran.log);
    assert!(ran.watchdogs.is_empty(), "the watchdog outlived the install: {:?}", ran.watchdogs);
}

/// Idle time adds up across checks and resets on progress (SME-456
/// review 1): with a 10 s bound, a go that logs a line every 7 s is quiet
/// for at most one 5 s check in a row, so it's never stopped, though
/// over its 56 s it has quiet checks adding up to well past 10 s.
#[test]
fn test_idle_resets_on_progress_and_adds_up_across_checks() {
    let go = "i=0; while [ $i -lt 8 ]; do echo step $i; sleep 7; i=$((i+1)); done";
    let ran = run(Run {
        name: "every7",
        go_body: go,
        wait: Duration::from_secs(80),
        go_idle: Duration::from_secs(10),
        ..Default::default()
    });
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

/// SME-476 R-a: an empty record file is the redirect before its write —
/// the record write is two steps, create/truncate then echo — so it is
/// not the record. `read_record` must end `wait` with `None` — bounded,
/// never stuck on an empty file — and never take the empty read (the
/// flake's recorded `Some("")`).
#[test]
fn test_an_empty_record_is_never_the_record() {
    let dir = test_dir("rc-empty");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the test's directory");
    let record = dir.join("install.rc");
    std::fs::write(&record, "").expect("an empty record file");
    let waiter = record.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(read_record(&waiter, Duration::from_millis(300)));
    });
    let rc = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("read_record ended its wait on an empty record within its bound");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(rc, None, "an empty record was taken as the record");
}

/// SME-476 R-b: the flake's exact shape, forced. A writer performs the
/// script's two steps — the redirect creates the file, the write follows
/// 300 ms later (`{ sleep 0.3; echo 0; } > …`, the pair `echo $rc > {rc}`
/// does µs apart). Caught at 0 bytes, `read_record` must keep polling
/// within `wait` and return the bytes: `Some("0")`.
#[test]
fn test_a_record_caught_empty_is_read_again() {
    let dir = test_dir("rc-caught-empty");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the test's directory");
    let record = dir.join("install.rc");
    let mut writer = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{{ sleep 0.3; echo 0; }} > '{}'", record.display()))
        .spawn()
        .expect("sh runs");
    // Catch the file between the redirect's create and the write's bytes.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(len) = std::fs::metadata(&record).map(|m| m.len()) {
            assert_eq!(len, 0, "the write landed before the create was caught");
            break;
        }
        assert!(Instant::now() < deadline, "the redirect never created the record");
        std::thread::sleep(Duration::from_millis(1));
    }
    let rc = read_record(&record, Duration::from_secs(5));
    let _ = writer.wait();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(rc.as_deref(), Some("0"), "an empty record was taken as the record");
}

/// The config's own `GOFLAGS` are kept, with `-x` after them (the
/// Driver's Q3).
#[test]
fn test_a_go_install_keeps_the_configs_goflags() {
    let ran = run_pod_script("goflags", "echo \"flags: $GOFLAGS\"", "go install x@v1", &[("GOFLAGS", "-mod=mod")], Duration::from_secs(15));
    assert_eq!(ran.rc.as_deref(), Some("0"), "{:?}", ran.log);
    assert!(ran.log.contains("flags: -mod=mod -x\n"), "{:?}", ran.log);
}

/// The `CURL_HOME` the install's curl ran with, given `home_files` and
/// `env`: the fake go prints it.
fn curl_home_with(name: &str, home_files: &[(&str, &str)], env: &[(&str, &str)]) -> String {
    let go = "echo \"CURL_HOME=[$CURL_HOME]\"";
    let ran = run(Run { name, go_body: go, home_files, env, ..Default::default() });
    assert_eq!(ran.rc.as_deref(), Some("0"), "{:?}", ran.log);
    ran.log.lines().find_map(|l| l.strip_prefix("CURL_HOME=[")?.strip_suffix(']').map(str::to_string)).expect("printed")
}

/// SME-456 review 1 (Low 1): curl reads only the first `.curlrc` it finds,
/// and `CURL_HOME`'s comes first, so smelt's would hide a user's own. When
/// the install starts with one in `$HOME` or `$XDG_CONFIG_HOME` (or
/// `~/.config`), `CURL_HOME` is left unset and curl reads the user's;
/// otherwise the install's curl gets smelt's bounds.
#[test]
fn test_a_users_own_curlrc_is_honoured() {
    let smelt = curl_home_with("curl-none", &[], &[]);
    assert!(smelt.ends_with("/smelt-curl"), "no CURL_HOME without a .curlrc of the user's: {smelt:?}");
    let rc = "proxy = http://example.invalid:3128\n";
    assert_eq!(curl_home_with("curl-home", &[(".curlrc", rc)], &[]), "", "~/.curlrc hidden");
    assert_eq!(curl_home_with("curl-config", &[(".config/curlrc", rc)], &[]), "", "~/.config/curlrc hidden");
    let xdg = test_dir("curl-xdg").join("home/xdg");
    let xdg = xdg.to_str().expect("a UTF-8 path");
    assert_eq!(curl_home_with("curl-xdg", &[("xdg/curlrc", rc)], &[("XDG_CONFIG_HOME", xdg)]), "", "$XDG_CONFIG_HOME/curlrc hidden");
    assert_eq!(curl_home_with("curl-own", &[], &[("CURL_HOME", "/elsewhere")]), "/elsewhere", "the config's CURL_HOME replaced");
}
