//! Launches `chrome-headless-shell` so that it can't outlive the process
//! that started it, then connects chromiumoxide to it. A Chrome that dies
//! before it is ready reports the last lines of its own stderr along with
//! the bare fact that it died — without that, a launch that failed had no
//! explanation anywhere at all, in the error or in the logs.
//!
//! `chromiumoxide::Browser::launch` only kills Chrome when its `Browser` is
//! dropped. The shared browser lives in a process-wide static that is never
//! dropped, so every server restart and test run used to leave a whole
//! Chrome running. Here Chrome is started with `PR_SET_PDEATHSIG`, so Linux
//! kills it as soon as its parent goes — however that happens, SIGKILL
//! included. Each launch also gets its own profile directory, rather than
//! chromiumoxide's single shared `/tmp/chromiumoxide-runner`, which let
//! concurrent browsers (and runs) share cookies and service workers.

use std::collections::VecDeque;
use std::io::BufRead;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chromiumoxide::Browser;
use futures_util::StreamExt;

/// Where `scripts/browser-check/setup.sh` puts Chrome and its libraries,
/// relative to its cache directory (see `cache_dir`).
const CHROME_BINARY: &str = "chrome/chrome-headless-shell-linux64/chrome-headless-shell";
const LIB_DIR: &str = "libs/usr/lib/x86_64-linux-gnu";
/// The cache directory when `BROWSER_CHECK_CACHE` isn't set, relative to
/// the checkout smelt was built from.
const DEFAULT_CACHE_DIR: &str = ".browser-check-cache";
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(20);
const PROFILE_PREFIX: &str = "smelt-chrome-";
/// How much of Chrome's stderr a failed launch reports: the last
/// `STDERR_TAIL_LINES` lines, and at most `STDERR_TAIL_CHARS` characters
/// of them. Chrome is chatty even when well, so the tail stays bounded
/// even though it's only ever shown on failure.
const STDERR_TAIL_LINES: usize = 40;
const STDERR_TAIL_CHARS: usize = 4_000;

/// chromiumoxide's own default flags, minus `--disable-popup-blocking`:
/// nothing here should open popups.
const BASE_ARGS: [&str; 24] = [
    "--disable-background-networking",
    "--enable-features=NetworkService,NetworkServiceInProcess",
    "--disable-background-timer-throttling",
    "--disable-backgrounding-occluded-windows",
    "--disable-breakpad",
    "--disable-client-side-phishing-detection",
    "--disable-component-extensions-with-background-pages",
    "--disable-default-apps",
    "--disable-dev-shm-usage",
    "--disable-extensions",
    "--disable-features=TranslateUI",
    "--disable-hang-monitor",
    "--disable-ipc-flooding-protection",
    "--disable-prompt-on-repost",
    "--disable-renderer-backgrounding",
    "--disable-sync",
    "--force-color-profile=srgb",
    "--metrics-recording-only",
    "--no-first-run",
    "--enable-automation",
    "--password-store=basic",
    "--use-mock-keychain",
    "--enable-blink-features=IdleDetection",
    "--lang=en_US",
];

/// The directory holding Chrome and its libraries: `setting`
/// (`BROWSER_CHECK_CACHE`, the same variable that moves where setup.sh
/// downloads), or `.browser-check-cache` in `build_root`, the checkout smelt
/// was built from, when it's unset or empty.
fn cache_dir(setting: Option<&str>, build_root: &Path) -> PathBuf {
    match setting.filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => build_root.join(DEFAULT_CACHE_DIR),
    }
}

/// Launches Chrome with `extra_args` on top of the base headless setup and
/// returns a connected `Browser`, with its CDP handler already being driven.
pub async fn launch(extra_args: &[String]) -> Result<Browser, String> {
    let cache = cache_dir(
        std::env::var("BROWSER_CHECK_CACHE").ok().as_deref(),
        Path::new(env!("CARGO_MANIFEST_DIR")),
    );
    launch_in(&cache, extra_args).await
}

