//! Model prices from the models.dev catalog (SME-106), for what each
//! model call cost. Fetched at startup and every hour; the last good copy
//! is kept in Postgres so a restart without network still has prices.
//! opencode does the same (`packages/core/src/models-dev.ts`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::anthropic::TokenUsage;

/// One set of prices, in dollars per million tokens. A missing cache
/// price is charged at the input price.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Prices {
    pub input: f64,
    pub output: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

/// A context-size tier: `prices` apply to a call whose input (cached or
/// not) is over `above` tokens.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Tier {
    pub above: u64,
    pub prices: Prices,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ModelPrices {
    pub base: Prices,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tiers: Vec<Tier>,
}

/// A catalog provider: a service, or a plan on one (a coding plan is its
/// own provider, priced at $0).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct CatalogProvider {
    pub name: String,
    /// Its API's base URL, when the catalog says; matched against a smelt
    /// provider's address to suggest the entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    pub models: BTreeMap<String, ModelPrices>,
}

/// The catalog's providers by id, keeping only what pricing needs.
pub type Providers = BTreeMap<String, CatalogProvider>;

/// Reads models.dev's `api.json`. A model without input and output
/// prices is left out: it has no price.
pub fn parse(json: &str) -> Result<Providers, String> {
    #[derive(Deserialize)]
    struct RawProvider {
        #[serde(default)]
        name: String,
        #[serde(default)]
        api: Option<String>,
        /// Read model by model below, so one malformed entry loses only
        /// itself: the catalog is community-edited.
        #[serde(default)]
        models: serde_json::Value,
    }
    #[derive(Deserialize)]
    struct RawModel {
        cost: RawCost,
    }
    #[derive(Deserialize)]
    struct RawCost {
        input: f64,
        output: f64,
        cache_read: Option<f64>,
        cache_write: Option<f64>,
        #[serde(default)]
        tiers: Vec<serde_json::Value>,
    }
    #[derive(Deserialize)]
    struct RawTier {
        input: f64,
        output: f64,
        cache_read: Option<f64>,
        cache_write: Option<f64>,
        tier: RawTierSize,
    }
    #[derive(Deserialize)]
    struct RawTierSize {
        #[serde(rename = "type")]
        kind: String,
        size: u64,
    }

    let raw: BTreeMap<String, RawProvider> =
        serde_json::from_str(json).map_err(|e| format!("not the models.dev catalog: {e}"))?;
    let model = |raw: serde_json::Value| -> Option<ModelPrices> {
        let cost = serde_json::from_value::<RawModel>(raw).ok()?.cost;
        // Only context-size tiers are understood; any other kind is left
        // out rather than guessed at.
        let tiers = cost
            .tiers
            .into_iter()
            .filter_map(|t| serde_json::from_value::<RawTier>(t).ok())
            .filter(|t| t.tier.kind == "context")
            .map(|t| Tier {
                above: t.tier.size,
                prices: Prices { input: t.input, output: t.output, cache_read: t.cache_read, cache_write: t.cache_write },
            })
            .collect();
        Some(ModelPrices {
            base: Prices { input: cost.input, output: cost.output, cache_read: cost.cache_read, cache_write: cost.cache_write },
            tiers,
        })
    };
    Ok(raw
        .into_iter()
        .map(|(id, provider)| {
            let models = match provider.models {
                serde_json::Value::Object(models) => models
                    .into_iter()
                    .filter_map(|(id, raw)| Some((id, model(raw)?)))
                    .collect(),
                _ => BTreeMap::new(),
            };
            (id, CatalogProvider { name: provider.name, api: provider.api, models })
        })
        .collect())
}

/// What a call with `usage` cost, in dollars, at `prices`. Anthropic-style
/// usage: `input_tokens` excludes the cached tokens, which are counted
/// apart. The tier is picked from all three, as opencode does.
pub fn cost(prices: &ModelPrices, usage: &TokenUsage) -> f64 {
    let context = (usage.input_tokens + usage.cache_creation_input_tokens + usage.cache_read_input_tokens).max(0) as u64;
    let applied = prices
        .tiers
        .iter()
        .filter(|tier| context > tier.above)
        .max_by_key(|tier| tier.above)
        .map_or(&prices.base, |tier| &tier.prices);
    let per_token = |count: i64, dollars_per_million: f64| count as f64 * dollars_per_million / 1_000_000.0;
    per_token(usage.input_tokens, applied.input)
        + per_token(usage.cache_creation_input_tokens, applied.cache_write.unwrap_or(applied.input))
        + per_token(usage.cache_read_input_tokens, applied.cache_read.unwrap_or(applied.input))
        + per_token(usage.output_tokens, applied.output)
}

