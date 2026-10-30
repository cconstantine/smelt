//! `agents` and `agent_messages` (SME-236).

use super::*;

/// A wake finds work in an undelivered report or notice, as in a finished
/// command (SME-218): otherwise a report queued to an idle parent would
/// never be drained.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_wake_has_work_sees_an_undelivered_agent_message(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    assert!(!wake_has_work(&pool, conversation.id).await.expect("wake_has_work"), "nothing queued yet");
    let mut conn = pool.acquire().await.expect("a connection");
    queue_agent_message(&mut conn, conversation.id, None, "agent #9", KIND_REPORT, "done")
        .await
        .expect("queue a report");
    assert!(wake_has_work(&pool, conversation.id).await.expect("wake_has_work"), "an undelivered report is work");
    sqlx::query("UPDATE agent_messages SET delivered_at = now()")
        .execute(&pool)
        .await
        .expect("mark it delivered");
    assert!(!wake_has_work(&pool, conversation.id).await.expect("wake_has_work"), "a delivered one isn't");
}

/// An agent row under `parent`, in `state`, with a persona when given.
async fn agent(pool: &PgPool, parent: i64, state: &str, persona: Option<(&str, &str)>) -> i64 {
    let child = sqlx::query_scalar::<_, i64>("INSERT INTO conversations (title, parent_id) VALUES ($1, $2) RETURNING id")
        .bind(format!("agent of {parent}"))
        .bind(parent)
        .fetch_one(pool)
        .await
        .expect("a child row");
    sqlx::query(
        "INSERT INTO agents (conversation_id, root_id, depth, brief, persona_name, persona_path, state)
         VALUES ($1, $2, 1, 'a brief', $3, $4, $5)",
    )
    .bind(child)
    .bind(parent)
    .bind(persona.map(|(name, _)| name))
    .bind(persona.map(|(_, path)| path))
    .bind(state)
    .execute(pool)
    .await
    .expect("its agent row");
    child
}

/// An AGENTS.md trust card waiting in `conversation_id`.
async fn trust_card(pool: &PgPool, conversation_id: i64) {
    let repo = create_conversation_repo(pool, conversation_id, "https://example.com/r.git", "example.com/r", None, "r")
        .await
        .expect("a repo row");
    let file = InstructionsFile { content: "Rules.".to_string(), file_bytes: 6, hash: "h".to_string(), commit: None };
    request_instruction(pool, conversation_id, repo.id, "AGENTS.md", &file).await.expect("a card");
}

/// A question in `conversation_id` asked of `asked_of`, unanswered.
async fn question(pool: &PgPool, conversation_id: i64, asked_of: &str) {
    sqlx::query("INSERT INTO pending_questions (conversation_id, tool_use_id, questions, asked_of) VALUES ($1, 'toolu_q', '[]', $2)")
        .bind(conversation_id)
        .bind(asked_of)
        .execute(pool)
        .await
        .expect("a question");
}

/// SME-246: the tree has every conversation, most recently active first,
/// with an agent's state and persona and what it waits on.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_conversation_tree_has_each_agents_state_persona_and_waits(pool: PgPool) {
    let root = test_conversation(&pool).await.id;
    let path = "/workspace/smelt/.agents/personas/developer.md";
    let asks_user = agent(&pool, root, AGENT_ACTIVE, Some(("developer", path))).await;
    let asks_parent = agent(&pool, root, AGENT_ACTIVE, None).await;
    let has_card = agent(&pool, asks_user, AGENT_FINISHED, None).await;
    let other = test_conversation(&pool).await.id;
    question(&pool, asks_user, ASKED_OF_USER).await;
    question(&pool, asks_parent, ASKED_OF_PARENT).await;
    trust_card(&pool, has_card).await;
    // Most recently active first: `other`, then the rest by their bump.
    for (id, minutes) in [(root, 5), (asks_user, 4), (asks_parent, 3), (has_card, 2), (other, 1)] {
        sqlx::query("UPDATE conversations SET updated_at = now() - make_interval(mins => $2) WHERE id = $1")
            .bind(id)
            .bind(minutes)
            .execute(&pool)
            .await
            .expect("age it");
    }

    let rows = conversation_tree(&pool).await.expect("the tree");
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![other, has_card, asks_parent, asks_user, root]);
    let by_id = |id: i64| rows.iter().find(|r| r.id == id).expect("listed").clone();
    let user_row = by_id(root);
    assert_eq!((user_row.state, user_row.parent_id, user_row.waiting_for_user), (None, None, false));
    let asking = by_id(asks_user);
    assert_eq!(asking.parent_id, Some(root));
    assert_eq!(asking.state.as_deref(), Some(AGENT_ACTIVE));
    assert_eq!((asking.persona_name.as_deref(), asking.persona_path.as_deref()), (Some("developer"), Some(path)));
    assert_eq!((asking.waiting_for_user, asking.waiting_for_parent), (true, false));
    let parent_asked = by_id(asks_parent);
    assert_eq!((parent_asked.waiting_for_user, parent_asked.waiting_for_parent), (false, true));
    let carded = by_id(has_card);
    assert_eq!(carded.state.as_deref(), Some(AGENT_FINISHED));
    assert_eq!((carded.parent_id, carded.waiting_for_user, carded.waiting_for_parent), (Some(asks_user), true, false));
}

