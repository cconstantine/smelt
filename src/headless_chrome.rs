//! Launches `chrome-headless-shell` so that it can't outlive the process
//! that started it, then connects chromiumoxide to it.
//!
//! `chromiumoxide::Browser::launch` only kills Chrome when its `Browser` is
//! dropped. The shared browser lives in a process-wide static that is never
//! dropped, so every server restart and test run used to leave a whole
//! Chrome running. Here Chrome is started with `PR_SET_PDEATHSIG`, so Linux
//! kills it as soon as its parent goes — however that happens, SIGKILL
//! included. Each launch also gets its own profile directory, rather than
//! chromiumoxide's single shared `/tmp/chromiumoxide-runner`, which let
//! concurrent browsers (and runs) share cookies and service workers.

use std::io::BufRead;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use chromiumoxide::Browser;
use futures_util::StreamExt;

const CHROME_BINARY: &str =
    ".browser-check-cache/chrome/chrome-headless-shell-linux64/chrome-headless-shell";
const LIB_DIR: &str = ".browser-check-cache/libs/usr/lib/x86_64-linux-gnu";
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(20);
const PROFILE_PREFIX: &str = "smelt-chrome-";

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

/// Launches Chrome with `extra_args` on top of the base headless setup and
/// returns a connected `Browser`, with its CDP handler already being driven.
pub async fn launch(extra_args: &[String]) -> Result<Browser, String> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let chrome_binary = repo_root.join(CHROME_BINARY);
    if !chrome_binary.is_file() {
        return Err(format!(
            "chrome-headless-shell not found at {} — run scripts/browser-check/setup.sh first",
            chrome_binary.display()
        ));
    }
    let lib_dir = repo_root.join(LIB_DIR);
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
            let stderr = child.stderr.take().expect("stderr is piped");
            for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Some(url) = line.strip_prefix("DevTools listening on ") {
                    let _ = updates.send(LaunchUpdate::Ready(url.trim().to_string()));
                }
            }
            // stderr closed: Chrome has exited.
            let _ = child.wait();
            let _ = updates.send(LaunchUpdate::Failed(
                "chrome-headless-shell exited before it was ready".to_string(),
            ));
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
    if outcome.is_err() {
        if let Some(pid) = pid {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid as i32),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
    outcome
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
