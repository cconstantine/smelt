//! A conversation's turn lifecycle at its edges (SME-179): turns counted,
//! ended, failed and stopped in the orders a hook or a held lock forces,
//! one interleaving per test.

use super::*;

/// Every `TurnState` `rx` has had so far, in order.
fn turn_states_so_far(rx: &mut tokio::sync::broadcast::Receiver<events::ConversationEvent>) -> Vec<bool> {
    let mut states = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let events::ConversationEvent::TurnState { running, .. } = event {
            states.push(running);
        }
    }
    states
}

/// Ends `ending` on a thread of its own, paused right after its count
/// changed, then counts `start_next` as the next turn while it's paused
/// there, and lets it finish. Today's code paused there before it
/// published or cleared anything.
fn end_a_turn_around_the_next_one(conversation_id: i64, ending: TurnInFlight, start_next: impl FnOnce()) {
    let (reached, release) = test_hooks::pause_after_in_flight_changed(conversation_id);
    let ending = std::thread::spawn(move || drop(ending));
    reached
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the ending turn reaches the hook");
    start_next();
    release.send(()).expect("the ending turn waits at the hook");
    ending.join().expect("the ending turn's thread");
}

/// B29a, interleaving: turn A's end counts 1 to 0, then turn B is
/// counted (0 to 1) before A's end is done. Tabs must end on "running":
/// today A published `running: false` after B's `running: true`, so
/// every tab hid Stop while B ran.
#[test]
fn test_a_turn_counted_as_the_last_one_ends_is_seen_running() {
    let conversation_id = 9_179_000_001;
    let mut rx = events::subscribe(conversation_id);
    let a = TurnInFlight::start(conversation_id);
    let mut b = None;
    end_a_turn_around_the_next_one(conversation_id, a, || b = Some(TurnInFlight::start(conversation_id)));

    assert_eq!(turn_states_so_far(&mut rx), vec![true, false, true], "what tabs were told, in order");
    assert!(turn_running(conversation_id), "B is running");
    drop(b);
    drop(rx);
    events::forget(conversation_id);
}

/// B29c, the same interleaving with B's reply streaming before A's end
/// is done: A's end mustn't clear B's reply, on the server or (with a
/// `running: false` after B's text) in any tab.
#[test]
fn test_a_turn_ending_doesnt_wipe_the_next_turns_reply() {
    let conversation_id = 9_179_000_002;
    let mut rx = events::subscribe(conversation_id);
    let a = TurnInFlight::start(conversation_id);
    let mut b = None;
    end_a_turn_around_the_next_one(conversation_id, a, || {
        b = Some(TurnInFlight::start(conversation_id));
        relay_reply_delta(conversation_id, "B's text");
    });

    assert_eq!(reply_in_progress(conversation_id).as_deref(), Some("B's text"), "B's reply is kept");
    let mut after_delta = Vec::new();
    let mut seen_delta = false;
    while let Ok(event) = rx.try_recv() {
        match event {
            events::ConversationEvent::ReplyDelta { .. } => seen_delta = true,
            events::ConversationEvent::TurnState { running, .. } if seen_delta => after_delta.push(running),
            _ => {}
        }
    }
    assert!(seen_delta, "B's text was published");
    assert_eq!(after_delta, Vec::<bool>::new(), "no turn state after B's text wipes it in tabs");
    drop(b);
    drop(rx);
    events::forget(conversation_id);
}

/// B29b, interleaving: a Stop lands right after the turn is counted,
/// before it waits for the lock (today, before it took its stop
/// receiver, with no earlier turn's counter left), and the lock is held
/// so only the stop can wake it. It must end stopped, through the
/// stopped-while-queued path, its message saved once; today the stop
/// only paused, and the turn went on to the model.
#[sqlx::test]
async fn test_a_stop_just_after_a_turn_is_counted_ends_it(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation_id = db::create_conversation(&pool).await.expect("create conversation").id;
    start_hanging_mock_upstream(&pool).await;
    let held = conversation_lock(conversation_id).lock_owned().await;
    let (counted, release_counted) = test_hooks::pause_after_turn_counted(conversation_id);
    let (mut saved, release_saved) = test_hooks::pause_after_message_saved(conversation_id);

    let turn = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, conversation_id, hello()).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), counted)
        .await
        .expect("the turn is counted")
        .expect("hook");
    stop_turn_now(conversation_id);
    release_counted.send(()).expect("the turn waits at the hook");
    // The turn reaches its `select!` with only the stop ready.
    for _ in 0..3 {
        tokio::task::yield_now().await;
    }
    drop(held);

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), turn)
        .await
        .expect("the stop ends the turn within 5 s")
        .expect("join");
    assert!(matches!(result, Err(TurnFailure::Stopped)), "{result:?}");
    assert!(
        matches!(saved.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)),
        "the turn ran past the lock instead of the stopped-while-queued path"
    );
    let messages = db::list_messages(&pool, conversation_id).await.expect("list");
    let copies = messages.iter().filter(|m| m.content.contains("\"hello\"")).count();
    assert_eq!(copies, 1, "the message is saved once: {messages:?}");

    // The unused pause, taken back.
    drop(release_saved);
    test_hooks::after_message_saved(conversation_id).await;
    resume_turns(conversation_id);
}