/// `model`'s prices at catalog provider `provider`: its exact id, or the
/// same id in another case.
pub fn model_prices<'a>(providers: &'a Providers, provider: &str, model: &str) -> Option<&'a ModelPrices> {
    let models = &providers.get(provider)?.models;
    models.get(model).or_else(|| {
        models
            .iter()
            .find_map(|(id, prices)| id.eq_ignore_ascii_case(model).then_some(prices))
    })
}

/// The catalog, unless `SMELT_PRICE_CATALOG_URL` says otherwise.
pub const DEFAULT_CATALOG_URL: &str = "https://models.dev/api.json";

/// How often the catalog is fetched again, as opencode does.
const REFRESH_EVERY: std::time::Duration = std::time::Duration::from_secs(60 * 60);
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// The whole catalog was 5.3 MB on 2026-10-05.
const FETCH_MAX_BYTES: usize = 32 * 1024 * 1024;

/// A catalog and when it was fetched.
#[derive(Clone, Debug, PartialEq)]
pub struct Catalog {
    pub fetched_at: chrono::NaiveDateTime,
    pub providers: Providers,
}

/// Where the catalog is kept in memory. One writer, `refresh` (and
/// `load_saved` at startup); readers take the current copy and keep it
/// for as long as they need it.
pub struct CatalogStore(std::sync::RwLock<Option<std::sync::Arc<Catalog>>>);

/// The catalog calls are priced with, kept current by `start`.
pub static CATALOG: CatalogStore = CatalogStore::new();

impl CatalogStore {
    pub const fn new() -> Self {
        Self(std::sync::RwLock::new(None))
    }

    pub fn current(&self) -> Option<std::sync::Arc<Catalog>> {
        self.0.read().map(|c| c.clone()).unwrap_or_else(|e| e.into_inner().clone())
    }

    /// Puts `providers` in place, as a refresh would, for tests elsewhere
    /// that price calls.
    #[cfg(test)]
    pub fn set_for_test(&self, providers: Providers) {
        self.set(Catalog { fetched_at: chrono::Utc::now().naive_utc(), providers });
    }

    fn set(&self, catalog: Catalog) {
        let mut slot = self.0.write().unwrap_or_else(|e| e.into_inner());
        *slot = Some(std::sync::Arc::new(catalog));
    }

    /// Takes the saved copy, unless one was fetched already.
    pub async fn load_saved(&self, pool: &sqlx::PgPool) -> Result<(), String> {
        let Some((fetched_at, providers)) = crate::db::get_price_catalog(pool).await.map_err(|e| e.to_string())? else {
            return Ok(());
        };
        let providers: Providers =
            serde_json::from_value(providers).map_err(|e| format!("the saved price catalog is unreadable: {e}"))?;
        if self.current().is_none() {
            self.set(Catalog { fetched_at, providers });
        }
        Ok(())
    }

    /// Fetches the catalog from `url` and keeps it, in memory and in
    /// Postgres. A fetch that fails or isn't the catalog changes nothing.
    /// A save that fails still leaves the new copy in memory; the next
    /// refresh saves again.
    pub async fn refresh(&self, pool: &sqlx::PgPool, url: &str) -> Result<(), String> {
        let providers = parse(&fetch(url).await?)?;
        if providers.values().all(|p| p.models.is_empty()) {
            return Err(format!("{url} lists no prices"));
        }
        // To the microsecond, as Postgres keeps it, so a restart's copy is
        // the same as this one.
        let fetched_at = chrono::SubsecRound::trunc_subsecs(chrono::Utc::now().naive_utc(), 6);
        let saved = serde_json::to_value(&providers).map_err(|e| e.to_string())?;
        self.set(Catalog { fetched_at, providers });
        crate::db::save_price_catalog(pool, fetched_at, &saved)
            .await
            .map_err(|e| format!("fetched the price catalog but couldn't save it: {e}"))
    }
}

