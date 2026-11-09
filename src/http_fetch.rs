//! One GET of a text file the server needs, with retries and a deadline
//! (SME-470): mason's registry and Helix's `languages.toml` for the LSP
//! catalog, and models.dev's price catalog.
//!
//! A transient failure is tried again only before any of the body has
//! arrived: a connect, timeout or request error, or a 408, 429 or 5xx. A
//! body cut off partway fails at once, so a second copy is never joined to
//! the first. The whole fetch, its tries and pauses together, is bounded by
//! the policy's deadline.

use std::time::Duration;

/// How hard one fetch tries.
pub(crate) struct Policy {
    /// Tries in all, the first included.
    pub attempts: u32,
    /// The pause before each try after the first; the last one repeats.
    pub backoff: &'static [Duration],
    /// Each try's own limit, its body included.
    pub per_try: Duration,
    /// The whole fetch's limit, every try and pause included.
    pub deadline: Duration,
    /// The biggest body taken.
    pub max_bytes: usize,
}

#[derive(Debug, PartialEq)]
pub(crate) enum Fetched {
    Body(String),
    /// The server answered 404, which is an answer, not a failure.
    NotFound,
}

/// The text at `url`, fetched as `policy` says.
pub(crate) async fn get_text(url: &str, policy: &Policy) -> Result<Fetched, String> {
    let client = reqwest::Client::builder()
        .timeout(policy.per_try)
        .build()
        .map_err(|e| format!("couldn't make an HTTP client: {e}"))?;
    let mut tries = 0;
    match tokio::time::timeout(policy.deadline, tries_until_done(&client, url, policy, &mut tries)).await {
        Ok(fetched) => fetched,
        Err(_) => Err(format!(
            "{url} didn't answer within {}s ({tries} tries)",
            policy.deadline.as_secs_f64()
        )),
    }
}

/// Tries `url` until an answer that isn't transient, or the last try.
/// Counts the tries in `tries`, for the deadline's message.
async fn tries_until_done(
    client: &reqwest::Client,
    url: &str,
    policy: &Policy,
    tries: &mut u32,
) -> Result<Fetched, String> {
    loop {
        *tries += 1;
        let last = *tries >= policy.attempts;
        match client.get(url).send().await {
            Err(e) if !last && (e.is_connect() || e.is_timeout() || e.is_request()) => {}
            Err(e) => return Err(format!("couldn't reach {url}: {e}")),
            Ok(response) if response.status() == reqwest::StatusCode::NOT_FOUND => return Ok(Fetched::NotFound),
            // Once a body has begun, a failure is final.
            Ok(response) if response.status().is_success() => {
                return read_body(response, url, policy.max_bytes).await.map(Fetched::Body);
            }
            Ok(response) if !last && transient(response.status()) => {}
            Ok(response) => return Err(format!("{url} answered {}", response.status())),
        }
        let pause = policy.backoff.get(*tries as usize - 1).or(policy.backoff.last());
        tokio::time::sleep(pause.copied().unwrap_or_default()).await;
    }
}

/// A status worth asking again for: the server is busy, not saying no.
fn transient(status: reqwest::StatusCode) -> bool {
    status.is_server_error()
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
}