/// A mock upstream whose first request streams "Once upon", waits for
/// the returned gate, then fails with an SSE `error` event; every later
/// request gets a plain "B answered".
async fn start_partial_then_failing_mock_upstream(pool: &PgPool) -> tokio::sync::oneshot::Sender<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let (open, gate) = tokio::sync::oneshot::channel::<()>();
    let gate = Arc::new(std::sync::Mutex::new(Some(gate)));
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move || {
            let gate = gate.lock().expect("the gate").take();
            async move {
                let Some(gate) = gate else {
                    return ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], text_reply_body("B answered"))
                        .into_response();
                };
                let start = sse_body(&[
                    ("message_start", r#"{"type":"message_start"}"#),
                    (
                        "content_block_delta",
                        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Once upon"}}"#,
                    ),
                ]);
                let error = sse_body(&[(
                    "error",
                    r#"{"type":"error","error":{"type":"api_error","message":"A failed mid-reply"}}"#,
                )]);
                let body = futures_util::StreamExt::chain(
                    futures_util::stream::once(async move { Ok::<_, std::io::Error>(start) }),
                    futures_util::stream::once(async move {
                        let _ = gate.await;
                        Ok::<_, std::io::Error>(error)
                    }),
                );
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    axum::body::Body::from_stream(body),
                )
                    .into_response()
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    crate::providers::test_support::add_mock_provider(pool, addr).await;
    open
}

/// Polls `check` every 20 ms until it holds, for up to 5 s.
async fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !check() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting until {what}"));
}

/// B28, interleaving: turn A fails mid-reply while turn B is queued
/// behind it, and B then holds the lock (paused right after saving its
/// message, before its own model call). A's partial text must be gone
/// by then, on the server and in every tab; today only the last turn
/// ending cleared it.
#[sqlx::test]
async fn test_a_failed_turns_reply_is_cleared_while_a_turn_waits_behind_it(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation_id = db::create_conversation(&pool).await.expect("create conversation").id;
    let open = start_partial_then_failing_mock_upstream(&pool).await;
    let mut rx = events::subscribe(conversation_id);

    start_turn(pool.clone(), conversation_id, "A".to_string()).await.expect("send A");
    wait_until("A's reply streams", || reply_in_progress(conversation_id).as_deref() == Some("Once upon")).await;
    start_turn(pool.clone(), conversation_id, "B".to_string()).await.expect("send B");
    wait_until("B is queued", || turns_in_flight(conversation_id) == 2).await;
    let (reached, release) = test_hooks::pause_after_message_saved(conversation_id);
    open.send(()).expect("A's call waits at the gate");
    tokio::time::timeout(std::time::Duration::from_secs(5), reached)
        .await
        .expect("B takes the lock and saves its message")
        .expect("hook");

    assert_eq!(reply_in_progress(conversation_id), None, "A's partial reply is still kept");
    let mut seen_delta = false;
    let mut reset_after_delta = false;
    while let Ok(event) = rx.try_recv() {
        match event {
            events::ConversationEvent::ReplyDelta { .. } => seen_delta = true,
            events::ConversationEvent::ReplyReset {} if seen_delta => reset_after_delta = true,
            _ => {}
        }
    }
    assert!(seen_delta, "A's text was published");
    assert!(reset_after_delta, "tabs weren't told to drop A's partial reply before B ran");

    let _ = release.send(());
    wait_until("B ends", || !turn_running(conversation_id)).await;
    drop(rx);
    events::forget(conversation_id);
}