/// SME-246: the banner's list is every agent under a conversation, at any
/// depth, waiting for the user (a question or a card), and never one that
/// only asked its parent, nor one under another conversation.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_agents_waiting_under_finds_every_depth_and_only_the_users_waits(pool: PgPool) {
    let root = test_conversation(&pool).await.id;
    let child = agent(&pool, root, AGENT_ACTIVE, None).await;
    let grandchild = agent(&pool, child, AGENT_ACTIVE, None).await;
    let asks_parent = agent(&pool, root, AGENT_ACTIVE, None).await;
    let elsewhere = test_conversation(&pool).await.id;
    let not_mine = agent(&pool, elsewhere, AGENT_ACTIVE, None).await;
    question(&pool, root, ASKED_OF_USER).await;
    question(&pool, child, ASKED_OF_USER).await;
    trust_card(&pool, grandchild).await;
    question(&pool, asks_parent, ASKED_OF_PARENT).await;
    question(&pool, not_mine, ASKED_OF_USER).await;

    let titles = |id: i64| format!("agent of {id}");
    assert_eq!(
        agents_waiting_under(&pool, root).await.expect("waiting"),
        vec![(child, titles(root)), (grandchild, titles(child))],
        "the root's own question isn't an agent's"
    );
    assert_eq!(agents_waiting_under(&pool, child).await.expect("waiting"), vec![(grandchild, titles(child))]);
    assert!(agents_waiting_under(&pool, grandchild).await.expect("waiting").is_empty());
}

/// SME-241: `create_child`'s new shape — a caller row, and a `NewChild`
/// with the depth ceiling set directly (never by env in tests).
fn new_child(caller: i64, max_depth: i32) -> NewChild<'static> {
    NewChild { caller, brief: "b", persona_name: None, model: None, repo: None, max_depth, volumes: &[] }
}

/// A conversation under `parent` with an `active` agent row at `depth`,
/// rooted at `root`, storing `chain` and `credentials`: the stand-in
/// for a started agent.
async fn nested_agent(
    pool: &PgPool,
    parent: i64,
    root: i64,
    depth: i32,
    chain: Option<serde_json::Value>,
    credentials: Option<&[&str]>,
) -> i64 {
    let child = sqlx::query_scalar::<_, i64>("INSERT INTO conversations (title, parent_id) VALUES ('n', $1) RETURNING id")
        .bind(parent)
        .fetch_one(pool)
        .await
        .expect("a conversation");
    let credentials: Option<Vec<String>> = credentials.map(|list| list.iter().map(|name| name.to_string()).collect());
    sqlx::query(
        "INSERT INTO agents (conversation_id, root_id, depth, brief, state, rule_chain, credentials)
         VALUES ($1, $2, $3, 'b', 'active', $4, $5)",
    )
    .bind(child)
    .bind(root)
    .bind(depth)
    .bind(chain)
    .bind(credentials)
    .execute(pool)
    .await
    .expect("an agent row");
    child
}

