//! `start_agent`: the checks and the transaction that make a child, then
//! its start, a spawned task that ends with the child `active` and woken,
//! or `failed` with the error as its report.

use super::*;

/// What a child's start does besides its done step: the child's checkout,
/// when it has one, and the persona to read in it.
pub(crate) struct StartRequest {
    repo: Option<(db::ConversationRepo, crate::git::CloneTarget)>,
    persona: Option<String>,
}

/// What the start's steps found, for its done step.
#[derive(Default)]
struct Prepared {
    persona: Option<db::PersonaSnapshot>,
    model: Option<(i64, String)>,
    parent_notice: Option<String>,
}

/// `start_agent`'s `repo`: one of the caller's `ready` checkouts, by its
/// `/workspace/<dir>` path.
async fn parent_checkout(pool: &PgPool, caller: i64, path: &str) -> Result<db::ConversationRepo, String> {
    let repos = db::list_conversation_repos(pool, caller)
        .await
        .map_err(|e| format!("couldn't read this conversation's repos: {e}"))?;
    let dir = path
        .trim()
        .trim_end_matches('/')
        .strip_prefix(crate::sandbox::WORKSPACE_DIR)
        .and_then(|rest| rest.strip_prefix('/'))
        .filter(|dir| !dir.contains('/'));
    let ready: Vec<String> = repos
        .iter()
        .filter(|r| r.status == "ready")
        .map(|r| format!("{}/{}", crate::sandbox::WORKSPACE_DIR, r.dir))
        .collect();
    match repos.into_iter().find(|r| Some(r.dir.as_str()) == dir) {
        Some(repo) if repo.status == "ready" => Ok(repo),
        Some(repo) => Err(format!("{path} isn't ready to share (it's {})", repo.status)),
        None if ready.is_empty() => Err(format!("{path} isn't one of your checkouts, and you have none ready")),
        None => Err(format!("{path} isn't one of your checkouts; yours: {}", ready.join(", "))),
    }
}

/// An optional string field of a tool's input.
fn optional_str<'a>(input: &'a serde_json::Value, field: &str) -> Result<Option<&'a str>, String> {
    match input.get(field) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => value.as_str().map(Some).ok_or_else(|| format!("{field} must be a string")),
    }
}

/// `start_agent`'s run: checks the input, records the child (`starting`)
/// with its brief as its first message, spawns its start, and returns the
/// child's id at once.
pub(crate) async fn start_agent(pool: &PgPool, caller: i64, input: &serde_json::Value) -> Result<String, String> {
    let brief = input
        .get("brief")
        .and_then(serde_json::Value::as_str)
        .ok_or("missing or non-string field: brief")?;
    let chars = brief.chars().count();
    if brief.trim().is_empty() || chars > MAX_BRIEF_CHARS {
        return Err(format!("brief must be 1 to {MAX_BRIEF_CHARS} characters (it has {chars})"));
    }
    let repo_path = optional_str(input, "repo")?;
    let git_ref = optional_str(input, "ref")?;
    if git_ref.is_some() && repo_path.is_none() {
        return Err("ref needs repo: the checkout whose remote it's on".to_string());
    }
    let persona = optional_str(input, "persona")?;
    if let Some(name) = persona {
        if repo_path.is_none() {
            return Err("persona needs repo: the checkout whose .agents/personas/ it's in".to_string());
        }
        persona::check_name(name)?;
    }
    let repo = match repo_path {
        None => None,
        Some(path) => {
            let parent_repo = parent_checkout(pool, caller, path).await?;
            let target = match git_ref {
                Some(git_ref) => {
                    crate::git::validate_ref(git_ref)?;
                    crate::git::CloneTarget::Ref(git_ref.to_string())
                }
                // The parent's exact commit, which must be on its upstream.
                None => {
                    if let Some(name) = persona {
                        persona::check_in_parent(pool, caller, &parent_repo.dir, name).await?;
                    }
                    let (sha, branch) = checkout_for(caller).parent_commit(pool, caller, &parent_repo.dir).await?;
                    // Read in the parent's pod, which its model controls, and
                    // passed to the child's git as arguments (review 2).
                    crate::git::validate_commit_id(&sha)?;
                    crate::git::validate_ref(&branch)?;
                    crate::git::CloneTarget::Commit { sha, branch }
                }
            };
            Some((parent_repo, target))
        }
    };
    // The parent's model, resolved now: the child runs on it.
    let model = crate::providers::conversation_model(pool, caller)
        .await
        .map_err(|e| format!("couldn't read this conversation's model: {e}"))?
        .as_ref()
        .and_then(crate::providers::ConversationModel::choice)
        .map(|choice| (choice.provider_id, choice.model.clone()));
    let branch = repo.as_ref().and_then(|(_, target)| crate::git::requested_branch(target));
    let new = db::NewChild {
        caller,
        brief,
        persona_name: persona,
        model,
        max_depth: agent_depth(),
        repo: repo.as_ref().map(|(parent_repo, _)| db::NewChildRepo {
            url: &parent_repo.url,
            remote_key: &parent_repo.remote_key,
            dir: &parent_repo.dir,
            branch: branch.as_deref(),
            parent_repo_id: parent_repo.id,
        }),
    };
    let (child, child_repo) = match db::create_child(pool, &new).await.map_err(|e| format!("couldn't start the agent: {e}"))? {
        db::CreateChild::Created { child, repo } => (child, repo),
        db::CreateChild::Refused { why, asked } => {
            if asked {
                // The card must reach open tabs, as load_instructions' does.
                crate::git::publish_repos(pool, caller).await;
            }
            return Err(why);
        }
    };
    crate::events::publish_app(crate::events::AppEvent::ConversationsChanged);
    let on = match &repo {
        Some((parent_repo, crate::git::CloneTarget::Ref(r))) => format!(" on {} at {r}", parent_repo.url),
        Some((parent_repo, crate::git::CloneTarget::Commit { sha, branch })) => {
            format!(" on {} at your commit {sha} (branch {branch})", parent_repo.url)
        }
        None => String::new(),
    };
    let named = match (&repo, persona) {
        (Some((parent_repo, _)), Some(name)) => format!("{}:{name}", parent_repo.dir),
        _ => "no persona".to_string(),
    };
    let request = StartRequest {
        repo: child_repo.zip(repo.map(|(_, target)| target)),
        persona: persona.map(str::to_string),
    };
    let start_pool = pool.clone();
    // In the tool call's span, so the start is its child.
    tokio::spawn(tracing::Instrument::in_current_span(async move {
        run_start(&start_pool, child, caller, request).await;
    }));
    Ok(format!(
        "Started {} ({named}){on}. It works on its brief in its own conversation; its report \
         arrives here as a message when it calls finish, message_agent writes to it, and \
         list_agents shows its status.",
        label(child, None)
    ))
}

