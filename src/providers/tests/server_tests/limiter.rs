//! SME-162: a provider's request slots. The pools are process-wide and
//! keyed by provider id, while every `#[sqlx::test]` database numbers its
//! providers from 1; so these tests use ids of their own (9_162_…), on
//! rows built in memory, and never share one between tests.

use super::*;
use crate::providers::limiter;

fn provider_row(id: i64, max: Option<i32>, total_slots: Option<u32>) -> db::InferenceProvider {
    let caps = total_slots.map(|n| {
        serde_json::to_value(LlamaServerInfo { total_slots: Some(n), ..Default::default() }).expect("caps")
    });
    db::InferenceProvider {
        id,
        name: "llama".to_string(),
        kind: "llama_cpp".to_string(),
        base_url: "http://llama".to_string(),
        auth_kind: "bearer".to_string(),
        secret: "k".to_string(),
        prompt_caching: false,
        price_catalog_provider: None,
        keep_reasoning: true,
        server_caps: caps,
        max_concurrent_requests: max,
        created_at: chrono::NaiveDateTime::default(),
        updated_at: chrono::NaiveDateTime::default(),
    }
}

/// A row version (`updated_at`), `seconds` after the epoch.
fn at(seconds: i64) -> chrono::NaiveDateTime {
    chrono::DateTime::from_timestamp(seconds, 0).expect("a time").naive_utc()
}

/// The free slots of the provider an endpoint takes its slots from now.
fn permits(endpoint: &Endpoint) -> Option<usize> {
    limiter::current(endpoint.slots_of.expect("a model endpoint")).map(|pool| pool.available_permits())
}

#[test]
fn test_a_limit_keeps_one_pool_resized_in_place() {
    let id = 9_162_000_001;
    limiter::set(id, at(10), None);
    assert!(limiter::current(id).is_none(), "no limit, no pool");

    limiter::set(id, at(20), Some(2));
    let first = limiter::current(id).expect("a pool");
    assert_eq!(first.available_permits(), 2);
    limiter::set(id, at(30), Some(2));
    assert!(Arc::ptr_eq(&first, &limiter::current(id).expect("a pool")), "the same limit keeps its pool");

    // SME-162 review 2: a new count resizes the pool, counting the slots
    // already held.
    let held = first.try_acquire().expect("a free slot");
    limiter::set(id, at(40), Some(3));
    assert!(Arc::ptr_eq(&first, &limiter::current(id).expect("a pool")), "resized in place");
    assert_eq!(first.available_permits(), 2, "3, one held");
    let second = first.try_acquire().expect("a free slot");
    limiter::set(id, at(50), Some(1));
    assert_eq!(first.available_permits(), 0, "1, two held: the one free slot is taken back");
    drop(held);
    assert_eq!(first.available_permits(), 0, "still owed one: the slot given back is kept");
    drop(second);
    assert_eq!(first.available_permits(), 1, "under the limit: the next is freed");
    limiter::set(id, at(60), Some(2));
    assert_eq!(first.available_permits(), 2);
    assert!(!first.is_closed());
}

/// SME-162 review 2: a limit raised before a lowered one's slots are back
/// pays off what's owed first.
#[test]
fn test_raising_a_lowered_limit_pays_off_what_it_owes_first() {
    let id = 9_162_000_009;
    limiter::set(id, at(10), Some(2));
    let pool = limiter::current(id).expect("a pool");
    let (a, b) = (pool.try_acquire().expect("a"), pool.try_acquire().expect("b"));
    limiter::set(id, at(20), Some(1));
    limiter::set(id, at(30), Some(2));
    assert_eq!(pool.available_permits(), 0, "two held, limit 2");
    drop(a);
    assert_eq!(pool.available_permits(), 1);
    drop(b);
    assert_eq!(pool.available_permits(), 2);
}

#[test]
fn test_clearing_a_limit_or_forgetting_a_provider_closes_its_pool() {
    let id = 9_162_000_002;
    limiter::set(id, at(50), Some(2));
    let first = limiter::current(id).expect("a pool");
    limiter::set(id, at(60), None);
    assert!(limiter::current(id).is_none(), "cleared");
    assert!(first.is_closed());

    limiter::set(id, at(70), Some(2));
    let set_again = limiter::current(id).expect("a pool");
    assert!(!Arc::ptr_eq(&first, &set_again), "cleared and set again starts afresh");
    limiter::forget(id);
    assert!(limiter::current(id).is_none(), "forgotten");
    assert!(!set_again.is_closed(), "a forgotten pool isn't closed");
}

#[test]
fn test_an_endpoint_names_its_provider_and_sets_its_pool() {
    let fixed = provider_row(9_162_000_003, Some(2), Some(5));
    let a = endpoint(&fixed);
    assert_eq!(a.slots_of, Some(fixed.id));
    assert_eq!(permits(&a), Some(2), "a set count wins over the server's");
    let pool = limiter::current(fixed.id).expect("a pool");
    endpoint(&fixed);
    assert!(Arc::ptr_eq(&pool, &limiter::current(fixed.id).expect("a pool")), "two builds keep the one pool");

    assert_eq!(permits(&endpoint(&provider_row(9_162_000_004, None, Some(5)))), None, "no limit");
}

