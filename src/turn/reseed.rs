//! Keeping the prompt cache of Claude Code conversations expected to wake
//! warm (SME-523): `watch` looks at the candidates (`claude_cli::reseed`)
//! every `TICK`, and re-seeds each whose cache is near its TTL while it
//! still may wake by itself — a command still running, a child agent
//! still at work, or its own question to its parent.
//!
//! A candidate is re-seeded only if every one of these holds (`consider`):
//! no turn runs in it; no Stop has paused it (PR #353 review 1 M3); its
//! cache is still alive (`CACHE_TTL` less
//! `MARGIN` from its start); something is expected to wake it
//! (`db::expects_wake`); it has had fewer than `RESEED_CAP` re-seeds this
//! idle spell; its model still resolves to the same Claude Code provider,
//! with a session recorded for it; the provider's request limit, if any,
//! has a slot free now; and the usage gate is open with no one waiting
//! (`gate::try_pass`). A re-seed never waits for a slot or the gate: it
//! tries again on the next tick while the cache lives. A dead cache, the
//! cap, no wake source or another provider drops the candidate; so does
//! a failed re-seed, whose cache state is then unknown.

use super::*;
use crate::claude_cli::reseed as candidates;
use chrono::{DateTime, Duration, Utc};

/// How long after a call its cache is re-seeded.
pub(crate) const RESEED_AFTER: Duration = Duration::minutes(50);
/// The prompt cache's lifetime (M1, and `CLAUDE_CODE_PROMPT_CACHE_TTL`).
pub(crate) const CACHE_TTL: Duration = Duration::hours(1);
/// How much of the TTL must be left for a re-seed to be worth sending.
pub(crate) const MARGIN: Duration = Duration::minutes(2);
/// Re-seeds per idle spell (Q5): about five hours of keeping warm.
pub(crate) const RESEED_CAP: u32 = 6;
/// How often `watch` looks.
const TICK: std::time::Duration = std::time::Duration::from_secs(60);

