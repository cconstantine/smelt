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
}

impl Endpoint {
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
