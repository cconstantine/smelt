use super::*;

async fn start_test_server(body: &'static str) -> (String, tokio::task::JoinHandle<()>) {
    let router = axum::Router::new().route(
        "/",
        axum::routing::get(move || async move { axum::response::Html(body) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a test-local port");
    let port = listener.local_addr().expect("local addr").port();
    let task = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("test server error");
    });
    (format!("http://127.0.0.1:{port}/"), task)
}

/// A start page exercising a navigating link, a non-navigating
/// (JS-only) toggle, and a fillable input with a live mirror —
/// plus a second page the link leads to, for
/// `click`/`fill`/`go_back`.
async fn start_interactive_test_server() -> (String, tokio::task::JoinHandle<()>) {
    let router = axum::Router::new()
        .route(
            "/",
            axum::routing::get(|| async {
                axum::response::Html(
                    "<html><body>\
                             <h1>Start page</h1>\
                             <a href=\"/page2\">Go to page 2</a>\
                             <button onclick=\"document.getElementById('toggle-out').innerText='toggled'\">Toggle</button>\
                             <div id=\"toggle-out\">not toggled</div>\
                             <input type=\"text\" oninput=\"document.getElementById('mirror').innerText=this.value\">\
                             <div id=\"mirror\"></div>\
                             </body></html>",
                )
            }),
        )
        .route(
            "/page2",
            axum::routing::get(|| async {
                axum::response::Html("<html><body><h1>Page two</h1></body></html>")
            }),
        )
        .route(
            "/set-data",
            axum::routing::get(|| async {
                (
                    [(axum::http::header::SET_COOKIE, "smelt_probe=from-first; Path=/")],
                    axum::response::Html(
                        "<html><body>data set<script>\
                                 localStorage.setItem('smelt_probe', 'from-first');\
                                 </script></body></html>",
                    ),
                )
            }),
        )
        .route(
            "/show-data",
            axum::routing::get(|| async { axum::response::Html(SHOW_DATA_PAGE) }),
        )
        .route(
            "/delayed",
            axum::routing::get(|| async {
                axum::response::Html(
                    "<html><body>\
                             <button onclick=\"setTimeout(() => { document.getElementById('late').innerText = 'delayed-done'; }, 300)\">Later</button>\
                             <div id=\"late\">waiting</div>\
                             <a href=\"/slow\">Slow link</a>\
                             </body></html>",
                )
            }),
        )
        .route(
            "/slow",
            axum::routing::get(|| async {
                tokio::time::sleep(Duration::from_millis(1500)).await;
                axum::response::Html("<html><body><h1>Slow page</h1></body></html>")
            }),
        )
        .route(
            "/form-fields",
            axum::routing::get(|| async {
                axum::response::Html(
                    "<html><body>\
                             <input id=\"prefilled\" type=\"text\" value=\"1\">\
                             <input id=\"pw\" type=\"password\">\
                             <textarea id=\"notes\"></textarea>\
                             </body></html>",
                )
            }),
        )
        .route(
            "/input-events",
            axum::routing::get(|| async {
                axum::response::Html(
                    "<html><body>\
                             <div id=\"buttons-out\">no move yet</div>\
                             <form onsubmit=\"event.preventDefault(); document.getElementById('submit-out').innerText='submitted';\">\
                             <input id=\"q\" type=\"text\">\
                             </form>\
                             <div id=\"submit-out\">not sent</div>\
                             <script>document.addEventListener('mousemove', e => { document.getElementById('buttons-out').innerText = 'buttons=' + e.buttons; });</script>\
                             </body></html>",
                )
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a test-local port");
    let port = listener.local_addr().expect("local addr").port();
    let task = tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("test server error");
    });
    (format!("http://127.0.0.1:{port}"), task)
}

/// Shows whatever cookie and localStorage value the page can see.
pub(crate) const SHOW_DATA_PAGE: &str = "<html><body><div id=\"out\"></div><script>\
             document.getElementById('out').innerText = 'cookie=[' + document.cookie + \
             '] storage=[' + (localStorage.getItem('smelt_probe') || '') + ']';\
             </script></body></html>";
pub(crate) const SEES_FIRST_DATA: &str = "cookie=[smelt_probe=from-first] storage=[from-first]";
pub(crate) const SEES_NO_DATA: &str = "cookie=[] storage=[]";

/// A loopback page with a `window.open` button and a
/// target=`_blank` link, both aimed at a route on the same server
/// that counts every request it gets: a popup that really opened
/// would show up in that count.
async fn start_popup_test_server(
) -> (
    String,
    std::sync::Arc<Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
) {
    let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
    let page: &'static str = "<html><body>\
                 <button onclick=\"window.open('/popup')\">Popup</button>\
                 <a href=\"/blank\" target=\"_blank\">Blank link</a>\
                 </body></html>";
    let (hits_a, hits_b) = (hits.clone(), hits.clone());
    let router = axum::Router::new()
        .route("/", axum::routing::get(move || async move { axum::response::Html(page) }))
        .route("/popup", axum::routing::get(move || count_popup_hit(hits_a.clone())))
        .route("/blank", axum::routing::get(move || count_popup_hit(hits_b.clone())));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a test-local port");
    let port = listener.local_addr().expect("local addr").port();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("test server error");
    });
    (format!("http://127.0.0.1:{port}/"), hits, task)
}

