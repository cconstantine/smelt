mod anthropic;
mod api;
mod browsing;
#[cfg(feature = "server")]
mod db;
#[cfg(feature = "server")]
mod egress_proxy;
mod events;
#[cfg(feature = "server")]
mod fetch_guard;
mod frontend;
#[cfg(feature = "server")]
mod headless_chrome;
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

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(log_filter_directives(
            &std::env::var("RUST_LOG").unwrap_or_default(),
        )))
        .init();

    let pool = db::init().await;
    sqlx::migrate!()
        .run(pool)
        .await
        .expect("failed to run database migrations");
    tracing::info!("database initialized and migrations applied");

    mcp::ensure_default_servers(pool).await;

    sandbox::init().await;
    tracing::info!("sandbox manager initialized");

    // Pod records whose pods are gone from Kubernetes (a cluster rebuild,
    // a pod deleted outside smelt, one that died while smelt was down) are
    // closed at startup and then every minute, so the sidebar's dots and
    // /pods match the cluster. Only here, never in the browser test
    // harness: see `sandbox::reconcile_live_pods`.
    tokio::spawn(async move {
        loop {
            match sandbox::reconcile_live_pods(pool).await {
                Ok(0) => {}
                Ok(closed) => tracing::info!(closed, "closed records of pods that no longer exist"),
                Err(e) => tracing::warn!(error = %e, "couldn't reconcile pod records with the cluster"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });

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

/// The `EnvFilter` directives to log with, given `RUST_LOG`'s value.
///
/// chromiumoxide (0.7, and still 0.9.1) expects a `privateNetworkRequestPolicy`
/// field in `Network.requestWillBeSentExtraInfo` that current Chrome builds
/// renamed to `localNetworkAccessRequestPolicy`, so every such event fails to
/// deserialize and logs at ERROR — harmless (its WS streams carry on past it)
/// but constant on any real page. Only those two call sites are silenced, not
/// the whole crate. An empty `RUST_LOG` still means "errors only":
/// `EnvFilter` falls back to that only when given no directives at all, so
/// adding the two targeted ones would otherwise silence everything.
#[cfg(feature = "server")]
fn log_filter_directives(rust_log: &str) -> String {
    let rust_log = rust_log.trim();
    let base = if rust_log.is_empty() { "error" } else { rust_log };
    format!("{base},chromiumoxide::conn=off,chromiumoxide::handler=off")
}

#[cfg(all(test, feature = "server"))]
mod tests {
    use super::log_filter_directives;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Everything logged by `emit` under the filter built from `rust_log`.
    fn logged_with(rust_log: &str, emit: impl FnOnce()) -> String {
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(log_filter_directives(rust_log)))
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, emit);
        String::from_utf8(captured.0.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn test_unset_rust_log_still_shows_errors_but_not_the_chromiumoxide_noise() {
        let out = logged_with("", || {
            tracing::error!(target: "smelt::db", "a real error");
            tracing::warn!(target: "smelt::db", "a warning");
            tracing::error!(target: "chromiumoxide::conn", "Failed to deserialize WS response");
            tracing::error!(target: "chromiumoxide::handler", "WS Connection error");
            tracing::error!(target: "chromiumoxide::browser", "some other chromiumoxide error");
        });
        assert!(out.contains("a real error"), "got: {out}");
        assert!(!out.contains("a warning"), "got: {out}");
        assert!(!out.contains("Failed to deserialize"), "got: {out}");
        assert!(!out.contains("WS Connection error"), "got: {out}");
        assert!(out.contains("some other chromiumoxide error"), "got: {out}");
    }

    #[test]
    fn test_an_explicit_rust_log_is_kept_with_the_noise_still_silenced() {
        let out = logged_with("info", || {
            tracing::info!(target: "smelt::db", "an info line");
            tracing::error!(target: "chromiumoxide::conn", "Failed to deserialize WS response");
        });
        assert!(out.contains("an info line"), "got: {out}");
        assert!(!out.contains("Failed to deserialize"), "got: {out}");
    }
}

#[cfg(not(feature = "server"))]
fn main() {
    dioxus::launch(frontend::App);
}
