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

/// `start_agent`'s optional `volumes`: the names of the volumes the caller
/// holds, to mount in the child's pod too (SME-239). Kept in the order asked
/// and deduplicated, so a name written twice is one mount row; whether the
/// caller holds each name is `create_child`'s check, inside its transaction.
fn optional_volumes(input: &serde_json::Value) -> Result<Vec<&str>, String> {
    match input.get("volumes") {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::Array(values)) => {
            let mut names = Vec::new();
            for value in values {
                let name = value.as_str().ok_or("volumes must be an array of volume names")?;
                if !names.contains(&name) {
                    names.push(name);
                }
            }
            Ok(names)
        }
        Some(_) => Err("volumes must be an array of volume names".to_string()),
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
    let (persona, repo_path) =
        persona::with_repo(pool, caller, optional_str(input, "persona")?, optional_str(input, "repo")?).await?;
    let repo_path = repo_path.as_deref();
    let git_ref = optional_str(input, "ref")?;
    if git_ref.is_some() && repo_path.is_none() {
        return Err("ref needs repo: the checkout whose remote it's on".to_string());
    }
    let volumes = optional_volumes(input)?;
    // What the child's pods ask of their node, from the persona in the
    // parent's checkout (SME-484).
    let mut pod_resources = None;
    let repo = match repo_path {
        None => None,
        Some(path) => {
            let parent_repo = parent_checkout(pool, caller, path).await?;
            let target = match git_ref {
                Some(git_ref) => {
                    crate::git::validate_ref(git_ref)?;
                    // Best-effort (M1): the parent's tree, not the ref, so
                    // anything it can't give is no refusal; the persona only
                    // has to be at the ref, and the start re-clones when the
                    // ref's file asks for other room.
                    if let Some(name) = persona {
                        let checkout = checkout_for(caller);
                        pod_resources = persona::parent_resources(pool, checkout.as_ref(), caller, &parent_repo.dir, name)
                            .await
                            .unwrap_or_else(|e| {
                                tracing::debug!(error = %e, "couldn't read the persona in the parent's tree for a ref start");
                                None
                            });
                    }
                    crate::git::CloneTarget::Ref(git_ref.to_string())
                }
                // The parent's exact commit, which must be on its upstream.
                None => {
                    if let Some(name) = persona {
                        persona::check_in_parent(pool, caller, &parent_repo.dir, name).await?;
                        let checkout = checkout_for(caller);
                        pod_resources =
                            persona::parent_resources(pool, checkout.as_ref(), caller, &parent_repo.dir, name).await?;
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
    let new = OwnedNewChild {
        caller,
        brief: brief.to_string(),
        persona: persona.map(str::to_string),
        model,
        volumes: volumes.iter().map(|name| name.to_string()).collect(),
        repo: repo.clone(),
        pod_resources,
    };
    // The child's transaction and the spawn of its start run in a task of
    // their own (SME-354 B7): a turn dropped while the commit is in
    // flight can't leave the child committed `starting` with no start.
    let created = tokio::spawn(tracing::Instrument::in_current_span(create_and_start(pool.clone(), new)))
        .await
        .map_err(|e| format!("couldn't start the agent: {e}"))?;
    let child = match created.map_err(|e| format!("couldn't start the agent: {e}"))? {
        db::CreateChild::Created { child, .. } => child,
        db::CreateChild::Refused { why, .. } => return Err(why),
    };
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
    // What the start mounted for the child, named back to the caller: those
    // volumes mount in the child's pod at their own paths, and the child can
    // pass them on (SME-239).
    let vols = if volumes.is_empty() {
        String::new()
    } else {
        format!(", with volumes {}", volumes.iter().map(|name| format!("{name:?}")).collect::<Vec<_>>().join(", "))
    };
    Ok(format!(
        "Started {} ({named}){on}{vols}. It works on its brief in its own conversation; its report \
         arrives here as a message when it calls finish, message_agent writes to it, and \
         list_agents shows its status.",
        label(child, None)
    ))
}

/// What `create_and_start` records for a new child, owned so the task
/// that runs it outlives the turn that asked for it.
struct OwnedNewChild {
    caller: i64,
    brief: String,
    persona: Option<String>,
    model: Option<(i64, String)>,
    volumes: Vec<String>,
    /// The parent's checkout the child works on, and its target.
    repo: Option<(db::ConversationRepo, crate::git::CloneTarget)>,
    /// The persona's resources as the parent's checkout has them (SME-484).
    pod_resources: Option<serde_json::Value>,
}

/// `create_child`, then what follows its commit (the sidebar told, the
/// child's start spawned; a refusal's trust card published): run as a
/// task of its own by `start_agent`, so no drop of the caller's turn can
/// land between the commit and the rest (SME-354 B7). The start runs in the
/// tool call's span, so it's the call's child.
async fn create_and_start(pool: PgPool, new: OwnedNewChild) -> Result<db::CreateChild, sqlx::Error> {
    let branch = new.repo.as_ref().and_then(|(_, target)| crate::git::requested_branch(target));
    let volumes: Vec<&str> = new.volumes.iter().map(String::as_str).collect();
    let record = db::NewChild {
        caller: new.caller,
        brief: &new.brief,
        persona_name: new.persona.as_deref(),
        model: new.model.clone(),
        max_depth: agent_depth(),
        volumes: &volumes,
        repo: new.repo.as_ref().map(|(parent_repo, _)| db::NewChildRepo {
            url: &parent_repo.url,
            remote_key: &parent_repo.remote_key,
            dir: &parent_repo.dir,
            branch: branch.as_deref(),
            parent_repo_id: parent_repo.id,
        }),
    };
    let created = db::create_child(&pool, &record).await?;
    let (child, child_repo) = match &created {
        db::CreateChild::Created { child, repo } => (*child, repo.clone()),
        db::CreateChild::Refused { asked, .. } => {
            if *asked {
                // The card must reach open tabs, as load_instructions' does,
                // and the user's desktop, as a question (SME-377).
                crate::git::publish_repos(&pool, new.caller).await;
                crate::turn::note_trust_card(new.caller);
            }
            return Ok(created);
        }
    };
    crate::events::publish_app(crate::events::AppEvent::ConversationsChanged);
    // Before the start's clone makes the child's first pod (SME-484). A
    // failed write leaves the defaults, which the start puts right from
    // the snapshot.
    if let Some(resources) = &new.pod_resources {
        if let Err(e) = db::set_conversation_pod_resources(&pool, child, Some(resources)).await {
            tracing::warn!(conversation_id = child, error = %e, "couldn't record the agent's pod resources before its clone");
        }
    }
    #[cfg(test)]
    test_hooks::reach(test_hooks::AFTER_CREATE_CHILD, new.caller).await;
    let request = StartRequest {
        repo: child_repo.zip(new.repo.map(|(_, target)| target)),
        persona: new.persona,
    };
    let caller = new.caller;
    tokio::spawn(tracing::Instrument::in_current_span(async move {
        run_start(&pool, child, caller, request).await;
    }));
    Ok(created)
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
    let mut prepared = match prepare(pool, checkout.as_ref(), child, parent, request).await {
        Ok(prepared) => prepared,
        Err(error) => {
            let _ = fail_start(pool, child, parent, &error).await;
            return;
        }
    };
    // Its pod as big as its persona says (SME-484), before anything runs
    // in it. `prepare`'s `size_first_pod` has already made its storage
    // again for a persona that differed; this backstop restarts in place
    // only when the ref moved between the two clones, persona and all.
    match persona::match_pod_resources(pool, checkout.as_ref(), child, prepared.persona.as_ref()).await {
        Ok(note) => prepared.parent_notice = one_notice([prepared.parent_notice.take(), note]),
        Err(error) => {
            let _ = fail_start(pool, child, parent, &error).await;
            return;
        }
    }
    // The pod gets the decision before the child is `active` and its
    // first turn woken (SME-242, D4): the key a persona excluded goes,
    // the secrets it named come. A failure fails the start rather than
    // waking a turn into a pod that doesn't match its grants; the exit
    // tries again as the backstop for every other way out.
    if let Err(error) = crate::credentials::reconcile_pod(pool, child, prepared.persona.as_ref()).await {
        let _ = fail_start(pool, child, parent, &format!("couldn't write its credentials into its pod: {error}")).await;
        return;
    }
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
        Err(e) => {
            let _ = fail_start(pool, child, parent, &format!("couldn't complete the start: {e}")).await;
        }
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
    // Its storage sized for the persona at its ref (SME-484, M1), before
    // anything is read or loaded from the checkout it may replace.
    let sized = match &request.persona {
        Some(name) => persona::size_first_pod(pool, checkout, child, &repo, &target, name).await?,
        None => None,
    };
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
    let (model, model_notice) = persona::persona_model(pool, child, &snapshot).await?;
    // A rule naming a server that isn't configured rules nothing: the
    // parent hears that too, on the notice channel (SME-241's report).
    let rules_notice = persona::rules_notice(pool, child, &snapshot).await;
    // The credentials notes before the narrowing, which is what makes
    // a `git-ssh` ask denied (SME-242, D3's write 2 reads the column
    // first). The narrowing itself is D3's: the column ∩ the persona's
    // own list, and no persona means `prepare` never reaches here.
    let credentials_notes = credentials_notes(pool, child, &snapshot).await;
    let wants_git_ssh = crate::credentials::snapshot_credential_names(&snapshot)
        .iter()
        .any(|name| name == crate::credentials::CREDENTIAL_GIT_SSH);
    db::narrow_agent_credentials(pool, child, wants_git_ssh)
        .await
        .map_err(|e| format!("couldn't narrow the agent's credentials: {e}"))?;
    let mut notes = vec![sized, model_notice, rules_notice];
    notes.extend(credentials_notes.into_iter().map(Some));
    Ok(Prepared { persona: Some(snapshot), model, parent_notice: one_notice(notes) })
}

/// What the child's persona asked of its credentials and this start
/// cannot give, each said once to its parent on the notice channel
/// (SME-242, D6's second place): a `git-ssh` the chain doesn't have —
/// asked before `prepare` narrows, which is what makes the ask denied
/// — and a name the store doesn't back, which is Rule S's only
/// failure mode and is said of the store alone, never of an ancestor.
/// A store that can't be read accuses no name (SME-241's precedent):
/// the reconcile simply writes no file for it, and the next install
/// will.
pub(crate) async fn credentials_notes(pool: &PgPool, child: i64, snapshot: &db::PersonaSnapshot) -> Vec<String> {
    let names = crate::credentials::snapshot_credential_names(snapshot);
    let label = || label(child, crate::agents::persona_label(&snapshot.path).as_deref());
    let wants_git_ssh = names.iter().any(|name| name == crate::credentials::CREDENTIAL_GIT_SSH);
    let mut notes = Vec::new();
    if wants_git_ssh && !chain_has_git_ssh(pool, child).await {
        notes.push(format!(
            "{}'s persona asks for `git-ssh`, which its parent's chain doesn't have, so it gets none",
            label()
        ));
    } else if wants_git_ssh && !key_stored(pool).await {
        // Granted, but nothing to grant yet (SME-354 B11): the store is
        // short, not the chain, and the next install after a key is
        // added writes it.
        notes.push(format!(
            "{}'s persona asks for `git-ssh`, but no SSH key is stored, so it has none until you add one",
            label()
        ));
    }
    let secrets: Vec<String> =
        names.into_iter().filter(|name| name != crate::credentials::CREDENTIAL_GIT_SSH).collect();
    if secrets.is_empty() {
        return notes;
    }
    let stored: Vec<String> = match db::list_named_secrets(pool).await {
        Ok(list) => list.into_iter().map(|secret| secret.name).collect(),
        Err(e) => {
            tracing::warn!(error = %e, "couldn't read the stored secrets for the agent's credentials note");
            return notes;
        }
    };
    for name in secrets {
        if !stored.contains(&name) {
            notes.push(format!(
                "{}'s persona lists `{name}`, which isn't one of your stored secrets, so it gets none until you add it",
                label()
            ));
        }
    }
    notes
}

/// Rule G's question while the column still holds the chain's answer,
/// before `prepare` narrows it: does the chain grant `git-ssh`? A
/// NULL column — a pre-ticket row — reads granted the way every Rule
/// G read does; no row is no agent, and only a child with a persona
/// gets here.
async fn chain_has_git_ssh(pool: &PgPool, child: i64) -> bool {
    match db::agent_credentials(pool, child).await {
        Ok(Some(Some(list))) => list.iter().any(|name| name == crate::credentials::CREDENTIAL_GIT_SSH),
        Ok(_) => true,
        Err(e) => {
            tracing::warn!(conversation_id = child, error = %e, "couldn't read the agent's chain bit for its credentials note");
            true
        }
    }
}

/// Whether an SSH key is stored, for the credentials note: a store that
/// can't be read accuses nothing, as for the secrets.
async fn key_stored(pool: &PgPool) -> bool {
    db::ssh_key_stored(pool).await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "couldn't read whether an SSH key is stored for the agent's credentials note");
        true
    })
}

/// One notice to the parent, whatever the start found to tell it: the
/// channel carries a single message, so several notes go in it one per
/// line, each naming the child it's about.
fn one_notice(notes: impl IntoIterator<Item = Option<String>>) -> Option<String> {
    let notes: Vec<String> = notes.into_iter().flatten().collect();
    (!notes.is_empty()).then(|| notes.join("\n"))
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
/// woken — and that wake's handle returned, so whoever provoked the start
/// can await it: the startup sweep collects every wake it provokes into the
/// set its caller awaits (SME-341). The start's own callers drop it, as
/// they always did. A child no longer starting (stopped or deleted
/// meanwhile) gets nothing, and returns `None`.
pub(crate) async fn fail_start(
    pool: &PgPool,
    child: i64,
    parent: i64,
    error: &str,
) -> Option<tokio::task::JoinHandle<()>> {
    let label = label(child, None);
    let text = format!("{label}'s start failed: {error}");
    // A write that errors is tried again (SME-354 B7): given up on, the
    // child would stay `starting` with no start task until a restart's
    // sweep, its parent waiting on a report that never comes.
    let mut attempt = 0;
    let written = loop {
        attempt += 1;
        #[cfg(test)]
        let written = if test_hooks::fail_start_write_fails(parent) {
            Err(sqlx::Error::Protocol("a test failed the failed start's write".to_string()))
        } else {
            db::fail_start(pool, child, parent, &label, &text).await
        };
        #[cfg(not(test))]
        let written = db::fail_start(pool, child, parent, &label, &text).await;
        match written {
            Err(e) if attempt < FAIL_START_ATTEMPTS => {
                tracing::warn!(conversation_id = child, error = %e, attempt, "couldn't record a failed start; trying again");
                tokio::time::sleep(std::time::Duration::from_millis(200 * attempt)).await;
            }
            written => break written,
        }
    };
    match written {
        Ok(true) => {
            let wake = crate::turn::notify(pool, parent, vec![crate::turn::Notice::Wake]);
            crate::events::publish_app(crate::events::AppEvent::ConversationsChanged);
            Some(wake)
        }
        Ok(false) => {
            tracing::info!(conversation_id = child, "a failed start's agent was stopped or deleted meanwhile");
            None
        }
        Err(e) => {
            tracing::error!(
                conversation_id = child,
                error = %e,
                start_error = %error,
                attempts = FAIL_START_ATTEMPTS,
                "couldn't record a failed start: the agent stays starting until the next restart's sweep"
            );
            None
        }
    }
}

/// How many times `fail_start` tries its write.
const FAIL_START_ATTEMPTS: u64 = 3;

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
        // D3's write 3, before the pod is reconciled under it: a start
        // that never completed gives its chain bit back, so no way out
        // leaves a resumable credential behind (Trade-off 4) and the
        // reconcile below reads a column that already says `no key`.
        if let Err(e) = db::clear_agent_credentials(pool, child).await {
            tracing::warn!(conversation_id = child, error = %e, "couldn't clear a cut-off start's chain bit");
        }
    }
    drop_start_credentials(pool, child).await;
    #[cfg(test)]
    test_hooks::start_ended(child);
}

/// SME-236's exit hook, wired to the decision by SME-242: writes the
/// child's live pod whatever its rows say now — after `end_start`'s
/// clearing, that is nothing, for a start that didn't complete, and
/// the committed rows' own grant for one that did (the same install
/// `run_start` just ran, so this is the backstop for the ways out
/// that never reached it: failed, stopped, task dropped, the startup
/// sweep). A no-op when the conversation has no live pod, the common
/// case for an agent with no repo.
async fn drop_start_credentials(pool: &PgPool, child: i64) {
    if let Err(e) = crate::credentials::reconcile_pod(pool, child, None).await {
        tracing::warn!(conversation_id = child, error = %e, "couldn't reconcile a start exit's credentials");
    }
}
