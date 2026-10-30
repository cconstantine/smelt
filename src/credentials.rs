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

/// Installs the stored keys and commit identity into pod `pod_id`.
pub async fn install_into_pod(pool: &PgPool, pod_id: i64) -> Result<(), String> {
    with_install_lock(async {
        let keys = db::list_ssh_keys(pool).await.map_err(|e| e.to_string())?;
        let identity = db::get_git_identity(pool).await.map_err(|e| e.to_string())?;
        let keys: Vec<(String, String)> = keys.into_iter().map(|k| (k.name, k.private_key)).collect();
        // Bounded: every install waits on this lock, so a stalled exec
        // mustn't hold it.
        let shell = sandbox::PodShell::for_pod(pool, pod_id).await.map_err(|e| e.to_string())?;
        let files = pod_git_files(&keys, &identity);
        let keys_dir = format!("{POD_GIT_DIR}/keys");
        tokio::time::timeout(INSTALL_TIMEOUT, install_pod_files(&shell, &files, &[keys_dir.as_str()]))
        .await
        .map_err(|_| format!("writing git files into the pod took over {}s", INSTALL_TIMEOUT.as_secs()))?
        .map_err(|e| e.to_string())
    })
    .await
}

/// How long writing the git files into one pod may take.
const INSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// A new pod's git files: nothing to write when there's no key and no
/// identity, so a pod on an image without `/etc/smelt` still starts.
pub async fn install_into_new_pod(pool: &PgPool, pod_id: i64) -> Result<(), String> {
    let keys = db::list_ssh_keys(pool).await.map_err(|e| e.to_string())?;
    let identity = db::get_git_identity(pool).await.map_err(|e| e.to_string())?;
    if keys.is_empty() && identity == GitIdentity::default() {
        return Ok(());
    }
    install_into_pod(pool, pod_id).await
}

/// After a key or the identity changes: every live pod gets the new
/// files, so a key added mid-conversation works without a new pod. A
/// pod that can't be reached is logged and skipped; it may be starting
/// (and installs for itself once running) or ending.
pub async fn install_into_live_pods(pool: &PgPool) {
    let pods = match db::list_live_pods(pool).await {
        Ok(pods) => pods,
        Err(e) => {
            tracing::warn!(error = %e, "couldn't list live pods to install git files");
            return;
        }
    };
    for pod in pods {
        if let Err(e) = install_into_pod(pool, pod.pod_id).await {
            tracing::warn!(pod_id = pod.pod_id, error = %e, "couldn't install git files");
        }
    }
}
