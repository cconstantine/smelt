//! A conversation's checkout directory held by another remote's
//! uncommitted repo row, for the browser tier's late "Work on a repo"
//! failure (SME-263): the attach trusts its remote and then waits on the
//! row, and committing it makes the attach fail with "already used by",
//! the way `api::git`'s
//! `test_work_on_a_repo_that_fails_after_trusting_still_tells_the_models`
//! does. The tier shares its database with a live server, so a wait on the
//! hold is counted by the holding session's pid, not by every lock waiter
//! in the database (code review 1).

use super::*;
use std::time::{Duration, Instant};

/// `dir` in `conversation`, held by another remote's uncommitted repo row
/// until it's committed or dropped.
pub(crate) struct HeldDirectory {
    holding: Option<sqlx::Transaction<'static, sqlx::Postgres>>,
    /// The holding session's backend pid, for [`blocked_by`].
    pub(crate) pid: i32,
}

impl HeldDirectory {
    /// Commits the held row: an attach waiting on it fails with "already
    /// used by".
    pub(crate) async fn commit(mut self) {
        if let Some(holding) = self.holding.take() {
            holding.commit().await.expect("commit the other remote's repo");
        }
    }
}

/// A hold dropped without [`HeldDirectory::commit`] (a scenario that failed
/// or ran out of time) commits too, rather than rolling back: rolled back,
/// it let the waiting attach go on to start a real sandbox and clone from
/// GitHub (code review 1). Committed, the attach fails fast with "already
/// used by", as when the scenario releases it.
impl Drop for HeldDirectory {
    fn drop(&mut self) {
        if let Some(holding) = self.holding.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                if let Err(e) = holding.commit().await {
                    eprintln!("a dropped directory hold didn't commit: {e}");
                }
            });
        }
    }
}

/// Holds `dir` in `conversation` with another remote's uncommitted repo
/// row.
pub(crate) async fn hold_directory(pool: &PgPool, conversation: i64, dir: &str) -> HeldDirectory {
    let mut holding = pool.begin().await.expect("begin the holding transaction");
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *holding)
        .await
        .expect("the holding session's pid");
    sqlx::query(
        "INSERT INTO conversation_repos (conversation_id, url, remote_key, dir, status)
         VALUES ($1, $2, $3, $4, 'cloning')",
    )
    .bind(conversation)
    .bind(format!("https://github.com/x/{dir}"))
    .bind(format!("github.com/x/{dir}"))
    .bind(dir)
    .execute(&mut *holding)
    .await
    .expect("hold the directory, uncommitted");
    HeldDirectory { holding: Some(holding), pid }
}

/// Sessions waiting on a lock backend `pid` holds.
pub(crate) async fn blocked_by(pool: &PgPool, pid: i32) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid))")
        .bind(pid)
        .fetch_one(pool)
        .await
        .expect("pg_stat_activity")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::test_support::{ONE_AT_A_TIME, waiters_or_finished};

    const LIMIT: Duration = Duration::from_secs(10);

    /// Inserts another repo row at `dir`, which waits while `dir` is held.
    fn spawn_insert(pool: &PgPool, conversation: i64, dir: &str) -> tokio::task::JoinHandle<Result<(), sqlx::Error>> {
        let (pool, dir) = (pool.clone(), dir.to_string());
        tokio::spawn(async move {
            sqlx::query(
                "INSERT INTO conversation_repos (conversation_id, url, remote_key, dir, status)
                 VALUES ($1, 'https://github.com/y/r', 'github.com/y/r', $2, 'cloning')",
            )
            .bind(conversation)
            .bind(dir)
            .execute(&pool)
            .await
            .map(|_| ())
        })
    }

    async fn conflicts(task: tokio::task::JoinHandle<Result<(), sqlx::Error>>, what: &str) {
        let result = tokio::time::timeout(LIMIT, task)
            .await
            .unwrap_or_else(|_| panic!("{what} didn't finish"))
            .unwrap_or_else(|e| panic!("{what} task: {e}"));
        assert!(result.is_err_and(|e| e.to_string().contains("unique")), "{what} should hit the held directory");
    }

    /// SME-263 code review 1: another session waiting on a lock the hold
    /// has nothing to do with (a turn still saving, in the browser tier's
    /// shared database) isn't counted as waiting on the hold.
    #[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
    async fn test_blocked_by_counts_only_the_holds_own_waiters(pool: PgPool) {
        let _turn = ONE_AT_A_TIME.lock().await;
        let conversation = db::create_conversation(&pool).await.expect("conversation").id;
        let held = hold_directory(&pool, conversation, "held").await;
        let other = hold_directory(&pool, conversation, "other").await;
        let unrelated = spawn_insert(&pool, conversation, "other");
        assert!(waiters_or_finished(&pool, 1, &unrelated).await, "the unrelated insert never waited");
        assert_eq!(blocked_by(&pool, held.pid).await, 0, "a wait on another session's lock counted as the hold's");
        assert_eq!(blocked_by(&pool, other.pid).await, 1, "the other hold's own waiter");

        let mine = spawn_insert(&pool, conversation, "held");
        let deadline = Instant::now() + LIMIT;
        while blocked_by(&pool, held.pid).await != 1 {
            assert!(Instant::now() < deadline, "the insert at the held directory never counted as its waiter");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        held.commit().await;
        other.commit().await;
        conflicts(mine, "the insert at the held directory").await;
        conflicts(unrelated, "the unrelated insert").await;
    }

    /// SME-263 code review 1: a scenario that fails or times out drops its
    /// hold. Rolled back, it let the waiting attach go on to start a real
    /// sandbox and clone from GitHub; committed, the attach fails fast with
    /// "already used by", as it does when the scenario releases it.
    #[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
    async fn test_a_hold_dropped_without_its_commit_still_commits(pool: PgPool) {
        let conversation = db::create_conversation(&pool).await.expect("conversation").id;
        drop(hold_directory(&pool, conversation, "dropped").await);
        let deadline = Instant::now() + LIMIT;
        loop {
            let held: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM conversation_repos WHERE conversation_id = $1 AND dir = 'dropped'",
            )
            .bind(conversation)
            .fetch_one(&pool)
            .await
            .expect("count the held row");
            if held == 1 {
                break;
            }
            assert!(Instant::now() < deadline, "the dropped hold rolled back instead of committing");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
