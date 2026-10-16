//! A provider to send requests to, and how they authenticate (SME-72).

/// How a request authenticates to a provider (SME-72).
#[derive(Clone, PartialEq)]
pub enum Auth {
    /// An `x-api-key` header, as Anthropic (and local Ollama) expect.
    ApiKey(String),
    /// An `Authorization: Bearer` header, for gateways that want one.
    Bearer(String),
}

// By hand, so a secret never reaches a log line.
impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApiKey(_) => f.write_str("ApiKey(..)"),
            Self::Bearer(_) => f.write_str("Bearer(..)"),
        }
    }
}

/// A model provider to send requests to: its base URL and credential,
/// from its `inference_providers` row (`providers::endpoint`).
#[derive(Clone, Debug, PartialEq)]
pub struct Endpoint {
    pub base_url: String,
    pub auth: Auth,
    /// The provider whose request slots a model call takes (SME-162),
    /// looked up at each call (`providers::limiter`), so a limit changed
    /// mid-turn applies from the turn's next call. `None` for every
    /// probe, which takes none.
    pub slots_of: Option<i64>,
}

/// How long a model call waits for a request slot before it says so in
/// the log (SME-162).
const SLOW_SLOT_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

impl Endpoint {
    /// Waits for one of the provider's request slots, when it has a limit
    /// (SME-162), and holds it until the returned slot drops: when the
    /// call ends, fails, or is dropped (a Stop). A new count resizes the
    /// pool it waits on; a pool closed while it waits (the limit was
    /// cleared) sends it on at once, or to the pool set since. A wait past
    /// `SLOW_SLOT_WAIT` is logged once, naming the host only. `None` when
    /// the provider has no limit; otherwise the slot, if it ended with one,
    /// and how long it waited (review 2: either way).
    pub(super) async fn slot(&self) -> Option<(Option<crate::providers::limiter::Slot>, std::time::Duration)> {
        let provider_id = self.slots_of?;
        let mut pool = crate::providers::limiter::current(provider_id)?;
        let started = tokio::time::Instant::now();
        let slow = started + SLOW_SLOT_WAIT;
        let mut warned = false;
        loop {
            let acquired = {
                let acquire = pool.acquire();
                tokio::pin!(acquire);
                if warned {
                    acquire.await
                } else {
                    tokio::select! {
                        acquired = &mut acquire => acquired,
                        () = tokio::time::sleep_until(slow) => {
                            warned = true;
                            let host = url::Url::parse(&self.base_url).ok().and_then(|u| u.host_str().map(str::to_string));
                            tracing::warn!(
                                waited_secs = SLOW_SLOT_WAIT.as_secs(),
                                host = host.as_deref().unwrap_or(""),
                                "waiting for a free request slot on the model provider"
                            );
                            acquire.await
                        }
                    }
                }
            };
            if acquired.is_some() {
                return Some((acquired, started.elapsed()));
            }
            // Only a pool whose limit was cleared refuses: look again.
            match crate::providers::limiter::current(provider_id) {
                Some(next) => pool = next,
                None => return Some((None, started.elapsed())),
            }
        }
    }

    /// `path` (starting with `/`) under the base URL.
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url.trim_end_matches('/'))
    }

    /// The HTTP client for requests to this provider. It doesn't follow
    /// redirects: reqwest drops `Authorization` on a redirect to another
    /// host but not `x-api-key`, so following one could hand the key to a
    /// host it wasn't entered for. Anthropic-compatible APIs don't redirect.
    /// One client for every provider, so connections are reused.
    pub fn client(&self) -> Result<reqwest::Client, String> {
        static CLIENT: std::sync::LazyLock<Result<reqwest::Client, String>> =
            std::sync::LazyLock::new(|| {
                reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    // smelt's own name on every model request, listing
                    // included: Kimi Code's terms count a hidden or faked
                    // client identity as a violation (SME-106).
                    .user_agent(concat!("smelt/", env!("CARGO_PKG_VERSION")))
                    .build()
                    .map_err(|e| format!("couldn't set up the HTTP client: {e}"))
            });
        CLIENT.clone()
    }

    /// Adds the credential and the `anthropic-version` header every
    /// Anthropic-compatible request carries.
    pub fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let request = request.header("anthropic-version", "2023-06-01");
        match &self.auth {
            Auth::ApiKey(key) => request.header("x-api-key", key),
            Auth::Bearer(token) => request.header("Authorization", format!("Bearer {token}")),
        }
    }
}
