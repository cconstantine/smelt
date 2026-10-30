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
        let made = shell
            .run(&["sh", "-c", r#"mkdir -p -m 700 "$1""#, "sh", dir], None)
            .await?;
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
        let written = shell
            .run(
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
        let pruned = shell.run(&prune, None).await?;
        if pruned.exit_code != 0 {
            return Err(SandboxError::GitSetup(format!(
                "couldn't remove deleted files from {dir}: {}",
                pruned.stderr.trim()
            )));
        }
    }
    Ok(())
}

/// Runs `install` (read the keys, then write them into a pod) with no
/// other install in between, so an install that read the keys before a
/// change can't write them over one that read them after.
pub(crate) async fn with_install_lock<F: std::future::Future>(install: F) -> F::Output {
    static INSTALLING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _held = INSTALLING.lock().await;
    install.await
}

/// Installs this pod's conversation's credential files (SME-242): both
/// rules are asked per conversation, so a key saved into the store
/// reaches the conversations Rule G grants and not a persona-narrowed
/// one, and a secret reaches the persona that named it. A pod with no
/// row — gone mid-install — installs nothing rather than everything.
pub async fn install_into_pod(pool: &PgPool, pod_id: i64) -> Result<(), String> {
    with_install_lock(async {
        let conversation = db::sandbox_pod_conversation_id(pool, pod_id).await.map_err(|e| e.to_string())?;
        let Some(conversation) = conversation else {
            return Ok(()); // the pod is gone; there is no decision to install
        };
        let files = pod_files(pool, conversation).await.map_err(|e| e.to_string())?;
        write_pod_files(pool, pod_id, &files).await
    })
    .await
}

/// Writes `files` into `pod_id`'s sandbox container and prunes both
/// credential directories to them. The caller holds `with_install_lock`
/// across its reads and this write, so no install interleaves between
/// a decision and its files.
async fn write_pod_files(pool: &PgPool, pod_id: i64, files: &[PodFile]) -> Result<(), String> {
    // Bounded: every install waits on this lock, so a stalled exec
    // mustn't hold it.
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
    with_install_lock(async {
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
/// behaviour).
pub async fn install_into_live_pods(pool: &PgPool) {
    let pods = match db::list_live_pods(pool).await {
        Ok(pods) => pods,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't list live pods to install credentials");
            return;
        }
    };
    for pod in pods {
        if let Err(e) = install_into_pod(pool, pod.pod_id).await {
            tracing::warn!(pod_id = pod.pod_id, error = %e, "couldn't install credentials");
        }
    }
}