async fn read_body(mut response: reqwest::Response, url: &str, max_bytes: usize) -> Result<String, String> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        body.extend_from_slice(&chunk);
        if body.len() > max_bytes {
            return Err(format!("{url} is too big (over {} MB)", max_bytes / 1024 / 1024));
        }
    }
    String::from_utf8(body).map_err(|_| format!("{url} isn't text"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const QUICK: Policy = Policy {
        attempts: 3,
        backoff: &[Duration::from_millis(10), Duration::from_millis(10)],
        per_try: Duration::from_secs(5),
        deadline: Duration::from_secs(10),
        max_bytes: 1024,
    };

    /// A stand-in answering `statuses` in turn (the last repeats), with
    /// "hello" as each body, and its count of GETs.
    async fn scripted(statuses: &[u16]) -> (String, Arc<AtomicUsize>) {
        let statuses = statuses.to_vec();
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = hits.clone();
        let app = axum::Router::new().route(
            "/file",
            axum::routing::get(move || {
                let i = counted.fetch_add(1, Ordering::SeqCst).min(statuses.len() - 1);
                let status = axum::http::StatusCode::from_u16(statuses[i]).expect("status");
                async move { (status, "hello") }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.ok() });
        (format!("http://{addr}/file"), hits)
    }

    /// A raw stand-in: each connection it accepts is counted and handed to
    /// `serve`, which may answer badly or not at all.
    async fn raw<F, Fut>(serve: F) -> (String, Arc<AtomicUsize>)
    where
        F: Fn(tokio::net::TcpStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = hits.clone();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                counted.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(serve(socket));
            }
        });
        (format!("http://{addr}/file"), hits)
    }

    #[tokio::test]
    async fn test_a_503_then_a_200_is_the_body() {
        let (url, hits) = scripted(&[503, 200]).await;
        assert_eq!(get_text(&url, &QUICK).await, Ok(Fetched::Body("hello".to_string())));
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_a_429_is_tried_again_and_other_4xx_are_not() {
        let (url, hits) = scripted(&[429, 200]).await;
        assert_eq!(get_text(&url, &QUICK).await, Ok(Fetched::Body("hello".to_string())));
        assert_eq!(hits.load(Ordering::SeqCst), 2, "429 then 200");

        let (url, hits) = scripted(&[400, 200]).await;
        assert_eq!(get_text(&url, &QUICK).await, Err(format!("{url} answered 400 Bad Request")));
        assert_eq!(hits.load(Ordering::SeqCst), 1, "a 400 isn't asked again");

        let (url, hits) = scripted(&[404, 200]).await;
        assert_eq!(get_text(&url, &QUICK).await, Ok(Fetched::NotFound));
        assert_eq!(hits.load(Ordering::SeqCst), 1, "a 404 is an answer");
    }

    #[tokio::test]
    async fn test_a_server_that_always_fails_is_tried_attempts_times() {
        let (url, hits) = scripted(&[500]).await;
        assert_eq!(get_text(&url, &QUICK).await, Err(format!("{url} answered 500 Internal Server Error")));
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_a_stalled_server_fails_at_the_deadline() {
        // Accepts, reads the request and never answers.
        let (url, hits) = raw(|mut socket| async move {
            let mut buf = [0; 1024];
            while matches!(tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await, Ok(n) if n > 0) {}
        })
        .await;
        let policy = Policy { per_try: Duration::from_millis(200), deadline: Duration::from_millis(500), ..QUICK };
        let started = std::time::Instant::now();
        let fetched = tokio::time::timeout(Duration::from_secs(5), get_text(&url, &policy)).await.expect("no hang");
        let elapsed = started.elapsed();
        let hits = hits.load(Ordering::SeqCst);
        let e = fetched.expect_err("stalled");
        assert!(e.starts_with(&format!("{url} didn't answer within 0.5s (")), "{e}");
        assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
        assert!((1..=3).contains(&hits), "{hits}");
    }

    #[tokio::test]
    async fn test_a_body_cut_off_partway_fails_without_a_second_get() {
        // Promises 1000 bytes, sends 10 and hangs up.
        let (url, hits) = raw(|mut socket| async move {
            let mut buf = [0; 1024];
            tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await.ok();
            let head = "HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n0123456789";
            tokio::io::AsyncWriteExt::write_all(&mut socket, head.as_bytes()).await.ok();
        })
        .await;
        let fetched = get_text(&url, &QUICK).await;
        assert!(fetched.is_err(), "{fetched:?}");
        assert_eq!(hits.load(Ordering::SeqCst), 1, "no retry once a body began");
    }
}
