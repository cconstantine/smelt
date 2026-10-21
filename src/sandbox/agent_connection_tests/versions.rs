use super::*;

/// An agent from before the protocol was versioned: its first message
/// isn't a hello. The pod is outdated for good, without a retry or crash
/// cleanup, and the next call fails without dialling.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_protocol_0_agent_is_outdated_for_good(pool: PgPool) {
    let (_, pod_id) = pod_row(&pool).await;
    let v0_line = r#"{"id":"cmd-1","terminal_id":"3","stream":"stdout","seq":1,"data":"hi"}"#;
    let agent = fake_agent(pod_id, Greeting::Raw(v0_line.into()), Duration::ZERO).await;

    let err = reconnect_if_needed(&pool, pod_id).await.map(|_| ()).expect_err("outdated");
    assert!(matches!(err, TerminalError::AgentOutdated { found: None, .. }), "{err:?}");
    let text = err.to_string();
    assert!(text.contains("terminate_pod") && text.contains("create_pod"), "{text}");
    assert_eq!(agent.dials.load(Ordering::SeqCst), 1, "an outdated agent was retried");
    assert!(db::sandbox_pod_is_live(&pool, pod_id).await.expect("live?"), "the pod was cleaned up as a crash");

    let again = reconnect_if_needed(&pool, pod_id).await.map(|_| ());
    assert!(matches!(again, Err(TerminalError::AgentOutdated { found: None, .. })), "{again:?}");
    assert_eq!(agent.dials.load(Ordering::SeqCst), 1, "the second call dialled again");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_agent_on_another_major_is_outdated(pool: PgPool) {
    let (_, pod_id) = pod_row(&pool).await;
    let next_major = ProtocolVersion { major: PROTOCOL_VERSION.major + 1, minor: 0 };
    let _agent = fake_agent(pod_id, Greeting::Hello(next_major), Duration::ZERO).await;
    let err = reconnect_if_needed(&pool, pod_id).await.map(|_| ()).expect_err("outdated");
    assert!(matches!(err, TerminalError::AgentOutdated { found: Some(v), .. } if v == next_major), "{err:?}");
    assert!(err.to_string().contains(&format!("protocol {next_major}")), "{err}");
}

/// SME-121: a refused pod that runs another image than a new pod would
/// (an older build, or `:latest`) is told to recreate, which gets it a
/// newer agent.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_refused_pod_on_an_older_image_is_told_to_recreate(pool: PgPool) {
    let (_, pod_id) = pod_row(&pool).await;
    let older = "docker.io/library/smelt-sandbox:src-0000000000000000";
    assert_ne!(older, default_sandbox_image());
    fake_images().lock().unwrap_or_else(|e| e.into_inner()).insert(pod_id, older.to_string());
    let next_major = ProtocolVersion { major: PROTOCOL_VERSION.major + 1, minor: 0 };
    let _agent = fake_agent(pod_id, Greeting::Hello(next_major), Duration::ZERO).await;

    let err = reconnect_if_needed(&pool, pod_id).await.map(|_| ()).expect_err("outdated");
    assert!(matches!(err, TerminalError::AgentOutdated { image: PodImage::Older, .. }), "{err:?}");
    let text = err.to_string();
    assert!(text.contains("Call terminate_pod, then create_pod"), "{text}");
    assert!(!text.contains("build-sandbox-image"), "{text}");
    assert_eq!(
        agent_status(pod_id),
        Some(AgentStatus::RestartRequired { version: Some(next_major.to_string()), rebuild_image: false })
    );
}

