# Web search tool (websearch)

**Branch:** `websearch` · **Idea:** `projects/ideas/websearch.md`

## What

A native `websearch(query, limit)` tool that runs a query against a search provider and returns a short, bounded list of results (title, URL, snippet, date when known). `webfetch` and `http_request` only help once the model already knows a URL. Most research starts from a question, and this closes that gap.

Three providers, all behind one tool, chosen by configuration:

| Provider | Endpoint | Auth | Results | Cost (checked 2026-09-25) |
|---|---|---|---|---|
| **Exa** | `POST https://api.exa.ai/search` (`query`, `numResults`, `type: "auto"`, `contents.highlights`) | `x-api-key` header | `results[]`: `title`, `url`, `publishedDate`, `highlights[]` | $7/1k + $1/1k for highlights; $10/month free, no payment method |
| **Kagi** | v1 search (see open questions for the exact shape) | `Authorization: Bot <key>` | `data.search[]`: `url`, `title`, `snippet`, `time` | $12/1k; account + payment method |
| **Brave** | `GET https://api.search.brave.com/res/v1/web/search?q=…&count=…` | `X-Subscription-Token` header | `web.results[]`: `title`, `url`, `description`, `extra_snippets[]` (with `extra_snippets=true`) | $5/1k, $5/month free credit; account + card |

Bing's API was retired in August 2025 and Google's Custom Search API is closed to new customers, so neither is an option. SearXNG (no signup) was considered and left out for now. It can be added later as a fourth adapter.

## How

### Module: `src/websearch.rs` (server only, like `http_request.rs`)

```rust
#[derive(Serialize)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,           // Exa: highlights joined; Brave: description + extra_snippets; Kagi: snippet
    pub published: Option<String>, // as the provider gives it
}

#[derive(Serialize)]
pub struct SearchResponse {
    pub provider: String,          // "exa" / "kagi" / "brave", so the model knows the source
    pub query: String,
    pub results: Vec<SearchResult>,
}

pub enum Provider { Exa, Kagi, Brave }

pub struct ProviderConfig { provider: Provider, api_key: String, base_url: String }

pub async fn search(query: &str, limit: usize) -> Result<SearchResponse, String>;          // reads config from env
async fn search_with(config: &ProviderConfig, query: &str, limit: usize) -> Result<SearchResponse, String>;
```

