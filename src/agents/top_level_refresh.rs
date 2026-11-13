//! A refresh of a top-level conversation's applied persona (SME-516): the
//! set again, on a conversation that has started, re-reads the same persona
//! at a commit on its remote's default branch and replaces the snapshot
//! (body, settings, rule chain, Rule G's bit, commit). Each setting takes
//! effect from the conversation's next turn.
//!
//! It reads only what reached the default branch, never the working tree:
//! a live seat can edit, commit and push its own file, and a top-level
//! chain has no ancestor to intersect with, so any other input would let a
//! seat widen itself (docs/design/personas/top-level.md, "Refresh").
//!
//! The read runs outside the turn lock; the apply under it, at once when
//! it's free, else in a detached task that waits for it (`pending`). One
//! refresh at a time per conversation, held in `REFRESHES`.

use super::*;
use crate::api::personas::{RefreshState, SetPersona};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

/// The last refresh asked for per conversation since smelt started.
static REFRESHES: LazyLock<Mutex<HashMap<i64, RefreshState>>> = LazyLock::new(Default::default);

/// The conversation's last refresh, if one was asked for since smelt started.
pub(crate) fn refresh_of(id: i64) -> Option<RefreshState> {
    REFRESHES.lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned()
}

/// Forgets the conversation's refresh: it was deleted.
pub(crate) fn forget(id: i64) {
    REFRESHES.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
}

fn record(id: i64, state: &str, commit: Option<&str>, why: Option<&str>) {
    let entry = RefreshState {
        state: state.to_string(),
        commit: commit.map(str::to_string),
        why: why.map(str::to_string),
        at: chrono::Utc::now().naive_utc(),
    };
    REFRESHES.lock().unwrap_or_else(|e| e.into_inner()).insert(id, entry);
}

/// Claims the conversation's one refresh, `pending` with no commit yet;
/// refused while another is pending.
fn claim(id: i64) -> Result<(), String> {
    let mut refreshes = REFRESHES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(waiting) = refreshes.get(&id).filter(|r| r.state == "pending") {
        let at = waiting.commit.as_deref().map(|c| format!(" at {}", c.get(..7).unwrap_or(c))).unwrap_or_default();
        return Err(format!("a refresh{at} is already waiting for this conversation's turn to end"));
    }
    let at = chrono::Utc::now().naive_utc();
    refreshes.insert(id, RefreshState { state: "pending".to_string(), commit: None, why: None, at });
    Ok(())
}

/// What the apply needs, read before the lock.
struct Refresh {
    repo: db::ConversationRepo,
    snapshot: db::PersonaSnapshot,
    git_ssh: bool,
}

/// The refresh of `id`'s applied persona to `dir:name` at `commit` (the
/// default branch's tip when `None`): applied now when the turn lock is
/// free, else `pending` until the current turn ends.
pub(crate) async fn refresh(
    pool: &PgPool,
    id: i64,
    row: &db::TopLevelPersonaRow,
    dir: &str,
    name: &str,
    commit: Option<&str>,
) -> Result<SetPersona, String> {
    let label = persona_label(&row.persona_path).unwrap_or_else(|| row.persona_name.clone());
    let rel = persona::persona_file(name);
    if format!("{}/{dir}/{rel}", crate::sandbox::WORKSPACE_DIR) != row.persona_path {
        return Err(format!(
            "a refresh keeps its persona: this conversation is `{label}`; start a new conversation for another"
        ));
    }
    claim(id)?;
    let read = read(pool, id, dir, &rel, commit).await;
    let refresh = match read {
        Ok(refresh) => Arc::new(refresh),
        Err(why) => {
            record(id, "refused", None, Some(&why));
            return Err(why);
        }
    };
    let sha = refresh.snapshot.commit.clone();
    record(id, "pending", sha.as_deref(), None);
    let now = {
        let (pool, refresh) = (pool.clone(), refresh.clone());
        crate::turn::between_turns_or_queue(id, move || async move { apply(&pool, id, &refresh).await }).await
    };
    if let Some(Err(why)) = now {
        return Err(why);
    }
    top_level::set_of(pool, id).await?.ok_or_else(|| "this conversation's persona is gone".to_string())
}

