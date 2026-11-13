//! A top-level conversation's persona (SME-481): the start screen's picker
//! and the API's `GET`/`POST /api/conversations/{id}/persona`. The set and
//! its checks are `crate::agents::top_level`'s; these are thin wrappers.

use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

/// What the picker shows for a conversation.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TopLevelPersona {
    /// The persona it's set to, once set.
    pub set: Option<SetPersona>,
    /// Why no persona can be set now; `None` when one can.
    pub can_set: Option<String>,
    /// The personas it can be set to: listed only when one can be set,
    /// and only from a ready checkout whose remote the user trusts.
    pub options: Vec<PersonaOption>,
    /// One line per ready checkout whose personas aren't listed, saying
    /// why (its remote untrusted or undecided, or it has none).
    pub notes: Vec<String>,
}

/// A conversation's persona once set.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SetPersona {
    /// `<dir>:<name>`.
    pub label: String,
    /// Its absolute path in the checkout.
    pub path: String,
    /// The checkout's commit it was read at.
    pub commit: Option<String>,
    /// The pod matches its grant and turns are admitted.
    pub applied: bool,
    /// The model and provider the conversation is on now.
    pub model: Option<String>,
    pub provider: Option<String>,
    /// The snapshot's `persona_settings` (SME-516): `compact_at`, `tools`,
    /// `credentials` (names only, never a value), `resources`, `ignored`.
    pub settings: serde_json::Value,
    /// The last refresh asked for since smelt started, if any.
    pub refresh: Option<RefreshState>,
}

/// A refresh of an applied persona (SME-516), as last asked for.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RefreshState {
    /// `pending` (it applies when the conversation's current turn ends),
    /// `applied` or `refused`.
    pub state: String,
    /// The commit it reads, once resolved.
    pub commit: Option<String>,
    /// Why it was refused.
    pub why: Option<String>,
    /// When it reached this state.
    pub at: chrono::NaiveDateTime,
}

/// One persona the conversation can be set to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PersonaOption {
    /// `<dir>:<name>`, what the set takes.
    pub label: String,
    pub description: Option<String>,
    /// The file doesn't parse: why. It can't be picked.
    pub error: Option<String>,
}

/// The conversation's persona, or what it can be set to.
#[get("/api/conversations/{id}/persona")]
pub async fn get_top_level_persona(id: i64) -> ServerFnResult<TopLevelPersona> {
    crate::agents::top_level::get_persona(crate::db::get(), id).await.map_err(ServerFnError::new)
}

/// Sets the conversation to `persona` (`<dir>:<name>`), once, after its
/// repo is attached and before its first message. On a conversation whose
/// persona is applied, it refreshes it instead (SME-516): the same persona
/// read at `commit`, which must be on its remote's default branch (that
/// branch's tip when `None`); `refresh` in the answer says whether it
/// applied or waits for the current turn to end.
#[post("/api/conversations/{id}/persona")]
pub async fn set_top_level_persona(id: i64, persona: String, commit: Option<String>) -> ServerFnResult<SetPersona> {
    crate::agents::top_level::set_persona(crate::db::get(), id, &persona, commit.as_deref())
        .await
        .map_err(ServerFnError::new)
}
