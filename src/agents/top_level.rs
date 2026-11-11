//! A top-level conversation set to a persona (SME-481): once, after its
//! repo is attached and before its first message, read from its own
//! checkout as a child's is from its own. From then on it runs under the
//! persona's instructions, tool rules, credentials and `smelt.model`, as a
//! child of the user's root with that persona does
//! (docs/design/personas/top-level.md).
//!
//! The whole set runs under the conversation's turn lock, taken without
//! waiting: a lock held or queued for, or a turn admitted, means the
//! conversation has started. The apply (insert, pod reconcile, AGENTS.md,
//! mark applied) finishes before the lock is released, so no model call,
//! tool call or saved message comes before the persona is applied.

use super::*;
use crate::api::personas::{PersonaOption, SetPersona, TopLevelPersona};

/// What a turn is refused with while the persona is being applied.
pub(crate) const PERSONA_PENDING_TEXT: &str =
    "This conversation's persona is still being applied; try again, or start a new conversation.";

/// The set, as `POST /api/conversations/{id}/persona` runs it: in a task
/// the request awaits (SME-354 B7's pattern), so a hung-up client can't
/// stop it between its steps.
pub(crate) async fn set_persona(pool: &PgPool, id: i64, persona: &str) -> Result<SetPersona, String> {
    let (pool, persona) = (pool.clone(), persona.to_string());
    tokio::spawn(tracing::Instrument::in_current_span(async move { set_in_task(&pool, id, &persona).await }))
        .await
        .map_err(|e| format!("the persona's set didn't finish: {e}"))?
}

async fn set_in_task(pool: &PgPool, id: i64, persona: &str) -> Result<SetPersona, String> {
    persona::check_name(persona)?;
    let (Some(dir), name) = persona::split_name(persona) else {
        return Err(format!("name the persona with its repo, as `<repo dir>:{persona}` (the picker and list_personas name it so)"));
    };
    match crate::turn::between_turns(id, false, || apply(pool, id, dir, name)).await {
        Some(result) => result,
        None => Err(db::PERSONA_STARTED.to_string()),
    }
}

/// The set's steps, the turn lock held.
async fn apply(pool: &PgPool, id: i64, dir: &str, name: &str) -> Result<SetPersona, String> {
    let e = |e: sqlx::Error| format!("couldn't set the persona: {e}");
    // A turn admitted and still waiting for the lock has started too.
    if crate::turn::turn_running(id) {
        return Err(db::PERSONA_STARTED.to_string());
    }
    if !db::conversation_exists(pool, id).await.map_err(e)? {
        return Err("this conversation no longer exists".to_string());
    }
    if db::conversation_parent(pool, id).await.map_err(e)?.is_some() {
        return Err("an agent's persona is set by start_agent, not here".to_string());
    }
    let repo = ready_checkout(pool, id, dir).await?;
    // Nothing is read from an untrusted checkout; the transaction reads
    // the trust again under its lock.
    if remote_trust(pool, &repo.remote_key).await? != Some(true) {
        return Err(untrusted(&repo));
    }
    let checkout = checkout_for(id);
    let snapshot = persona::snapshot(pool, checkout.as_ref(), id, &repo, name).await?;
    #[cfg(test)]
    test_hooks::reach(test_hooks::TOP_LEVEL_AFTER_READ, id).await;
    let label = format!("{dir}:{name}");
    let model = persona_model(pool, id, &label, &snapshot).await?;
    let git_ssh = crate::credentials::snapshot_credential_names(&snapshot)
        .iter()
        .any(|n| n == crate::credentials::CREDENTIAL_GIT_SSH);
    let new = db::NewTopLevelPersona { conversation: id, remote_key: &repo.remote_key, snapshot: &snapshot, git_ssh, model };
    if let db::InsertTopLevel::Refused(why) = db::insert_top_level_persona(pool, &new).await.map_err(e)? {
        return Err(why);
    }
    #[cfg(test)]
    test_hooks::reach(test_hooks::TOP_LEVEL_AFTER_INSERT, id).await;
    if let Err(error) = finish_apply(pool, id, checkout.as_ref(), &repo, &snapshot).await {
        undo_apply(pool, id).await;
        return Err(error);
    }
    crate::events::publish_app(crate::events::AppEvent::ConversationsChanged);
    set_of(pool, id).await?.ok_or_else(|| "the persona was set, but its row is gone".to_string())
}

