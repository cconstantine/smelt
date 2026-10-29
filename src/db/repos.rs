use super::*;

// --- A conversation's git repos (SME-32) ---

#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub struct ConversationRepo {
    pub id: i64,
    pub conversation_id: i64,
    pub url: String,
    pub remote_key: String,
    pub branch: Option<String>,
    pub dir: String,
    /// `cloning`, `ready` or `failed`.
    pub status: String,
    pub error: Option<String>,
    pub checked_out_branch: Option<String>,
    pub commit_sha: Option<String>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    /// The checkout's AGENTS.md files, relative to it, top-level first.
    pub agents_files: Vec<String>,
    /// Which attempt at the clone is current; a retry increments it.
    pub attempt: i32,
    /// The checkout's persona files (SME-235): the tracked files directly
    /// in `.agents/personas/` named `<name>.md` with a valid persona name,
    /// relative to it, sorted. Paths only.
    pub persona_files: Vec<String>,
}

/// Sets a checkout's AGENTS.md files, for tests; a clone records them
/// with `set_repo_ready`.
#[cfg(test)]
pub async fn set_repo_agents_files(pool: &PgPool, id: i64, files: &[String]) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE conversation_repos SET agents_files = $2, updated_at = now() WHERE id = $1")
        .bind(id)
        .bind(files)
        .execute(pool)
        .await?;
    Ok(())
}

/// An AGENTS.md as read from a checkout, to load or to ask about.
#[derive(Clone, Debug, PartialEq)]
pub struct InstructionsFile {
    /// At most 32 KiB of it.
    pub content: String,
    pub file_bytes: i64,
    pub hash: String,
    pub commit: Option<String>,
}

/// An AGENTS.md in the model's context (see the migration).
#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub struct LoadedInstruction {
    pub id: i64,
    pub conversation_id: i64,
    pub repo_id: i64,
    pub path: String,
    pub content: String,
    pub file_bytes: i64,
    pub hash: String,
    pub commit_sha: Option<String>,
    pub loaded_at: NaiveDateTime,
}

/// Loads `path` of repo `repo_id` into the conversation's context, or
/// replaces what was loaded from it.
pub async fn load_instruction(
    executor: impl sqlx::PgExecutor<'_>,
    conversation_id: i64,
    repo_id: i64,
    path: &str,
    file: &InstructionsFile,
) -> Result<LoadedInstruction, sqlx::Error> {
    sqlx::query_as::<_, LoadedInstruction>(
        "INSERT INTO loaded_instructions (conversation_id, repo_id, path, content, file_bytes, hash, commit_sha)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (repo_id, path) DO UPDATE
            SET content = $4, file_bytes = $5, hash = $6, commit_sha = $7, loaded_at = now()
         RETURNING *",
    )
    .bind(conversation_id)
    .bind(repo_id)
    .bind(path)
    .bind(&file.content)
    .bind(file.file_bytes)
    .bind(&file.hash)
    .bind(file.commit.as_deref())
    .fetch_one(executor)
    .await
}

/// The conversation's loaded AGENTS.md files, oldest first.
pub async fn list_loaded_instructions(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<Vec<LoadedInstruction>, sqlx::Error> {
    sqlx::query_as::<_, LoadedInstruction>(
        "SELECT * FROM loaded_instructions WHERE conversation_id = $1 ORDER BY id ASC",
    )
    .bind(conversation_id)
    .fetch_all(pool)
    .await
}

/// A load waiting on the user's trust decision (see the migration).
#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub struct InstructionRequest {
    pub id: i64,
    pub conversation_id: i64,
    pub repo_id: i64,
    pub path: String,
    pub content: String,
    pub file_bytes: i64,
    pub hash: String,
    pub commit_sha: Option<String>,
    pub created_at: NaiveDateTime,
}

impl InstructionRequest {
    pub fn file(&self) -> InstructionsFile {
        InstructionsFile {
            content: self.content.clone(),
            file_bytes: self.file_bytes,
            hash: self.hash.clone(),
            commit: self.commit_sha.clone(),
        }
    }
}