/// A mock upstream that answers its first `failures` requests with a 400
/// ("the user's turn failed") and every later one with "Noted.", and
/// counts them.
async fn start_failing_then_answering_mock_upstream(pool: &PgPool, failures: usize) -> Arc<AtomicUsize> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let counter = Arc::new(AtomicUsize::new(0));
    let counted = counter.clone();
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move || {
            let i = counted.fetch_add(1, Ordering::SeqCst);
            async move {
                if i < failures {
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        r#"{"type":"error","error":{"type":"invalid_request_error","message":"the user's turn failed"}}"#
                            .to_string(),
                    )
                        .into_response();
                }
                (
                    axum::http::StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    text_reply_body("Noted."),
                )
                    .into_response()
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    crate::providers::test_support::add_mock_provider(pool, addr).await;
    counter
}

/// Sends the user's message and waits for its turn to fail and end.
async fn fail_a_users_turn(pool: &PgPool, conversation_id: i64) {
    start_turn(pool.clone(), conversation_id, "please".to_string()).await.expect("send");
    wait_until("the user's turn fails", || {
        !turn_running(conversation_id) && last_turn_error(conversation_id).is_some()
    })
    .await;
}

/// B30, interleaving: a wake (a command finished during the user's
/// turn) is counted and queued while the user's turn runs, and that
/// turn then fails; the wake finds nothing to do. The user's error must
/// be kept: today the wake took a newer turn generation when it was
/// counted, so the failure kept nothing.
#[sqlx::test]
async fn test_a_wake_queued_behind_a_failing_turn_keeps_its_error(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation_id = db::create_conversation(&pool).await.expect("create conversation").id;
    let requests = start_failing_then_answering_mock_upstream(&pool, usize::MAX).await;
    let (reached, release) = test_hooks::pause_after_message_saved(conversation_id);

    start_turn(pool.clone(), conversation_id, "please".to_string()).await.expect("send");
    tokio::time::timeout(std::time::Duration::from_secs(5), reached)
        .await
        .expect("the user's turn saves its message")
        .expect("hook");
    let wake = tokio::spawn({
        let pool = pool.clone();
        async move { wake_conversation(&pool, conversation_id).await }
    });
    wait_until("the wake is queued", || turns_in_flight(conversation_id) == 2).await;
    let _ = release.send(());

    let woke = tokio::time::timeout(std::time::Duration::from_secs(5), wake)
        .await
        .expect("the wake ends")
        .expect("join")
        .expect("a wake with nothing to do succeeds");
    assert!(woke.is_empty(), "the wake had nothing to do: {woke:?}");
    wait_until("the user's turn ends", || !turn_running(conversation_id)).await;
    assert!(
        last_turn_error(conversation_id).is_some_and(|e| e.contains("the user's turn failed")),
        "the user's error isn't kept: {:?}",
        last_turn_error(conversation_id)
    );
    assert_eq!(requests.load(Ordering::SeqCst), 1, "the wake called the model");
}

/// B30, a wake with nothing pending after the user's turn failed: it
/// calls no model and must leave the kept error as it was; today it
/// cleared it.
#[sqlx::test]
async fn test_a_wake_with_nothing_pending_keeps_the_kept_error(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation_id = db::create_conversation(&pool).await.expect("create conversation").id;
    let requests = start_failing_then_answering_mock_upstream(&pool, usize::MAX).await;
    fail_a_users_turn(&pool, conversation_id).await;
    let kept = last_turn_error(conversation_id);

    let woke = wake_conversation(&pool, conversation_id).await.expect("nothing to do");

    assert!(woke.is_empty(), "the wake had nothing to do: {woke:?}");
    assert_eq!(last_turn_error(conversation_id), kept, "the wake changed the kept error");
    assert_eq!(requests.load(Ordering::SeqCst), 1, "the wake called the model");
}

/// B30's guard: a wake that does call the model (a finished command's
/// notice to deliver) is a newer turn, and replaces the kept error, as
/// any newer turn does.
#[sqlx::test]
async fn test_a_wake_that_calls_the_model_replaces_the_kept_error(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation_id = db::create_conversation(&pool).await.expect("create conversation").id;
    let requests = start_failing_then_answering_mock_upstream(&pool, 1).await;
    fail_a_users_turn(&pool, conversation_id).await;
    unnotified_finished_command(&pool, conversation_id, "cmd-sme-179").await;

    let woke = wake_conversation(&pool, conversation_id).await.expect("the wake runs");

    assert_eq!(woke.len(), 2, "the notice and the reply: {woke:?}");
    assert_eq!(requests.load(Ordering::SeqCst), 2, "the wake called the model");
    assert_eq!(last_turn_error(conversation_id), None, "the older failure outlived a newer turn");
}
