//! A child's persona (SME-236): checked in the parent's checkout when
//! `start_agent` is called without a ref, read at the child's own ref
//! during its start and kept as a snapshot, with its model resolved
//! (docs/design/personas/persona-files.md).

use super::*;

/// A persona's path in a checkout.
fn persona_file(name: &str) -> String {
    format!(".agents/personas/{name}.md")
}

/// `start_agent`'s `persona`, `<repo dir>:<name>` as list_personas names
/// it, split at its last `:` (a name holds none); a bare name has no dir.
pub(crate) fn split_name(persona: &str) -> (Option<&str>, &str) {
    match persona.rsplit_once(':') {
        Some((dir, name)) => (Some(dir), name),
        None => (None, persona),
    }
}

/// `start_agent`'s `persona`: a name a persona file can have, scoped to a
/// checkout dir or bare.
pub(crate) fn check_name(persona: &str) -> Result<(), String> {
    let (dir, name) = split_name(persona);
    if dir != Some("") && crate::personas::is_persona_file_name(&format!("{name}.md")) {
        Ok(())
    } else {
        Err(format!(
            "{persona:?} isn't a persona name: <repo dir>:<name>, the name 1 to 64 lowercase letters, \
             digits and -, as in smelt:developer, or developer with repo (list_personas lists them)"
        ))
    }
}

/// `start_agent`'s `persona` and `repo`, checked against each other: the
/// bare persona name and the checkout path. A scoped name supplies its
/// repo; a bare one needs `repo`, and without it is refused naming the
/// scoped names that match (SME-368).
pub(crate) async fn with_repo<'a>(
    pool: &PgPool,
    caller: i64,
    persona: Option<&'a str>,
    repo: Option<&'a str>,
) -> Result<(Option<&'a str>, Option<String>), String> {
    let Some(persona) = persona else {
        return Ok((None, repo.map(str::to_string)));
    };
    check_name(persona)?;
    let (dir, name) = split_name(persona);
    match (dir, repo) {
        (Some(dir), None) => Ok((Some(name), Some(format!("{}/{dir}", crate::sandbox::WORKSPACE_DIR)))),
        (Some(dir), Some(path)) => {
            let named = format!("{}/{dir}", crate::sandbox::WORKSPACE_DIR);
            if path.trim().trim_end_matches('/') == named {
                Ok((Some(name), Some(path.to_string())))
            } else {
                Err(format!("persona `{persona}` is in {named}, but repo is {path}; drop repo or make them agree"))
            }
        }
        (None, Some(path)) => Ok((Some(name), Some(path.to_string()))),
        (None, None) => Err(bare_refusal(pool, caller, name).await),
    }
}

/// A bare persona name with no repo, refused: the scoped names it could
/// mean, from the caller's ready checkouts' recorded persona files (the
/// clone's, refreshed by list_personas). A hint only: the start itself
/// checks the checkout.
async fn bare_refusal(pool: &PgPool, caller: i64, name: &str) -> String {
    let file = persona_file(name);
    let scoped: Vec<String> = match db::list_conversation_repos(pool, caller).await {
        Ok(repos) => repos
            .into_iter()
            .filter(|r| r.status == "ready" && r.persona_files.contains(&file))
            .map(|r| format!("{}:{name}", r.dir))
            .collect(),
        Err(e) => {
            tracing::warn!(error = %e, "couldn't read the checkouts for a bare persona's refusal");
            Vec::new()
        }
    };
    let hint = if scoped.is_empty() {
        "no checkout of yours has one; list_personas lists them".to_string()
    } else {
        scoped.join(", ")
    };
    format!("persona `{name}` needs its repo: name it scoped, as list_personas does ({hint})")
}

/// Without a ref, the persona must be in the parent's checkout now, so a
/// typo is refused before anything is made.
pub(crate) async fn check_in_parent(pool: &PgPool, parent: i64, dir: &str, name: &str) -> Result<(), String> {
    let paths = checkout_for(parent).parent_persona_paths(pool, parent, dir).await?;
    if paths.iter().any(|p| *p == persona_file(name)) {
        return Ok(());
    }
    let names: Vec<String> = paths
        .iter()
        .filter_map(|p| p.strip_prefix(".agents/personas/").and_then(|f| f.strip_suffix(".md")))
        .map(|n| format!("{dir}:{n}"))
        .collect();
    let theirs = if names.is_empty() { "none".to_string() } else { names.join(", ") };
    Err(format!(
        "no persona `{dir}:{name}` in {}/{dir} (its personas: {theirs}); list_personas lists them",
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
        // Named as start_agent takes it, `<repo dir>:<name>`, like the other
        // refusals (SME-368; SME-386 A8).
        .ok_or_else(|| format!("no persona `{}:{name}` in {} at {at}", repo.dir, repo.url))?;
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
