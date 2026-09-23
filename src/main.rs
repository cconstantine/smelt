mod anthropic;
mod api;
mod browsing;
#[cfg(feature = "server")]
mod db;
mod events;
#[cfg(feature = "server")]
mod fetch_guard;
mod frontend;
#[cfg(feature = "server")]
mod http_request;
#[cfg(feature = "server")]
mod mcp;
#[cfg(feature = "server")]
mod mcp_oauth;
mod models;
#[cfg(feature = "server")]
mod sandbox;
#[cfg(feature = "server")]
mod webfetch;

#[cfg(all(test, feature = "browser-test"))]
mod browser_tests;

/// The real Axum router `main()` serves — factored out so
/// `src/browser_tests.rs` can build the exact same router, in-process, on a
/// test-local port, without duplicating it. See
/// `docs/projects/completed/20260815-sandbox-visibility.md`.
#[cfg(feature = "server")]
fn build_router() -> axum::Router {
    use dioxus::prelude::DioxusRouterExt;

    axum::Router::new()
        // A plain Axum route, not a Dioxus server function — see
        // src/mcp_oauth.rs's `callback_handler` doc comment for why.
        .route(
            "/oauth/mcp-callback/{id}",
            axum::routing::get(mcp_oauth::callback_handler),
        )
        .serve_dioxus_application(dioxus::prelude::ServeConfig::new(), frontend::App)
        .layer(tower_http::trace::TraceLayer::new_for_http())
}

#[cfg(feature = "server")]
#[tokio::main]
async fn main() {
    // Optional: absent in prod, where real env vars are set directly.
    let _ = dotenvy::dotenv();

    // kube's rustls-tls stack only auto-installs a default CryptoProvider
    // when built with its aws-lc-rs feature (which needs cmake/nasm); this
    // project uses ring instead (see Cargo.toml), which kube does NOT
    // auto-install for. Must happen before any kube::Client is built.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // chromiumoxide (as of 0.7 and still 0.9.1) has a real, dated CDP
    // protocol mismatch: Network.requestWillBeSentExtraInfo's
    // ClientSecurityState requires a `privateNetworkRequestPolicy` field
    // that current Chrome builds no longer send (renamed to
    // `localNetworkAccessRequestPolicy`), so every such event fails to
    // deserialize. Confirmed non-fatal — chromiumoxide's own
    // Connection/Handler streams tolerate the error and keep running — but
    // it logs at ERROR on every occurrence, which is noisy in a page with
    // any real network traffic. Suppress just these two known call sites
    // (chromiumoxide::conn's "Failed to deserialize WS response" and
    // chromiumoxide::handler's "WS Connection error") rather than a
    // crate-wide silence, so other chromiumoxide errors still surface.
    let mut rust_log = std::env::var("RUST_LOG").unwrap_or_default();
    if !rust_log.is_empty() {
        rust_log.push(',');
    }
    rust_log.push_str("chromiumoxide::conn=off,chromiumoxide::handler=off");

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(rust_log))
        .init();

    let pool = db::init().await;
    sqlx::migrate!()
        .run(pool)
        .await
        .expect("failed to run database migrations");
    tracing::info!("database initialized and migrations applied");

    sandbox::init().await;
    tracing::info!("sandbox manager initialized");

    let router = build_router();

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("failed to bind listener");
    tracing::info!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, router).await.expect("server error");
}

#[cfg(not(feature = "server"))]
fn main() {
    dioxus::launch(frontend::App);
}