/// SME-121: a refused pod that already runs the image a new pod would get
/// can't be fixed by recreating it, so the error says not to retry and to
/// have the image rebuilt, now and on every later call; `/pods` says the
/// image needs rebuilding, then a restart.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_refused_pod_on_the_image_a_new_pod_gets_says_to_rebuild_it(pool: PgPool) {
    let (_, pod_id) = pod_row(&pool).await;
    let image = default_sandbox_image();
    fake_images().lock().unwrap_or_else(|e| e.into_inner()).insert(pod_id, image.clone());
    let v0_line = r#"{"id":"cmd-1","terminal_id":"3","stream":"stdout","seq":1,"data":"hi"}"#;
    let _agent = fake_agent(pod_id, Greeting::Raw(v0_line.into()), Duration::ZERO).await;

    for attempt in ["first", "remembered"] {
        let err = reconnect_if_needed(&pool, pod_id).await.map(|_| ()).expect_err("outdated");
        assert!(
            matches!(&err, TerminalError::AgentOutdated { image: PodImage::SameAsNew(i), .. } if *i == image),
            "{attempt}: {err:?}"
        );
        let text = err.to_string();
        assert!(text.contains(&image), "{attempt}: names the image: {text}");
        assert!(text.contains("won't help") && text.contains("Don't retry"), "{attempt}: {text}");
        // Review 1: the image is rebuilt under the same name, and the pod
        // still runs the old one, so the advice says what to do after.
        assert!(text.contains("Once it's rebuilt, call terminate_pod, then create_pod"), "{attempt}: {text}");
        assert!(text.contains("scripts/build-sandbox-image.sh"), "{attempt}: {text}");
        assert!(!text.contains("Call terminate_pod, then create_pod"), "{attempt}: {text}");
    }
    assert_eq!(agent_status(pod_id), Some(AgentStatus::RestartRequired { version: None, rebuild_image: true }));
}

/// SME-121: when the pod's image can't be read, the error keeps the
/// recreate advice and adds what to do if a new pod is refused too.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_refused_pod_whose_image_is_unknown_hedges_its_advice(pool: PgPool) {
    let (_, pod_id) = pod_row(&pool).await;
    let next_major = ProtocolVersion { major: PROTOCOL_VERSION.major + 1, minor: 0 };
    let _agent = fake_agent(pod_id, Greeting::Hello(next_major), Duration::ZERO).await;

    let err = reconnect_if_needed(&pool, pod_id).await.map(|_| ()).expect_err("outdated");
    assert!(matches!(err, TerminalError::AgentOutdated { image: PodImage::Unknown, .. }), "{err:?}");
    let text = err.to_string();
    assert!(text.contains("Call terminate_pod, then create_pod"), "{text}");
    assert!(text.contains("If a new pod is refused too") && text.contains("scripts/build-sandbox-image.sh"), "{text}");
}

/// Review 1: an image read that failed isn't remembered as the answer:
/// the next call reads it again (without dialling the agent again), and
/// gives the advice that image calls for.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_image_that_couldnt_be_read_is_read_again_next_time(pool: PgPool) {
    let (_, pod_id) = pod_row(&pool).await;
    let v0_line = r#"{"id":"cmd-1","terminal_id":"3","stream":"stdout","seq":1,"data":"hi"}"#;
    let agent = fake_agent(pod_id, Greeting::Raw(v0_line.into()), Duration::ZERO).await;

    let first = reconnect_if_needed(&pool, pod_id).await.map(|_| ()).expect_err("outdated");
    assert!(matches!(first, TerminalError::AgentOutdated { image: PodImage::Unknown, .. }), "{first:?}");

    let image = default_sandbox_image();
    fake_images().lock().unwrap_or_else(|e| e.into_inner()).insert(pod_id, image.clone());
    let again = reconnect_if_needed(&pool, pod_id).await.map(|_| ()).expect_err("outdated");
    assert!(matches!(&again, TerminalError::AgentOutdated { image: PodImage::SameAsNew(i), .. } if *i == image), "{again:?}");
    assert_eq!(agent_status(pod_id), Some(AgentStatus::RestartRequired { version: None, rebuild_image: true }));
    assert_eq!(agent.dials.load(Ordering::SeqCst), 1, "the agent was dialled again");
}

#[test]
fn test_judge_image_compares_a_pods_image_with_a_new_pods() {
    let new = "docker.io/library/smelt-sandbox:src-1111111111111111";
    assert_eq!(judge_image(Some(new), new), PodImage::SameAsNew(new.to_string()));
    assert_eq!(judge_image(Some("docker.io/library/smelt-sandbox:latest"), new), PodImage::Older);
    assert_eq!(judge_image(None, new), PodImage::Unknown);
}