/// `launch`, with Chrome taken from the cache directory `cache`.
async fn launch_in(cache: &Path, extra_args: &[String]) -> Result<Browser, String> {
    let chrome_binary = cache.join(CHROME_BINARY);
    if !chrome_binary.is_file() {
        return Err(format!(
            "chrome-headless-shell not found at {} — run scripts/browser-check/setup.sh first, \
             or set BROWSER_CHECK_CACHE to the directory it downloaded into",
            chrome_binary.display()
        ));
    }
    let lib_dir = cache.join(LIB_DIR);
    let existing = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
    let ld_library_path = format!("{}:{}/dri:{existing}", lib_dir.display(), lib_dir.display());

    remove_stale_profiles(&std::env::temp_dir());
    // One directory per launch: Chrome locks its profile, and one process
    // can run more than one browser (tests do).
    static LAUNCHES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let launch = LAUNCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let profile = std::env::temp_dir().join(format!(
        "{PROFILE_PREFIX}{}-{launch}",
        std::process::id()
    ));
    let mut args: Vec<String> = BASE_ARGS.iter().map(|a| a.to_string()).collect();
    args.extend([
        "--headless".to_string(),
        "--hide-scrollbars".to_string(),
        "--mute-audio".to_string(),
        "--no-sandbox".to_string(),
        "--disable-setuid-sandbox".to_string(),
        "--disable-gpu".to_string(),
        "--window-size=1400,900".to_string(),
        "--remote-debugging-port=0".to_string(),
        format!("--user-data-dir={}", profile.display()),
    ]);
    args.extend(extra_args.iter().cloned());

    let ws_url = spawn_owned(&chrome_binary, &args, &ld_library_path).await?;
    let (browser, mut handler) = Browser::connect(ws_url)
        .await
        .map_err(|e| format!("failed to connect to chrome-headless-shell: {e}"))?;
    // chromiumoxide needs this driven continuously to process the CDP
    // connection at all (command responses, events).
    tokio::spawn(async move { while handler.next().await.is_some() {} });
    Ok(browser)
}

enum LaunchUpdate {
    Spawned(u32),
    Ready(String),
    Failed(String),
}

/// Starts Chrome and waits for its DevTools URL. `PR_SET_PDEATHSIG` fires
/// when the *thread* that forked the child exits, not only the process, so
/// Chrome is spawned from a dedicated thread that lives exactly as long as
/// Chrome does (it drains Chrome's stderr until Chrome exits).
async fn spawn_owned(binary: &Path, args: &[String], ld_library_path: &str) -> Result<String, String> {
    let mut command = Command::new(binary);
    command
        .args(args)
        .env("LD_LIBRARY_PATH", ld_library_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let smelt = nix::unistd::getpid();
    // SAFETY: runs in the forked child before exec; prctl and getppid are
    // both async-signal-safe and nothing here allocates or takes locks.
    unsafe {
        command.pre_exec(move || {
            nix::sys::prctl::set_pdeathsig(nix::sys::signal::Signal::SIGKILL)
                .map_err(std::io::Error::from)?;
            // If smelt died before the line above took effect, no signal is
            // coming — don't start an orphan.
            if nix::unistd::getppid() != smelt {
                return Err(std::io::Error::other("parent exited during launch"));
            }
            Ok(())
        });
    }

    let (updates, mut update_rx) = tokio::sync::mpsc::unbounded_channel();
    // Chrome's stderr outlives the launch attempt (the owner thread drains
    // it for good), but the last few lines of it are the only record of why
    // a launch failed, so they're kept — bounded, because a browser that
    // becomes ready keeps logging for the life of the server.
    let stderr_tail: Arc<Mutex<VecDeque<String>>> = Arc::default();
    let tail_for_owner = Arc::clone(&stderr_tail);
    std::thread::Builder::new()
        .name("chrome-owner".to_string())
        .spawn(move || {
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(e) => {
                    let _ = updates.send(LaunchUpdate::Failed(format!(
                        "chrome-headless-shell failed to start: {e}"
                    )));
                    return;
                }
            };
            let _ = updates.send(LaunchUpdate::Spawned(child.id()));
            #[expect(clippy::expect_used, reason = "Chrome was spawned with stderr piped, and this is its one take")]
            let stderr = child.stderr.take().expect("stderr is piped");
            for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
                note_line(&tail_for_owner, &line);
                if let Some(url) = line.strip_prefix("DevTools listening on ") {
                    let _ = updates.send(LaunchUpdate::Ready(url.trim().to_string()));
                }
            }
            // stderr closed: Chrome has exited.
            let _ = child.wait();
            let _ = updates.send(LaunchUpdate::Failed(format!(
                "chrome-headless-shell exited before it was ready{}",
                take_tail(&tail_for_owner)
            )));
        })
        .map_err(|e| format!("failed to start the chrome-owner thread: {e}"))?;

    let mut pid = None;
    let outcome = tokio::time::timeout(LAUNCH_TIMEOUT, async {
        while let Some(update) = update_rx.recv().await {
            match update {
                LaunchUpdate::Spawned(child) => pid = Some(child),
                LaunchUpdate::Ready(url) => return Ok(url),
                LaunchUpdate::Failed(e) => return Err(e),
            }
        }
        Err("chrome-headless-shell launch ended without a result".to_string())
    })
    .await
    .unwrap_or_else(|_| Err("timed out waiting for chrome-headless-shell to start".to_string()));
    if outcome.is_err()
        && let Some(pid) = pid
    {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    // A Chrome that died on the way reported its own stderr already (the
    // Failed update carries the tail); a launch that timed out, or ended
    // without a result, gets whatever it had printed appended here. Either
    // way the reason reaches both the error and the server's log.
    let outcome = outcome.map_err(|e| {
        let e = format!("{e}{}", take_tail(&stderr_tail));
        tracing::error!("{e}");
        e
    });
    outcome
}