async fn count_popup_hit(hits: std::sync::Arc<Mutex<Vec<String>>>) -> &'static str {
    hits.lock().unwrap().push("popup page".to_string());
    "opened"
}

/// Waits for a `BrowsingUrlUpdate` carrying exactly `url`, skipping
/// any other events (and URL updates for pages passed on the way).
async fn expect_url_event(
    events: &mut tokio::sync::broadcast::Receiver<crate::events::ConversationEvent>,
    url: &str,
) {
    let found = tokio::time::timeout(Duration::from_secs(5), async {
        let mut seen = Vec::new();
        loop {
            match events.recv().await {
                Ok(crate::events::ConversationEvent::BrowsingUrlUpdate { url: got }) => {
                    if got == url {
                        return Ok(());
                    }
                    seen.push(got);
                }
                Ok(_) => {}
                Err(e) => return Err(format!("event channel error: {e}; saw {seen:?}")),
            }
        }
    })
    .await;
    match found {
        Ok(Ok(())) => {}
        Ok(Err(e)) => panic!("{e}"),
        Err(_) => panic!("no BrowsingUrlUpdate for {url} within 5s"),
    }
}

async fn field_value(page: &Page, selector: &str) -> String {
    page.evaluate(format!("document.querySelector('{selector}').value"))
        .await
        .expect("evaluate should succeed")
        .into_value()
        .expect("value should be a string")
}

/// Waits for a frame produced *after* this call — the current frame
/// is marked seen first, so a stalled screencast can't pass on a
/// stale one.
async fn expect_fresh_frame(sub: &mut FrameSubscription) {
    sub.receiver.borrow_and_update();
    let frame = tokio::time::timeout(Duration::from_secs(5), sub.next_frame())
        .await
        .expect("expected a fresh frame within 5s — the screencast stalled")
        .expect("the session should still be open");
    assert!(!frame.data.is_empty());
}

/// Navigates to a page whose content changes continuously (a
/// `setInterval`-driven counter) — screencast only sends a new
/// frame when something visually changes, so a static page can't
/// distinguish "ack is working" from "nothing new to send
/// anyway."
async fn navigate_to_animated_page(conversation_id: i64) {
    let (animated_url, _server) = start_test_server(
        "<html><body><div id=\"counter\">0</div><script>
                    let n = 0;
                    setInterval(() => { document.getElementById('counter').innerText = String(++n); }, 100);
                </script></body></html>",
    )
    .await;
    navigate(conversation_id, &animated_url)
        .await
        .expect("navigate to the animated page should succeed");
}

mod sessions;
pub(crate) use sessions::run_session_scenarios;