/// Asks about `path` of repo `repo_id`, replacing an earlier request for
/// the same file (the card then shows the newest copy).
pub async fn request_instruction(
    executor: impl sqlx::PgExecutor<'_>,
    conversation_id: i64,
    repo_id: i64,
    path: &str,
    file: &InstructionsFile,
) -> Result<InstructionRequest, sqlx::Error> {
    sqlx::query_as::<_, InstructionRequest>(
        "INSERT INTO instruction_requests (conversation_id, repo_id, path, content, file_bytes, hash, commit_sha)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (repo_id, path) DO UPDATE
            SET content = $4, file_bytes = $5, hash = $6, commit_sha = $7, created_at = now()
         RETURNING *",
    )
    .bind(conversation_id)
    .bind(repo_id)
    .bind(path)
    .bind(&file.content)
    .bind(file.file_bytes)
    .bind(&file.hash)
    .bind(file.commit.as_deref())
    .fetch_one(executor)
    .await
}

/// Asks the user to trust the repo itself — a card about no file
/// (SME-279): the sentinel path `''`, empty content, and that empty
/// string's sha256 as its hash, so the card's hash round-trip works
/// like a file card's. `UNIQUE (repo_id, path)` keeps one per checkout
/// row, and a repeat ask keeps the same card.
pub async fn request_repo_trust_card(
    executor: impl sqlx::PgExecutor<'_>,
    conversation_id: i64,
    repo_id: i64,
) -> Result<InstructionRequest, sqlx::Error> {
    use sha2::Digest;
    let hash = sha2::Sha256::digest("")
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    request_instruction(
        executor,
        conversation_id,
        repo_id,
        "",
        &InstructionsFile { content: String::new(), file_bytes: 0, hash, commit: None },
    )
    .await
}

/// Share-locks, in id order, the rows of conversation `id` and of every
/// conversation with a repo of this remote, until the transaction ends,
/// as a foreign key to them would: deleting any of them then waits, or
/// finishes first (SME-181). Whether conversation `id` is still there
/// (one deleted before the lock was granted isn't).
pub async fn share_lock_conversations_for(
    executor: impl sqlx::PgExecutor<'_>,
    id: i64,
    remote_key: &str,
) -> Result<bool, sqlx::Error> {
    let locked: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM conversations
          WHERE id = $1 OR id IN (SELECT conversation_id FROM conversation_repos WHERE remote_key = $2)
          ORDER BY id FOR KEY SHARE",
    )
    .bind(id)
    .bind(remote_key)
    .fetch_all(executor)
    .await?;
    Ok(locked.contains(&id))
}

/// Takes the card `id` from conversation `conversation_id` to answer it:
/// `None` when it's gone (answered already, or not this conversation's).
pub async fn take_instruction_request(
    executor: impl sqlx::PgExecutor<'_>,
    id: i64,
    conversation_id: i64,
) -> Result<Option<InstructionRequest>, sqlx::Error> {
    sqlx::query_as::<_, InstructionRequest>(
        "DELETE FROM instruction_requests WHERE id = $1 AND conversation_id = $2 RETURNING *",
    )
    .bind(id)
    .bind(conversation_id)
    .fetch_optional(executor)
    .await
}

