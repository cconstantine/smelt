//! Git support (SME-32): SSH keys and a commit identity installed into
//! every sandbox pod, so the model can clone and push.
//!
//! The summaries at the top cross the client/server boundary (the `/git`
//! settings page), so they're ungated. Key handling and what a pod gets
//! live in the `server`-only module below, re-exported.

use serde::{Deserialize, Serialize};

/// A stored SSH key as the settings page and the model see it: never the
/// private half.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SshKeySummary {
    pub id: i64,
    pub name: String,
    /// OpenSSH one-line format, ready to paste into GitHub.
    pub public_key: String,
    /// `SHA256:…`, as `ssh-keygen -l` and GitHub show it.
    pub fingerprint: String,
}

/// The name and email commits are made with.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct GitIdentity {
    pub name: String,
    pub email: String,
}

#[cfg(feature = "server")]
mod server {
    use super::{GitIdentity, SshKeySummary};
    use crate::{db, sandbox};
    use sqlx::PgPool;
    use ssh_key::{Algorithm, HashAlg, LineEnding, PrivateKey, PublicKey, rand_core::OsRng};

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

    /// Installs the stored keys and commit identity into pod `pod_id`.
    pub async fn install_into_pod(pool: &PgPool, pod_id: i64) -> Result<(), String> {
        let keys = db::list_ssh_keys(pool).await.map_err(|e| e.to_string())?;
        let identity = db::get_git_identity(pool).await.map_err(|e| e.to_string())?;
        let keys: Vec<(String, String)> = keys.into_iter().map(|k| (k.name, k.private_key)).collect();
        sandbox::install_git_files_in_pod(pod_id, &pod_git_files(&keys, &identity))
            .await
            .map_err(|e| e.to_string())
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
        store_key(pool, name, generate_key(name)).await
    }

    /// Stores a pasted private key, and installs it into every live pod.
    pub async fn import_key_named(
        pool: &PgPool,
        name: &str,
        private_key: &str,
    ) -> Result<SshKeySummary, String> {
        validate_key_name(name)?;
        store_key(pool, name, import_key(private_key)?).await
    }

