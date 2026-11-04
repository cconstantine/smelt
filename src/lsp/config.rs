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

/// The config and version server `name` starts with, when its saved
/// install is a GitHub release's from before SME-360, which downloads the
/// x86-64 asset on every node: looked up in the catalog (mason's registry
/// at `registry`, Helix's file at `helix`) under its name, and when the
/// saved command is that package's old suggestion, unedited, the row takes
/// the per-arch install today's suggestion has, so an arm64 node installs
/// an arm64 binary (SME-386 A3). `None` leaves the start as saved: another
/// install, a lookup that fails, an edited command, or an edit saved since.
pub async fn rederive_github_install(
    pool: &PgPool,
    name: &str,
    registry: &str,
    helix: &str,
) -> Option<(LanguageServerConfig, String)> {
    let server = match db::list_language_servers(pool).await {
        Ok(servers) => servers.into_iter().find(|s| s.config.name == name)?,
        Err(e) => {
            tracing::warn!(server = %name, error = %e, "couldn't read a language server to re-derive its install");
            return None;
        }
    };
    let was = &server.config.install_command;
    if !crate::lsp::catalog::is_single_arch_github_install(was) {
        return None;
    }
    let suggested = match crate::lsp::catalog::lookup(name, registry, helix).await {
        Ok(suggestion) => suggestion.config.install_command,
        Err(e) => {
            tracing::warn!(server = %name, error = %e, "couldn't look a language server up to re-derive its install");
            return None;
        }
    };
    let install = crate::lsp::catalog::rederived_install(was, &suggested)?;
    match db::replace_language_server_install(pool, server.id, was, &install).await {
        Ok(updated) => updated.map(|s| (s.config, s.updated_at.to_string())),
        Err(e) => {
            tracing::warn!(server = %name, error = %e, "couldn't save a language server's re-derived install");
            None
        }
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

    const HELIX: &str = include_str!("fixtures/helix-languages-excerpt.toml");
    /// What the catalog suggested for rust-analyzer before SME-360: the
    /// x86-64 asset on every node.
    const OLD_INSTALL: &str = "mkdir -p /tmp/home/bin && curl -fsSL \
        https://github.com/rust-lang/rust-analyzer/releases/download/2026-09-21/rust-analyzer-x86_64-unknown-linux-gnu.gz \
        | gunzip > /tmp/home/bin/rust-analyzer && chmod +x /tmp/home/bin/rust-analyzer";

    /// A stand-in for mason's registry, with rust-analyzer, and Helix's file.
    async fn catalog() -> String {
        let router = axum::Router::new()
            .route(
                "/packages/rust-analyzer/package.yaml",
                axum::routing::get(|| async { include_str!("fixtures/mason-rust-analyzer.yaml") }),
            )
            .route("/languages.toml", axum::routing::get(|| async { HELIX }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, router).await.expect("serve") });
        format!("http://{addr}")
    }

    fn rust_analyzer(install: &str) -> LanguageServerConfig {
        let yaml = include_str!("fixtures/mason-rust-analyzer.yaml");
        let config = crate::lsp::catalog::suggest(yaml, HELIX).expect("a suggestion").config;
        LanguageServerConfig { install_command: install.to_string(), ..config }
    }

    /// SME-386 A3: a rust-analyzer row saved before SME-360 still installed
    /// the x86-64 release on an arm64 node. Its start re-derives it, and the
    /// row keeps the per-arch install; an edited command is left as saved.
    #[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
    async fn test_a_github_install_saved_before_multiarch_is_rederived(pool: sqlx::PgPool) {
        let base = catalog().await;
        let helix = format!("{base}/languages.toml");
        let saved = save(&pool, None, &rust_analyzer(OLD_INSTALL)).await.expect("saved");
        assert!(crate::lsp::catalog::is_single_arch_github_install(&saved.config.install_command));
        let (config, version) = rederive_github_install(&pool, "rust-analyzer", &base, &helix)
            .await
            .expect("the old suggestion is re-derived");
        assert!(config.install_command.starts_with("case \"$(uname -m)\" in x86_64) "), "{}", config.install_command);
        assert!(config.install_command.contains("rust-analyzer-aarch64-unknown-linux-gnu.gz"), "{}", config.install_command);
        let row = db::get_language_server(&pool, saved.id).await.expect("read").expect("the row");
        assert_eq!(row.config, config, "the row keeps it");
        assert_eq!(row.updated_at.to_string(), version, "the version the start labels its pod with");
        assert_ne!(version, saved.updated_at.to_string());
        assert_eq!(rederive_github_install(&pool, "rust-analyzer", &base, &helix).await, None, "once is enough");

        // The user's own edit of a single-arch command stays theirs.
        let edited = format!("{OLD_INSTALL} && echo mine");
        save(&pool, Some(saved.id), &rust_analyzer(&edited)).await.expect("edited");
        assert_eq!(rederive_github_install(&pool, "rust-analyzer", &base, &helix).await, None);
        let row = db::get_language_server(&pool, saved.id).await.expect("read").expect("the row");
        assert_eq!(row.config.install_command, edited);
    }

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

    #[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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
