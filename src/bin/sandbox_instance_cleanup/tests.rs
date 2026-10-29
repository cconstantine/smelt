use super::*;
use sqlx::PgPool;

// --- Pure: selection ---

fn meta(name: &str, labels: Option<&str>, uid: Option<&str>) -> ObjectMeta {
    ObjectMeta {
        name: Some(name.to_string()),
        uid: uid.map(str::to_string),
        labels: labels.map(|value| [(INSTANCE_LABEL.to_string(), value.to_string())].into()),
        ..Default::default()
    }
}

/// An object labelled exactly with the instance is kept for deletion.
/// Unlabelled, another instance, an empty label, a label that merely
/// starts with or contains the instance, and one with no uid are all
/// left.
#[test]
fn test_to_delete_keeps_only_an_object_labelled_exactly_with_the_instance() {
    let instance = "0f8e2a5c-1b2d-4e6f-8a9b-0c1d2e3f4a5b";
    let listed = [
        meta("ours", Some(instance), Some("uid-ours")),
        meta(
            "theirs",
            Some("11111111-2222-3333-4444-555555555555"),
            Some("uid-theirs"),
        ),
        meta("unlabelled", None, Some("uid-unlabelled")),
        meta("empty", Some(""), Some("uid-empty")),
        meta("prefix", Some(&format!("{instance}-x")), Some("uid-prefix")),
        meta("contains", Some(&format!("x{instance}")), Some("uid-contains")),
        meta("no-uid", Some(instance), None),
    ];

    assert_eq!(
        to_delete(&listed, instance),
        vec![("ours".to_string(), "uid-ours".to_string())]
    );
}

/// Every object of the instance, in name order, whatever order they were
/// listed in.
#[test]
fn test_to_delete_returns_every_object_of_the_instance_in_name_order() {
    let instance = "0f8e2a5c-1b2d-4e6f-8a9b-0c1d2e3f4a5b";
    let listed = [
        meta("b", Some(instance), Some("uid-b")),
        meta("a", Some(instance), Some("uid-a")),
    ];

    assert_eq!(
        to_delete(&listed, instance),
        vec![
            ("a".to_string(), "uid-a".to_string()),
            ("b".to_string(), "uid-b".to_string())
        ]
    );
}

/// An empty instance (never read) matches nothing, not even an empty
/// label: no selector can be built from it.
#[test]
fn test_to_delete_matches_nothing_for_an_empty_instance() {
    let listed = [
        meta("empty", Some(""), Some("uid-empty")),
        meta("ours", Some("abc"), Some("uid-abc")),
    ];

    assert!(to_delete(&listed, "").is_empty());
}

// --- Pure: guards ---

const SCRATCH: &str = "postgres://smelt:smelt@postgres/smelt_scratch_worktree";
const DEV: &str = "postgres://smelt:smelt@postgres/smelt";
const UUID: &str = "0f8e2a5c-1b2d-4e6f-8a9b-0c1d2e3f4a5b";

/// A real scratch case is accepted.
#[test]
fn test_check_target_accepts_a_scratch_database_with_a_uuid_instance() {
    assert_eq!(check_target(SCRATCH, DEV, UUID, false), Ok(()));
    assert_eq!(
        scratch_database(SCRATCH, DEV),
        Ok("smelt_scratch_worktree".to_string())
    );
}

/// Guard 1: a database that isn't a check server's scratch database.
#[test]
fn test_check_target_refuses_a_database_that_isnt_smelt_scratch() {
    let why = check_target(DEV, DEV, UUID, false).expect_err("the dev database must be refused");
    assert!(why.contains("smelt_scratch_"), "{why}");
}

/// Guard 2: the dev URL's database, whatever its name.
#[test]
fn test_check_target_refuses_the_dev_databases_name() {
    let same = "postgres://smelt:smelt@postgres/smelt_scratch_dev";
    let dev = "postgres://other:x@elsewhere/smelt_scratch_dev";
    let why = check_target(same, dev, UUID, false).expect_err("the dev database must be refused");
    assert!(why.contains("--dev-database-url"), "{why}");
}

/// Guard 3: a database that owns the objects made before SME-115.
#[test]
fn test_check_target_refuses_a_database_that_owns_unlabelled_objects() {
    let why =
        check_target(SCRATCH, DEV, UUID, true).expect_err("an owning database must be refused");
    assert!(why.contains("owns_unlabelled"), "{why}");
}

/// Guard 4: an instance that isn't a UUID, an empty one included.
#[test]
fn test_check_target_refuses_a_non_uuid_instance() {
    for instance in [
        "",
        "not-a-uuid",
        "0F8E2A5C-1B2D-4E6F-8A9B-0C1D2E3F4A5B",
        "0f8e2a5c1b2d4e6f8a9b0c1d2e3f4a5b",
    ] {
        assert!(
            check_target(SCRATCH, DEV, instance, false).is_err(),
            "{instance:?} must be refused"
        );
    }
}

/// The database name is read from the URL's path, query and all; a URL
/// naming no database has none.
#[test]
fn test_database_name_reads_the_urls_path() {
    assert_eq!(database_name("postgres://u@h/db"), Some("db".to_string()));
    assert_eq!(
        database_name("postgres://u@h/db?sslmode=disable"),
        Some("db".to_string())
    );
    assert_eq!(database_name("postgres://u@h/"), None);
    assert_eq!(database_name("postgres://u@h"), None);
}

