//! The turn engine: running a conversation's turns against its model,
//! compacting it as it nears the context window, and notices that reach
//! the model between turns. Server-only; `api::chat` holds the server
//! functions the page calls.

use dioxus::prelude::*;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use crate::api::chat::{STOP_NOTICE, TURN_STOPPED, chat_error_text};
use crate::models::Message;
use crate::{anthropic, db};

mod compaction;
mod history;
mod notify;
mod run;
mod state;
#[cfg(test)]
mod test_hooks;
#[cfg(test)]
mod tests;

use self::compaction::*;
pub(crate) use self::history::*;
pub(crate) use self::notify::*;
pub(crate) use self::run::*;
pub(crate) use self::state::*;