/// The apply after the insert: the pod to the persona's grant, the
/// checkout's AGENTS.md loaded as a child's start loads it (unless the
/// conversation already has it), then the row marked applied.
async fn finish_apply(
    pool: &PgPool,
    id: i64,
    checkout: &dyn ChildCheckout,
    repo: &db::ConversationRepo,
    snapshot: &db::PersonaSnapshot,
) -> Result<(), String> {
    crate::credentials::reconcile_pod(pool, id, Some(snapshot))
        .await
        .map_err(|e| format!("couldn't write the persona's credentials into the sandbox: {e}"))?;
    let e = |e: sqlx::Error| format!("couldn't apply the persona: {e}");
    if repo.agents_files.iter().any(|f| f == "AGENTS.md") {
        let loaded = db::list_loaded_instructions(pool, id).await.map_err(e)?;
        if !loaded.iter().any(|l| l.repo_id == repo.id && l.path == "AGENTS.md") {
            load_agents_md(pool, checkout, id, id, repo).await?;
        }
    }
    if !db::mark_top_level_persona_applied(pool, id).await.map_err(e)? {
        return Err("the persona's row was gone before it was applied".to_string());
    }
    Ok(())
}

/// A failed apply, still under the turn lock: the unapplied row deleted,
/// then the pod put back to the persona-less grant. A failed delete leaves
/// the row unapplied: turns are refused, a retried set replaces it, and
/// the startup sweep deletes it.
async fn undo_apply(pool: &PgPool, id: i64) {
    if let Err(e) = db::delete_unapplied_top_level_persona(pool, id).await {
        tracing::warn!(conversation_id = id, error = %e, "couldn't delete a failed top-level persona's row");
        return;
    }
    if let Err(e) = crate::credentials::reconcile_pod(pool, id, None).await {
        tracing::warn!(conversation_id = id, error = %e, "couldn't put a failed top-level persona's pod back");
    }
}

/// The conversation's `ready` checkout `dir`.
async fn ready_checkout(pool: &PgPool, id: i64, dir: &str) -> Result<db::ConversationRepo, String> {
    let repos = db::list_conversation_repos(pool, id)
        .await
        .map_err(|e| format!("couldn't read this conversation's repos: {e}"))?;
    match repos.into_iter().find(|r| r.dir == dir) {
        Some(repo) if repo.status == "ready" => Ok(repo),
        Some(repo) => Err(format!("{}/{dir} isn't ready (it's {})", crate::sandbox::WORKSPACE_DIR, repo.status)),
        None => Err(format!(
            "this conversation has no checkout {}/{dir}: attach the repo first (Work on a repo)",
            crate::sandbox::WORKSPACE_DIR
        )),
    }
}

fn untrusted(repo: &db::ConversationRepo) -> String {
    format!("the user doesn't trust {}, so its personas can't be used", repo.url)
}

/// A top-level persona's `smelt.model` (SME-481): resolved only on the
/// provider the conversation is on now — its chosen one, else the
/// default's — and refused, naming the providers that list it, when that
/// provider doesn't. Never a fallback to another provider: that is how a
/// ClaudeCode user would land on the paid API (SME-460).
async fn persona_model(
    pool: &PgPool,
    id: i64,
    label: &str,
    snapshot: &db::PersonaSnapshot,
) -> Result<Option<(i64, String)>, String> {
    let Some(model) = snapshot.settings.get("model").and_then(serde_json::Value::as_str) else {
        return Ok(None);
    };
    let e = |e: sqlx::Error| format!("couldn't look up the persona's model: {e}");
    let current = crate::providers::conversation_model(pool, id).await.map_err(e)?;
    let Some(choice) = current.as_ref().and_then(crate::providers::ConversationModel::choice) else {
        return Err(format!(
            "persona `{label}` asks for model `{model}`: pick a model first (the model picker, or \
             POST /api/conversations/{id}/model)"
        ));
    };
    if db::provider_lists_model(pool, choice.provider_id, model).await.map_err(e)? {
        return Ok(Some((choice.provider_id, model.to_string())));
    }
    let others = db::providers_listing_model(pool, model).await.map_err(e)?;
    let others = if others.is_empty() {
        "none of the providers lists it".to_string()
    } else {
        format!("providers that do: {}", others.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(", "))
    };
    Err(format!(
        "persona `{label}` asks for model `{model}`, which `{}` (this conversation's provider) doesn't list; \
         {others}. Pick one of those first (the model picker, or POST /api/conversations/{id}/model), or use a \
         persona without smelt.model.",
        choice.provider_name
    ))
}

