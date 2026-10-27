//! The volume tools (SME-239): `create_volume` and `delete_volume`, claimed
//! by the conversation that calls them and mounted in its pod — and in the
//! pods of every agent it passes one to — on top of the `/workspace` claim
//! every conversation already has. The rows are `db::agent_volumes` and
//! `db::conversation_volume_mounts`; the claim behind them is
//! `agent-volume-<id>`, built and labelled by [`crate::sandbox`].
//!
//! A pod's volumes are fixed when it starts, since Kubernetes can't add one
//! to a live pod: what a conversation mounts is read once at its start, both
//! tools say when that means the next one, and smelt never restarts the pod on
//! its own — that would kill the model's running commands. The design keeps
//! that decision with the model
//! ([checkouts-and-volumes](../../docs/design/personas/checkouts-and-volumes.md#volumes)).

use serde_json::Value;
use sqlx::PgPool;

use crate::{db, sandbox};

/// A volume name, as the migration's CHECK and this tool both require it: 1
/// to 64 letters, digits, `-` and `_`. Checked here so the model gets this
/// sentence rather than a constraint name.
fn valid_volume_name(name: &str) -> bool {
    (1..=64).contains(&name.chars().count()) && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// One required string argument, read the way the other agent tools read
/// their input.
fn argument<'a>(input: &'a Value, field: &str) -> Result<&'a str, String> {
    input
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{field} is required, as a string"))
}

/// Whether this conversation has a pod running: the one thing that decides
/// whether a mount lands at its next start or has nowhere to land yet. A
/// plain db read, no cluster — `sandbox_pods` is the record of what's live —
/// and a failed read says nothing is live, which only leaves a sentence out:
/// the next pod start mounts whatever the rows hold.
async fn holds_a_live_pod(pool: &PgPool, conversation_id: i64) -> bool {
    match db::list_sandbox_pods(pool, conversation_id).await {
        Ok(pods) => !pods.is_empty(),
        Err(e) => {
            tracing::warn!(error = %e, "couldn't read this conversation's pods for a volume's next-start sentence");
            false
        }
    }
}

/// What a create says about when the volume mounts. Kubernetes mounts at a
/// pod's start and smelt never restarts one, so with a pod live the mount
/// waits for the next start — which is the design's "the tool's result says
/// so", said in the tool's own words because restarting is the model's call.
/// `pub(crate)` so the test reads it directly: reaching it through the tool
/// needs a live pod, which is the cluster tier's job.
pub(crate) fn create_sentence(live_pod: bool) -> &'static str {
    if live_pod {
        "Your pod is already running, and Kubernetes can't add a volume to a live pod, so it mounts there from \
         your next start: terminate_pod then create_pod when you're ready (/workspace is a claim and survives)."
    } else {
        "It mounts there when your pod next starts."
    }
}

/// The same for a delete: the rows are gone now, the mount isn't.
/// `claim_gone` is whether the claim actually came down: with the cluster
/// unreachable it waits, and the sentence then names what will take it —
/// the startup sweep — rather than promising the model it's gone (A-F1).
/// `pub(crate)` for the same reason as [`create_sentence`].
pub(crate) fn delete_sentence(live_pod: bool, claim_gone: bool) -> String {
    let claim = if claim_gone {
        "its row, every mount row and its claim are gone."
    } else {
        "its row and every mount row are gone; its claim is left for the startup sweep."
    };
    let mounts = if live_pod {
        "A pod that's already running keeps what it mounted until its next start."
    } else {
        "Nothing mounts it now."
    };
    format!("{claim} {mounts}")
}

