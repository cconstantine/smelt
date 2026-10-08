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

/// The servers whose running pods a change to a config stops: all of a
/// deleted or disabled server's, and a renamed server's under its old name
/// (SME-35's state model). Other edits reach a running server when it's
/// restarted.
pub fn servers_to_stop(before: Option<&LanguageServerConfig>, after: Option<&LanguageServerConfig>) -> Vec<String> {
    match (before, after) {
        (Some(before), None) => vec![before.name.clone()],
        (Some(before), Some(after)) if !after.enabled || after.name != before.name => vec![before.name.clone()],
        _ => Vec::new(),
    }
}

/// Stops the pods `servers_to_stop` names, logging (not failing on) a
/// cluster that can't be reached: the config change itself is saved.
pub async fn stop_servers(pool: &sqlx::PgPool, names: Vec<String>) {
    let client = match crate::sandbox::kube_client() {
        Ok(client) => client,
        Err(e) => {
            tracing::warn!(servers = ?names, error = %e, "couldn't stop language servers' pods");
            return;
        }
    };
    let instance = match crate::db::smelt_instance(pool).await {
        Ok(instance) => instance.id,
        Err(e) => {
            tracing::warn!(servers = ?names, error = %e, "couldn't read this database's instance to stop language servers");
            return;
        }
    };
    for name in names {
        if let Err(e) = crate::lsp::pods::stop_everywhere_with(&client, &name, &instance).await {
            tracing::warn!(server = %name, error = %e, "couldn't stop a language server's pods");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deleting_disabling_or_renaming_stops_a_servers_pods() {
        let on = pyright();
        let off = LanguageServerConfig { enabled: false, ..pyright() };
        let renamed = LanguageServerConfig { name: "pyright2".to_string(), ..pyright() };
        let edited = LanguageServerConfig { memory_limit: "4Gi".to_string(), ..pyright() };
        assert_eq!(servers_to_stop(Some(&on), None), vec!["pyright"]);
        assert_eq!(servers_to_stop(Some(&on), Some(&off)), vec!["pyright"]);
        assert_eq!(servers_to_stop(Some(&on), Some(&renamed)), vec!["pyright"]);
        assert!(servers_to_stop(Some(&on), Some(&edited)).is_empty(), "an edit waits for a restart");
        assert!(servers_to_stop(None, Some(&on)).is_empty());
    }

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
