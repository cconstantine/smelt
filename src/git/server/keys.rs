use super::*;

/// Where smelt's git and SSH files live in a pod. The image makes it
/// (owned by `sandbox`) and points `/etc/ssh/ssh_config.d/smelt.conf`
/// and `/etc/gitconfig` at files in it, so nothing in the user's home
/// directory, which may be a volume of their own, is touched.
pub const POD_GIT_DIR: &str = "/etc/smelt";

/// A key pair in OpenSSH format.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyPair {
    pub public_key: String,
    pub private_key: String,
}

/// A key name becomes a file name in the pod, so it's kept to
/// letters, digits, `-` and `_`.
pub fn validate_key_name(name: &str) -> Result<(), String> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if valid {
        Ok(())
    } else {
        Err("A key name is 1 to 64 letters, digits, - or _.".to_string())
    }
}

/// A new ed25519 key, commented `smelt:<name>` so it's recognisable in
/// GitHub's key list.
#[expect(clippy::expect_used, reason = "ed25519 generation and encoding a fresh key don't fail")]
pub fn generate_key(name: &str) -> KeyPair {
    let mut key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519)
        .expect("ed25519 generation doesn't fail");
    key.set_comment(format!("smelt:{name}"));
    key_pair(&key).expect("a freshly generated key encodes")
}

fn key_pair(key: &PrivateKey) -> Result<KeyPair, String> {
    Ok(KeyPair {
        public_key: key.public_key().to_openssh().map_err(|e| e.to_string())?,
        private_key: key
            .to_openssh(LineEnding::LF)
            .map_err(|e| e.to_string())?
            .to_string(),
    })
}

/// Parses a pasted OpenSSH private key and derives its public half.
/// A passphrase-protected key is refused: nothing could type the
/// passphrase in the pod.
pub fn import_key(private_key: &str) -> Result<KeyPair, String> {
    let pem = private_key.trim().replace("\r\n", "\n");
    let key = PrivateKey::from_openssh(&pem).map_err(|e| {
        format!(
            "That isn't an OpenSSH private key ({e}). Paste the whole file, \
                 from -----BEGIN OPENSSH PRIVATE KEY----- to the END line."
        )
    })?;
    if key.is_encrypted() {
        return Err("That key has a passphrase, which nothing in the sandbox \
                        could type. Remove it with `ssh-keygen -p -f <file>` and \
                        paste it again, or generate a new key here."
            .to_string());
    }
    key_pair(&key)
}

/// `SHA256:…` for an OpenSSH public key line.
pub fn fingerprint(public_key: &str) -> Result<String, String> {
    let key = PublicKey::from_openssh(public_key).map_err(|e| e.to_string())?;
    Ok(key.fingerprint(HashAlg::Sha256).to_string())
}

/// Commit identities go into a git config file, where a newline would
/// start a new setting.
pub fn validate_identity(identity: &GitIdentity) -> Result<(), String> {
    let has_control = |s: &str| s.chars().any(char::is_control);
    if has_control(&identity.name) || has_control(&identity.email) {
        Err("The name and email must each be one line.".to_string())
    } else {
        Ok(())
    }
}

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

