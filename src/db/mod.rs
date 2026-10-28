use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};

use crate::anthropic::ContentBlock;
use crate::models::{Conversation, Message};

static POOL: OnceLock<PgPool> = OnceLock::new();

const CONNECT_RETRIES: u32 = 10;
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(500);

pub async fn init() -> &'static PgPool {
    #[expect(clippy::expect_used, reason = "startup: smelt can't run without its database")]
    let db_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");

    #[expect(clippy::expect_used, reason = "startup: smelt can't run without its database")]
    let options = db_url
        .parse::<PgConnectOptions>()
        .expect("Invalid DATABASE_URL");

    // A freshly-started container doesn't mean Postgres is accepting
    // connections yet — especially on first boot, while it initializes its
    // data directory. A bounded retry loop tolerates that startup window
    // without hanging forever if Postgres is genuinely misconfigured.
    let mut attempt = 0;
    let pool = loop {
        attempt += 1;
        match PgPoolOptions::new()
            .max_connections(5)
            .connect_with(options.clone())
            .await
        {
            Ok(pool) => break pool,
            Err(err) if attempt < CONNECT_RETRIES => {
                tracing::warn!(
                    "failed to connect to Postgres (attempt {attempt}/{CONNECT_RETRIES}): {err}"
                );
                tokio::time::sleep(CONNECT_RETRY_DELAY).await;
            }
            Err(err) => {
                panic!("Failed to connect to Postgres after {CONNECT_RETRIES} attempts: {err}")
            }
        }
    };

    #[expect(clippy::expect_used, reason = "a second db::init() is a programming error")]
    POOL.set(pool).expect("Database already initialized");
    get()
}

#[expect(clippy::expect_used, reason = "db::get() before db::init() is a programming error, by design")]
pub fn get() -> &'static PgPool {
    POOL.get()
        .expect("Database not initialized. Call db::init() first.")
}

mod agents;
mod conversations;
mod usage;
mod questions;
mod sandbox;
mod volumes;
mod git;
mod repos;
mod mcp;
mod language_servers;
mod providers;

pub use agents::*;
pub use conversations::*;
pub use usage::*;
pub use questions::*;
pub use sandbox::*;
pub use volumes::*;
pub use git::*;
pub use repos::*;
pub use mcp::*;
pub use language_servers::*;
pub use providers::*;

#[cfg(test)]
mod tests;

/// Database helpers for tests.
#[cfg(all(test, feature = "server"))]
pub(crate) mod test_support {
    use sqlx::PgPool;

    /// Moves every id sequence in this test database to a base of their
    /// own, different on every call, and returns it, used or not (unlike
    /// `start_ids_clear_if_fresh`, which `main` runs — that one moves the
    /// production list). The browser tier shares its namespace with the
    /// real-cluster unit tests, whose databases also count from 1: a run
    /// that met a pod an earlier run was still stopping waited it out
    /// and failed (SME-99); moving everything, not a list, is what keeps
    /// the next table whose ids name cluster objects out of the way too
    /// (SME-255).
    pub(crate) async fn start_ids_clear_of_other_runs(pool: &PgPool) -> Result<i64, sqlx::Error> {
        let base = super::clear_ids_base();
        super::move_id_sequences(pool, base).await?;
        Ok(base)
    }
}