async fn fetch(url: &str) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .build()
        .map_err(|e| format!("couldn't make an HTTP client: {e}"))?;
    let mut response = client.get(url).send().await.map_err(|e| format!("couldn't reach {url}: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("{url} answered {}", response.status()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        body.extend_from_slice(&chunk);
        if body.len() > FETCH_MAX_BYTES {
            return Err(format!("{url} is too big (over {} MB)", FETCH_MAX_BYTES / 1024 / 1024));
        }
    }
    String::from_utf8(body).map_err(|_| format!("{url} isn't text"))
}

/// `SMELT_PRICE_CATALOG_URL`, set-but-empty treated as unset (as
/// everywhere else in smelt).
pub fn catalog_url() -> String {
    std::env::var("SMELT_PRICE_CATALOG_URL")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_CATALOG_URL.to_string())
}

/// Loads the saved catalog, then fetches it now and every hour.
pub fn start(pool: sqlx::PgPool) {
    tokio::spawn(async move {
        if let Err(e) = CATALOG.load_saved(&pool).await {
            tracing::warn!(error = %e, "couldn't load the saved price catalog");
        }
        let url = catalog_url();
        loop {
            match CATALOG.refresh(&pool, &url).await {
                Ok(()) => tracing::info!("price catalog refreshed"),
                Err(e) => tracing::warn!(error = %e, "couldn't refresh the price catalog; keeping the last one"),
            }
            tokio::time::sleep(REFRESH_EVERY).await;
        }
    });
}

/// The current catalog's providers that price at least one model, by
/// name, for the provider form.
pub fn price_sources() -> Vec<crate::providers::PriceSource> {
    let Some(catalog) = CATALOG.current() else {
        return Vec::new();
    };
    let mut sources: Vec<_> = catalog
        .providers
        .iter()
        .filter(|(_, provider)| !provider.models.is_empty())
        .map(|(id, provider)| crate::providers::PriceSource {
            id: id.clone(),
            name: if provider.name.is_empty() { id.clone() } else { provider.name.clone() },
            api: provider.api.clone(),
        })
        .collect();
    sources.sort_by_key(|s| s.name.to_lowercase());
    sources
}