/// The read and its checks, outside the lock: top-level, a ready and
/// trusted checkout, the file at a commit on the default branch, parsed.
async fn read(pool: &PgPool, id: i64, dir: &str, rel: &str, commit: Option<&str>) -> Result<Refresh, String> {
    let e = |e: sqlx::Error| format!("couldn't refresh the persona: {e}");
    if db::conversation_parent(pool, id).await.map_err(e)?.is_some() {
        return Err("an agent's persona is set by start_agent, not here".to_string());
    }
    let repo = top_level::ready_checkout(pool, id, dir).await?;
    if remote_trust(pool, &repo.remote_key).await? != Some(true) {
        return Err(top_level::untrusted(&repo));
    }
    let checkout = checkout_for(id);
    let (sha, content) = checkout.persona_at(pool, id, dir, rel, commit).await?;
    let snapshot = persona::snapshot_from(rel, &content, content.len() as u64, &repo, Some(sha))?;
    #[cfg(test)]
    test_hooks::reach(test_hooks::TOP_LEVEL_AFTER_READ, id).await;
    let git_ssh = crate::credentials::snapshot_credential_names(&snapshot)
        .iter()
        .any(|n| n == crate::credentials::CREDENTIAL_GIT_SSH);
    Ok(Refresh { repo, snapshot, git_ssh })
}

/// The apply, under the turn lock, recorded in `REFRESHES` as it ends.
async fn apply(pool: &PgPool, id: i64, refresh: &Refresh) -> Result<(), String> {
    let applied = apply_steps(pool, id, refresh).await;
    let commit = refresh.snapshot.commit.as_deref();
    match &applied {
        Ok(()) => record(id, "applied", commit, None),
        Err(why) => {
            tracing::warn!(conversation_id = id, error = %why, "a persona refresh was refused");
            record(id, "refused", commit, Some(why));
        }
    }
    applied
}

async fn apply_steps(pool: &PgPool, id: i64, refresh: &Refresh) -> Result<(), String> {
    let e = |e: sqlx::Error| format!("couldn't refresh the persona: {e}");
    let snapshot = &refresh.snapshot;
    match db::get_top_level_persona(pool, id).await.map_err(e)? {
        Some(row) if row.applied && row.persona_path == snapshot.path => {}
        _ => return Err("this conversation's persona changed or is gone; nothing was refreshed".to_string()),
    }
    let sha7 = snapshot.commit.as_deref().map(|c| c.get(..7).unwrap_or(c)).unwrap_or("its commit");
    let restarted = resources(pool, id, snapshot, sha7).await?;
    let granted = crate::credentials::refresh_grant(pool, id, snapshot, refresh.git_ssh, || async {
        #[cfg(test)]
        if test_hooks::fails(test_hooks::TOP_LEVEL_REFRESH_TX, id) {
            return Err("a test failed the refresh's transaction".to_string());
        }
        match db::refresh_top_level_persona(pool, id, &refresh.repo.remote_key, snapshot, refresh.git_ssh).await {
            Ok(None) => Ok(()),
            Ok(Some(why)) => Err(why),
            Err(error) => Err(e(error)),
        }
    })
    .await;
    if let Err(why) = granted {
        // The pod may hold a partial or the new grant: back to the row's.
        if let Err(back) = crate::credentials::reconcile_pod(pool, id, None).await {
            tracing::warn!(conversation_id = id, error = %back, "couldn't put a refused refresh's pod back to its grant");
        }
        let note = if restarted { "; its pod was restarted with the new resources, which the next refresh keeps" } else { "" };
        return Err(format!("{why}{note}"));
    }
    crate::events::publish_app(crate::events::AppEvent::ConversationsChanged);
    Ok(())
}

/// The snapshot's `smelt.resources` made the conversation's: refused while
/// a live pod runs commands a restart would end. Says whether it restarted.
async fn resources(pool: &PgPool, id: i64, snapshot: &db::PersonaSnapshot, sha7: &str) -> Result<bool, String> {
    let e = |e: sqlx::Error| format!("couldn't refresh the persona's resources: {e}");
    let wanted = snapshot.settings.get("resources").filter(|r| !r.is_null()).cloned();
    let stored = db::conversation_pod_resources(pool, id).await.map_err(e)?;
    if wanted == stored {
        return Ok(false);
    }
    let live = !db::list_sandbox_pods(pool, id).await.map_err(e)?.is_empty();
    if live {
        let running = db::running_command_count(pool, id).await.map_err(e)?;
        if running > 0 {
            return Err(format!(
                "the persona at {sha7} asks for other pod resources, and restarting the pod would end its {running} \
                 running commands: stop them, then refresh again"
            ));
        }
    }
    db::set_conversation_pod_resources(pool, id, wanted.as_ref()).await.map_err(e)?;
    if !live {
        return Ok(false);
    }
    if let Err(why) = checkout_for(id).restart_pod(pool, id).await {
        if let Err(back) = db::set_conversation_pod_resources(pool, id, stored.as_ref()).await {
            tracing::warn!(conversation_id = id, error = %back, "couldn't put a refused refresh's pod resources back");
        }
        return Err(why);
    }
    Ok(true)
}
