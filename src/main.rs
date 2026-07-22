mod anthropic;
mod api;
#[cfg(feature = "server")]
mod db;
mod frontend;
mod models;

#[cfg(feature = "server")]
#[tokio::main]
async fn main() {
    use dioxus::prelude::DioxusRouterExt;

    // Optional: absent in prod, where real env vars are set directly.
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let pool = db::init().await;
    sqlx::migrate!()
        .run(pool)
        .await
        .expect("failed to run database migrations");
    tracing::info!("database initialized and migrations applied");

    let router = axum::Router::new()
        .serve_dioxus_application(dioxus::prelude::ServeConfig::new(), frontend::App)
        .layer(tower_http::trace::TraceLayer::new_for_http());

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
