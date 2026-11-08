//! The install wait's loop over fake reads (SME-452 round 1): what it
//! does when one read hangs, or a failed install's log can't be read.

use super::super::*;
use super::unready_server_pod;

/// One read's answer: given at once, or never.
#[derive(Clone)]
enum Answer<T> {
    Now(Result<T, String>),
    Never,
}

impl<T: Clone> Answer<T> {
    async fn give(&self) -> Result<T, String> {
        match self {
            Answer::Now(answer) => answer.clone(),
            Answer::Never => std::future::pending().await,
        }
    }
}

/// `InstallReads` that answer as told, with the pod always running.
struct FakeReads {
    rc: Answer<Option<String>>,
    log: Answer<String>,
    processes: Answer<String>,
}

impl InstallReads for FakeReads {
    async fn pod(&self) -> Result<Option<Pod>, String> {
        Ok(Some(unready_server_pod("a", "Running", None, false)))
    }
    async fn rc(&self) -> Result<Option<String>, String> {
        self.rc.give().await
    }
    async fn log(&self) -> Result<String, String> {
        self.log.give().await
    }
    async fn processes(&self) -> Result<String, String> {
        self.processes.give().await
    }
}

/// `PROCESS_LISTING` run here, by this machine's `sh`, with `rc` and
/// `log` as the record and log paths.
fn listing(rc: &str, log: &str) -> String {
    let ran = std::process::Command::new("sh")
        .args(["-c", PROCESS_LISTING, "sh", rc, log])
        .output()
        .expect("run the listing");
    assert!(ran.status.success(), "the listing failed: {}", String::from_utf8_lossy(&ran.stderr));
    String::from_utf8_lossy(&ran.stdout).into_owned()
}

/// SME-452 round 1, finding 2: a process's second word is printed only
/// when it's a plain word. Measured: `curl http://user:SECRET@…` was
/// listed whole, credential and all, into a tool result and the logs.
#[test]
fn test_the_process_listing_never_prints_a_second_word_that_isnt_plain() {
    let secret = "http://user:SECRET-452@10.255.255.1/";
    // `yes` takes the URL as its argv[1] and blocks on the full pipe.
    let mut child = std::process::Command::new("sh")
        .args(["-c", r#"yes "$1" | sleep 30"#, "sh", secret])
        .spawn()
        .expect("spawn the process to list");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut listed = String::new();
    while std::time::Instant::now() < deadline {
        listed = listing("/nonexistent/install.rc", "/nonexistent/install.log");
        if listed.contains("] yes ") && listed.contains("] sleep 30") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(!listed.contains("SECRET-452"), "the listing printed a credential: {listed}");
    assert!(listed.lines().any(|line| line.ends_with("] yes …")), "{listed}");
    assert!(listed.lines().any(|line| line.ends_with("] sleep 30")), "a plain word is kept: {listed}");
}

/// SME-452 round 1, finding 5: the listing reads the record and log at
/// the paths it's given (`INSTALL_RC`, `INSTALL_LOG`), not paths of its
/// own, and a missing log reads "missing", not "missing bytes".
#[test]
fn test_the_process_listing_reads_the_record_and_log_it_is_given() {
    let dir = std::env::temp_dir().join(format!("sme-452-listing-paths-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let (rc, log) = (dir.join("my.rc"), dir.join("my.log"));
    let (rc, log) = (rc.to_str().expect("a path"), log.to_str().expect("a path"));
    let listed = listing(rc, log);
    assert!(listed.starts_with("my.rc: not written\nmy.log: missing\n"), "{listed}");
    std::fs::write(rc, "0\n").expect("write the record");
    std::fs::write(log, "added").expect("write the log");
    let listed = listing(rc, log);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(listed.starts_with("my.rc: 0\nmy.log: 5 bytes\n"), "{listed}");
}

/// SME-452 round 1, (B′): a record read that never answers is given up
/// on after `RC_READ_TIMEOUT` and counted as a failed read, so the wait
/// still ends at its bound and says so. It once hung the wait, and
/// `start_language_server`, for good, with nothing said.
#[tokio::test(start_paused = true)]
async fn test_a_record_read_that_never_answers_still_ends_the_wait() {
    let reads = FakeReads {
        rc: Answer::Never,
        log: Answer::Now(Ok(String::new())),
        processes: Answer::Now(Ok("8 [S (sleeping)] npm install\n".to_string())),
    };
    let bound = std::time::Duration::from_secs(5);
    let limit = bound + RC_READ_TIMEOUT + LOG_READ_TIMEOUT + std::time::Duration::from_secs(5);
    let waited = tokio::time::timeout(limit, wait_install(&reads, "lsp-7-ra", "a", bound)).await;
    let error = waited.expect("a record read that never answered held the wait past its bound").expect_err("installed");
    assert!(
        error.starts_with(
            "The install didn't finish within 5 s; its record couldn't be read 1 times (last: the read didn't answer \
             within 30 s); 1 record reads, the slowest 30 s. It wrote no output."
        ),
        "{error}"
    );
}

/// SME-452 round 1, finding 6: a timed-out install names what was still
/// running when the listing came through, and says why when it didn't.
#[test]
fn test_a_timed_out_install_names_its_processes_or_why_it_couldnt() {
    let mut watch = InstallWatch::new("lsp-7-ra", "a");
    watch.saw_processes(Ok("install.rc: not written\n8 [S (sleeping)] npm install\n".to_string()));
    let error = watch.timed_out(INSTALL_TIMEOUT, Ok(String::new()));
    assert!(
        error.ends_with(". It wrote no output.\nStill running then:\ninstall.rc: not written\n8 [S (sleeping)] npm install"),
        "{error}"
    );
    watch.saw_processes(Err("the read didn't answer within 10 s".to_string()));
    let error = watch.timed_out(INSTALL_TIMEOUT, Ok(String::new()));
    assert!(
        error.ends_with(". It wrote no output.\n(its processes couldn't be read: the read didn't answer within 10 s)"),
        "{error}"
    );
}

/// What a failed install (exit 1) says, given its log read's answer.
async fn failed_with(log: Answer<String>) -> String {
    let reads = FakeReads { rc: Answer::Now(Ok(Some("1".to_string()))), log, processes: Answer::Never };
    let limit = LOG_READ_TIMEOUT + std::time::Duration::from_secs(5);
    let waited = tokio::time::timeout(limit, wait_install(&reads, "lsp-7-ra", "a", INSTALL_TIMEOUT)).await;
    waited.expect("a failed install's log read held the call past its bound").expect_err("installed")
}

/// SME-452 round 1, finding 4: a failed install says why its log
/// couldn't be read, or that it wrote nothing, as a timed-out one does,
/// and its log read is bounded too. It once dropped `log_of`'s error and
/// ended a bare `The install failed (exit 1):`.
#[tokio::test(start_paused = true)]
async fn test_a_failed_install_says_what_became_of_its_output() {
    let error = failed_with(Answer::Now(Err("tail exited 1: no such file".to_string()))).await;
    assert_eq!(error, "The install failed (exit 1) (its output couldn't be read: tail exited 1: no such file)");
    let error = failed_with(Answer::Now(Ok(String::new()))).await;
    assert_eq!(error, "The install failed (exit 1). It wrote no output.");
    let error = failed_with(Answer::Now(Ok("npm error 404\n".to_string()))).await;
    assert_eq!(error, "The install failed (exit 1). Its output:\nnpm error 404");
    let error = failed_with(Answer::Never).await;
    assert_eq!(error, "The install failed (exit 1) (its output couldn't be read: the read didn't answer within 10 s)");
}
