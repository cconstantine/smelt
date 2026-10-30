//! A child's start (SME-236): the transaction that makes it, the one that
//! ends its start (done or failed), and the startup sweep's read.

use super::*;

/// What `start_agent` records for a new child.
pub struct NewChild<'a> {
    pub caller: i64,
    pub brief: &'a str,
    pub persona_name: Option<&'a str>,
    /// The parent's resolved model, the child's until its persona says otherwise.
    pub model: Option<(i64, String)>,
    /// The child's checkout, recorded `cloning`: one of the caller's repos.
    pub repo: Option<NewChildRepo<'a>>,
    /// The deepest the caller may be and still start one (SME-241):
    /// `SMELT_AGENT_DEPTH`, passed in so tests set it directly.
    pub max_depth: i32,
    /// The names of the volumes the caller holds and passes to this child
    /// (SME-239). Each becomes a mount row for the child in this transaction;
    /// a name the caller doesn't hold refuses the whole call, because a child
    /// must never mount more than its parent does.
    pub volumes: &'a [&'a str],
}

/// The child's copy of one of its parent's checkouts.
pub struct NewChildRepo<'a> {
    pub url: &'a str,
    pub remote_key: &'a str,
    pub dir: &'a str,
    /// The branch its ref asks for, if any.
    pub branch: Option<&'a str>,
    /// The parent's checkout row: an undecided remote's trust card asks
    /// about it, in the parent's conversation (SME-279).
    pub parent_repo_id: i64,
}

/// `create_child`'s outcome.
#[derive(Debug, PartialEq)]
pub enum CreateChild {
    /// The child, and its repo row when it has one.
    Created { child: i64, repo: Option<ConversationRepo> },
    /// The caller can't start an agent now, and why. `asked`: the user
    /// has just been asked to trust the repo — the refusal recorded a
    /// card — so the tool layer publishes the conversation's repos to
    /// open tabs (SME-279).
    Refused { why: String, asked: bool },
}