/// The start notice, the child's own: what its first turn drains.
fn start_notice() -> String {
    "Your start is done: work on the brief above. When your work is done, call finish with your \
     report. The user can also write to you here."
        .to_string()
}

/// The child's start: ends with it `active` and its first turn woken, or
/// `failed` (`fail_start`). A child stopped or deleted meanwhile is left
/// as it is. `StartExit` cleans up on every way out, a dropped task too.
async fn run_start(pool: &PgPool, child: i64, parent: i64, request: StartRequest) {
    let mut exit = StartExit { pool: pool.clone(), child, done: false };
    let checkout = checkout_for(parent);
    let prepared = match prepare(pool, checkout.as_ref(), child, parent, request).await {
        Ok(prepared) => prepared,
        Err(error) => {
            fail_start(pool, child, parent, &error).await;
            return;
        }
    };
    #[cfg(test)]
    test_hooks::reach(test_hooks::BEFORE_START_DONE, parent).await;
    let label = label(child, prepared.persona.as_ref().and_then(|p| persona_label(&p.path)).as_deref());
    let notice = start_notice();
    let done = db::StartDone {
        notice: &notice,
        persona: prepared.persona,
        model: prepared.model,
        parent_notice: prepared.parent_notice,
        label: &label,
    };
    match db::complete_start(pool, child, parent, &done).await {
        Ok(true) => {
            exit.done = true;
            crate::turn::notify(pool, child, vec![crate::turn::Notice::Wake]);
        }
        Ok(false) => tracing::info!(conversation_id = child, "the agent was stopped or deleted during its start"),
        Err(e) => fail_start(pool, child, parent, &format!("couldn't complete the start: {e}")).await,
    }
}

/// The start's steps before its done step: with a repo, its clone at the
/// child's ref, the remote's trust read again, its top-level AGENTS.md
/// loaded, and its persona read there, with the persona's model.
async fn prepare(
    pool: &PgPool,
    checkout: &dyn ChildCheckout,
    child: i64,
    parent: i64,
    request: StartRequest,
) -> Result<Prepared, String> {
    #[cfg(test)]
    if let Some(error) = test_hooks::injected_start_failure(parent) {
        return Err(error);
    }
    let _ = parent;
    let Some((repo, target)) = request.repo else {
        return Ok(Prepared::default());
    };
    checkout.clone_repo(pool, child, &repo, &target).await?;
    // The user may have forgotten the remote while it cloned.
    if remote_trust(pool, &repo.remote_key).await? != Some(true) {
        return Err(format!("the user no longer trusts {}", repo.url));
    }
    let repo = db::get_conversation_repo(pool, repo.id)
        .await
        .map_err(|e| format!("couldn't read the agent's checkout: {e}"))?
        .ok_or("the agent's checkout is gone")?;
    if repo.agents_files.iter().any(|f| f == "AGENTS.md") {
        load_agents_md(pool, checkout, child, parent, &repo).await?;
    }
    let Some(name) = request.persona else {
        return Ok(Prepared::default());
    };
    let snapshot = persona::snapshot(pool, checkout, child, &repo, &name).await?;
    let (model, parent_notice) = persona::persona_model(pool, child, &snapshot).await?;
    Ok(Prepared { persona: Some(snapshot), model, parent_notice })
}

