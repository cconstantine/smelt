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

fn permits(endpoint: &Endpoint) -> Option<usize> {
    endpoint.limiter.as_ref().map(|pool| pool.available_permits())
}

#[test]
fn test_a_limit_gives_one_pool_of_that_many_slots_until_it_changes() {
    let id = 9_162_000_001;
    assert!(limiter::slots(id, None).is_none(), "no limit, no pool");

    let first = limiter::slots(id, Some(2)).expect("a pool");
    assert_eq!(first.available_permits(), 2);
    let again = limiter::slots(id, Some(2)).expect("a pool");
    assert!(Arc::ptr_eq(&first, &again), "the same limit shares one pool");

    let held = first.clone().try_acquire_owned().expect("a free slot");
    let resized = limiter::slots(id, Some(3)).expect("a pool");
    assert!(!Arc::ptr_eq(&first, &resized), "a new limit, a new pool");
    assert_eq!(resized.available_permits(), 3, "the old pool's held slot isn't counted");
    assert_eq!(first.available_permits(), 1, "the old pool still works for what holds it");
    drop(held);
    assert_eq!(first.available_permits(), 2);
}

#[test]
fn test_clearing_a_limit_or_forgetting_a_provider_drops_its_pool() {
    let id = 9_162_000_002;
    let first = limiter::slots(id, Some(2)).expect("a pool");
    assert!(limiter::slots(id, None).is_none(), "cleared");
    let set_again = limiter::slots(id, Some(2)).expect("a pool");
    assert!(!Arc::ptr_eq(&first, &set_again), "cleared and set again starts afresh");

    limiter::forget(id);
    let after_forget = limiter::slots(id, Some(2)).expect("a pool");
    assert!(!Arc::ptr_eq(&set_again, &after_forget), "forgotten, then built afresh");
}

#[test]
fn test_an_endpoint_carries_its_providers_slots() {
    let fixed = provider_row(9_162_000_003, Some(2), Some(5));
    let a = endpoint(&fixed);
    let b = endpoint(&fixed);
    assert_eq!(permits(&a), Some(2), "a set count wins over the server's");
    assert!(
        Arc::ptr_eq(a.limiter.as_ref().expect("slots"), b.limiter.as_ref().expect("slots")),
        "two builds share the one pool"
    );
    assert_eq!(a, b, "and still compare equal");

    assert_eq!(permits(&endpoint(&provider_row(9_162_000_004, None, Some(5)))), None, "no limit");
}

/// The user's answer 2: `0` takes the slot count the server reported
/// (`/props`' total_slots), and is no limit until one is read.
#[test]
fn test_automatic_takes_the_servers_slot_count_once_it_has_one() {
    let id = 9_162_000_005;
    assert_eq!(resolved_max(&provider_row(id, Some(0), None)), None, "nothing read yet");
    assert_eq!(permits(&endpoint(&provider_row(id, Some(0), None))), None);
    assert_eq!(resolved_max(&provider_row(id, Some(0), Some(0))), None, "a server reporting no slots");

    let three = endpoint(&provider_row(id, Some(0), Some(3)));
    assert_eq!(permits(&three), Some(3));
    let one = endpoint(&provider_row(id, Some(0), Some(1)));
    assert_eq!(permits(&one), Some(1), "a new report, read on the next build");
    assert!(!Arc::ptr_eq(three.limiter.as_ref().expect("slots"), one.limiter.as_ref().expect("slots")));
}

/// Deleting a provider forgets its pool, which would otherwise stay
/// until a restart.
#[sqlx::test]
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
    let built = endpoint(&row).limiter.expect("slots");

    delete_provider(&pool, row.id).await.expect("delete");

    let rebuilt = limiter::slots(row.id, Some(2)).expect("a pool");
    assert!(!Arc::ptr_eq(&built, &rebuilt), "the deleted provider's pool was forgotten");
}
