//! The turn engine: running a conversation's turns against its model,
//! compacting it as it nears the context window, and notices that reach
//! the model between turns. Server-only; `api::chat` holds the server
//! functions the page calls.

use dioxus::prelude::*;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use crate::api::chat::{STOP_NOTICE, TURN_FAILED_NOTED, TURN_STOPPED, chat_error_text};
use crate::models::Message;
use crate::{anthropic, db};

mod call;
mod compaction;
mod compaction_progress;
mod compaction_transcript;
mod failure;
mod history;
mod lifecycle;
mod notify;
mod on_request;
mod overflow;
mod outside_turn;
mod questions;
mod reply;
mod room;
mod run;
mod state;
#[cfg(test)]
pub(crate) mod test_hooks;
#[cfg(test)]
pub(crate) mod tests;

use self::call::*;
use self::compaction::*;
#[cfg(feature = "server")]
pub(crate) use self::compaction::pinned_todos_text;
use self::compaction_progress::{CompactionProgress, CompactionStep, FloorAnswer, MIN_USEFUL_REPLY};
#[cfg(test)]
use self::compaction_progress::{END_AT, NOTICE_AT};
use self::compaction_transcript::*;
pub(crate) use self::failure::*;
pub(crate) use self::history::*;
pub(crate) use self::lifecycle::*;
pub(crate) use self::notify::*;
pub(crate) use self::on_request::{compact_now, request_compaction};
use self::on_request::*;
use self::overflow::recover as overflow_recovery;
pub(crate) use self::overflow::reported_per_request;
pub(crate) use self::outside_turn::*;
pub(crate) use self::questions::*;
use self::reply::*;
use self::room::*;
use self::run::*;
pub(crate) use self::state::*;