- **One function per provider** builds the request and converts the provider's JSON into `SearchResult`s. Each is pure over a parsed body (`parse_exa`, `parse_kagi`, `parse_brave`), so it's unit-testable from a fixture without any network.
- **Configuration** (env vars, in setup.md's table, same shape as `ANTHROPIC_API_KEY`):
  - `EXA_API_KEY`, `KAGI_API_KEY`, `BRAVE_API_KEY`: whichever are set are available.
  - `WEBSEARCH_PROVIDER` = `exa` | `kagi` | `brave`: which one to use. If unset, the first configured one in that order. If set to a provider with no key, startup logs a warning and the tool reports it clearly when called.
  - `base_url` is a field, not an env var, so tests point an adapter at a local mock server by building a `ProviderConfig` directly. No process-global env lock needed.
- **Bounds:** `limit` defaults to 5, capped at 10 (schema `minimum: 1, maximum: 10`). Each snippet is capped at ~500 characters on a char boundary, so a result set stays a few KB. One `REQUEST_TIMEOUT` (20s) on the client, as in `http_request`.
- **Errors** are readable tool errors, not raw bodies: `websearch (brave) failed: HTTP 401: <provider's own message>`, with the provider's JSON error unwrapped where it has one (same idea as `provider_error_message` in `anthropic::stream`). No retry: a failed search is cheap for the model to retry itself, unlike a failed turn.
- **No SSRF guard needed:** the tool only ever calls the three fixed provider hosts. The model controls the query, never the URL. The URLs *in* the results are just data; fetching one goes through `webfetch`/`http_request` and their guard as usual.

### Tool wiring: `src/anthropic/tools.rs`

- A `websearch` `ToolDefinition`: `query` (string, required) and `limit` (integer, 1–10). The description says it returns links and snippets, and points to `webfetch`/`http_request` to read a result in full.
- Dispatch `"websearch" => websearch_tool(input)` next to `webfetch`/`http_request`.
- **Only offered when a provider is configured:** `tool_definitions` leaves `websearch` out when no key is set, so the model never sees a tool that can only fail.

### UI

Nothing new. A `websearch` call and its result render like every other tool call and result.

### Tests (test-first, per development-process.md)

- **Parsing, per provider:** a fixture response parses into the right `SearchResult`s (title, URL, snippet, date), including missing optional fields and an empty result list. Per the "fixture real artifacts" rule, the fixtures should be real captured responses. Until keys exist, they start from each provider's documented example, marked as such; see open questions.
- **Snippet shaping:** Exa's highlights joined, Brave's `description` plus `extra_snippets`, the ~500-character cap splitting on a char boundary (non-ASCII input), and HTML tags that Brave puts in descriptions (`<strong>`) removed.
- **Request building, per provider, against a local mock server:** the right method, path, auth header and query/limit are sent, and the mock's response comes back as parsed results.
- **Errors:** a 401 and a 429 from the mock become the readable error text, and a hang hits the timeout.
- **Configuration:** provider selection from the env values (a pure function over the values, like `require_at_least_one_credential`, so no env mutation in tests): explicit choice, the fallback order, an explicit choice with no key, and nothing configured.
- **Tool wiring:** `tool_definitions` includes `websearch` only when a provider is configured, and `execute` dispatches to it.
- **Live checks, `#[ignore]`d:** one test per provider that runs a real query when its key is set in the environment, and skips with a message when it isn't. Not in CI (no keys there). Used once per provider to confirm the real API and capture real fixtures.

## Which files

- `src/websearch.rs`: new; providers, parsing, configuration, tests.
- `src/main.rs`: `#[cfg(feature = "server")] mod websearch;`
- `src/anthropic/tools.rs`: the tool definition, dispatch, the configured-only filter in `tool_definitions`, and wiring tests.
- `src/websearch/fixtures/` (or inline `const`s): one response per provider.
- `docs/setup.md`: the four env vars.
- `docs/architecture.md`: a module-table row for `src/websearch.rs`.
- Close-out: `docs/projects/completed/YYYYMMDD-websearch.md`, `docs/projects/state.md` (features, and the "on hold" note in Goals), removing `docs/projects/ideas/websearch.md` and this plan.

## Open questions and tradeoffs

1. **Real keys for verification.** The adapters can be built and tested against mocks without keys, but the only proof that each one matches the real API is a real call. Can you set any of `EXA_API_KEY` / `KAGI_API_KEY` / `BRAVE_API_KEY` in this environment (your `.env`) for the live checks? Each check is a handful of queries, well within the free credits for Exa and Brave. A provider nobody can verify would ship marked "unverified against the real API" in the completed doc.
2. **Kagi's exact request shape.** Kagi's help page shows `GET https://kagi.com/api/v1/search?q=…`, while its API reference shows `POST /search` with a JSON body (`query`, `limit`). Proposal: follow the API reference, and settle it with the live check.
3. **One provider per server, or the model chooses?** Proposal: one active provider (`WEBSEARCH_PROVIDER`), with no provider parameter on the tool. It's simpler, and the model has no good basis for picking between them. Alternative: an optional `provider` argument listing only the configured ones, useful for comparing results.
4. **Exa content:** highlights only (proposed; +$1/1k, short and query-relevant), or also full page text (more tokens, and mostly overlaps `webfetch`)?
5. **Configuration in the UI?** Env vars match how the Anthropic key is set today. A settings page for search keys could come later if switching providers turns out to be common.