/// What `consider` decided for one candidate.
pub(crate) enum Decision {
    /// Not now; look again on the next tick.
    Wait(&'static str),
    /// Never: the candidate is dropped.
    Drop(&'static str),
    /// Re-seed it now, with these.
    Launch(Ready),
}

/// What a re-seed is sent with, and the slot and lock it holds.
pub(crate) struct Ready {
    turn_model: crate::providers::TurnModel,
    session_id: String,
    _slot: Option<crate::providers::limiter::Slot>,
    /// The conversation's turn lock, held for the re-seed (review 1 M2):
    /// a turn started meanwhile waits for it, so the two never run on
    /// the session at once.
    _turn_lock: tokio::sync::OwnedMutexGuard<()>,
}

/// Re-seeds candidates due, every `TICK`, for good.
pub(crate) async fn watch(pool: PgPool) {
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        for handle in run_tick(&pool, Utc::now()).await {
            // Each re-seed runs by itself: a slow one doesn't hold the
            // others, and the next tick skips it while it's in flight.
            drop(handle);
        }
    }
}

/// One look at every candidate due at `now`: drops those `consider`
/// drops, and starts a re-seed of those it readies. Returns the started
/// re-seeds, for a test to await.
pub(crate) async fn run_tick(pool: &PgPool, now: DateTime<Utc>) -> Vec<tokio::task::JoinHandle<()>> {
    let mut started = Vec::new();
    for (conversation_id, candidate) in candidates::due(now - RESEED_AFTER) {
        match consider(pool, conversation_id, candidate, now).await {
            Decision::Wait(why) => tracing::trace!(conversation_id, why, "a cache re-seed waits for the next tick"),
            Decision::Drop(why) => {
                tracing::debug!(conversation_id, why, "no more cache re-seeds for this conversation");
                candidates::forget(conversation_id);
            }
            Decision::Launch(ready) if candidates::begin(conversation_id) => {
                let pool = pool.clone();
                started.push(tokio::spawn(async move { reseed(&pool, conversation_id, ready).await }));
            }
            Decision::Launch(_) => {}
        }
    }
    started
}

/// Whether `conversation_id`, a candidate due at `now`, is re-seeded now.
pub(crate) async fn consider(pool: &PgPool, conversation_id: i64, candidate: candidates::Candidate, now: DateTime<Utc>) -> Decision {
    if candidates::off() {
        return Decision::Drop("re-seeding is off: one wrote to its session");
    }
    if turn_running(conversation_id) {
        return Decision::Wait("a turn is running");
    }
    if is_paused(conversation_id) {
        return Decision::Drop("a Stop paused it: nothing wakes it until the user writes");
    }
    if candidate.start + CACHE_TTL <= now + MARGIN {
        return Decision::Drop("its cache is gone");
    }
    if candidate.reseeds >= RESEED_CAP {
        return Decision::Drop("it was re-seeded as often as it may be");
    }
    match db::expects_wake(pool, conversation_id).await {
        Ok(true) => {}
        Ok(false) => return Decision::Drop("nothing is expected to wake it"),
        Err(e) => {
            tracing::warn!(conversation_id, "couldn't tell whether a conversation expects a wake: {e}");
            return Decision::Wait("its wake sources couldn't be read");
        }
    }
    let Ok(turn_model) = crate::providers::resolve_turn_model(pool, conversation_id).await else {
        return Decision::Drop("its model doesn't resolve");
    };
    if turn_model.kind != crate::providers::ProviderKind::ClaudeCli || turn_model.provider_id != candidate.provider_id {
        return Decision::Drop("its model is on another provider now");
    }
    let session = match db::get_claude_cli_session(pool, conversation_id).await {
        Ok(Some(session)) if session.provider_id == candidate.provider_id => session,
        Ok(_) => return Decision::Drop("it has no session to re-seed"),
        Err(e) => {
            tracing::warn!(conversation_id, "couldn't read the conversation's Claude Code session: {e}");
            return Decision::Wait("its session couldn't be read");
        }
    };
    // Taken last but the slot and the gate, so it's held for no await
    // here, and before them, so a busy lock takes neither.
    let Ok(turn_lock) = conversation_lock(conversation_id).try_lock_owned() else {
        return Decision::Wait("a turn holds the conversation");
    };
    let slot = match crate::providers::limiter::current(turn_model.provider_id) {
        Some(limit) => match limit.try_acquire() {
            Some(slot) => Some(slot),
            None => return Decision::Wait("its provider has no free slot"),
        },
        None => None,
    };
    if !crate::claude_cli::gate::try_pass(conversation_id) {
        return Decision::Wait("the usage gate is held or has others waiting");
    }
    Decision::Launch(Ready { turn_model, session_id: session.session_id, _slot: slot, _turn_lock: turn_lock })
}

/// One re-seed: the request a wake would send now, as `claude_cli::reseed`
/// sends it, recorded as a `reseed` call and its subscription reading
/// kept. A failure logs and drops the candidate.
async fn reseed(pool: &PgPool, conversation_id: i64, ready: Ready) {
    let started = Utc::now();
    let turn_model = &ready.turn_model;
    let request = match build_request(pool, conversation_id, turn_model, None, &[], false, 0).await {
        Ok((request, _)) => candidates::reseed_request(&request),
        Err(e) => {
            tracing::warn!(conversation_id, "couldn't build a cache re-seed's request: {e}");
            candidates::forget(conversation_id);
            return;
        }
    };
    crate::claude_cli::usage::discard(conversation_id);
    let recorder = CallRecorder::new(pool, conversation_id, turn_model, db::ModelCallKind::Reseed, &request);
    let replied = crate::claude_cli::reseed::launch(conversation_id, turn_model, &request, &ready.session_id, recorder.meter()).await;
    crate::claude_cli::usage::record(pool, conversation_id, turn_model.provider_id).await;
    match replied {
        Ok(turn) => {
            tracing::info!(
                conversation_id,
                cache_read = turn.usage.cache_read_input_tokens,
                cache_write = turn.usage.cache_creation_input_tokens,
                "re-seeded a Claude Code conversation's prompt cache"
            );
            if let Err(e) = recorder.finish(&turn, None).await {
                tracing::warn!(conversation_id, "couldn't record a cache re-seed: {e}");
            }
            candidates::note_reseed(conversation_id, started);
        }
        Err(e) => {
            tracing::warn!(conversation_id, "a cache re-seed failed, so its next wake starts cold: {e}");
            candidates::forget(conversation_id);
        }
    }
}
