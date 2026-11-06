//! Writing a conversation's credentials into its pod: the SSH keys, the
//! SSH config offering them and the commit identity (the machinery SME-32
//! built in the git module, moved here by SME-242), and from here the
//! user's named secrets too. It lives outside `git` because a secret
//! install must hold the same lock as a key install: two installs that
//! interleave could each prune the other's write.

use crate::git::GitIdentity;
use crate::{db, sandbox};
use sqlx::PgPool;

#[cfg(test)]
mod tests;

/// The built-in credential every persona may list in
/// `smelt.credentials`: the user's git SSH key (SME-242). No stored
/// secret may take its name — that refusal belongs to secret creation,
/// not to a persona listing it. This constant is Rule G's bit: the only
/// member `agents.credentials` holds today.
pub const CREDENTIAL_GIT_SSH: &str = "git-ssh";

/// The shape of a credential name — the `validate_key_name` rule, since
/// a name becomes a file name in a pod: 1 to 64 letters, digits, `-`
/// and `_`. `git-ssh` is a credential name like any other; what refuses
/// it is storing a secret under it (`validate_secret_name`, later).
pub fn is_credential_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Where smelt's git and SSH files live in a pod. The image makes it
/// (owned by `sandbox`) and points `/etc/ssh/ssh_config.d/smelt.conf`
/// and `/etc/gitconfig` at files in it, so nothing in the user's home
/// directory, which may be a volume of their own, is touched.
pub const POD_GIT_DIR: &str = "/etc/smelt";

/// One file written into a pod.
#[derive(Clone, Debug, PartialEq)]
pub struct PodFile {
    pub path: String,
    pub mode: u32,
    pub content: String,
}

/// Everything a pod needs for git over SSH: each private key, an SSH
/// config offering all of them, and the commit identity (left out when
/// it isn't set, so git's own "please tell me who you are" shows up).
pub fn pod_git_files(keys: &[(String, String)], identity: &GitIdentity) -> Vec<PodFile> {
    let mut files = Vec::new();
    let mut ssh_config = String::from("Host *\n    StrictHostKeyChecking accept-new\n");
    for (name, private_key) in keys {
        let path = format!("{POD_GIT_DIR}/keys/{name}");
        ssh_config.push_str(&format!("    IdentityFile {path}\n"));
        let mut content = private_key.clone();
        if !content.ends_with('\n') {
            content.push('\n');
        }
        files.push(PodFile {
            path,
            mode: 0o600,
            content,
        });
    }
    files.push(PodFile {
        path: format!("{POD_GIT_DIR}/ssh_config"),
        mode: 0o644,
        content: ssh_config,
    });
    let gitconfig = if identity.name.is_empty() && identity.email.is_empty() {
        String::new()
    } else {
        format!(
            "[user]\n\tname = {}\n\temail = {}\n",
            git_config_quote(&identity.name),
            git_config_quote(&identity.email)
        )
    };
    files.push(PodFile {
        path: format!("{POD_GIT_DIR}/gitconfig"),
        mode: 0o644,
        content: gitconfig,
    });
    files
}

/// A git config value in double quotes, with `\\` and `"` escaped
/// (git-config(1), "Syntax").
fn git_config_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Where this conversation's named-secret files live in a pod: one file
/// per granted secret, named exactly what its persona listed (SME-242).
/// Pruned by each install, like the keys directory, and off-limits to
/// volume mounts like all of `/etc/smelt`.
pub const POD_SECRETS_DIR: &str = "/etc/smelt/secrets";

/// What one conversation's two credential rules answer right now
/// (SME-242, D2 — stated as two rules so neither can inherit the
/// other's narrowing). Both are pure reads, re-run at every install.
#[derive(Clone, Debug, PartialEq)]
pub struct PodCredentials {
    /// Rule G: the chain credential. A root gets it iff a key is
    /// stored; an agent iff its `agents.credentials` bit says so (NULL
    /// — a pre-ticket row — falls open to granted).
    pub git_ssh: bool,
    /// Rule S: the granted secret names — this conversation's own
    /// persona's names that the store backs at this moment. Never an
    /// ancestor's, never a persona-less conversation's, never the
    /// root's. The values join at the file write, not here.
    pub secrets: Vec<String>,
}

