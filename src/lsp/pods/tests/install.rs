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