    async fn store_key(pool: &PgPool, name: &str, pair: KeyPair) -> Result<SshKeySummary, String> {
        let stored = db::create_ssh_key(pool, name, &pair.public_key, &pair.private_key)
            .await
            .map_err(|e| match e.as_database_error() {
                Some(db_err) if db_err.is_unique_violation() => {
                    format!("A key named {name} already exists.")
                }
                _ => e.to_string(),
            })?;
        install_into_live_pods(pool).await;
        Ok(summary(stored))
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

    #[cfg(test)]
    mod db_tests {
        use super::*;

        #[sqlx::test]
        async fn test_create_key_stores_it_and_returns_only_the_public_half(pool: PgPool) {
            let created = create_key(&pool, "github").await.expect("create key");
            assert_eq!(created.name, "github");
            assert!(created.public_key.starts_with("ssh-ed25519 "), "{}", created.public_key);
            assert_eq!(created.fingerprint, fingerprint(&created.public_key).expect("fp"));

            let stored = db::list_ssh_keys(&pool).await.expect("list");
            assert_eq!(stored.len(), 1);
            assert_eq!(stored[0].public_key, created.public_key);
            assert!(stored[0].private_key.contains("OPENSSH PRIVATE KEY"));
            assert_eq!(list_keys(&pool).await.expect("list keys"), vec![created]);
        }

        #[sqlx::test]
        async fn test_create_key_refuses_a_taken_or_unsafe_name(pool: PgPool) {
            create_key(&pool, "github").await.expect("first");
            let taken = create_key(&pool, "github").await.expect_err("taken");
            assert!(taken.contains("already"), "{taken}");
            let unsafe_name = create_key(&pool, "../x").await.expect_err("unsafe");
            assert!(unsafe_name.contains("letters"), "{unsafe_name}");
            assert_eq!(db::list_ssh_keys(&pool).await.expect("list").len(), 1);
        }

        #[sqlx::test]
        async fn test_import_key_stores_the_derived_public_half(pool: PgPool) {
            let pair = generate_key("elsewhere");
            let imported = import_key_named(&pool, "laptop", &pair.private_key)
                .await
                .expect("import");
            assert_eq!(imported.name, "laptop");
            assert_eq!(imported.public_key, pair.public_key);
            let refused = import_key_named(&pool, "bad", "nope").await.expect_err("garbage");
            assert!(refused.contains("private key"), "{refused}");
        }

        #[sqlx::test]
        async fn test_delete_key_and_save_identity(pool: PgPool) {
            let key = create_key(&pool, "gone").await.expect("create");
            delete_key(&pool, key.id).await.expect("delete");
            assert!(list_keys(&pool).await.expect("list").is_empty());

            let identity = GitIdentity {
                name: "Ada".into(),
                email: "ada@example.com".into(),
            };
            save_identity(&pool, &identity).await.expect("save");
            assert_eq!(db::get_git_identity(&pool).await.expect("get"), identity);
            let bad = GitIdentity {
                name: "Ada\nEvil".into(),
                email: "ada@example.com".into(),
            };
            assert!(save_identity(&pool, &bad).await.is_err());
            assert_eq!(db::get_git_identity(&pool).await.expect("get"), identity);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn test_generated_key_is_ed25519_and_its_halves_match() {
            let pair = generate_key("laptop");
            let private = ssh_key::PrivateKey::from_openssh(&pair.private_key)
                .expect("the private key should parse");
            assert_eq!(private.algorithm(), ssh_key::Algorithm::Ed25519);
            assert_eq!(
                private.public_key().to_openssh().expect("encode"),
                pair.public_key
            );
            assert!(pair.public_key.ends_with(" smelt:laptop"), "{}", pair.public_key);
        }

        #[test]
        fn test_two_generated_keys_differ() {
            assert_ne!(generate_key("a").private_key, generate_key("a").private_key);
        }

        #[test]
        fn test_import_derives_the_public_key() {
            let pair = generate_key("imported");
            let imported = import_key(&pair.private_key).expect("a valid key imports");
            assert_eq!(imported.public_key, pair.public_key);
            assert_eq!(imported.private_key, pair.private_key);
        }

        #[test]
        fn test_import_accepts_surrounding_whitespace_and_crlf() {
            let pair = generate_key("pasted");
            let pasted = format!("\n  {}  \n", pair.private_key.replace('\n', "\r\n"));
            let imported = import_key(&pasted).expect("a pasted key imports");
            assert_eq!(imported.public_key, pair.public_key);
        }

        #[test]
        fn test_import_refuses_garbage_with_a_readable_message() {
            let err = import_key("ssh-ed25519 AAAA… this is a public key").expect_err("refused");
            assert!(err.contains("private key"), "{err}");
        }

        #[test]
        fn test_import_refuses_a_passphrase_protected_key() {
            // Made with `ssh-keygen -t ed25519 -N secret -C test`.
            let err = import_key(ENCRYPTED_KEY).expect_err("refused");
            assert!(err.contains("passphrase"), "{err}");
        }

        #[test]
        fn test_fingerprint_matches_ssh_keygen() {
            // `ssh-keygen -lf` on ENCRYPTED_KEY's public half.
            assert_eq!(
                fingerprint(ENCRYPTED_KEY_PUB).expect("parses"),
                ENCRYPTED_KEY_FINGERPRINT
            );
        }

        #[test]
        fn test_key_names_are_file_name_safe() {
            for ok in ["github", "work-laptop", "deploy_smelt", "A1"] {
                assert!(validate_key_name(ok).is_ok(), "{ok}");
            }
            for bad in ["", "has space", "../escape", "a/b", "ünïcode", &"x".repeat(65)] {
                assert!(validate_key_name(bad).is_err(), "{bad:?}");
            }
        }

        #[test]
        fn test_identity_refuses_newlines() {
            let ok = GitIdentity {
                name: "Ada Lovelace".into(),
                email: "ada@example.com".into(),
            };
            assert!(validate_identity(&ok).is_ok());
            let bad = GitIdentity {
                name: "Ada\n[core]\n\tsshCommand = evil".into(),
                email: "ada@example.com".into(),
            };
            assert!(validate_identity(&bad).is_err());
        }

        fn file<'a>(files: &'a [PodFile], path: &str) -> Option<&'a PodFile> {
            files.iter().find(|f| f.path == path)
        }

        #[test]
        fn test_pod_files_hold_each_key_privately_and_offer_every_one() {
            let keys = vec![
                ("github".to_string(), "PRIVATE-1".to_string()),
                ("work".to_string(), "PRIVATE-2".to_string()),
            ];
            let files = pod_git_files(&keys, &GitIdentity::default());

            let github = file(&files, "/etc/smelt/keys/github").expect("key file");
            assert_eq!(github.content, "PRIVATE-1\n");
            assert_eq!(github.mode, 0o600);
            assert_eq!(file(&files, "/etc/smelt/keys/work").expect("key").content, "PRIVATE-2\n");

            let config = file(&files, "/etc/smelt/ssh_config").expect("ssh config");
            assert!(config.content.contains("IdentityFile /etc/smelt/keys/github\n"), "{}", config.content);
            assert!(config.content.contains("IdentityFile /etc/smelt/keys/work\n"), "{}", config.content);
            // Hosts not in the image's known_hosts are accepted the first
            // time, not refused with a prompt nobody can answer.
            assert!(config.content.contains("StrictHostKeyChecking accept-new"), "{}", config.content);
        }

        #[test]
        fn test_pod_files_quote_the_identity_for_git_config() {
            let identity = GitIdentity {
                name: r#"Ada "The Countess" Lovelace\"#.into(),
                email: "ada@example.com".into(),
            };
            let files = pod_git_files(&[], &identity);
            let config = file(&files, "/etc/smelt/gitconfig").expect("gitconfig");
            assert_eq!(
                config.content,
                "[user]\n\tname = \"Ada \\\"The Countess\\\" Lovelace\\\\\"\n\temail = \"ada@example.com\"\n"
            );
        }

        #[test]
        fn test_pod_files_leave_the_identity_empty_when_unset() {
            let files = pod_git_files(&[], &GitIdentity::default());
            let config = file(&files, "/etc/smelt/gitconfig").expect("gitconfig is always written");
            assert_eq!(config.content, "");
            // No keys: the SSH config still exists, offering none.
            let ssh = file(&files, "/etc/smelt/ssh_config").expect("ssh config");
            assert!(!ssh.content.contains("IdentityFile"), "{}", ssh.content);
        }

        const ENCRYPTED_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAACmFlczI1Ni1jdHIAAAAGYmNyeXB0AAAAGAAAABB0Ht94ps
1pc8RavlT/LsnzAAAAGAAAAAEAAAAzAAAAC3NzaC1lZDI1NTE5AAAAIF3I8kAQGuwp+D3C
ue9eeOLmUaTpNtML5gyjN0/jzbdFAAAAkD0ecctju+ef8Bx/ZuprZrtEAA0QDSHEgPw02S
r1/jznE5AU9c9fCQlvapkCYD2SZ0PDjF4OzRqdu5GA3mx6L6hwOtB2q9IbXhG09Pemnx+N
dIwV1foqIg1UBR+irWu2+JpBxBBTWoW4q5Ckq6HA2qM8p4J5V0cNqsKB9dUSZlxMu4EG+O
TeG9b4Wi3JZlayBA==
-----END OPENSSH PRIVATE KEY-----
";
        const ENCRYPTED_KEY_PUB: &str =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIF3I8kAQGuwp+D3Cue9eeOLmUaTpNtML5gyjN0/jzbdF test";
        const ENCRYPTED_KEY_FINGERPRINT: &str = "SHA256:oYEa62H9Lg8UaDiZ0jy+2GXNR5OMjJ4DEfTZcZ0ncuM";
    }
}

#[cfg(feature = "server")]
pub use server::*;
