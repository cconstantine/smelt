//! A top-level conversation's persona (SME-481): the `top_level_personas`
//! row the set writes once, after the conversation's repo is attached and
//! before its first message. It holds what a child's `agents` row holds of
//! its persona — the snapshot, the rule chain and Rule G's bit — and
//! `applied_at`, set once the pod matches the grant. The readers beside
//! `agents` are in `agents/grants.rs`, `agents/start.rs` and `agents/tree.rs`
//! (docs/design/personas/top-level.md).

use super::*;

/// `admission_state`'s answer for a conversation whose top-level persona
/// is still being applied: no turn is admitted.
pub const PERSONA_PENDING: &str = "persona-pending";

/// Why a set is refused once the conversation has started: a message, a
/// turn held or queued, or an applied persona.
pub const PERSONA_STARTED: &str =
    "this conversation has started, so its persona can't be set or changed: start a new conversation";

/// What the set's transaction writes.
pub struct NewTopLevelPersona<'a> {
    pub conversation: i64,
    /// The checkout's remote: its trust is read again under its lock.
    pub remote_key: &'a str,
    pub snapshot: &'a PersonaSnapshot,
    /// Rule G's bit: whether the persona lists `git-ssh`.
    pub git_ssh: bool,
    /// The model to pin, when the persona names one its provider lists.
    pub model: Option<(i64, String)>,
    /// The conversation's `(provider_id, model)` when that was resolved
    /// (review 1, L1): a pin over a model changed since is refused.
    pub seen_model: (Option<i64>, Option<String>),
}

/// The set transaction's outcome.
#[derive(Debug, PartialEq)]
pub enum InsertTopLevel {
    Inserted,
    Refused(String),
}

/// The set's transaction (SME-481): the remote's trust lock first (the
/// lock order `create_child` uses), then the conversation's row, the
/// checks that it may take a persona, and the row inserted unapplied (or
/// an unapplied one replaced), with the persona's model pinned. Every
/// refusal rolls back: nothing is written.
pub async fn insert_top_level_persona(pool: &PgPool, new: &NewTopLevelPersona<'_>) -> Result<InsertTopLevel, sqlx::Error> {
    let mut tx = pool.begin().await?;
    lock_remote_trust(&mut *tx, new.remote_key).await?;
    type Row = (Option<i64>, Option<NaiveDateTime>, Option<i64>, Option<String>, Option<serde_json::Value>);
    let row = sqlx::query_as::<_, Row>(
        "SELECT parent_id, deleting_at, provider_id, model, pod_resources FROM conversations WHERE id = $1 FOR NO KEY UPDATE",
    )
    .bind(new.conversation)
    .fetch_optional(&mut *tx)
    .await?;
    let refused = |why: &str| Ok(InsertTopLevel::Refused(why.to_string()));
    let (prior_provider, prior_model, prior_resources) = match row {
        None => return refused("this conversation no longer exists"),
        Some((Some(_), ..)) => return refused("an agent's persona is set by start_agent, not here"),
        Some((None, Some(_), ..)) => return refused("this conversation is being deleted"),
        Some((None, None, provider, model, resources)) => (provider, model, resources),
    };
    let started = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM messages WHERE conversation_id = $1)
             OR EXISTS (SELECT 1 FROM top_level_personas WHERE conversation_id = $1 AND applied_at IS NOT NULL)",
    )
    .bind(new.conversation)
    .fetch_one(&mut *tx)
    .await?;
    if started {
        return refused(PERSONA_STARTED);
    }
    if new.model.is_some() && (prior_provider, prior_model.clone()) != new.seen_model {
        return refused(
            "this conversation's model changed while its persona was being set, so the persona's model wasn't \
             pinned over it: set the persona again",
        );
    }
    // Trust read again under its lock: a forget since the file was read
    // lands before (refused here) or after (the snapshot stands, as a
    // child's does).
    if get_repo_trust(&mut *tx, new.remote_key).await? != Some(true) {
        return refused("the user doesn't trust this repo's remote, so its personas can't be used");
    }
    let snapshot = new.snapshot;
    let chain = snapshot
        .settings
        .get("tools")
        .filter(|tools| !tools.is_null())
        .map(|tools| serde_json::Value::Array(vec![tools.clone()]));
    let credentials: Vec<String> =
        if new.git_ssh { vec![crate::credentials::CREDENTIAL_GIT_SSH.to_string()] } else { Vec::new() };
    sqlx::query(
        "INSERT INTO top_level_personas
             (conversation_id, persona_name, persona_path, persona_commit, persona_body, persona_settings, rule_chain, credentials,
              prior_provider_id, prior_model, prior_pod_resources)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
         ON CONFLICT (conversation_id) DO UPDATE SET
             persona_name = EXCLUDED.persona_name, persona_path = EXCLUDED.persona_path,
             persona_commit = EXCLUDED.persona_commit, persona_body = EXCLUDED.persona_body,
             persona_settings = EXCLUDED.persona_settings, rule_chain = EXCLUDED.rule_chain,
             credentials = EXCLUDED.credentials, created_at = now()
         WHERE top_level_personas.applied_at IS NULL",
        // An unapplied row being replaced keeps its own prior values: the
        // conversation may still hold what that set changed.
    )
    .bind(new.conversation)
    .bind(&snapshot.name)
    .bind(&snapshot.path)
    .bind(snapshot.commit.as_deref())
    .bind(&snapshot.body)
    .bind(&snapshot.settings)
    .bind(chain)
    .bind(&credentials)
    .bind(prior_provider)
    .bind(prior_model)
    .bind(prior_resources)
    .execute(&mut *tx)
    .await?;
    if let Some((provider_id, model)) = &new.model {
        sqlx::query("UPDATE conversations SET provider_id = $2, model = $3 WHERE id = $1")
            .bind(new.conversation)
            .bind(provider_id)
            .bind(model)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(InsertTopLevel::Inserted)
}

