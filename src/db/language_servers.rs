use super::*;

/// A `language_servers` row (SME-35); its list and map columns are JSON.
#[derive(Debug, sqlx::FromRow)]
struct LanguageServerRow {
    id: i64,
    name: String,
    image: String,
    install_command: String,
    command: String,
    args: sqlx::types::Json<Vec<String>>,
    env: sqlx::types::Json<std::collections::BTreeMap<String, String>>,
    file_types: sqlx::types::Json<std::collections::BTreeMap<String, String>>,
    root_markers: sqlx::types::Json<Vec<String>>,
    initialization_options: Option<serde_json::Value>,
    settings: Option<serde_json::Value>,
    memory_limit: String,
    cpu_limit: String,
    enabled: bool,
    updated_at: NaiveDateTime,
}

impl From<LanguageServerRow> for crate::models::LanguageServer {
    fn from(row: LanguageServerRow) -> Self {
        crate::models::LanguageServer {
            id: row.id,
            config: crate::models::LanguageServerConfig {
                name: row.name,
                image: row.image,
                install_command: row.install_command,
                command: row.command,
                args: row.args.0,
                env: row.env.0,
                file_types: row.file_types.0,
                root_markers: row.root_markers.0,
                initialization_options: row.initialization_options,
                settings: row.settings,
                memory_limit: row.memory_limit,
                cpu_limit: row.cpu_limit,
                enabled: row.enabled,
            },
            updated_at: row.updated_at,
        }
    }
}

const LANGUAGE_SERVER_COLUMNS: &str = "id, name, image, install_command, command, args, env, file_types, \
     root_markers, initialization_options, settings, memory_limit, cpu_limit, enabled, updated_at";

pub async fn create_language_server(
    pool: &PgPool,
    config: &crate::models::LanguageServerConfig,
) -> Result<crate::models::LanguageServer, sqlx::Error> {
    let sql = format!(
        "INSERT INTO language_servers (name, image, install_command, command, args, env, file_types, \
             root_markers, initialization_options, settings, memory_limit, cpu_limit, enabled) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) RETURNING {LANGUAGE_SERVER_COLUMNS}"
    );
    bind_language_server(sqlx::query_as::<_, LanguageServerRow>(&sql), config)
        .fetch_one(pool)
        .await
        .map(Into::into)
}

/// Replaces `id`'s settings; `None` if there's no such server.
pub async fn update_language_server(
    pool: &PgPool,
    id: i64,
    config: &crate::models::LanguageServerConfig,
) -> Result<Option<crate::models::LanguageServer>, sqlx::Error> {
    let sql = format!(
        "UPDATE language_servers SET name = $1, image = $2, install_command = $3, command = $4, args = $5, \
             env = $6, file_types = $7, root_markers = $8, initialization_options = $9, settings = $10, \
             memory_limit = $11, cpu_limit = $12, enabled = $13, updated_at = now() \
         WHERE id = $14 RETURNING {LANGUAGE_SERVER_COLUMNS}"
    );
    bind_language_server(sqlx::query_as::<_, LanguageServerRow>(&sql), config)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map(|row| row.map(Into::into))
}

fn bind_language_server<'q>(
    query: sqlx::query::QueryAs<'q, sqlx::Postgres, LanguageServerRow, sqlx::postgres::PgArguments>,
    config: &'q crate::models::LanguageServerConfig,
) -> sqlx::query::QueryAs<'q, sqlx::Postgres, LanguageServerRow, sqlx::postgres::PgArguments> {
    query
        .bind(&config.name)
        .bind(&config.image)
        .bind(&config.install_command)
        .bind(&config.command)
        .bind(sqlx::types::Json(&config.args))
        .bind(sqlx::types::Json(&config.env))
        .bind(sqlx::types::Json(&config.file_types))
        .bind(sqlx::types::Json(&config.root_markers))
        .bind(&config.initialization_options)
        .bind(&config.settings)
        .bind(&config.memory_limit)
        .bind(&config.cpu_limit)
        .bind(config.enabled)
}

pub async fn list_language_servers(pool: &PgPool) -> Result<Vec<crate::models::LanguageServer>, sqlx::Error> {
    let sql = format!("SELECT {LANGUAGE_SERVER_COLUMNS} FROM language_servers ORDER BY name");
    sqlx::query_as::<_, LanguageServerRow>(&sql)
        .fetch_all(pool)
        .await
        .map(|rows| rows.into_iter().map(Into::into).collect())
}

pub async fn get_language_server(pool: &PgPool, id: i64) -> Result<Option<crate::models::LanguageServer>, sqlx::Error> {
    let sql = format!("SELECT {LANGUAGE_SERVER_COLUMNS} FROM language_servers WHERE id = $1");
    sqlx::query_as::<_, LanguageServerRow>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map(|row| row.map(Into::into))
}

/// Deletes `id`, returning what it was (for stopping its pods), or `None`.
pub async fn delete_language_server(
    pool: &PgPool,
    id: i64,
) -> Result<Option<crate::models::LanguageServer>, sqlx::Error> {
    let sql = format!("DELETE FROM language_servers WHERE id = $1 RETURNING {LANGUAGE_SERVER_COLUMNS}");
    sqlx::query_as::<_, LanguageServerRow>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map(|row| row.map(Into::into))
}