/// Records one line of Chrome's stderr in the launch's bounded tail.
fn note_line(tail: &Mutex<VecDeque<String>>, line: &str) {
    if line.trim().is_empty() {
        return;
    }
    // A poisoned lock would only mean some earlier holder panicked; the
    // lines already in the deque are still valid.
    let mut tail = tail.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    tail.push_back(line.to_string());
    while tail.len() > STDERR_TAIL_LINES {
        tail.pop_front();
    }
}

/// The launch's tail as a suffix for an error message — empty if Chrome
/// printed nothing. Draining, so one failure's lines aren't repeated in
/// the error of the next launch attempt.
fn take_tail(tail: &Mutex<VecDeque<String>>) -> String {
    let mut tail = tail.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if tail.is_empty() {
        return String::new();
    }
    let text = tail.drain(..).collect::<Vec<_>>().join("\n");
    // Keep the *end* of what fits: the last thing Chrome said before dying
    // matters more than the first.
    let text = match text.chars().count() > STDERR_TAIL_CHARS {
        true => text
            .chars()
            .rev()
            .take(STDERR_TAIL_CHARS)
            .collect::<String>()
            .chars()
            .rev()
            .collect(),
        false => text,
    };
    format!("\nchrome's last output was:\n{text}")
}

/// Deletes profile directories left by smelt processes that are no longer
/// running (a process killed outright never gets to clean up its own).
fn remove_stale_profiles(temp_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(temp_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix(PROFILE_PREFIX))
            .and_then(|rest| rest.split('-').next()?.parse::<u32>().ok())
        else {
            continue;
        };
        if pid != std::process::id() && !Path::new(&format!("/proc/{pid}")).exists() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_a_set_cache_dir_is_used_as_given() {
        assert_eq!(
            cache_dir(Some("/opt/smelt/chrome-cache"), Path::new("/build/smelt")),
            PathBuf::from("/opt/smelt/chrome-cache")
        );
    }

    #[test]
    fn test_an_unset_or_empty_cache_dir_is_the_build_checkouts() {
        let default = PathBuf::from("/build/smelt/.browser-check-cache");
        assert_eq!(cache_dir(None, Path::new("/build/smelt")), default);
        assert_eq!(cache_dir(Some(""), Path::new("/build/smelt")), default);
    }

    #[tokio::test]
    async fn test_a_cache_dir_without_chrome_says_where_it_looked() {
        let empty = std::env::temp_dir().join(format!("smelt-no-chrome-{}", std::process::id()));
        std::fs::create_dir_all(&empty).expect("create temp dir");
        let error = launch_in(&empty, &[]).await.err().expect("no Chrome there");
        std::fs::remove_dir_all(&empty).expect("remove temp dir");
        assert!(error.contains(&empty.display().to_string()), "{error}");
        assert!(error.contains("BROWSER_CHECK_CACHE"), "{error}");
    }

    #[tokio::test]
    async fn test_a_chrome_that_dies_says_why_it_died() {
        use std::os::unix::fs::PermissionsExt;
        // A fake Chrome at the path `launch_in` expects: it prints a
        // plausible death rattle on stderr, then exits. The old code read
        // those lines and threw them away, and the error said nothing more
        // than "exited before it was ready".
        let cache = std::env::temp_dir().join(format!("smelt-chrome-dies-{}", std::process::id()));
        let bin = cache.join(CHROME_BINARY);
        std::fs::create_dir_all(bin.parent().expect("the fake chrome has a parent"))
            .expect("create the fake chrome's directory");
        std::fs::write(
            &bin,
            "#!/bin/sh\necho launching 1>&2\necho 'FATAL: cannot allocate shared memory' 1>&2\nexit 1\n",
        )
        .expect("write the fake chrome");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
            .expect("make the fake chrome runnable");

        let error = launch_in(&cache, &[])
            .await
            .err()
            .expect("a chrome that exits is never ready");
        std::fs::remove_dir_all(&cache).expect("remove the fake chrome's directory");

        assert!(error.contains("exited before it was ready"), "{error}");
        assert!(error.contains("cannot allocate shared memory"), "{error}");
    }

    #[test]
    fn test_remove_stale_profiles_keeps_live_processes_and_unrelated_dirs() {
        let temp = std::env::temp_dir().join(format!("smelt-profile-test-{}", std::process::id()));
        std::fs::create_dir_all(&temp).unwrap();
        // pid_max on Linux is at most 2^22, so this can't be a live process.
        let dead = temp.join(format!("{PROFILE_PREFIX}99999999-0"));
        let live = temp.join(format!("{PROFILE_PREFIX}{}-3", std::process::id()));
        let unrelated = temp.join("something-else");
        for dir in [&dead, &live, &unrelated] {
            std::fs::create_dir_all(dir).unwrap();
        }
        remove_stale_profiles(&temp);
        assert!(!dead.exists(), "a dead process's profile should be removed");
        assert!(live.exists(), "this process's own profile must be kept");
        assert!(unrelated.exists(), "other directories must be left alone");
        std::fs::remove_dir_all(&temp).unwrap();
    }
}