/// `start_agent`'s transaction: the caller's row locked (the lock a
/// delete takes, so a delete and a start exclude each other), the checks
/// that it may start an agent, then the child's conversation, its
/// `agents` row (`starting`) and the brief as its first message.
pub async fn create_child(pool: &PgPool, new: &NewChild<'_>) -> Result<CreateChild, sqlx::Error> {
    let mut tx = pool.begin().await?;
    // The remote's trust lock first (the lock order), so a trust decision
    // lands wholly before or after the check below and the `cloning` row.
    if let Some(repo) = &new.repo {
        lock_remote_trust(&mut *tx, repo.remote_key).await?;
    }
    let caller = sqlx::query_as::<_, (Option<NaiveDateTime>, Option<i64>)>(
        "SELECT deleting_at, parent_id FROM conversations WHERE id = $1 FOR NO KEY UPDATE",
    )
    .bind(new.caller)
    .fetch_optional(&mut *tx)
    .await?;
    let agent = match &caller {
        Some((None, _)) => Some(
            sqlx::query_as::<_, (String, i32, i64, Option<serde_json::Value>, Option<Vec<String>>)>(
                "SELECT state, depth, root_id, rule_chain, credentials FROM agents WHERE conversation_id = $1",
            )
            .bind(new.caller)
            .fetch_optional(&mut *tx)
            .await?,
        ),
        _ => None,
    };
    // Each refusal is a (why, asked) pair: `asked` means this
    // transaction also recorded the repo's trust card (SME-279).
    let mut refused = match (&caller, agent.as_ref().map(Option::as_ref)) {
        (None, _) => Some(("this conversation no longer exists".to_string(), false)),
        (Some((Some(_), _)), _) => Some(("this conversation is being deleted".to_string(), false)),
        (Some((None, _)), Some(Some((state, _, _, _, _)))) if state == AGENT_STOPPED => {
            Some(("you were stopped, so you can't start an agent".to_string(), false))
        }
        // SME-241: depth replaces the root-only rule — an agent may start
        // one below it until `SMELT_AGENT_DEPTH` levels are used up.
        (Some((None, _)), Some(Some((_, depth, _, _, _)))) if *depth >= new.max_depth => Some((format!(
            "start_agent can't go deeper than {} levels of agents (you're at depth {depth})",
            new.max_depth
        ), false)),
        // A parent but no `agents` row is never a conversation smelt
        // started: its depth and chain are unknown, so it can't start one.
        (Some((None, Some(_))), Some(None) | None) => Some((
            "this conversation has a parent but no agent row, so its depth is unknown — \
             start_agent needs a conversation smelt started"
                .to_string(),
            false,
        )),
        _ => None,
    };
    // The repo's trust checks, on every path that's otherwise allowed.
    if refused.is_none() {
        if let Some(repo) = &new.repo {
            match get_repo_trust(&mut *tx, repo.remote_key).await? {
                Some(true) => {}
                Some(false) => refused = Some((
                    format!("the user declined to trust {}, so no agent can work on it", repo.url),
                    false,
                )),
                None => {
                    // The repo itself asks: a card on the parent's own
                    // checkout row, in the parent's conversation, under
                    // the trust lock held since this transaction opened
                    // — a decision then lands wholly before or after it,
                    // as for a card a load asked with (SME-181, SME-279).
                    request_repo_trust_card(&mut *tx, new.caller, repo.parent_repo_id).await?;
                    refused = Some((
                        format!(
                            "the user hasn't decided whether to trust {}, so no agent can work on it yet: \
                             a card is asking them in this conversation now. You'll get a message when \
                             they decide; call start_agent again then.",
                            repo.url
                        ),
                        true,
                    ));
                }
            }
        }
    }
    // The volumes to pass, if any (SME-239): read over the caller's own mount
    // rows, each volume row held `FOR SHARE` — the pair that excludes a
    // concurrent `delete_volume`, see `agent_volumes_held_for`. The first
    // asked name with nothing behind it refuses the whole call: a child
    // mounts no more than its parent does.
    let mut passed: Vec<AgentVolume> = Vec::new();
    if refused.is_none() && !new.volumes.is_empty() {
        let held = agent_volumes_held_for(&mut *tx, new.caller, new.volumes).await?;
        match new.volumes.iter().find(|name| !held.iter().any(|v| v.name == **name)) {
            Some(missing) => refused = Some((
                format!(
                    "you don't hold a volume named {missing:?}: create_volume makes one, and you can pass \
                     only volumes you hold"
                ),
                false,
            )),
            None => passed = held,
        }
    }
    if let Some((why, asked)) = refused {
        if asked {
            // The card commits with the refusal (nothing else in this
            // transaction was written); every other way out rolls back.
            tx.commit().await?;
        }
        return Ok(CreateChild::Refused { why, asked });
    }
    // A started agent nests under its own row: its depth plus one, its
    // root's root, its rule chain (SME-241). An agent-less root starts
    // the line: depth 1, itself the root, no chain.
    //
    // Its `credentials` is Rule G's seed (SME-242, D3): the parent's
    // `git-ssh` bit passes down and nothing else does — a NULL column
    // reads granted, like every other Rule G read, so a pre-ticket line
    // keeps the key rather than stranding its children. An agent-less
    // caller passes Rule G's root answer: the key, as every pod has
    // today, if one is stored. The named-secret half of the credential
    // set is not here at all — Rule S reads the child's own persona
    // snapshot, so no secret name can ride this column down the tree.
    let nested = agent.flatten();
    let (depth, root, chain, credentials) = match nested {
        Some((_, depth, root, chain, parent_credentials)) => {
            let git_ssh = match &parent_credentials {
                None => true,
                Some(list) => list.iter().any(|name| name == crate::credentials::CREDENTIAL_GIT_SSH),
            };
            (depth + 1, root, chain, if git_ssh { vec![crate::credentials::CREDENTIAL_GIT_SSH.to_string()] } else { vec![] })
        }
        None => {
            let has_key: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM ssh_keys)")
                .fetch_one(&mut *tx)
                .await?;
            (
                1,
                new.caller,
                None,
                if has_key { vec![crate::credentials::CREDENTIAL_GIT_SSH.to_string()] } else { vec![] },
            )
        }
    };
    let (provider_id, model) = new.model.clone().unzip();
    let child = sqlx::query_scalar::<_, i64>(
        "INSERT INTO conversations (title, parent_id, provider_id, model) VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(super::conversations::DEFAULT_TITLE)
    .bind(new.caller)
    .bind(provider_id)
    .bind(model)
    .fetch_one(&mut *tx)
    .await?;
    // D2's trap, stated at the one `INSERT` that could re-introduce it:
    // `credentials` carries the chain bit computed above — the parent's
    // `git-ssh` or nothing — and never a named secret's name. Copying
    // the parent's secret names here, then intersecting at each read,
    // would quietly re-introduce narrowing for secrets, which the user
    // exempted (T4 asserts the subset; Rule S reads the snapshot).
    sqlx::query(
        "INSERT INTO agents (conversation_id, root_id, depth, brief, persona_name, state, rule_chain, credentials)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(child)
    .bind(root)
    .bind(depth)
    .bind(new.brief)
    .bind(new.persona_name)
    .bind(AGENT_STARTING)
    .bind(chain)
    .bind(credentials)
    .execute(&mut *tx)
    .await?;
    let brief = [ContentBlock::Text { text: new.brief.to_string() }];
    super::conversations::create_message_on(&mut tx, child, "user", &brief).await?;
    // Every passed volume gets the child a mount row exactly like its
    // creator's (SME-239): its pod mounts it at the volume's own path at its
    // first start, and it can pass it on to its own agents.
    for volume in &passed {
        sqlx::query("INSERT INTO conversation_volume_mounts (conversation_id, volume_id) VALUES ($1, $2)")
            .bind(child)
            .bind(volume.id)
            .execute(&mut *tx)
            .await?;
    }
    let repo = match &new.repo {
        None => None,
        Some(repo) => Some(
            sqlx::query_as::<_, ConversationRepo>(
                "INSERT INTO conversation_repos (conversation_id, url, remote_key, branch, dir, status)
                 VALUES ($1, $2, $3, $4, $5, 'cloning') RETURNING *",
            )
            .bind(child)
            .bind(repo.url)
            .bind(repo.remote_key)
            .bind(repo.branch)
            .bind(repo.dir)
            .fetch_one(&mut *tx)
            .await?,
        ),
    };
    tx.commit().await?;
    Ok(CreateChild::Created { child, repo })
}

/// A persona as read at the child's ref when it started: what it runs
/// under from then on, whatever happens to the file.
#[derive(Clone, Debug, PartialEq)]
pub struct PersonaSnapshot {
    pub name: String,
    /// Its absolute path in the child's checkout.
    pub path: String,
    pub commit: Option<String>,
    /// Its instructions, relative links made absolute.
    pub body: String,
    /// Its `smelt` settings as parsed (`{"model": …}`).
    pub settings: serde_json::Value,
}

/// What the start's done step writes besides the state.
pub struct StartDone<'a> {
    /// To the child itself: what its first turn drains.
    pub notice: &'a str,
    pub persona: Option<PersonaSnapshot>,
    /// The persona's model, when a provider has it.
    pub model: Option<(i64, String)>,
    /// To the parent, from the child named `label` (its persona's model
    /// no provider has, say).
    pub parent_notice: Option<String>,
    pub label: &'a str,
}