/// `create_volume(name, mount_path)`: a volume this conversation owns, to
/// mount at `mount_path` in its pod and in the pods of the agents it's passed
/// to. Every refusal comes before the cluster is reached and before a row is
/// written, so a refused call leaves nothing behind.
pub(crate) async fn create_volume(pool: &PgPool, conversation_id: i64, input: &Value) -> Result<String, String> {
    let name = argument(input, "name")?;
    let typed_path = argument(input, "mount_path")?;
    if !valid_volume_name(name) {
        return Err(format!("a volume name is 1-64 letters, digits, '-' and '_', not {name:?}"));
    }
    // `~` expanded, `.` segments and repeated slashes gone, absolute, no `..`,
    // and not at a reserved directory: the checks a global volume's path gets,
    // plus the ones only a model volume needs (`sandbox::spec`'s list).
    let mount_path = sandbox::model_volume_mount_path(typed_path)?;
    for global in db::list_sandbox_volumes(pool).await.map_err(|e| e.to_string())? {
        if global.mount_path == mount_path {
            return Err(format!(
                "{mount_path:?} is where the global volume {:?} mounts, in every pod; pick another path",
                global.name
            ));
        }
    }
    if db::volume_mounted_at(pool, conversation_id, &mount_path).await.map_err(|e| e.to_string())? {
        return Err(format!(
            "you already mount a volume at {mount_path:?}; a pod mounts one volume per path, so pick another"
        ));
    }
    if let Some(held) = db::agent_volume_held_by(pool, conversation_id, name).await.map_err(|e| e.to_string())? {
        return Err(format!(
            "you already mount a volume named {name:?} (at {:?}, created by conversation #{}); names are unique \
             across everything you mount, so pass that one to your agents instead of making a second",
            held.mount_path, held.owner_conversation_id
        ));
    }
    let manager = sandbox::get().map_err(|e| e.to_string())?;
    let instance = db::smelt_instance(pool).await.map_err(|e| e.to_string())?;
    let volume = {
        let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
        let volume = db::create_agent_volume_with_owner_mount(&mut *tx, conversation_id, name, &mount_path)
            .await
            .map_err(|e| e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())?;
        volume
    };
    // The claim after the commit and the row taken back if it fails — the
    // shape the user's own volumes page uses (`sandbox::create_volume`), so a
    // failed create never leaves a row with nothing behind it. A crash
    // between the two leaves a row without a claim, which every pod start of
    // a mounting conversation re-makes (`ensure_agent_volume_claims`).
    if let Err(e) = sandbox::create_agent_volume_claim(manager, &volume, &instance).await {
        let _ = db::delete_agent_volume_row(pool, conversation_id, volume.id).await;
        return Err(e.to_string());
    }
    Ok(format!(
        "volume {name:?} created at {mount_path:?}. {}",
        create_sentence(holds_a_live_pod(pool, conversation_id).await)
    ))
}

/// `delete_volume(name)`: only a volume this conversation created. One passed
/// to it stays — its creator deletes it, and the creator's subtree delete
/// takes every mount row with it.
pub(crate) async fn delete_volume(pool: &PgPool, conversation_id: i64, input: &Value) -> Result<String, String> {
    let name = argument(input, "name")?;
    // Asked about a volume this conversation mounts but didn't create: the
    // mount join says who did, which is the refusal worth giving.
    let held = db::agent_volume_held_by(pool, conversation_id, name).await.map_err(|e| e.to_string())?;
    if let Some(other) = held.filter(|volume| volume.owner_conversation_id != conversation_id) {
        return Err(format!(
            "{name:?} was created by conversation #{}, so only that conversation can delete it; you hold it as a \
             mount, and your mount row goes with its creator's subtree",
            other.owner_conversation_id
        ));
    }
    // The row's `FOR UPDATE`, the refusal check and the delete in one
    // transaction: `db::delete_agent_volume_if_unused` says why it's the pair
    // that makes `start_agent`'s pass and this delete exclude each other.
    match db::delete_agent_volume_if_unused(pool, conversation_id, name).await.map_err(|e| e.to_string())? {
        db::DeleteAgentVolume::NotFound => Err(format!(
            "no volume named {name:?} that you created. create_volume makes one, and delete_volume deletes only \
             what it created — a volume passed to you is deleted by the conversation that created it."
        )),
        db::DeleteAgentVolume::HeldBy(holders) => Err(format!(
            "{name:?} is still mounted by {} — stop or finish {} first, then delete it.",
            holders
                .iter()
                .map(|(conversation, state)| format!("conversation #{conversation} ({state})"))
                .collect::<Vec<_>>()
                .join(", "),
            if holders.len() == 1 { "it" } else { "them" }
        )),
        db::DeleteAgentVolume::Deleted(volume) => {
            // The claim goes after the commit, best-effort: a crash leaves it
            // for the startup sweep, and Kubernetes' `pvc-protection` holds a
            // claim until the last pod mounting it is gone, which smelt
            // doesn't do. Either way the row — the record — is gone; what the
            // claim's answer was decides the sentence, so a model whose
            // cluster is unreachable isn't told its claim came down (A-F1).
            let claim_gone = match sandbox::get() {
                Ok(manager) => {
                    let instance = db::smelt_instance(pool).await.map_err(|e| e.to_string())?;
                    match sandbox::delete_agent_volume_claim(manager, volume.id, &instance).await {
                        Ok(()) => true,
                        Err(e) => {
                            tracing::warn!(
                                volume = volume.id,
                                error = %e,
                                "the volume's rows are gone; its claim is left for the startup sweep"
                            );
                            false
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        volume = volume.id,
                        error = %e,
                        "the volume's rows are gone; its claim waits for the startup sweep"
                    );
                    false
                }
            };
            Ok(format!(
                "volume {name:?} deleted: {}",
                delete_sentence(holds_a_live_pod(pool, conversation_id).await, claim_gone)
            ))
        }
    }
}