/// Rule G, read fresh (SME-242): `git-ssh` follows the chain —
/// `create_child` seeded it from the parent's bit and `prepare` narrows
/// it by the persona — and the root simply has it when a key is stored.
async fn git_ssh_granted(pool: &PgPool, conversation: i64) -> Result<bool, sqlx::Error> {
    match db::agent_credentials(pool, conversation).await? {
        // No row is the user's own conversation: today's rule, the key
        // if one is stored.
        None => db::ssh_key_stored(pool).await,
        // A pre-ticket row falls open: that conversation had the key
        // before the column existed, and nothing here takes it away.
        Some(None) => Ok(true),
        Some(Some(list)) => Ok(list.iter().any(|name| name == CREDENTIAL_GIT_SSH)),
    }
}

/// Rule S, read fresh (SME-242): granted iff this conversation's own
/// persona snapshot lists the name and the store holds it now. A name
/// unbacked at start becomes granted at the next install with no
/// restart — the names are frozen, the backing is live.
async fn granted_secret_names(pool: &PgPool, conversation: i64) -> Result<Vec<String>, sqlx::Error> {
    let names = db::persona_credential_names(pool, conversation).await?;
    resolve_stored(pool, names).await
}

/// The credential names a persona snapshot lists — its
/// `smelt.credentials` as `snapshot` saved it: `git-ssh` and stored-
/// secret names alike (SME-242). The store resolution is
/// `resolve_stored`, asked at every read.
pub(crate) fn snapshot_credential_names(snapshot: &db::PersonaSnapshot) -> Vec<String> {
    snapshot
        .settings
        .get("credentials")
        .and_then(|c| c.as_array())
        .map(|list| list.iter().filter_map(|name| name.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

/// The names the store holds right now, in order: Rule S's store half,
/// asked at every read (install, reconcile, the start's notice).
async fn resolve_stored(pool: &PgPool, names: Vec<String>) -> Result<Vec<String>, sqlx::Error> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let stored = db::list_named_secrets(pool).await?;
    Ok(names.into_iter().filter(|name| stored.iter().any(|secret| secret.name == *name)).collect())
}

/// The conversation's credential set, both rules asked separately
/// (SME-242, D2). Every install calls this before reading any value;
/// the prompt and the start report ask it too, so pod and prompt can
/// disagree only for one install's worth of time (Trade-off 2).
pub async fn for_conversation(pool: &PgPool, conversation: i64) -> Result<PodCredentials, sqlx::Error> {
    Ok(PodCredentials {
        git_ssh: git_ssh_granted(pool, conversation).await?,
        secrets: granted_secret_names(pool, conversation).await?,
    })
}

/// What this turn's environment section says about credentials (SME-242,
/// D8): the granted set, plus the two things the lines name but the set
/// doesn't hold — the stored key's name, whose path line 1 shows, and
/// the persona's names the store doesn't back, which line 3 explains.
/// Names and paths only, by construction: no value is ever read here.
#[derive(Clone, Debug, PartialEq)]
pub struct PromptCredentials {
    pub granted: PodCredentials,
    /// The stored key's name when Rule G grants it and one is stored —
    /// `/etc/smelt/keys/<name>` is where the model will find it.
    pub key_name: Option<String>,
    /// Names the persona lists that are not stored secrets right now —
    /// never `git-ssh`, whose ask is Rule G's question, told by line 2.
    pub unbacked: Vec<String>,
}

/// Gather `PromptCredentials` (D8), the same two rules the install asks,
/// read fresh per turn.
pub async fn for_prompt(pool: &PgPool, conversation: i64) -> Result<PromptCredentials, sqlx::Error> {
    let granted = for_conversation(pool, conversation).await?;
    let key_name = if granted.git_ssh {
        db::list_ssh_keys(pool).await?.into_iter().next().map(|key| key.name)
    } else {
        None
    };
    let named = db::persona_credential_names(pool, conversation).await?;
    let unbacked = if named.iter().any(|name| name != CREDENTIAL_GIT_SSH) {
        let stored = db::list_named_secrets(pool).await?;
        named
            .into_iter()
            .filter(|name| name != CREDENTIAL_GIT_SSH && !stored.iter().any(|secret| secret.name == *name))
            .collect()
    } else {
        Vec::new()
    };
    Ok(PromptCredentials { granted, key_name, unbacked })
}

/// The files this conversation's pod gets: git's config and commit
/// identity always, key files only under Rule G, and one file per
/// granted secret under Rule S — mode 600, the name the persona listed.
/// The decision is made before any value is read, and each value
/// binds to its name, never to another conversation (state model).
pub async fn pod_files(pool: &PgPool, conversation: i64) -> Result<Vec<PodFile>, sqlx::Error> {
    let granted = for_conversation(pool, conversation).await?;
    files_for_granted(pool, &granted).await
}

/// The files behind a decided `granted`: the values join here and
/// nowhere earlier. Read and write under one `with_install_lock` so a
/// delete landing in between can't have its prune overwritten by the
/// read of an install that started before it.
async fn files_for_granted(pool: &PgPool, granted: &PodCredentials) -> Result<Vec<PodFile>, sqlx::Error> {
    let keys = if granted.git_ssh {
        db::list_ssh_keys(pool).await?.into_iter().map(|k| (k.name, k.private_key)).collect()
    } else {
        Vec::new()
    };
    let identity = db::get_git_identity(pool).await?;
    let mut files = pod_git_files(&keys, &identity);
    if !granted.secrets.is_empty() {
        for secret in db::list_named_secrets(pool).await? {
            if granted.secrets.iter().any(|name| *name == secret.name) {
                files.push(PodFile {
                    path: format!("{POD_SECRETS_DIR}/{}", secret.name),
                    mode: 0o600,
                    content: secret.value,
                });
            }
        }
    }
    Ok(files)
}

/// Writes `files` into the pod's sandbox container, then prunes each of
/// `prune_dirs` to the names kept in `files`, so a file deleted since the
/// last install goes too.
pub async fn install_pod_files(
    shell: &sandbox::PodShell,
    files: &[PodFile],
    prune_dirs: &[&str],
) -> Result<(), sandbox::SandboxError> {
    use sandbox::SandboxError;
    for dir in prune_dirs {
        let made = run_answered(shell, &["sh", "-c", r#"mkdir -p -m 700 "$1""#, "sh", dir], None).await?;
        if made.exit_code != 0 {
            return Err(SandboxError::GitSetup(format!(
                "couldn't make {dir}: {}",
                made.stderr.trim()
            )));
        }
    }
    // Never emptied: a clone or push running during a reinstall must
    // still find its key. Files are written over in place, and only the
    // ones no longer in `files` removed afterwards.
    for file in files {
        // Written beside the target and renamed over it, so ssh or git
        // never reads half a file; umask keeps a key private from its
        // first byte.
        let mode = format!("{:o}", file.mode);
        let written = run_answered(
            shell,
            &[
                "sh",
                "-c",
                r#"umask 077 && cat > "$1.new" && chmod "$2" "$1.new" && mv "$1.new" "$1""#,
                "sh",
                &file.path,
                &mode,
            ],
            Some(file.content.as_bytes()),
        )
        .await?;
        if written.exit_code != 0 {
            return Err(SandboxError::GitSetup(format!(
                "couldn't write {}: {}",
                file.path,
                written.stderr.trim()
            )));
        }
    }
    for dir in prune_dirs {
        let kept_prefix = format!("{dir}/");
        let keep: Vec<&str> = files
            .iter()
            .filter_map(|f| f.path.strip_prefix(&kept_prefix))
            .collect();
        let mut prune = vec![
            "sh",
            "-c",
            r#"cd "$1" && shift && for f in * .[!.]*; do
               [ -e "$f" ] || continue
               keep=; for k in "$@"; do [ "$f" = "$k" ] && keep=1; done
               [ -n "$keep" ] || rm -f -- "$f"
           done"#,
            "sh",
            dir,
        ];
        prune.extend(keep);
        let pruned = run_answered(shell, &prune, None).await?;
        if pruned.exit_code != 0 {
            return Err(SandboxError::GitSetup(format!(
                "couldn't remove deleted files from {dir}: {}",
                pruned.stderr.trim()
            )));
        }
    }
    Ok(())
}

/// How long one of an install's execs may go unanswered before it counts as
/// stalled and is run again (SME-440). On a loaded CI runner one exec of a
/// reinstall ran and never answered; with nothing bounding it but the whole
/// install's `INSTALL_TIMEOUT`, the install failed and the pod kept a key
/// and a secret its conversation no longer had — in production, a live pod
/// keeping what a narrowing or a delete took away, or a new pod failing to
/// start. Every install exec answers in about a second, 1.3 s under SME-341's
/// load, so 10 s is well past a slow one, and leaves `INSTALL_TIMEOUT` room
/// for several tries.
const EXEC_STALL_AFTER: std::time::Duration = std::time::Duration::from_secs(10);

/// `shell.run`, run again whenever it goes `EXEC_STALL_AFTER` without an
/// answer. Safe because every install command is idempotent: `mkdir -p`, a
/// write beside the target renamed over it, a prune to the same names. A
/// stalled exec may still finish in the pod after this has moved on, which
/// lands the same bytes (SME-410 is the case where a later install's change
/// is in between). Bounded by the caller: `write_pod_files` holds the whole
/// install to `INSTALL_TIMEOUT`.
async fn run_answered(
    shell: &sandbox::PodShell,
    command: &[&str],
    stdin: Option<&[u8]>,
) -> Result<sandbox::ExecResult, sandbox::SandboxError> {
    loop {
        match tokio::time::timeout(EXEC_STALL_AFTER, shell.run(command, stdin)).await {
            Ok(ran) => return ran,
            // The command's arguments name a path, never a value: file
            // contents go in on stdin, which isn't logged.
            Err(_) => tracing::warn!(
                after = ?EXEC_STALL_AFTER,
                target = command.get(4).copied().unwrap_or(""),
                "an install's exec stalled; running it again"
            ),
        }
    }
}

/// Runs `install` (read the keys, then write them into `conversation`'s
/// pod) with no other install into that conversation's pod in between, so
/// an install that read the keys before a change can't write them over one
/// that read them after. Only installs into the same pod can do that, so
/// other conversations' installs go on meanwhile: one lock for all of them
/// made a key or secret change write to every live pod in turn (SME-341).
/// Keyed by conversation, not pod, as `reconcile_pod` finds its pod under
/// the lock; a pod's conversation never changes. The map keeps an entry per
/// conversation that has ever installed, a few bytes each, which a
/// single-user app's conversation count makes fine.
pub(crate) async fn with_install_lock<F: std::future::Future>(conversation: i64, install: F) -> F::Output {
    static INSTALLING: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<i64, std::sync::Arc<tokio::sync::Mutex<()>>>>> =
        std::sync::LazyLock::new(Default::default);
    let lock = INSTALLING.lock().unwrap_or_else(|e| e.into_inner()).entry(conversation).or_default().clone();
    let _held = lock.lock().await;
    install.await
}

/// Installs this pod's conversation's credential files (SME-242): both
/// rules are asked per conversation, so a key saved into the store
/// reaches the conversations Rule G grants and not a persona-narrowed
/// one, and a secret reaches the persona that named it. A pod with no
/// row — gone mid-install — installs nothing rather than everything.
pub async fn install_into_pod(pool: &PgPool, pod_id: i64) -> Result<(), String> {
    install_into_pod_with(pool, pod_id, |pod_id, files| async move { write_pod_files(pool, pod_id, &files).await }).await
}

/// `install_into_pod` with the write passed in, so a test can see what it
/// would write. The files are read after the lock is taken: an install that
/// waited on it can't bring back what a change made meanwhile removed.
async fn install_into_pod_with<F, Fut>(pool: &PgPool, pod_id: i64, write: F) -> Result<(), String>
where
    F: FnOnce(i64, Vec<PodFile>) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    // Read before the lock, which is the conversation's: a pod's
    // conversation never changes.
    let conversation = db::sandbox_pod_conversation_id(pool, pod_id).await.map_err(|e| e.to_string())?;
    let Some(conversation) = conversation else {
        return Ok(()); // the pod is gone; there is no decision to install
    };
    with_install_lock(conversation, async {
        let files = pod_files(pool, conversation).await.map_err(|e| e.to_string())?;
        write(pod_id, files).await
    })
    .await
}

/// Writes `files` into `pod_id`'s sandbox container and prunes both
/// credential directories to them. The caller holds `with_install_lock`
/// across its reads and this write, so no install interleaves between
/// a decision and its files.
async fn write_pod_files(pool: &PgPool, pod_id: i64, files: &[PodFile]) -> Result<(), String> {
    // Bounded: every install into this conversation's pod waits on its
    // lock, so a stalled exec mustn't hold it.
    let shell = sandbox::PodShell::for_pod(pool, pod_id).await.map_err(|e| e.to_string())?;
    let keys_dir = format!("{POD_GIT_DIR}/keys");
    tokio::time::timeout(INSTALL_TIMEOUT, install_pod_files(&shell, files, &[keys_dir.as_str(), POD_SECRETS_DIR]))
        .await
        .map_err(|_| format!("writing credential files into the pod took over {}s", INSTALL_TIMEOUT.as_secs()))?
        .map_err(|e| e.to_string())
}

/// Writes a starting child's decision into its live pod before its
/// first turn (SME-242, D4): Rule G reads the column as `prepare`
/// narrowed it; Rule S resolves `snapshot`'s names — the start's own
/// snapshot, committed only by the done step, so during a start it is
/// passed by hand — against the store as it is now. A no-op when the
/// conversation has no live pod: an agent with no repo never starts
/// one, the common case. Every way out of a start runs this as the
/// backstop, the failed and stopped ones having had their column
/// cleared first (`end_start`), which is what makes the pod end up
/// with nothing.
pub async fn reconcile_pod(pool: &PgPool, child: i64, snapshot: Option<&db::PersonaSnapshot>) -> Result<(), String> {
    with_install_lock(child, async {
        // Newest first should two ever coexist: an old pod is being
        // torn down, the live one is the newest.
        let Some(pod) = db::list_sandbox_pods(pool, child).await.map_err(|e| e.to_string())?.into_iter().next_back()
        else {
            return Ok(()); // no pod, nothing to reconcile
        };
        let secrets = match snapshot {
            Some(snapshot) => resolve_stored(pool, snapshot_credential_names(snapshot)).await.map_err(|e| e.to_string())?,
            None => granted_secret_names(pool, child).await.map_err(|e| e.to_string())?,
        };
        let granted = PodCredentials { git_ssh: git_ssh_granted(pool, child).await.map_err(|e| e.to_string())?, secrets };
        let files = files_for_granted(pool, &granted).await.map_err(|e| e.to_string())?;
        write_pod_files(pool, pod.id, &files).await
    })
    .await
}

/// How long writing the credential files into one pod may take.
const INSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// A new pod's files: nothing to write when this conversation gets no
/// key, no granted secret and has no identity — so a pod on an image
/// without `/etc/smelt` still starts, and a store full of secrets is
/// no "install everything" trigger for the root, whose Rule S is none
/// (T5). A pod id naming no pod is likewise no attempt.
pub async fn install_into_new_pod(pool: &PgPool, pod_id: i64) -> Result<(), String> {
    let conversation = db::sandbox_pod_conversation_id(pool, pod_id).await.map_err(|e| e.to_string())?;
    let Some(conversation) = conversation else {
        return Ok(());
    };
    let granted = for_conversation(pool, conversation).await.map_err(|e| e.to_string())?;
    let has_key = granted.git_ssh && db::ssh_key_stored(pool).await.map_err(|e| e.to_string())?;
    let identity = db::get_git_identity(pool).await.map_err(|e| e.to_string())?;
    if !has_key && granted.secrets.is_empty() && identity == GitIdentity::default() {
        return Ok(());
    }
    install_into_pod(pool, pod_id).await
}

/// After a key, the identity or a secret changes: every live pod gets
/// the files its own conversation is granted — a key added mid-
/// conversation works without a new pod, and a persona-narrowed one is
/// never re-granted by the loop's blanket reach. A pod that can't be
/// reached is logged and skipped (it may be starting or ending); it
/// keeps its old files until its next create (Trade-off 2, today's
/// behaviour). The pods not known written when it answers, so a delete can
/// say a removed key or secret may still be in them (SME-354 B5): those it
/// couldn't reach, and those still installing at `ANSWER_WITHIN`, which go
/// on after it (SME-371). `None` when the live pods couldn't even be listed,
/// so none is known written.
pub async fn install_into_live_pods(pool: &PgPool) -> Option<Vec<i64>> {
    let pods = match db::list_live_pods(pool).await {
        Ok(pods) => pods,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't list live pods to install credentials");
            return None;
        }
    };
    let pods = pods.into_iter().map(|pod| pod.pod_id).collect();
    let pool = pool.clone();
    Some(
        installs_answered_within(ANSWER_WITHIN, pods, move |pod_id| {
            let pool = pool.clone();
            async move { install_into_pod(&pool, pod_id).await }
        })
        .await,
    )
}

/// Runs `install` for each pod, a few at a time, and answers with the pods
/// not known written: at once if they all finish, else at `answer_within`,
/// with the rest still going (SME-371). A change has committed before this
/// starts, so a pod that won't answer must not read as a change that didn't
/// happen: a stalled exec held "Adding..." on the page for up to its minute.
///
/// Two pieces of state, `PodInstalls`: every pod starts in flight, and leaves
/// when its install ends, for the failed list if it failed. The pods not known
/// written are the two together. The answer reads them once, so an install
/// landing later changes nothing the caller was told. Only a pod still in
/// flight at the deadline counts as outlasting it, in the warning and the
/// tier's report. The installs own the task they run in, so neither the
/// deadline nor a dropped request calls one off: each ends on its own
/// `INSTALL_TIMEOUT`, and reads its files under its conversation's lock, so
/// no install in this process can write over a newer change's files. A
/// command already running in the pod when `INSTALL_TIMEOUT` drops its future
/// isn't killed, though, so it can still land after that.
async fn installs_answered_within<F, Fut>(answer_within: std::time::Duration, pods: Vec<i64>, install: F) -> Vec<i64>
where
    F: Fn(i64) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
{
    let installs = std::sync::Arc::new(std::sync::Mutex::new(PodInstalls { in_flight: pods.clone(), failed: Vec::new() }));
    let tracker = installs.clone();
    let (finished, all_finished) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(tracing::Instrument::in_current_span(async move {
        // Dropped at the end, a panicking install's included.
        let _finished = finished;
        // Side by side, a few at a time: one pod after another made a save
        // wait on every live pod in turn, about 1.3 s each under load, and
        // with eight live pods the secrets page sat on "Adding..." for 11 s
        // (SME-341). Each conversation's install takes only its own lock.
        let (install, tracker) = (&install, &tracker);
        futures_util::StreamExt::for_each_concurrent(futures_util::stream::iter(pods), INSTALLS_AT_ONCE, |pod_id| async move {
            let result = install(pod_id).await;
            let mut installs = tracker.lock().unwrap_or_else(|e| e.into_inner());
            installs.in_flight.retain(|pod| *pod != pod_id);
            if let Err(e) = result {
                tracing::warn!(pod_id, error = %e, "couldn't install credentials");
                installs.failed.push(pod_id);
            }
        })
        .await;
    }));
    let deadline_fired = tokio::time::timeout(answer_within, all_finished).await.is_err();
    let PodInstalls { in_flight, failed } = installs.lock().unwrap_or_else(|e| e.into_inner()).clone();
    // Empty when the last one finished just as the deadline passed: nothing
    // outlasted it.
    if deadline_fired && !in_flight.is_empty() {
        tracing::warn!(after = ?answer_within, pods = ?in_flight, ?failed, "answering before these live pods' credentials installs finished");
        #[cfg(test)]
        INSTALL_DEADLINE_FIRINGS.lock().unwrap_or_else(|e| e.into_inner()).push(in_flight.clone());
    }
    in_flight.into_iter().chain(failed).collect()
}

/// Where a round's installs stand. An install that outlasted the deadline
/// and one that failed quickly both leave a pod not known written, which is
/// all a delete's answer needs, but only the first is a stall (SME-371).
#[derive(Clone)]
struct PodInstalls {
    in_flight: Vec<i64>,
    failed: Vec<i64>,
}

/// The pods still installing at each firing of the deadline, for the browser
/// tier's report: its in-process server has no log to warn into.
#[cfg(test)]
static INSTALL_DEADLINE_FIRINGS: std::sync::Mutex<Vec<Vec<i64>>> = std::sync::Mutex::new(Vec::new());

/// Each firing's pods without those in `held`, and without the firings that
/// held nothing else: what a scenario's own hold can't explain.
#[cfg(test)]
pub(crate) fn install_deadline_firings_besides(held: &[i64]) -> Vec<Vec<i64>> {
    INSTALL_DEADLINE_FIRINGS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|pods| pods.iter().copied().filter(|pod| !held.contains(pod)).collect::<Vec<_>>())
        .filter(|pods| !pods.is_empty())
        .collect()
}

/// What a delete tells the page about the live pods not confirmed pruned
/// (SME-354 B5, SME-371): `None` when every one was, else how many may still
/// hold the deleted file, those still installing at the answer included —
/// every live pod when they couldn't be listed.
pub async fn prune_live_pods(pool: &PgPool) -> Option<u32> {
    let missed = match install_into_live_pods(pool).await {
        Some(missed) => missed.len(),
        None => match db::list_live_pods(pool).await {
            Ok(pods) => pods.len().max(1),
            Err(_) => 1,
        },
    };
    (missed > 0).then(|| u32::try_from(missed).unwrap_or(u32::MAX))
}

/// How long a change's answer waits for its live-pod installs (SME-371).
/// Well under the 10 s the browser tier gives a page to show a saved row.
pub(crate) const ANSWER_WITHIN: std::time::Duration = std::time::Duration::from_secs(5);

/// How many live pods `install_into_live_pods` writes to at once.
const INSTALLS_AT_ONCE: usize = 8;