/// The start's done step: the child `active` and started, its persona
/// snapshot and model, the notice that gives its first turn something to
/// drain, and any notice to its parent, in one transaction. `false` when
/// the child is no longer `starting` (stopped or deleted meanwhile):
/// nothing is written.
pub async fn complete_start(pool: &PgPool, child: i64, parent: i64, done: &StartDone<'_>) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    if !lock_conversations(&mut tx, &[(parent, RowLock::KeyShare), (child, RowLock::NoKeyUpdate)]).await? {
        return Ok(false);
    }
    let persona = done.persona.as_ref();
    let updated = sqlx::query(
        "UPDATE agents SET state = $2, started_at = now(), state_changed_at = now(),
                persona_name = COALESCE($4, persona_name), persona_path = $5, persona_commit = $6,
                persona_body = $7, persona_settings = $8
         WHERE conversation_id = $1 AND state = $3",
    )
    .bind(child)
    .bind(AGENT_ACTIVE)
    .bind(AGENT_STARTING)
    .bind(persona.map(|p| p.name.as_str()))
    .bind(persona.map(|p| p.path.as_str()))
    .bind(persona.and_then(|p| p.commit.as_deref()))
    .bind(persona.map(|p| p.body.as_str()))
    .bind(persona.map(|p| &p.settings))
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if updated == 0 {
        return Ok(false);
    }
    // SME-241: the persona's own entry joins the chain it inherited at
    // `create_child` — read-modify-write inside this transaction, the
    // child's row already locked. The entry is the snapshot's, not the
    // file's: a later edit changes nothing, and a persona without
    // `smelt.tools` appends nothing, its parent's chain (or none) as is.
    if let Some(tools) = persona.and_then(|p| p.settings.get("tools")).filter(|t| !t.is_null()) {
        sqlx::query(
            "UPDATE agents SET rule_chain = COALESCE(rule_chain, '[]'::jsonb) || $2::jsonb
             WHERE conversation_id = $1",
        )
        .bind(child)
        .bind(tools)
        .execute(&mut *tx)
        .await?;
    }
    if let Some((provider_id, model)) = &done.model {
        sqlx::query("UPDATE conversations SET provider_id = $2, model = $3 WHERE id = $1")
            .bind(child)
            .bind(provider_id)
            .bind(model)
            .execute(&mut *tx)
            .await?;
    }
    queue_agent_message(&mut tx, child, None, "smelt", KIND_NOTICE, done.notice).await?;
    if let Some(notice) = &done.parent_notice {
        queue_agent_message(&mut tx, parent, Some(child), done.label, KIND_NOTICE, notice).await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// The provider a persona's `smelt.model` runs on: one whose models
/// (`provider_models`) include it, tried in persona-files.md's order: the
/// parent's provider, the default model's, then the others by name.
pub async fn provider_for_model(pool: &PgPool, model: &str, parent_provider: Option<i64>) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "SELECT pm.provider_id FROM provider_models pm
         JOIN inference_providers p ON p.id = pm.provider_id
         LEFT JOIN inference_settings s ON true
         WHERE pm.model = $1
         ORDER BY (pm.provider_id IS NOT DISTINCT FROM $2) DESC,
                  (pm.provider_id IS NOT DISTINCT FROM s.default_provider_id) DESC,
                  p.name ASC
         LIMIT 1",
    )
    .bind(model)
    .bind(parent_provider)
    .fetch_optional(pool)
    .await
}