/// What a call cost, priced from the current catalog as catalog provider
/// `catalog_provider`; `None` without a catalog, a provider or a price.
pub fn call_cost(catalog_provider: Option<&str>, model: &str, usage: &TokenUsage) -> Option<f64> {
    let catalog = CATALOG.current()?;
    Some(cost(model_prices(&catalog.providers, catalog_provider?, model)?, usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed `api.json`: real field names and shapes (2026-10-05).
    const CATALOG: &str = r#"{
        "anthropic": {
            "id": "anthropic", "name": "Anthropic", "npm": "@ai-sdk/anthropic",
            "models": {
                "claude-opus-5-5": {"id": "claude-opus-5-5", "name": "Claude Opus 5.5",
                    "cost": {"input": 4, "output": 20, "cache_read": 0.2, "cache_write": 5}},
                "claude-unpriced": {"id": "claude-unpriced", "name": "No cost field"}
            }
        },
        "alibaba": {
            "id": "alibaba", "name": "Alibaba", "api": "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
            "models": {
                "qwen3.7-plus": {"id": "qwen3.7-plus", "cost": {
                    "input": 0.4, "output": 1.6, "cache_read": 0.04, "cache_write": 0.5,
                    "tiers": [{"input": 1.2, "output": 4.8, "cache_read": 0.12, "cache_write": 1.5,
                               "tier": {"type": "context", "size": 256000}}],
                    "context_over_200k": {"input": 1.2, "output": 4.8}}}
            }
        },
        "deepseek": {
            "id": "deepseek", "name": "DeepSeek", "api": "https://api.deepseek.com",
            "models": {
                "deepseek-v4-pro": {"id": "deepseek-v4-pro",
                    "cost": {"input": 0.66, "output": 1.98, "reasoning": 1.98, "cache_read": 0.022}}
            }
        },
        "minimax-coding-plan": {
            "id": "minimax-coding-plan", "name": "MiniMax Coding Plan", "api": "https://api.minimax.io/anthropic/v1",
            "models": {
                "MiniMax-M3": {"id": "MiniMax-M3", "cost": {"input": 0, "output": 0, "cache_read": 0, "cache_write": 0}}
            }
        }
    }"#;

    fn usage(input: i64, write: i64, read: i64, output: i64) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            output_tokens: output,
            cache_creation_input_tokens: write,
            cache_read_input_tokens: read,
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn test_parse_keeps_each_providers_name_address_and_priced_models() {
        let providers = parse(CATALOG).expect("parses");
        let anthropic = &providers["anthropic"];
        assert_eq!(anthropic.name, "Anthropic");
        assert_eq!(anthropic.api, None);
        assert_eq!(
            anthropic.models["claude-opus-5-5"],
            ModelPrices {
                base: Prices { input: 4.0, output: 20.0, cache_read: Some(0.2), cache_write: Some(5.0) },
                tiers: vec![],
            }
        );
        assert!(!anthropic.models.contains_key("claude-unpriced"), "no cost, no price");
        assert_eq!(providers["deepseek"].api.as_deref(), Some("https://api.deepseek.com"));
        assert_eq!(providers["deepseek"].models["deepseek-v4-pro"].base.cache_write, None);
        assert_eq!(
            providers["alibaba"].models["qwen3.7-plus"].tiers,
            vec![Tier {
                above: 256_000,
                prices: Prices { input: 1.2, output: 4.8, cache_read: Some(0.12), cache_write: Some(1.5) },
            }]
        );
        assert_eq!(providers["minimax-coding-plan"].models["MiniMax-M3"].base, Prices {
            input: 0.0,
            output: 0.0,
            cache_read: Some(0.0),
            cache_write: Some(0.0),
        });
    }

    #[test]
    fn test_one_malformed_entry_doesnt_lose_the_rest() {
        let providers = parse(
            r#"{
                "odd": {"id": "odd", "name": "Odd", "models": "not a map"},
                "p": {"id": "p", "name": "P", "models": {
                    "bad": {"cost": {"input": "4 dollars", "output": 20}},
                    "good": {"cost": {"input": 1, "output": 2}}
                }}
            }"#,
        )
        .expect("parses");
        assert!(providers["odd"].models.is_empty());
        assert!(!providers["p"].models.contains_key("bad"));
        assert_eq!(providers["p"].models["good"].base.output, 2.0);
    }

    #[test]
    fn test_parse_refuses_what_isnt_the_catalog() {
        assert!(parse("<html>rate limited</html>").is_err());
        assert!(parse(r#"{"anthropic": "nope"}"#).is_err());
    }

    #[test]
    fn test_a_calls_cost_charges_each_kind_of_token_at_its_own_price() {
        let providers = parse(CATALOG).expect("parses");
        let opus = &providers["anthropic"].models["claude-opus-5-5"];
        // 1M uncached at $4, 1M written at $5, 10M read at $0.20, 0.1M out at $20.
        let dollars = cost(opus, &usage(1_000_000, 1_000_000, 10_000_000, 100_000));
        assert!(close(dollars, 4.0 + 5.0 + 2.0 + 2.0), "{dollars}");
    }

    #[test]
    fn test_a_missing_cache_price_is_charged_as_input() {
        let providers = parse(CATALOG).expect("parses");
        let deepseek = &providers["deepseek"].models["deepseek-v4-pro"];
        let dollars = cost(deepseek, &usage(0, 1_000_000, 1_000_000, 0));
        assert!(close(dollars, 0.66 + 0.022), "a write at the input price, a read at its own: {dollars}");
    }

    #[test]
    fn test_a_call_over_a_tiers_size_pays_that_tiers_prices() {
        let providers = parse(CATALOG).expect("parses");
        let qwen = &providers["alibaba"].models["qwen3.7-plus"];
        let at_size = cost(qwen, &usage(56_000, 0, 200_000, 0));
        assert!(close(at_size, 0.056 * 0.4 + 0.2 * 0.04), "256,000 exactly isn't over: {at_size}");
        let over = cost(qwen, &usage(56_001, 0, 200_000, 0));
        assert!(close(over, 0.056001 * 1.2 + 0.2 * 0.12), "cached input counts toward the size: {over}");
    }

    #[test]
    fn test_a_plans_zero_prices_cost_nothing() {
        let providers = parse(CATALOG).expect("parses");
        let plan = &providers["minimax-coding-plan"].models["MiniMax-M3"];
        assert_eq!(cost(plan, &usage(1_000, 1_000, 1_000, 1_000)), 0.0);
    }

    #[test]
    fn test_model_prices_match_the_id_in_any_case_and_nothing_else() {
        let providers = parse(CATALOG).expect("parses");
        assert!(model_prices(&providers, "anthropic", "claude-opus-5-5").is_some());
        assert!(model_prices(&providers, "minimax-coding-plan", "minimax-m3").is_some(), "case");
        assert!(model_prices(&providers, "anthropic", "claude-unpriced").is_none());
        assert!(model_prices(&providers, "anthropic", "claude-opus").is_none(), "no prefix guessing");
        assert!(model_prices(&providers, "nobody", "claude-opus-5-5").is_none());
    }

    /// Serves each of `bodies` in turn (the last repeats) at `/api.json`.
    async fn catalog_server(bodies: Vec<(u16, String)>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let served = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let app = axum::Router::new().route(
            "/api.json",
            axum::routing::get(move || {
                let bodies = bodies.clone();
                let served = served.clone();
                async move {
                    let i = served.fetch_add(1, std::sync::atomic::Ordering::SeqCst).min(bodies.len() - 1);
                    let (status, body) = bodies[i].clone();
                    (axum::http::StatusCode::from_u16(status).expect("status"), body)
                }
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        format!("http://{addr}/api.json")
    }

    #[sqlx::test]
    async fn test_a_refresh_keeps_the_catalog_in_memory_and_saved(pool: sqlx::PgPool) {
        let url = catalog_server(vec![(200, CATALOG.to_string())]).await;
        let store = CatalogStore::new();
        assert!(store.current().is_none());

        store.refresh(&pool, &url).await.expect("refresh");
        let fetched = store.current().expect("in memory");
        assert!(model_prices(&fetched.providers, "anthropic", "claude-opus-5-5").is_some());

        let restarted = CatalogStore::new();
        restarted.load_saved(&pool).await.expect("load");
        assert_eq!(restarted.current().as_deref(), Some(fetched.as_ref()), "a restart has the saved copy");
    }

    #[sqlx::test]
    async fn test_a_failed_refresh_keeps_the_last_good_catalog(pool: sqlx::PgPool) {
        let url = catalog_server(vec![
            (200, CATALOG.to_string()),
            (500, "oops".to_string()),
            (200, "<html>rate limited</html>".to_string()),
            (200, "{}".to_string()),
        ])
        .await;
        let store = CatalogStore::new();
        store.refresh(&pool, &url).await.expect("first refresh");
        let good = store.current().expect("in memory");

        for _ in 0..3 {
            assert!(store.refresh(&pool, &url).await.is_err());
            assert_eq!(store.current(), Some(good.clone()), "memory unchanged");
        }
        let restarted = CatalogStore::new();
        restarted.load_saved(&pool).await.expect("load");
        assert_eq!(restarted.current().map(|c| c.fetched_at), Some(good.fetched_at), "saved copy unchanged");
    }

    #[sqlx::test]
    async fn test_loading_the_saved_copy_doesnt_replace_a_fresher_fetch(pool: sqlx::PgPool) {
        let url = catalog_server(vec![(200, CATALOG.to_string())]).await;
        let old = CatalogStore::new();
        old.refresh(&pool, &url).await.expect("an earlier run's fetch");
        let store = CatalogStore::new();
        store.refresh(&pool, &url).await.expect("this run's fetch");
        let fresh = store.current().expect("fetched");
        sqlx::query("UPDATE price_catalog SET fetched_at = fetched_at - interval '1 day'")
            .execute(&pool)
            .await
            .expect("age the saved copy");
        store.load_saved(&pool).await.expect("load");
        assert_eq!(store.current(), Some(fresh));
    }
}