/// A refresh's transaction (SME-516): the remote's trust lock, the trust
/// read again under it, and the row, still applied and at `snapshot`'s
/// path, rewritten with the new snapshot, chain and Rule G bit. `prior_*`,
/// `created_at` and `applied_at` stay. `Some(why)` is a refusal, which
/// writes nothing.
pub async fn refresh_top_level_persona(
    pool: &PgPool,
    id: i64,
    remote_key: &str,
    snapshot: &PersonaSnapshot,
    git_ssh: bool,
) -> Result<Option<String>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    lock_remote_trust(&mut *tx, remote_key).await?;
    let deleting = sqlx::query_scalar::<_, Option<NaiveDateTime>>(
        "SELECT deleting_at FROM conversations WHERE id = $1 FOR NO KEY UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    match deleting {
        None => return Ok(Some("this conversation no longer exists".to_string())),
        Some(Some(_)) => return Ok(Some("this conversation is being deleted".to_string())),
        Some(None) => {}
    }
    if get_repo_trust(&mut *tx, remote_key).await? != Some(true) {
        return Ok(Some("the user doesn't trust this repo's remote, so its personas can't be used".to_string()));
    }
    let path = sqlx::query_scalar::<_, String>(
        "SELECT persona_path FROM top_level_personas WHERE conversation_id = $1 AND applied_at IS NOT NULL FOR UPDATE",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    match path {
        None => return Ok(Some("this conversation's persona is no longer applied".to_string())),
        Some(path) if path != snapshot.path => {
            return Ok(Some(format!("this conversation's persona is now {path}, not {}", snapshot.path)))
        }
        Some(_) => {}
    }
    let chain = snapshot
        .settings
        .get("tools")
        .filter(|tools| !tools.is_null())
        .map(|tools| serde_json::Value::Array(vec![tools.clone()]));
    let credentials: Vec<String> = if git_ssh { vec![crate::credentials::CREDENTIAL_GIT_SSH.to_string()] } else { Vec::new() };
    sqlx::query(
        "UPDATE top_level_personas SET persona_commit = $2, persona_body = $3, persona_settings = $4,
             rule_chain = $5, credentials = $6
         WHERE conversation_id = $1 AND applied_at IS NOT NULL",
    )
    .bind(id)
    .bind(snapshot.commit.as_deref())
    .bind(&snapshot.body)
    .bind(&snapshot.settings)
    .bind(chain)
    .bind(&credentials)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(None)
}

/// The set's last step: the row applied, so turns are admitted. Says
/// whether it wrote (an applied row, or none, gives `false`).
pub async fn mark_top_level_persona_applied(pool: &PgPool, id: i64) -> Result<bool, sqlx::Error> {
    let wrote = sqlx::query(
        "UPDATE top_level_personas SET applied_at = now() WHERE conversation_id = $1 AND applied_at IS NULL",
    )
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(wrote == 1)
}

/// What an unapplied set changed, as it was before (review 1, M1), and the
/// model its persona pinned, if any.
#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub struct TopLevelPrior {
    pub prior_provider_id: Option<i64>,
    pub prior_model: Option<String>,
    pub prior_pod_resources: Option<serde_json::Value>,
    pub persona_model: Option<String>,
}