/// Review 1: Kubernetes keeps a pod's image as written, so a pod made
/// under `docker.io/library/smelt-sandbox:latest` and a `SANDBOX_IMAGE` of
/// `smelt-sandbox:latest` name one image, and recreating can't help.
#[test]
fn test_judge_image_takes_short_and_full_names_of_one_image_as_the_same() {
    let full = "docker.io/library/smelt-sandbox:latest";
    for short in ["smelt-sandbox:latest", "smelt-sandbox", "library/smelt-sandbox:latest", "docker.io/smelt-sandbox"] {
        assert!(matches!(judge_image(Some(full), short), PodImage::SameAsNew(_)), "{short}");
        assert!(matches!(judge_image(Some(short), full), PodImage::SameAsNew(_)), "{short}");
    }
    assert_eq!(judge_image(Some("smelt-sandbox:v0"), full), PodImage::Older);
    assert_eq!(judge_image(Some("registry.example:5000/smelt-sandbox:latest"), full), PodImage::Older);
    assert_eq!(judge_image(Some("localhost/smelt-sandbox:latest"), full), PodImage::Older);
}

/// Silence could be an old agent with nothing to say, or a slow link, so
/// it fails this attempt but isn't remembered.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_agent_that_says_nothing_is_outdated_but_not_for_good(pool: PgPool) {
    let (_, pod_id) = pod_row(&pool).await;
    let agent = fake_agent(pod_id, Greeting::Nothing, Duration::ZERO).await;
    let err = reconnect_if_needed(&pool, pod_id).await.map(|_| ()).expect_err("outdated");
    assert!(matches!(err, TerminalError::AgentOutdated { found: None, .. }), "{err:?}");
    let _ = reconnect_if_needed(&pool, pod_id).await;
    assert_eq!(agent.dials.load(Ordering::SeqCst), 2, "silence was remembered as outdated");
}

/// Any minor of smelt's major connects; a feature from a later minor
/// fails alone, asking for a restart.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_another_minor_connects_and_gates_only_newer_features(pool: PgPool) {
    let (_, pod_id) = pod_row(&pool).await;
    let older = ProtocolVersion { major: PROTOCOL_VERSION.major, minor: 3 };
    let _agent = fake_agent(pod_id, Greeting::Hello(older), Duration::ZERO).await;
    let conn = reconnect_if_needed(&pool, pod_id).await.expect("another minor connects");
    assert_eq!(conn.agent_version, older);
    assert!(conn.require_minor(3).is_ok());
    let err = conn.require_minor(4).expect_err("minor 4 is newer than the agent");
    assert!(matches!(err, TerminalError::AgentTooOld { found, .. } if found == older), "{err:?}");
    assert!(err.to_string().contains("needs a newer sandbox agent"), "{err}");
}

/// The Sandboxes page and `list_pods` say what each pod's agent speaks.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_agent_status_follows_what_the_agent_said(pool: PgPool) {
    let (_, unconnected) = pod_row(&pool).await;
    let _a = fake_agent(unconnected, current(), Duration::ZERO).await;
    assert_eq!(agent_status(unconnected), None, "not connected yet, so unknown");

    let (_, same) = pod_row(&pool).await;
    let _b = fake_agent(same, current(), Duration::ZERO).await;
    reconnect_if_needed(&pool, same).await.expect("connects");
    assert_eq!(agent_status(same), Some(AgentStatus::Current { version: PROTOCOL_VERSION.to_string() }));

    let (_, newer) = pod_row(&pool).await;
    let newer_minor = ProtocolVersion { major: PROTOCOL_VERSION.major, minor: PROTOCOL_VERSION.minor + 1 };
    let _c = fake_agent(newer, Greeting::Hello(newer_minor), Duration::ZERO).await;
    reconnect_if_needed(&pool, newer).await.expect("connects");
    assert_eq!(agent_status(newer), Some(AgentStatus::Current { version: newer_minor.to_string() }));

    // An older minor can't be faked while smelt is at minor 0, so check
    // the classification itself.
    let older = ProtocolVersion { major: PROTOCOL_VERSION.major, minor: 2 };
    let current_minor_3 = ProtocolVersion { major: PROTOCOL_VERSION.major, minor: 3 };
    assert_eq!(
        classify_agent(older, current_minor_3),
        AgentStatus::RestartRecommended { version: older.to_string() }
    );

    let (_, old) = pod_row(&pool).await;
    let v0_line = r#"{"id":"c","terminal_id":"1","stream":"stdout","seq":1,"data":"x"}"#;
    let _d = fake_agent(old, Greeting::Raw(v0_line.into()), Duration::ZERO).await;
    let _ = reconnect_if_needed(&pool, old).await;
    assert_eq!(agent_status(old), Some(AgentStatus::RestartRequired { version: None, rebuild_image: false }));
}
