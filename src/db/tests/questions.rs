use super::*;

#[sqlx::test]
async fn test_a_pending_question_is_answered_once_then_taken(pool: PgPool) {
    let conversation = create_conversation(&pool).await.expect("conversation");
    let id = conversation.id;
    assert_eq!(get_pending_question(&pool, id).await.expect("get"), None);
    create_pending_question(&pool, id, "toolu_q", &sample_questions()).await.expect("create");
    let waiting = get_pending_question(&pool, id).await.expect("get").expect("a question");
    assert_eq!(waiting.tool_use_id, "toolu_q");
    assert_eq!(waiting.questions, sample_questions());
    assert_eq!(waiting.answer, None);
    assert_eq!(list_waiting_conversations(&pool).await.expect("list"), vec![id]);

    // A waiting question stays unless the take includes waiting ones.
    assert_eq!(take_pending_question(&pool, id, false).await.expect("take"), None);
    assert!(!answer_pending_question(&pool, id, "toolu_other", &sample_answer("Yes")).await.expect("answer"));
    assert!(answer_pending_question(&pool, id, "toolu_q", &sample_answer("Yes")).await.expect("answer"));
    assert!(
        !answer_pending_question(&pool, id, "toolu_q", &sample_answer("No")).await.expect("answer"),
        "a second answer must not replace the first"
    );
    assert!(list_waiting_conversations(&pool).await.expect("list").is_empty());

    let taken = take_pending_question(&pool, id, false).await.expect("take").expect("the answered question");
    assert_eq!(taken.answer, Some(sample_answer("Yes")));
    assert_eq!(get_pending_question(&pool, id).await.expect("get"), None);
}

#[sqlx::test]
async fn test_a_waiting_question_is_taken_only_when_asked_for(pool: PgPool) {
    let conversation = create_conversation(&pool).await.expect("conversation");
    create_pending_question(&pool, conversation.id, "toolu_q", &sample_questions()).await.expect("create");
    let taken = take_pending_question(&pool, conversation.id, true).await.expect("take").expect("the waiting question");
    assert_eq!(taken.answer, None);
    assert_eq!(get_pending_question(&pool, conversation.id).await.expect("get"), None);
}

#[sqlx::test]
async fn test_a_pending_question_goes_with_its_conversation(pool: PgPool) {
    let conversation = create_conversation(&pool).await.expect("conversation");
    create_pending_question(&pool, conversation.id, "toolu_q", &sample_questions()).await.expect("create");
    delete_conversation(&pool, conversation.id).await.expect("delete");
    assert!(list_waiting_conversations(&pool).await.expect("list").is_empty());
}