pub async fn get_instruction_request(pool: &PgPool, id: i64) -> Result<Option<InstructionRequest>, sqlx::Error> {
    sqlx::query_as::<_, InstructionRequest>("SELECT * FROM instruction_requests WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn list_instruction_requests(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<Vec<InstructionRequest>, sqlx::Error> {
    sqlx::query_as::<_, InstructionRequest>(
        "SELECT * FROM instruction_requests WHERE conversation_id = $1 ORDER BY id ASC",
    )
    .bind(conversation_id)
    .fetch_all(pool)
    .await
}

/// Every conversation's requests about repos with this remote.
pub async fn list_instruction_requests_for_remote(
    executor: impl sqlx::PgExecutor<'_>,
    remote_key: &str,
) -> Result<Vec<InstructionRequest>, sqlx::Error> {
    sqlx::query_as::<_, InstructionRequest>(
        "SELECT r.* FROM instruction_requests r
           JOIN conversation_repos c ON c.id = r.repo_id
          WHERE c.remote_key = $1 ORDER BY r.id ASC",
    )
    .bind(remote_key)
    .fetch_all(executor)
    .await
}

/// Unloads every conversation's instructions from repos with this remote
/// (the user declined it). Returns `(conversation, checkout dir, path)` of
/// each file unloaded.
pub async fn unload_instructions_for_remote(
    executor: impl sqlx::PgExecutor<'_>,
    remote_key: &str,
) -> Result<Vec<(i64, String, String)>, sqlx::Error> {
    sqlx::query_as(
        "DELETE FROM loaded_instructions l
          USING conversation_repos c
          WHERE c.id = l.repo_id AND c.remote_key = $1
      RETURNING l.conversation_id, c.dir, l.path",
    )
    .bind(remote_key)
    .fetch_all(executor)
    .await
}

/// Drops a pending request for `path` of repo `repo_id`, if there is one.
pub async fn delete_instruction_request_for(executor: impl sqlx::PgExecutor<'_>, repo_id: i64, path: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM instruction_requests WHERE repo_id = $1 AND path = $2")
        .bind(repo_id)
        .bind(path)
        .execute(executor)
        .await?;
    Ok(())
}

pub async fn delete_instruction_request(executor: impl sqlx::PgExecutor<'_>, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM instruction_requests WHERE id = $1")
        .bind(id)
        .execute(executor)
        .await?;
    Ok(())
}

pub async fn get_conversation_repo(executor: impl sqlx::PgExecutor<'_>, id: i64) -> Result<Option<ConversationRepo>, sqlx::Error> {
    sqlx::query_as::<_, ConversationRepo>("SELECT * FROM conversation_repos WHERE id = $1")
        .bind(id)
        .fetch_optional(executor)
        .await
}

#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub struct RepoTrust {
    pub remote_key: String,
    pub trusted: bool,
    pub decided_at: NaiveDateTime,
}

/// Takes the remote's trust lock (SME-181), held until the transaction
/// ends: a trust decision and a load for one remote then come wholly
/// before or after each other. An advisory lock, since an undecided
/// remote has no `repo_trust` row to lock; two remotes whose keys hash
/// alike only take turns.
pub async fn lock_remote_trust(executor: impl sqlx::PgExecutor<'_>, remote_key: &str) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('repo_trust:' || $1, 0))")
        .bind(remote_key)
        .execute(executor)
        .await?;
    Ok(())
}

/// `Some(trusted)` once the user has decided about this remote.
pub async fn get_repo_trust(executor: impl sqlx::PgExecutor<'_>, remote_key: &str) -> Result<Option<bool>, sqlx::Error> {
    sqlx::query_scalar("SELECT trusted FROM repo_trust WHERE remote_key = $1")
        .bind(remote_key)
        .fetch_optional(executor)
        .await
}

pub async fn set_repo_trust(executor: impl sqlx::PgExecutor<'_>, remote_key: &str, trusted: bool) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO repo_trust (remote_key, trusted) VALUES ($1, $2)
         ON CONFLICT (remote_key) DO UPDATE SET trusted = $2, decided_at = now()",
    )
    .bind(remote_key)
    .bind(trusted)
    .execute(executor)
    .await?;
    Ok(())
}

pub async fn list_repo_trust(pool: &PgPool) -> Result<Vec<RepoTrust>, sqlx::Error> {
    sqlx::query_as::<_, RepoTrust>("SELECT * FROM repo_trust ORDER BY remote_key ASC")
        .fetch_all(pool)
        .await
}

/// Forgets a decision, so the user is asked again next time.
pub async fn delete_repo_trust(pool: &PgPool, remote_key: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM repo_trust WHERE remote_key = $1")
        .bind(remote_key)
        .execute(pool)
        .await?;
    Ok(())
}

/// Records a repo, `cloning`. Fails on a `dir` the conversation already uses.
pub async fn create_conversation_repo(
    pool: &PgPool,
    conversation_id: i64,
    url: &str,
    remote_key: &str,
    branch: Option<&str>,
    dir: &str,
) -> Result<ConversationRepo, sqlx::Error> {
    sqlx::query_as::<_, ConversationRepo>(
        "INSERT INTO conversation_repos (conversation_id, url, remote_key, branch, dir, status)
         VALUES ($1, $2, $3, $4, $5, 'cloning') RETURNING *",
    )
    .bind(conversation_id)
    .bind(url)
    .bind(remote_key)
    .bind(branch)
    .bind(dir)
    .fetch_one(pool)
    .await
}

/// A conversation's repos, in the order they were added.
pub async fn list_conversation_repos(
    pool: &PgPool,
    conversation_id: i64,
) -> Result<Vec<ConversationRepo>, sqlx::Error> {
    sqlx::query_as::<_, ConversationRepo>(
        "SELECT * FROM conversation_repos WHERE conversation_id = $1 ORDER BY id ASC",
    )
    .bind(conversation_id)
    .fetch_all(pool)
    .await
}

