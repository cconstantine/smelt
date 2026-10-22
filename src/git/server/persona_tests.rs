//! Persona files (SME-235): recorded at clone, and listed for the model
//! only from a repo the user trusts.

use super::*;

/// Git's pathspec `*` matches `/`, so a file in a subdirectory of
/// `.agents/personas/` comes through `ls-files`; only the files directly
/// in it are personas.
#[test]
fn test_only_markdown_files_directly_in_the_personas_directory_are_persona_files() {
    let listed = ".agents/personas/reviewer.md\0.agents/personas/nested/x.md\0.agents/personas/developer.md\0\
                  .agents/personas/notes.txt\0.agents/personas/.md\0other/.agents/personas/y.md\0";
    assert_eq!(
        persona_paths(listed),
        vec![".agents/personas/developer.md".to_string(), ".agents/personas/reviewer.md".to_string()]
    );
    assert!(persona_paths("").is_empty());
}

/// T5: a finished clone records its persona files in the same update
/// as its AGENTS.md files; one whose persona files can't be listed is
/// still ready, with the error saying why, beside the AGENTS.md one.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_finished_clone_records_its_persona_files(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let cloned = ClonedRepo { commit: Some("abc".to_string()), branch: "main".to_string() };
    let personas = vec![".agents/personas/developer.md".to_string()];

    let listed = db::create_conversation_repo(&pool, conversation.id, "u", "k", None, "listed").await.expect("repo");
    let guard = CloneGuard::new(&pool, &listed, conversation.id);
    record_cloned(&pool, &listed, &cloned, Ok(vec!["AGENTS.md".to_string()]), Ok(personas.clone()), guard)
        .await
        .expect("record");
    let row = db::get_conversation_repo(&pool, listed.id).await.expect("get").expect("exists");
    assert_eq!((row.status.as_str(), row.error.as_deref()), ("ready", None));
    assert_eq!(row.persona_files, personas);

    let unlisted = db::create_conversation_repo(&pool, conversation.id, "u", "k", None, "unlisted").await.expect("repo");
    let guard = CloneGuard::new(&pool, &unlisted, conversation.id);
    record_cloned(&pool, &unlisted, &cloned, Ok(vec![]), Err("exec failed".to_string()), guard)
        .await
        .expect("record");
    let row = db::get_conversation_repo(&pool, unlisted.id).await.expect("get").expect("exists");
    assert_eq!(row.status, "ready", "the clone itself is good");
    assert!(row.persona_files.is_empty(), "{row:?}");
    assert_eq!(row.error.as_deref(), Some("Couldn't list its persona files: exec failed"));

    let neither = db::create_conversation_repo(&pool, conversation.id, "u", "k", None, "neither").await.expect("repo");
    let guard = CloneGuard::new(&pool, &neither, conversation.id);
    record_cloned(&pool, &neither, &cloned, Err("no git".to_string()), Err("no git".to_string()), guard)
        .await
        .expect("record");
    let row = db::get_conversation_repo(&pool, neither.id).await.expect("get").expect("exists");
    assert_eq!(
        row.error.as_deref(),
        Some("Couldn't list its AGENTS.md files: no git; Couldn't list its persona files: no git")
    );
}