/// The created child's (depth, root_id, rule_chain).
async fn child_row(pool: &PgPool, id: i64) -> (i32, i64, Option<serde_json::Value>) {
    sqlx::query_as::<_, (i32, i64, Option<serde_json::Value>)>(
        "SELECT depth, root_id, rule_chain FROM agents WHERE conversation_id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("the agent row")
}

/// The `credentials` column as written — a `create_child` row's is
/// never NULL, so this reads the array itself.
async fn child_credentials(pool: &PgPool, id: i64) -> Vec<String> {
    sqlx::query_scalar::<_, Vec<String>>("SELECT credentials FROM agents WHERE conversation_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("the agent row")
}

/// `create_child` under a caller below the depth ceiling.
async fn child_of(pool: &PgPool, caller: i64) -> i64 {
    let CreateChild::Created { child, .. } = create_child(pool, &new_child(caller, 5)).await.expect("start") else {
        panic!("a caller below the ceiling starts one");
    };
    child
}

/// SME-242 T4 (its `create_child` rows): the seed is Rule G's bit and
/// nothing else — the parent's `git-ssh`, or Rule G's root answer for
/// an agent-less caller — and a named secret's name never enters the
/// column from above, the trap D2 exists to keep out.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_create_child_seeds_only_the_git_ssh_chain_bit(pool: PgPool) {
    let root = test_conversation(&pool).await.id;
    // An agent-less caller with no key stored: Rule G's root answer is
    // "there is no key to grant", an empty bit.
    let child = child_of(&pool, root).await;
    assert_eq!(child_credentials(&pool, child).await, Vec::<String>::new(), "no key stored, nothing to pass");
    // With a key stored, the same caller passes the bit — today's rule,
    // every pod in the line has the key.
    sqlx::query("INSERT INTO ssh_keys (name, public_key, private_key) VALUES ('id_ed25519', 'pub', 'priv')")
        .execute(&pool)
        .await
        .expect("a key");
    let child = child_of(&pool, root).await;
    assert_eq!(child_credentials(&pool, child).await, vec!["git-ssh".to_string()], "a stored key passes the bit");
    // An agent parent passes on its bit and nothing more.
    let holder = nested_agent(&pool, root, root, 1, None, Some(&["git-ssh"])).await;
    let child = child_of(&pool, holder).await;
    assert_eq!(child_credentials(&pool, child).await, vec!["git-ssh".to_string()], "the bit nests");
    // A parent narrowed away from `git-ssh` passes nothing down.
    let narrowed = nested_agent(&pool, root, root, 1, None, Some(&[])).await;
    let child = child_of(&pool, narrowed).await;
    assert_eq!(child_credentials(&pool, child).await, Vec::<String>::new(), "an empty bit stays empty down the line");
    // A pre-ticket parent — NULL column — reads granted, the same
    // fall-open every other Rule G read uses, so an old line keeps the
    // key rather than being stranded by the column's arrival.
    let legacy = nested_agent(&pool, root, root, 1, None, None).await;
    let child = child_of(&pool, legacy).await;
    assert_eq!(child_credentials(&pool, child).await, vec!["git-ssh".to_string()], "NULL falls open");
    // And the subset assertion that stops the trap: even a parent row
    // somehow holding a secret name in this column — only a hand-written
    // row can, the INSERT's comment forbids it — hands the child
    // `git-ssh` alone. Secrets travel in the persona snapshot, never
    // here, so no ancestor's list can narrow a descendant's.
    let trap = nested_agent(&pool, root, root, 1, None, Some(&["git-ssh", "deep-key"])).await;
    let child = child_of(&pool, trap).await;
    let seeded = child_credentials(&pool, child).await;
    assert_eq!(seeded, vec!["git-ssh".to_string()], "never a secret's name");
    assert!(!seeded.contains(&"deep-key".to_string()));
}

/// An agent row written by hand: `state`, the given column (NULL
/// included) and whether its start completed (`started_at`) — the
/// writers' subject, including states no start writes any more.
async fn hand_row(
    pool: &PgPool,
    root: i64,
    state: &str,
    credentials: Option<&[&str]>,
    started: bool,
) -> i64 {
    let id = sqlx::query_scalar::<_, i64>("INSERT INTO conversations (title, parent_id) VALUES ('h', $1) RETURNING id")
        .bind(root)
        .fetch_one(pool)
        .await
        .expect("a conversation");
    let credentials: Option<Vec<String>> = credentials.map(|list| list.iter().map(|name| name.to_string()).collect());
    sqlx::query(
        "INSERT INTO agents (conversation_id, root_id, depth, brief, state, credentials, started_at)
         VALUES ($1, $1, 1, 'b', $2, $3, CASE WHEN $4 THEN now() END)",
    )
    .bind(id)
    .bind(state)
    .bind(credentials)
    .bind(started)
    .execute(pool)
    .await
    .expect("an agent row");
    id
}

/// SME-242 T4 (the two lifecycle writers): `narrow_agent_credentials`
/// keeps the chain bit only if the persona lists `git-ssh` — writing a
/// `starting` row and nothing else, and treating a NULL column as the
/// granted fall-open every Rule G read uses — and
/// `clear_agent_credentials` empties the column of a start that never
/// completed, whatever state its exit left it in, while a completed
/// start's row is never touched.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_narrow_and_clear_write_the_chain_bit_under_their_gates(pool: PgPool) {
    let root = test_conversation(&pool).await.id;

    // A starting child whose persona lists `git-ssh` keeps the bit its
    // parent seeded.
    let wanted = hand_row(&pool, root, "starting", Some(&["git-ssh"]), false).await;
    assert!(narrow_agent_credentials(&pool, wanted, true).await.expect("narrow"), "a starting row is written");
    assert_eq!(child_credentials(&pool, wanted).await, vec!["git-ssh".to_string()]);
    // A NULL (pre-ticket) column reads granted, so narrowing a persona
    // that wants the key writes the bit rather than stranding the row.
    let legacy = hand_row(&pool, root, "starting", None, false).await;
    narrow_agent_credentials(&pool, legacy, true).await.expect("narrow");
    assert_eq!(child_credentials(&pool, legacy).await, vec!["git-ssh".to_string()], "NULL falls open");
    // A persona that doesn't list it narrows it away — `credentials: []`
    // and a bare persona alike.
    let excluded = hand_row(&pool, root, "starting", Some(&["git-ssh"]), false).await;
    narrow_agent_credentials(&pool, excluded, false).await.expect("narrow");
    assert_eq!(child_credentials(&pool, excluded).await, Vec::<String>::new(), "narrowed away");
    // An empty column stays empty however the persona asks: narrowing
    // keeps, never grants.
    let empty = hand_row(&pool, root, "starting", Some(&[]), false).await;
    narrow_agent_credentials(&pool, empty, true).await.expect("narrow");
    assert_eq!(child_credentials(&pool, empty).await, Vec::<String>::new(), "narrowing never grants");
    // And a row that isn't `starting` is refused outright — every
    // `agents` write here rides a state gate.
    let done = hand_row(&pool, root, "active", Some(&["git-ssh"]), true).await;
    assert!(!narrow_agent_credentials(&pool, done, false).await.expect("refused"), "not starting, not written");
    assert_eq!(child_credentials(&pool, done).await, vec!["git-ssh".to_string()], "its column stands");

    // The clearing writer, `end_start`'s safety write: a start that
    // never completed gives its bit back however its exit left the
    // state — cut off (`starting`), failed, or stopped mid-start — so
    // no exit path can leave a resumable credential behind.
    for state in ["starting", "failed", "stopped"] {
        let cut_off = hand_row(&pool, root, state, Some(&["git-ssh"]), false).await;
        assert!(clear_agent_credentials(&pool, cut_off).await.expect("clear"), "{state}");
        assert_eq!(child_credentials(&pool, cut_off).await, Vec::<String>::new(), "a never-started child gives the bit back");
    }
    // A start that completed never gives it back: `started_at` says its
    // model has run, and rewriting a live conversation's chain bit
    // mid-life is nobody's business but its own start's.
    let started = hand_row(&pool, root, "active", Some(&["git-ssh"]), true).await;
    assert!(!clear_agent_credentials(&pool, started).await.expect("refused"), "started, not cleared");
    assert_eq!(child_credentials(&pool, started).await, vec!["git-ssh".to_string()]);
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_create_child_nests_depth_under_its_caller_and_inherits_root_and_chain(pool: PgPool) {
    let root = test_conversation(&pool).await.id;
    let chain = serde_json::json!([{"class": "read-only", "allow": [], "deny": ["x*"]}]);
    let caller = nested_agent(&pool, root, root, 3, Some(chain.clone()), None).await;
    let CreateChild::Created { child, repo } = create_child(&pool, &new_child(caller, 5)).await.expect("start") else {
        panic!("a caller below the ceiling starts one");
    };
    assert!(repo.is_none());
    assert_eq!(child_row(&pool, child).await, (4, root, Some(chain)), "depth nests; root and chain are the line's");
    assert_eq!(conversation_parent(&pool, child).await.expect("parent"), Some(caller), "its parent is its caller");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_create_child_refuses_only_at_the_depth_ceiling(pool: PgPool) {
    let root = test_conversation(&pool).await.id;
    let caller = nested_agent(&pool, root, root, 5, None, None).await;
    let CreateChild::Refused { why, asked } = create_child(&pool, &new_child(caller, 5)).await.expect("start") else {
        panic!("a caller at the ceiling can't go deeper");
    };
    assert_eq!(why, "start_agent can't go deeper than 5 levels of agents (you're at depth 5)");
    assert!(!asked);
    // The same caller under a higher ceiling nests one more level.
    let CreateChild::Created { child, .. } = create_child(&pool, &new_child(caller, 6)).await.expect("start") else {
        panic!("the ceiling is the setting, not a stored number");
    };
    assert_eq!(child_row(&pool, child).await.0, 6);
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_create_child_refuses_a_parented_conversation_with_no_agent_row(pool: PgPool) {
    let root = test_conversation(&pool).await.id;
    let bare = sqlx::query_scalar::<_, i64>("INSERT INTO conversations (title, parent_id) VALUES ('bare', $1) RETURNING id")
        .bind(root)
        .fetch_one(&pool)
        .await
        .expect("a bare child row");
    let CreateChild::Refused { why, asked } = create_child(&pool, &new_child(bare, 5)).await.expect("start") else {
        panic!("its depth is unknown");
    };
    assert_eq!(
        why,
        "this conversation has a parent but no agent row, so its depth is unknown — \
         start_agent needs a conversation smelt started"
    );
    assert!(!asked);
    let children = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM conversations WHERE parent_id = $1")
        .bind(bare)
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(children, 0, "the refusal wrote nothing");
}