/// A failed clone is being retried, with the URL and branch asked for
/// this time (the same remote, maybe written another way). `None` when the
/// clone isn't failed any more (another retry got there first).
pub async fn retry_repo_clone(
    pool: &PgPool,
    id: i64,
    url: &str,
    remote_key: &str,
    branch: Option<&str>,
) -> Result<Option<ConversationRepo>, sqlx::Error> {
    sqlx::query_as::<_, ConversationRepo>(
        "UPDATE conversation_repos
            SET url = $2, remote_key = $3, branch = $4, status = 'cloning', error = NULL,
                agents_files = '{}', persona_files = '{}', attempt = attempt + 1, updated_at = now()
          WHERE id = $1 AND status = 'failed'
      RETURNING *",
    )
    .bind(id)
    .bind(url)
    .bind(remote_key)
    .bind(branch)
    .fetch_optional(pool)
    .await
}

#[cfg(test)]
pub async fn set_repo_cloned(
    pool: &PgPool,
    id: i64,
    checked_out_branch: &str,
    commit_sha: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE conversation_repos
            SET status = 'ready', error = NULL, checked_out_branch = $2, commit_sha = $3,
                updated_at = now()
          WHERE id = $1",
    )
    .bind(id)
    .bind(checked_out_branch)
    .bind(commit_sha)
    .execute(pool)
    .await?;
    Ok(())
}

/// A clone that finished, with the AGENTS.md and persona files it has (or
/// why they couldn't be listed) recorded in the same update, so nothing
/// sees it ready without them. Only `attempt`, still cloning, can finish it
/// (SME-86); false when it's no longer the current attempt.
pub async fn set_repo_ready(
    pool: &PgPool,
    id: i64,
    attempt: i32,
    checked_out_branch: &str,
    commit_sha: Option<&str>,
    agents_files: &[String],
    persona_files: &[String],
    error: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE conversation_repos
            SET status = 'ready', error = $4, checked_out_branch = $2, commit_sha = $3,
                agents_files = $5, persona_files = $7, updated_at = now()
          WHERE id = $1 AND status = 'cloning' AND attempt = $6",
    )
    .bind(id)
    .bind(checked_out_branch)
    .bind(commit_sha)
    .bind(error)
    .bind(agents_files)
    .bind(attempt)
    .bind(persona_files)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Re-records a checkout's persona files from what `list_personas` read
/// in it (SME-235): only on a ready row of the attempt it read, so it never
/// writes over a newer clone's. False when it wrote nothing.
pub async fn set_repo_persona_files(pool: &PgPool, id: i64, attempt: i32, files: &[String]) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE conversation_repos SET persona_files = $3, updated_at = now()
          WHERE id = $1 AND attempt = $2 AND status = 'ready'",
    )
    .bind(id)
    .bind(attempt)
    .bind(files)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// At startup: a clone still marked cloning was cut off by the restart.
/// Returns how many there were.
pub async fn fail_unfinished_clones(pool: &PgPool, error: &str) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE conversation_repos SET status = 'failed', error = $1, updated_at = now()
          WHERE status = 'cloning'",
    )
    .bind(error)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Attempt `attempt` at a clone failed. Like `set_repo_ready`, only the
/// current attempt, still cloning, can; false otherwise.
pub async fn set_repo_failed(pool: &PgPool, id: i64, attempt: i32, error: &str) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE conversation_repos SET status = 'failed', error = $2, updated_at = now()
          WHERE id = $1 AND status = 'cloning' AND attempt = $3",
    )
    .bind(id)
    .bind(error)
    .bind(attempt)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// SME-253: a conversation's sandbox storage was removed, so the checkouts
/// on it are gone. Marks its finished clones failed — the panel shows
/// Clone failed and `clone_repo`'s retry path re-creates them; clones still
/// running are left to the normal crash paths. Returns how many there were.
pub async fn fail_repos_after_storage_removal(pool: &PgPool, conversation_id: i64, error: &str) -> Result<u64, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE conversation_repos SET status = 'failed', error = $2, updated_at = now()
          WHERE conversation_id = $1 AND status = 'ready'",
    )
    .bind(conversation_id)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}
