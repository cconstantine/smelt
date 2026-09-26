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
/// SME-10.
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
    // Reported once logging is set up, below.
    let dotenv_problem = dotenv_problem(dotenvy::dotenv());

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
    if let Some(problem) = dotenv_problem {
        tracing::error!("{problem}");
    }

    let pool = db::init().await;
    sqlx::migrate!()
        .run(pool)
        .await
        .expect("failed to run database migrations");
    tracing::info!("database initialized and migrations applied");

    mcp::ensure_default_servers(pool).await;

    sandbox::init().await;
    tracing::info!("sandbox manager initialized");

    // Keeps pod records in step with the cluster, so the sidebar's dots
    // and /pods match it. Only here, never in the browser test harness:
    // see `sandbox::watch_pods`.
    tokio::spawn(sandbox::watch_pods(pool.clone()));

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

/// What to tell the user about loading `.env`: nothing when it loaded or
/// there isn't one (production sets real env vars instead), otherwise the
/// parse error. dotenvy stops at a line it can't parse, so that line and
/// every one after it are silently not applied (SME-40 F6: an unquoted
/// value with a space dropped `ANTHROPIC_MODEL`).
#[cfg(feature = "server")]
fn dotenv_problem(result: Result<std::path::PathBuf, dotenvy::Error>) -> Option<String> {
    match result {
        Ok(_) => None,
        Err(e) if e.not_found() => None,
        Err(e) => Some(format!(
            "couldn't load .env: {e}. That line and every line after it were not applied; \
             quote a value that contains spaces"
        )),
    }
}

#[cfg(all(test, feature = "server"))]
mod dotenv_tests {
    use super::dotenv_problem;

    #[test]
    fn test_an_unparseable_env_file_is_reported() {
        let dir = std::env::temp_dir().join(format!("smelt-dotenv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join(".env");
        std::fs::write(&path, "A=1\nANTHROPIC_MODEL=Qwen3.8-Flash-Next (UD-Q2_K_XL)\nB=2\n").expect("write");
        let problem = dotenv_problem(dotenvy::from_path_iter(&path).and_then(|iter| {
            iter.collect::<Result<Vec<_>, _>>().map(|_| path.clone())
        }));
        let problem = problem.expect("an unparseable line should be reported");
        assert!(problem.contains(".env"), "{problem}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_a_missing_env_file_is_fine() {
        let missing = std::env::temp_dir().join("smelt-no-such-dir/.env");
        assert_eq!(dotenv_problem(dotenvy::from_path(&missing).map(|_| missing.clone())), None);
    }
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