#[test]
fn test_is_uuid_takes_only_a_lower_case_uuid() {
    assert!(is_uuid(UUID));
    assert!(!is_uuid(""));
    assert!(!is_uuid("0f8e2a5c-1b2d-4e6f-8a9b-0c1d2e3f4a5"));
    assert!(!is_uuid("0f8e2a5c-1b2d-4e6f-8a9b-0c1d2e3f4a5b-"));
    assert!(!is_uuid("0f8e2a5c-1b2d-4e6f-8a9b-0c1d2e3f4a5g"));
}

// --- Pure: arguments ---

#[test]
fn test_parse_args_requires_both_urls() {
    let both =
        parse_args(["--database-url", SCRATCH, "--dev-database-url", DEV].map(str::to_string))
            .expect("both URLs parse");
    assert_eq!(both.database_url.as_deref(), Some(SCRATCH));
    assert_eq!(both.dev_database_url.as_deref(), Some(DEV));
    assert!(!both.dry_run && !both.list);

    assert!(parse_args(["--database-url".to_string(), SCRATCH.to_string()]).is_err());
    assert!(parse_args(["--dev-database-url".to_string(), DEV.to_string()]).is_err());
    assert!(parse_args(["--database-url".to_string()]).is_err());
}

#[test]
fn test_parse_args_takes_list_alone_and_rejects_unknown_flags() {
    let listed = parse_args(["--list".to_string()]).expect("--list parses");
    assert!(listed.list);
    assert!(parse_args(["--list".to_string(), "--dry-run".to_string()]).is_err());
    assert!(parse_args(["--nope".to_string()]).is_err());
}

/// `--dry-run` is a flag, and a URL's value is the argument after its
/// flag, never a flag itself.
#[test]
fn test_parse_args_takes_dry_run_with_the_urls() {
    let args = parse_args(
        ["--dry-run", "--database-url", SCRATCH, "--dev-database-url", DEV].map(str::to_string),
    )
    .expect("parses");
    assert!(args.dry_run && !args.list);
}

// --- Against a real Postgres ---

/// A unique number for a test's own object and database names: the
/// same seed-once-plus-counter scheme as the server's `crate::id_alloc`
/// (this bin is its own crate and can't reach it; SME-255) — nanos
/// mixed with the pid read once, then a call counter. The bare
/// `as_nanos()` this replaces handed two calls in one clock tick the
/// same scratch database and object names.
fn unique() -> u128 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    static CALLS: AtomicU64 = AtomicU64::new(0);
    let seed = *SEED.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default() as u64;
        nanos ^ u64::from(std::process::id()).rotate_left(32)
    });
    u128::from(seed) * 1_000 + u128::from(CALLS.fetch_add(1, Ordering::Relaxed))
}

/// SME-255: 1000 back-to-back calls never repeat (the bare-nanos fold
/// did, on a coarse clock, two calls in one tick).
#[test]
fn test_unique_never_repeats() {
    let seen: std::collections::HashSet<u128> = (0..1000).map(|_| unique()).collect();
    assert_eq!(seen.len(), 1000, "unique() repeated a value across 1000 calls");
}

/// A connection of our own to the test database, the pool's own closed
/// first so guard 5's count is only what the test makes.
async fn connection(pool: &PgPool) -> PgConnection {
    let options: PgConnectOptions = pool.connect_options().as_ref().clone();
    pool.close().await;
    PgConnection::connect_with(&options)
        .await
        .expect("a connection to the test database")
}

/// A migrated database gives its instance: a UUID, owning nothing from
/// before SME-115.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_migrated_database_reads_its_instance(pool: PgPool) {
    let mut conn = connection(&pool).await;
    match read_instance(&mut conn).await.expect("the instance") {
        ScratchState::Instance {
            id,
            owns_unlabelled,
        } => {
            assert!(
                is_uuid(&id),
                "a migrated database's instance id must be a UUID: {id}"
            );
            assert!(
                !owns_unlabelled,
                "a scratch database owns no objects from before SME-115"
            );
        }
        other => panic!("a migrated database must read its instance, not {other:?}"),
    }
}

/// A database without `smelt_instance` was never migrated: nothing can
/// carry its instance, so the cleanup succeeds deleting nothing.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_database_without_smelt_instance_reads_as_never_migrated(pool: PgPool) {
    sqlx::query("DROP TABLE smelt_instance")
        .execute(&pool)
        .await
        .expect("drop the table");
    let mut conn = connection(&pool).await;
    assert_eq!(
        read_instance(&mut conn).await.expect("the state"),
        ScratchState::Unmigrated
    );
}

/// A database that isn't there reads as absent, not as a failure.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_missing_database_reads_as_absent(pool: PgPool) {
    let mut options: PgConnectOptions = pool.connect_options().as_ref().clone();
    options = options.database(&format!("smelt_instance_cleanup_missing_{}", unique()));
    assert!(
        open_scratch(&options).await.expect("connecting").is_none(),
        "a database that isn't there must read as absent"
    );
}

/// Guard 5 counts every other connection to the database, and never this
/// one's own.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_other_connections_counts_every_other_connection(pool: PgPool) {
    let database = pool
        .connect_options()
        .get_database()
        .unwrap_or_default()
        .to_string();
    let options: PgConnectOptions = pool.connect_options().as_ref().clone();
    let mut ours = connection(&pool).await;
    assert_eq!(
        other_connections(&mut ours, &database)
            .await
            .expect("count"),
        Vec::<i32>::new(),
        "this tool's own connection must not count"
    );
    let other = PgConnection::connect_with(&options)
        .await
        .expect("a second connection");
    let pids = other_connections(&mut ours, &database)
        .await
        .expect("count");
    assert_eq!(
        pids.len(),
        1,
        "another open connection to {database} must be counted: {pids:?}"
    );
    other.close().await.ok();
}

// --- Against the real cluster, in `smelt-park-test` only ---

mod cluster;