/// Loads the child's top-level AGENTS.md, which needs no card: the remote
/// is trusted. One forgotten since the last read gets a card instead,
/// which fails the start (its exit deletes the card).
async fn load_agents_md(
    pool: &PgPool,
    checkout: &dyn ChildCheckout,
    child: i64,
    parent: i64,
    repo: &db::ConversationRepo,
) -> Result<(), String> {
    let Some(mut file) = checkout.read_file(pool, child, &repo.dir, "AGENTS.md").await? else {
        return Ok(());
    };
    file.commit = repo.commit_sha.clone();
    #[cfg(test)]
    test_hooks::reach(test_hooks::BEFORE_AGENTS_MD_LOAD, parent).await;
    let _ = parent;
    match crate::git::request_or_load(pool, child, repo, "AGENTS.md", &file).await? {
        crate::git::LoadOutcome::Loaded => Ok(()),
        crate::git::LoadOutcome::AwaitingTrust => Err(format!("the user no longer trusts {}", repo.url)),
    }
}

/// Whether the user trusts `remote_key`, read under its trust lock.
async fn remote_trust(pool: &PgPool, remote_key: &str) -> Result<Option<bool>, String> {
    let e = |e: sqlx::Error| format!("couldn't read the remote's trust: {e}");
    let mut tx = pool.begin().await.map_err(e)?;
    db::lock_remote_trust(&mut *tx, remote_key).await.map_err(e)?;
    let trusted = db::get_repo_trust(&mut *tx, remote_key).await.map_err(e)?;
    tx.commit().await.map_err(e)?;
    Ok(trusted)
}

/// Ends a start that failed: the child `failed`, the error queued to its
/// parent as its report and saved in its own transcript, then the parent
/// woken. A child no longer starting (stopped or deleted meanwhile) gets
/// nothing.
pub(crate) async fn fail_start(pool: &PgPool, child: i64, parent: i64, error: &str) {
    let label = label(child, None);
    let text = format!("{label}'s start failed: {error}");
    match db::fail_start(pool, child, parent, &label, &text).await {
        Ok(true) => {
            crate::turn::notify(pool, parent, vec![crate::turn::Notice::Wake]);
            crate::events::publish_app(crate::events::AppEvent::ConversationsChanged);
        }
        Ok(false) => tracing::info!(conversation_id = child, "a failed start's agent was stopped or deleted meanwhile"),
        Err(e) => tracing::warn!(conversation_id = child, error = %e, start_error = %error, "couldn't record a failed start"),
    }
}

/// Held by a start from its first step: on every way out (done, failed,
/// the child stopped or deleted, the task dropped) it removes the start's
/// credentials and, unless the start is done, deletes any AGENTS.md trust
/// card in the child, in a task of its own, as `CloneGuard`'s cleanup runs.
struct StartExit {
    pool: PgPool,
    child: i64,
    /// The start's done step went through: the child is `active`.
    done: bool,
}

impl Drop for StartExit {
    fn drop(&mut self) {
        let (pool, child, done) = (self.pool.clone(), self.child, self.done);
        tokio::spawn(async move {
            end_start(&pool, child, done).await;
        });
    }
}

/// What every way out of a start leaves done: the start's credentials
/// removed and, unless the start is `done`, no trust card left in the
/// child. A done start's AGENTS.md load made no card (a card fails the
/// start), and the child's first turn, already woken, may have asked about
/// another file by now (review 1), so its cards are left alone.
pub(crate) async fn end_start(pool: &PgPool, child: i64, done: bool) {
    #[cfg(test)]
    test_hooks::reach(test_hooks::BEFORE_START_EXIT, child).await;
    if !done {
        match db::delete_instruction_requests_of(pool, child).await {
            Ok(0) => {}
            Ok(_) => crate::git::publish_repos(pool, child).await,
            Err(e) => tracing::warn!(conversation_id = child, error = %e, "couldn't delete an agent's trust cards"),
        }
    }
    drop_start_credentials(child).await;
    #[cfg(test)]
    test_hooks::start_ended(child);
}

/// Removes the SSH key from a child that mustn't keep it. Every child
/// keeps it until SME-242 says which mustn't (`agents.credentials`), as
/// every pod does today; the exits that call this are built here.
async fn drop_start_credentials(child: i64) {
    let _ = child;
}
