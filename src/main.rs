mod anthropic;
mod api;
#[cfg(feature = "server")]
mod db;
mod events;
mod frontend;
#[cfg(feature = "server")]
mod mcp;
mod models;
#[cfg(feature = "server")]
mod sandbox;

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

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
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