/// An unapplied row's `TopLevelPrior`; `None` without one.
pub async fn top_level_prior(pool: &PgPool, id: i64) -> Result<Option<TopLevelPrior>, sqlx::Error> {
    sqlx::query_as::<_, TopLevelPrior>(
        "SELECT prior_provider_id, prior_model, prior_pod_resources, persona_settings->>'model' AS persona_model
         FROM top_level_personas WHERE conversation_id = $1 AND applied_at IS NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// Puts back the model a failed set pinned: only while the conversation
/// is still on the persona's `model` (a later choice of the user's stands),
/// and to a provider that still exists, else to the default.
pub async fn restore_conversation_model(
    pool: &PgPool,
    id: i64,
    prior: (Option<i64>, Option<&str>),
    pinned: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE conversations SET
             provider_id = CASE WHEN EXISTS (SELECT 1 FROM inference_providers WHERE id = $2) THEN $2 END,
             model = CASE WHEN EXISTS (SELECT 1 FROM inference_providers WHERE id = $2) THEN $3 END
         WHERE id = $1 AND model = $4",
    )
    .bind(id)
    .bind(prior.0)
    .bind(prior.1)
    .bind(pinned)
    .execute(pool)
    .await?;
    Ok(())
}

/// A failed apply's compensating delete: only an unapplied row goes.
pub async fn delete_unapplied_top_level_persona(pool: &PgPool, id: i64) -> Result<bool, sqlx::Error> {
    let deleted = sqlx::query("DELETE FROM top_level_personas WHERE conversation_id = $1 AND applied_at IS NULL")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(deleted == 1)
}

/// The startup sweep's: the conversations with an unapplied row (a set
/// never survives a restart), in ascending id.
pub async fn unapplied_top_level_personas(pool: &PgPool) -> Result<Vec<i64>, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "SELECT conversation_id FROM top_level_personas WHERE applied_at IS NULL ORDER BY conversation_id",
    )
    .fetch_all(pool)
    .await
}

/// A top-level conversation's persona as the set and its GET report it.
#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub struct TopLevelPersonaRow {
    pub persona_name: String,
    pub persona_path: String,
    pub persona_commit: Option<String>,
    pub applied: bool,
    /// The snapshot's settings: names only, never a secret's value.
    pub persona_settings: serde_json::Value,
}

/// The conversation's top-level persona row, applied or not.
pub async fn get_top_level_persona(pool: &PgPool, id: i64) -> Result<Option<TopLevelPersonaRow>, sqlx::Error> {
    sqlx::query_as::<_, TopLevelPersonaRow>(
        "SELECT persona_name, persona_path, persona_commit, applied_at IS NOT NULL AS applied, persona_settings
         FROM top_level_personas WHERE conversation_id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// The admission gates' one read (SME-481): the agent's `state`, or
/// `PERSONA_PENDING` for a top-level conversation whose persona is still
/// being applied; `None` for any other conversation. Primary key both
/// sides, and no conversation has both rows.
pub async fn admission_state(pool: &PgPool, id: i64) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>(
        "SELECT state FROM agents WHERE conversation_id = $1
         UNION ALL
         SELECT $2 FROM top_level_personas WHERE conversation_id = $1 AND applied_at IS NULL
         LIMIT 1",
    )
    .bind(id)
    .bind(PERSONA_PENDING)
    .fetch_optional(pool)
    .await
}

/// Whether the conversation has any message: one of the ways it has
/// started.
pub async fn conversation_has_messages(pool: &PgPool, id: i64) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM messages WHERE conversation_id = $1)")
        .bind(id)
        .fetch_one(pool)
        .await
}

/// Whether `provider_id` lists `model` (`provider_models`): the only
/// provider a top-level persona's `smelt.model` may resolve on.
pub async fn provider_lists_model(pool: &PgPool, provider_id: i64, model: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM provider_models WHERE provider_id = $1 AND model = $2)",
    )
    .bind(provider_id)
    .bind(model)
    .fetch_one(pool)
    .await
}

/// The names of the providers that list `model`, by name: what a refused
/// set names as the providers to pick first.
pub async fn providers_listing_model(pool: &PgPool, model: &str) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar::<_, String>(
        "SELECT p.name FROM provider_models pm JOIN inference_providers p ON p.id = pm.provider_id
         WHERE pm.model = $1 ORDER BY p.name",
    )
    .bind(model)
    .fetch_all(pool)
    .await
}
