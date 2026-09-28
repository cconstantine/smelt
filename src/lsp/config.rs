//! Saving language server configs (SME-35), with the errors the settings
//! page shows.

use sqlx::PgPool;

use crate::db;
use crate::models::{LanguageServer, LanguageServerConfig};

/// Creates (`id` `None`) or replaces a config, after checking it.
pub async fn save(pool: &PgPool, id: Option<i64>, config: &LanguageServerConfig) -> Result<LanguageServer, String> {
    config.validate()?;
    let saved = match id {
        None => db::create_language_server(pool, config).await.map(Some),
        Some(id) => db::update_language_server(pool, id, config).await,
    };
    match saved {
        Ok(Some(server)) => Ok(server),
        Ok(None) => Err("That language server no longer exists.".to_string()),
        Err(e) if e.as_database_error().is_some_and(|d| d.is_unique_violation()) => {
            Err(format!("A language server named {} already exists.", config.name))
        }
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pyright() -> LanguageServerConfig {
        LanguageServerConfig {
            name: "pyright".to_string(),
            image: "node:22-slim".to_string(),
            command: "pyright-langserver".to_string(),
            args: vec!["--stdio".to_string()],
            file_types: [("py".to_string(), "python".to_string())].into(),
            memory_limit: "1Gi".to_string(),
            cpu_limit: "1".to_string(),
            enabled: true,
            ..Default::default()
        }
    }

    #[sqlx::test]
    async fn test_saving_checks_the_config_and_names_a_clash(pool: PgPool) {
        let created = save(&pool, None, &pyright()).await.expect("create");
        assert_eq!(created.config, pyright());

        let clash = save(&pool, None, &pyright()).await.expect_err("same name");
        assert!(clash.contains("already") && clash.contains("pyright"), "{clash}");

        let invalid = LanguageServerConfig { name: "Py Right".to_string(), ..pyright() };
        assert!(save(&pool, None, &invalid).await.expect_err("bad name").contains("name"));

        let edited = LanguageServerConfig { memory_limit: "2Gi".to_string(), ..pyright() };
        assert_eq!(save(&pool, Some(created.id), &edited).await.expect("update").config.memory_limit, "2Gi");

        let missing = save(&pool, Some(created.id + 100), &edited).await.expect_err("no such server");
        assert!(missing.contains("no longer exists"), "{missing}");
    }
}
