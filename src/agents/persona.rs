//! A child's persona (SME-236): checked in the parent's checkout when
//! `start_agent` is called without a ref, read at the child's own ref
//! during its start and kept as a snapshot, with its model resolved
//! (docs/design/personas/persona-files.md).

use super::*;

/// A persona's path in a checkout.
fn persona_file(name: &str) -> String {
    format!(".agents/personas/{name}.md")
}

/// `start_agent`'s `persona`: a name a persona file can have.
pub(crate) fn check_name(name: &str) -> Result<(), String> {
    if crate::personas::is_persona_file_name(&format!("{name}.md")) {
        Ok(())
    } else {
        Err(format!(
            "{name:?} isn't a persona name: 1 to 64 lowercase letters, digits and -, as in \
             .agents/personas/<name>.md (list_personas lists them)"
        ))
    }
}

/// Without a ref, the persona must be in the parent's checkout now, so a
/// typo is refused before anything is made.
pub(crate) async fn check_in_parent(pool: &PgPool, parent: i64, dir: &str, name: &str) -> Result<(), String> {
    let paths = checkout_for(parent).parent_persona_paths(pool, parent, dir).await?;
    if paths.iter().any(|p| *p == persona_file(name)) {
        return Ok(());
    }
    let names: Vec<&str> = paths
        .iter()
        .filter_map(|p| p.strip_prefix(".agents/personas/").and_then(|f| f.strip_suffix(".md")))
        .collect();
    let theirs = if names.is_empty() { "none".to_string() } else { names.join(", ") };
    Err(format!(
        "no persona `{name}` in {}/{dir} (its personas: {theirs}); list_personas lists them",
        crate::sandbox::WORKSPACE_DIR
    ))
}

/// The persona `name` read in the child's checkout at its ref, parsed, and
/// its relative links made absolute: what the child runs under.
pub(crate) async fn snapshot(
    pool: &PgPool,
    checkout: &dyn ChildCheckout,
    child: i64,
    repo: &db::ConversationRepo,
    name: &str,
) -> Result<db::PersonaSnapshot, String> {
    let rel = persona_file(name);
    let at = repo.commit_sha.as_deref().unwrap_or("its ref");
    let file = checkout
        .read_file(pool, child, &repo.dir, &rel)
        .await?
        .ok_or_else(|| format!("no persona `{name}` in {} at {at}", repo.url))?;
    let persona = crate::personas::parse(&rel, &file.content, file.file_bytes.max(0) as u64)?;
    let checkout_dir = format!("{}/{}", crate::sandbox::WORKSPACE_DIR, repo.dir);
    let body = crate::personas::links::absolute(&persona.instructions, &format!("{checkout_dir}/.agents/personas"));
    Ok(db::PersonaSnapshot {
        name: persona.name,
        path: format!("{checkout_dir}/{rel}"),
        commit: repo.commit_sha.clone(),
        body,
        // `tools` rides along so the start can append it to the child's
        // rule chain without re-reading the file (SME-241);
        // `credentials` so Rule S can grant this conversation's own
        // secrets from the snapshot alone (SME-242).
        settings: serde_json::json!({
            "model": persona.model,
            "tools": persona.tools,
            "credentials": persona.credentials,
        }),
    })
}

/// The rules note for a child's persona snapshot: which of its
/// `smelt.tools` patterns name no configured server (SME-241's report —
/// what a rename leaves behind), said to its parent on the notice channel.
/// A snapshot with no rules, or a server list that can't be read, says
/// nothing: an unknown list mustn't accuse every pattern.
pub(crate) async fn rules_notice(pool: &PgPool, child: i64, persona: &db::PersonaSnapshot) -> Option<String> {
    let rules = persona
        .settings
        .get("tools")
        .filter(|tools| !tools.is_null())
        .and_then(|tools| serde_json::from_value::<crate::personas::rules::ToolRules>(tools.clone()).ok())?;
    let servers: Vec<String> = match db::list_mcp_server_configs(pool).await {
        Ok(configs) => configs.into_iter().map(|config| config.name).collect(),
        Err(e) => {
            tracing::warn!(error = %e, "couldn't read the configured MCP servers for the agent's rules note");
            return None;
        }
    };
    let note = crate::personas::rules::unmatched_note(&rules.unmatched_servers(&servers))?;
    Some(format!("{}'s {note}", label(child, crate::agents::persona_label(&persona.path).as_deref())))
}

/// The model a persona asks for (`smelt.model`), on a provider that has
/// it; `None` keeps the parent's. When no provider has it, the second is
/// the notice its parent gets.
pub(crate) async fn persona_model(
    pool: &PgPool,
    child: i64,
    persona: &db::PersonaSnapshot,
) -> Result<(Option<(i64, String)>, Option<String>), String> {
    let Some(model) = persona.settings.get("model").and_then(serde_json::Value::as_str) else {
        return Ok((None, None));
    };
    let current = db::get_conversation_model(pool, child)
        .await
        .map_err(|e| format!("couldn't read the agent's model: {e}"))?;
    let parent_provider = current.as_ref().and_then(|row| row.provider_id);
    match db::provider_for_model(pool, model, parent_provider)
        .await
        .map_err(|e| format!("couldn't look up the persona's model: {e}"))?
    {
        Some(provider_id) => Ok((Some((provider_id, model.to_string())), None)),
        None => {
            let runs_on = current.and_then(|row| row.model).unwrap_or_else(|| "the default model".to_string());
            Ok((
                None,
                Some(format!(
                    "{}'s persona asks for the model `{model}`, which none of the providers has; it runs on {runs_on} instead",
                    label(child, crate::agents::persona_label(&persona.path).as_deref())
                )),
            ))
        }
    }
}