/// A child's persona snapshot, as its system prompt shows it.
#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub struct AgentPersona {
    pub persona_path: String,
    pub persona_commit: Option<String>,
    pub persona_body: String,
}

/// The persona child `id` runs under, `None` without one.
pub async fn agent_persona(pool: &PgPool, id: i64) -> Result<Option<AgentPersona>, sqlx::Error> {
    sqlx::query_as::<_, AgentPersona>(
        "SELECT persona_path, persona_commit, persona_body FROM agents
         WHERE conversation_id = $1 AND persona_path IS NOT NULL AND persona_body IS NOT NULL",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// A failed start (`fail_start`): the child `failed`, the error queued to
/// its parent as its report, and saved in the child's own transcript, in
/// one transaction. `false` when the child is no longer `starting`
/// (stopped or deleted meanwhile): nothing is written.
pub async fn fail_start(pool: &PgPool, child: i64, parent: i64, label: &str, text: &str) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    if !lock_conversations(&mut tx, &[(parent, RowLock::KeyShare), (child, RowLock::NoKeyUpdate)]).await? {
        return Ok(false);
    }
    let failed = sqlx::query(
        "UPDATE agents SET state = $2, state_changed_at = now() WHERE conversation_id = $1 AND state = $3",
    )
    .bind(child)
    .bind(AGENT_FAILED)
    .bind(AGENT_STARTING)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if failed == 0 {
        return Ok(false);
    }
    queue_agent_message(&mut tx, parent, Some(child), label, KIND_REPORT, text).await?;
    super::conversations::create_message_on(&mut tx, child, "user", &[ContentBlock::Text { text: text.to_string() }]).await?;
    tx.commit().await?;
    Ok(true)
}

/// Every child still `starting`, with its parent: the startup sweep's, as
/// no start survives a restart.
pub async fn starting_agents(pool: &PgPool) -> Result<Vec<(i64, i64)>, sqlx::Error> {
    sqlx::query_as::<_, (i64, i64)>(
        "SELECT c.id, c.parent_id FROM conversations c JOIN agents a ON a.conversation_id = c.id
         WHERE a.state = $1 AND c.parent_id IS NOT NULL ORDER BY c.id",
    )
    .bind(AGENT_STARTING)
    .fetch_all(pool)
    .await
}