/// A git config value in double quotes, with `\` and `"` escaped
/// (git-config(1), "Syntax").
fn git_config_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Writes git's files (`pod_git_files`) into the pod's sandbox
/// container. The keys directory is replaced wholesale, so a key
/// deleted since the last install goes too.
pub async fn install_git_files(shell: &sandbox::PodShell, files: &[PodFile]) -> Result<(), sandbox::SandboxError> {
    use sandbox::SandboxError;
    // Never emptied: a clone or push running during a reinstall must
    // still find its key. Keys are written over in place, and only the
    // ones no longer in `files` removed afterwards.
    let keys_dir = format!("{POD_GIT_DIR}/keys");
    let made = shell.run(&["sh", "-c", r#"mkdir -p -m 700 "$1""#, "sh", &keys_dir], None).await?;
    if made.exit_code != 0 {
        return Err(SandboxError::GitSetup(format!(
            "couldn't make {keys_dir}: {}",
            made.stderr.trim()
        )));
    }
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
    let keep: Vec<&str> = files
        .iter()
        .filter_map(|f| f.path.strip_prefix(&format!("{keys_dir}/")))
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
        &keys_dir,
    ];
    prune.extend(keep);
    let pruned = shell.run(&prune, None).await?;
    if pruned.exit_code != 0 {
        return Err(SandboxError::GitSetup(format!(
            "couldn't remove deleted keys from {keys_dir}: {}",
            pruned.stderr.trim()
        )));
    }
    Ok(())
}

/// Runs `install` (read the keys, then write them into a pod) with no
/// other install in between, so an install that read the keys before a
/// change can't write them over one that read them after.
pub(super) async fn with_install_lock<F: std::future::Future>(install: F) -> F::Output {
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
        let shell = sandbox::PodShell::for_pod(pod_id).map_err(|e| e.to_string())?;
        tokio::time::timeout(INSTALL_TIMEOUT, install_git_files(&shell, &pod_git_files(&keys, &identity)))
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

fn summary(key: db::SshKey) -> SshKeySummary {
    SshKeySummary {
        fingerprint: fingerprint(&key.public_key).unwrap_or_default(),
        id: key.id,
        name: key.name,
        public_key: key.public_key,
    }
}

/// Every stored key, public halves only.
pub async fn list_keys(pool: &PgPool) -> Result<Vec<SshKeySummary>, String> {
    let keys = db::list_ssh_keys(pool).await.map_err(|e| e.to_string())?;
    Ok(keys.into_iter().map(summary).collect())
}

/// Generates and stores a new key, and installs it into every live pod.
pub async fn create_key(pool: &PgPool, name: &str) -> Result<SshKeySummary, String> {
    validate_key_name(name)?;
    refuse_a_second_key(pool).await?;
    store_key(pool, name, generate_key(name)).await
}

/// Stores a pasted private key, and installs it into every live pod.
pub async fn import_key_named(
    pool: &PgPool,
    name: &str,
    private_key: &str,
) -> Result<SshKeySummary, String> {
    validate_key_name(name)?;
    let pair = import_key(private_key)?;
    refuse_a_second_key(pool).await?;
    store_key(pool, name, pair).await
}

/// The refusal for a second key: one at a time, for now.
fn one_key_only(existing: &str) -> String {
    format!("There's already an SSH key ({existing}). Delete it first to add another.")
}

async fn store_key(pool: &PgPool, name: &str, pair: KeyPair) -> Result<SshKeySummary, String> {
    let stored = db::create_ssh_key(pool, name, &pair.public_key, &pair.private_key)
        .await
        .map_err(|e| match e.as_database_error() {
            // `ssh_keys_only_one`: another request added one meanwhile.
            Some(db_err) if db_err.is_unique_violation() => one_key_only("added just now"),
            _ => e.to_string(),
        })?;
    install_into_live_pods(pool).await;
    Ok(summary(stored))
}

async fn refuse_a_second_key(pool: &PgPool) -> Result<(), String> {
    match db::list_ssh_keys(pool).await.map_err(|e| e.to_string())?.first() {
        Some(existing) => Err(one_key_only(&existing.name)),
        None => Ok(()),
    }
}

/// Deletes a key and removes it from every live pod.
pub async fn delete_key(pool: &PgPool, id: i64) -> Result<(), String> {
    db::delete_ssh_key(pool, id).await.map_err(|e| e.to_string())?;
    install_into_live_pods(pool).await;
    Ok(())
}

/// Saves the commit identity, and installs it into every live pod.
pub async fn save_identity(pool: &PgPool, identity: &GitIdentity) -> Result<(), String> {
    validate_identity(identity)?;
    db::set_git_identity(pool, identity)
        .await
        .map_err(|e| e.to_string())?;
    install_into_live_pods(pool).await;
    Ok(())
}