/// SME-162 review 1: a turn that read its provider's row before a save
/// sets its pool from that older row; the newer pool stays, open.
#[test]
fn test_a_pool_set_from_an_older_row_is_ignored() {
    let id = 9_162_000_008;
    limiter::set(id, at(20), Some(1));
    let newer = limiter::current(id).expect("a pool");
    limiter::set(id, at(10), Some(2));
    assert!(Arc::ptr_eq(&newer, &limiter::current(id).expect("a pool")), "the older row's 2 is ignored");
    assert!(!newer.is_closed());
    limiter::set(id, at(10), None);
    assert!(limiter::current(id).is_some(), "and so is its no limit");
    limiter::set(id, at(20), Some(3));
    assert_eq!(limiter::current(id).map(|p| p.available_permits()), Some(3), "the same version may change it");
    limiter::set(id, at(30), None);
    assert!(limiter::current(id).is_none(), "a newer row clears it");
    limiter::set(id, at(25), Some(1));
    assert!(limiter::current(id).is_none(), "and an older one can't bring it back");
}

/// The user's answer 2: `0` takes the slot count the server reported
/// (`/props`' total_slots), and is no limit until one is read.
#[test]
fn test_automatic_takes_the_servers_slot_count_once_it_has_one() {
    let id = 9_162_000_005;
    assert_eq!(resolved_max(&provider_row(id, Some(0), None)), None, "nothing read yet");
    assert_eq!(permits(&endpoint(&provider_row(id, Some(0), None))), None);
    assert_eq!(resolved_max(&provider_row(id, Some(0), Some(0))), None, "a server reporting no slots");

    let three = endpoint(&db::InferenceProvider { updated_at: at(10), ..provider_row(id, Some(0), Some(3)) });
    assert_eq!(permits(&three), Some(3));
    endpoint(&db::InferenceProvider { updated_at: at(20), ..provider_row(id, Some(0), Some(1)) });
    assert_eq!(permits(&three), Some(1), "a new report applies to an endpoint built before it");
}

/// Deleting a provider forgets its pool, which would otherwise stay
/// until a restart.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_deleting_a_provider_forgets_its_slots(pool: PgPool) {
    // An id of its own, as above.
    sqlx::query("ALTER TABLE inference_providers ALTER COLUMN id RESTART WITH 9162000006")
        .execute(&pool)
        .await
        .expect("renumber");
    let row = db::create_inference_provider(&pool, "llama", "llama_cpp", "http://llama", "bearer", "k", false, None, true, Some(2))
        .await
        .expect("create");
    assert_eq!(row.id, 9_162_000_006);
    endpoint(&row);
    let built = limiter::current(row.id).expect("slots");

    delete_provider(&pool, row.id).await.expect("delete");

    assert!(limiter::current(row.id).is_none(), "the deleted provider's pool was forgotten");
    assert!(!built.is_closed(), "but not closed: its waiters still take their turn");
}

/// SME-162 review 1: saving a new limit sets the provider's pool at
/// once, not at the next endpoint build, so a turn already running takes
/// it from its next model call.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_saving_a_limit_sets_the_providers_pool_at_once(pool: PgPool) {
    sqlx::query("ALTER TABLE inference_providers ALTER COLUMN id RESTART WITH 9162000007")
        .execute(&pool)
        .await
        .expect("renumber");
    let created = create_provider(&pool, ProviderInput { max_concurrent_requests: Some(2), ..input("h", "http://h", "k") })
        .await
        .expect("create");
    assert_eq!(limiter::current(created.id).map(|p| p.available_permits()), Some(2), "set on create");
    update_provider(&pool, created.id, ProviderInput { max_concurrent_requests: Some(1), ..input("h", "http://h", "") })
        .await
        .expect("update");
    assert_eq!(limiter::current(created.id).map(|p| p.available_permits()), Some(1), "set on update");
    update_provider(&pool, created.id, input("h", "http://h", "")).await.expect("clear");
    assert!(limiter::current(created.id).is_none(), "cleared on update");
}

/// SME-162 review 2: a save always makes a newer version than the last,
/// even when the clock has stepped back (here, a row stamped a day
/// ahead), so the saved limit isn't ignored as older.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_save_after_a_clock_step_back_still_sets_the_limit(pool: PgPool) {
    sqlx::query("ALTER TABLE inference_providers ALTER COLUMN id RESTART WITH 9162000010")
        .execute(&pool)
        .await
        .expect("renumber");
    let created = create_provider(&pool, input("h", "http://h", "k")).await.expect("create");
    sqlx::query("UPDATE inference_providers SET updated_at = now() + interval '1 day' WHERE id = $1")
        .bind(created.id)
        .execute(&pool)
        .await
        .expect("stamp ahead");
    let ahead = db::get_inference_provider(&pool, created.id).await.expect("get").expect("exists");
    endpoint(&ahead);

    update_provider(&pool, created.id, ProviderInput { max_concurrent_requests: Some(1), ..input("h", "http://h", "") })
        .await
        .expect("save 1");
    assert_eq!(limiter::current(created.id).map(|p| p.available_permits()), Some(1), "the save is newer");
    let saved = db::get_inference_provider(&pool, created.id).await.expect("get").expect("exists");
    assert!(saved.updated_at > ahead.updated_at);
}