/// The persona the conversation is set to, with the model it's on now.
async fn set_of(pool: &PgPool, id: i64) -> Result<Option<SetPersona>, String> {
    let e = |e: sqlx::Error| format!("couldn't read the conversation's persona: {e}");
    let Some(row) = db::get_top_level_persona(pool, id).await.map_err(e)? else {
        return Ok(None);
    };
    let choice = crate::providers::conversation_model(pool, id)
        .await
        .map_err(e)?
        .as_ref()
        .and_then(crate::providers::ConversationModel::choice)
        .cloned();
    Ok(Some(SetPersona {
        label: persona_label(&row.persona_path).unwrap_or(row.persona_name),
        path: row.persona_path,
        commit: row.persona_commit,
        applied: row.applied,
        model: choice.as_ref().map(|c| c.model.clone()),
        provider: choice.map(|c| c.provider_name),
    }))
}

/// `GET /api/conversations/{id}/persona`: the persona it's set to, or why
/// it can't take one, or the options — each ready checkout's persona
/// files, read now, listed only for a remote the user trusts (read under
/// its trust lock). Nothing from an untrusted checkout's files reaches it.
pub(crate) async fn get_persona(pool: &PgPool, id: i64) -> Result<TopLevelPersona, String> {
    let e = |e: sqlx::Error| format!("couldn't read the conversation's persona: {e}");
    if !db::conversation_exists(pool, id).await.map_err(e)? {
        return Err("this conversation no longer exists".to_string());
    }
    if let Some(set) = set_of(pool, id).await? {
        let can_set = Some(if set.applied { db::PERSONA_STARTED.to_string() } else { PERSONA_PENDING_TEXT.to_string() });
        return Ok(TopLevelPersona { set: Some(set), can_set, ..Default::default() });
    }
    let can_set = if db::conversation_parent(pool, id).await.map_err(e)?.is_some() {
        Some("an agent's persona is set by start_agent".to_string())
    } else if crate::turn::turn_running(id) || db::conversation_has_messages(pool, id).await.map_err(e)? {
        Some(db::PERSONA_STARTED.to_string())
    } else {
        None
    };
    if can_set.is_some() {
        return Ok(TopLevelPersona { can_set, ..Default::default() });
    }
    let repos = db::list_conversation_repos(pool, id).await.map_err(e)?;
    let checkout = checkout_for(id);
    let (mut options, mut notes) = (Vec::new(), Vec::new());
    for repo in repos.iter().filter(|r| r.status == "ready") {
        match remote_trust(pool, &repo.remote_key).await? {
            Some(true) => {}
            Some(false) => {
                notes.push(format!("The user declined to trust {}, so its personas aren't available.", repo.url));
                continue;
            }
            None => {
                notes.push(format!("Trust {} (Work on a repo trusts it) to use its personas.", repo.url));
                continue;
            }
        }
        let read = repo_options(pool, checkout.as_ref(), id, repo).await;
        match read {
            Ok(found) if found.is_empty() => notes.push(format!("{} has no persona files (.agents/personas/).", repo.url)),
            Ok(found) => options.extend(found),
            Err(error) => notes.push(format!("Couldn't list {}'s personas: {error}", repo.url)),
        }
    }
    Ok(TopLevelPersona { set: None, can_set: None, options, notes })
}

/// One trusted checkout's personas as options, read and parsed now.
async fn repo_options(
    pool: &PgPool,
    checkout: &dyn ChildCheckout,
    id: i64,
    repo: &db::ConversationRepo,
) -> Result<Vec<PersonaOption>, String> {
    let mut options = Vec::new();
    for rel in checkout.parent_persona_paths(pool, id, &repo.dir).await? {
        let Some(stem) = rel.strip_prefix(".agents/personas/").and_then(|f| f.strip_suffix(".md")) else {
            continue;
        };
        let label = format!("{}:{stem}", repo.dir);
        let option = match checkout.read_file(pool, id, &repo.dir, &rel).await {
            Ok(Some(file)) => match crate::personas::parse(&rel, &file.content, file.file_bytes.max(0) as u64) {
                Ok(persona) => PersonaOption { label, description: Some(persona.description), error: None },
                Err(error) => PersonaOption { label, description: None, error: Some(error) },
            },
            Ok(None) => PersonaOption { label, description: None, error: Some(format!("{rel} is no longer there")) },
            Err(error) => PersonaOption { label, description: None, error: Some(error) },
        };
        options.push(option);
    }
    Ok(options)
}
