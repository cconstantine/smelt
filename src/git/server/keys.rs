use super::*;

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
    crate::credentials::install_into_live_pods(pool).await;
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
    crate::credentials::install_into_live_pods(pool).await;
    Ok(())
}

/// Saves the commit identity, and installs it into every live pod.
pub async fn save_identity(pool: &PgPool, identity: &GitIdentity) -> Result<(), String> {
    validate_identity(identity)?;
    db::set_git_identity(pool, identity)
        .await
        .map_err(|e| e.to_string())?;
    crate::credentials::install_into_live_pods(pool).await;
    Ok(())
}
