use super::super::*;
use super::cluster::with_sandbox;

/// A server on `127.0.0.1:8080` and `127.0.0.1:8443` inside the server
/// pod that accepts each connection, appends a line to `/tmp/held` and
/// never answers anything: no HTTP reply, no TLS ServerHello. Then
/// `install`. The server's own output goes nowhere, so the log is the
/// install's alone. `golang:1` has `python3`, `curl` and `go`.
fn never_answering(install: &str) -> String {
    format!(
        r#"set -e
cat > /tmp/hold.py <<'EOF'
import select, socket
socks = []
for port in (8080, 8443):
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(('127.0.0.1', port))
    s.listen(64)
    socks.append(s)
open('/tmp/hold.ready', 'w').close()
held = []
while True:
    ready, _, _ = select.select(socks, [], [])
    for s in ready:
        held.append(s.accept()[0])
        with open('/tmp/held', 'a') as f:
            f.write('held %d\n' % len(held))
EOF
python3 /tmp/hold.py > /dev/null 2>&1 &
i=0
until [ -e /tmp/hold.ready ]; do i=$((i+1)); [ $i -lt 100 ] || exit 9; sleep 0.1; done
{install}"#
    )
}

/// What an install in a `golang:1` server pod came to, waited on through
/// the real wait with `bound` standing in for the 10 minutes.
struct Ran {
    result: Result<(), String>,
    took: std::time::Duration,
    /// `/tmp/held`, read before the pod was deleted.
    held: Result<String, String>,
    /// Every process's command line in the pod once the wait ended.
    processes: Result<String, String>,
}

/// Each process's command line in the server container, one a line.
const COMMAND_LINES: &str = "for d in /proc/[0-9]*; do tr '\\0' ' ' < \"$d/cmdline\" 2>/dev/null; echo; done";

async fn run_in_golang_pod(server: &str, install: String, bound: std::time::Duration) -> Ran {
    let ran = std::sync::Arc::new(std::sync::Mutex::new(None));
    let out = ran.clone();
    with_sandbox(|client, sandbox| async move {
        let config = LanguageServerConfig {
            name: server.to_string(),
            image: "golang:1".to_string(),
            install_command: install,
            command: "cat".to_string(),
            file_types: [("txt".to_string(), "plaintext".to_string())].into(),
            memory_limit: "512Mi".to_string(),
            cpu_limit: "1".to_string(),
            enabled: true,
            ..Default::default()
        };
        let pods = crate::sandbox::pods_api(&client);
        let name = server_pod_name(sandbox.pod_id, &config.name);
        pods.create(&kube::api::PostParams::default(), &server_pod_spec(&sandbox, &config, "v1"))
            .await
            .expect("create the server pod");
        let running = wait_running(&pods, &name).await;
        let started = std::time::Instant::now();
        let installed = match &running {
            Ok(uid) => Some(wait_installed_within(&client, &pods, &name, uid, bound).await),
            Err(_) => None,
        };
        let took = started.elapsed();
        let read = |command: &'static [&'static str]| {
            let (client, name) = (client.clone(), name.clone());
            async move {
                crate::sandbox::exec_with(&client, &name, "server", command, None)
                    .await
                    .map(|r| r.stdout)
                    .map_err(|e| e.to_string())
            }
        };
        let read = match &running {
            Ok(_) => Some((read(&["cat", "/tmp/held"]).await, read(&["sh", "-c", COMMAND_LINES]).await)),
            Err(_) => None,
        };
        let _ = pods.delete(&name, &crate::sandbox::pod_delete_params()).await;
        running.expect("the server pod runs");
        let (held, processes) = read.expect("ran");
        let ran = Ran { result: installed.expect("waited"), took, held, processes };
        *out.lock().unwrap_or_else(|e| e.into_inner()) = Some(ran);
    })
    .await;
    let ran = ran.lock().unwrap_or_else(|e| e.into_inner()).take();
    ran.expect("the install ran")
}

/// SME-456: cmd/go bounds no fetch past the TLS handshake and writes
/// nothing while it fetches, so a module proxy that takes the request and
/// never answers held `go install` (gopls) silent to the install bound
/// (`The install didn't finish within …`, `It wrote no output`). With
/// `-x` and the go watchdog the install fails with go's own status in
/// about 125 s, its log naming the stalled URL and why go was stopped,
/// and no watchdog is left in the pod after the install.
#[tokio::test]
async fn test_a_go_install_ends_on_a_proxy_that_never_answers() {
    let install = never_answering(
        "GOPROXY=http://127.0.0.1:8080 GOSUMDB=off GOBIN=/tmp/home/go/bin go install example.com/stalled@v1.0.0",
    );
    let ran = run_in_golang_pod("gostall", install, std::time::Duration::from_secs(180)).await;
    let error = ran.result.expect_err("an install from a proxy that never answers finished");
    eprintln!("after {} s the install said: {error:?}", ran.took.as_secs());
    assert!(error.starts_with("The install failed (exit "), "{error}");
    assert!(error.contains("# get http://127.0.0.1:8080/"), "{error}");
    assert!(error.contains("smelt: go wrote nothing and received under 1 KB/s for 120 s"), "{error}");
    assert!(ran.took < std::time::Duration::from_secs(170), "took {} s", ran.took.as_secs());
    assert!(ran.held.expect("read /tmp/held").lines().count() >= 1, "go never reached the proxy");
    let processes = ran.processes.expect("read the pod's processes");
    // The watchdog and its subshells, not the pod script (PID 1), whose
    // own command line holds the watchdog's path too.
    let watchdogs: Vec<&str> = processes.lines().filter(|l| l.starts_with("sh /tmp/smelt-go-idle.sh ")).collect();
    assert!(watchdogs.is_empty(), "the watchdog outlived the install: {watchdogs:?}");
    assert!(processes.lines().any(|l| l.starts_with("python3 /tmp/hold.py")), "the listing missed the pod's processes:\n{processes}");
}

/// A release download in the catalog's tar.gz shape, as
/// `github_download` builds it (`curl -fsSL … | tar -xz`), from `base`
/// instead of GitHub. (The actionlint fixture's asset name keeps a mason
/// template the catalog doesn't fill in, which curl refuses as a URL.)
fn release_install(base: &str) -> String {
    format!(
        "mkdir -p /tmp/home/bin /tmp/home/opt && curl -fsSL {base}owner/tool/releases/download/v1/tool.tar.gz \
         | tar -xz -C /tmp/home/opt && ln -sf /tmp/home/opt/tool /tmp/home/bin/tool"
    )
}

/// SME-456: curl bounds nothing once a connection is up, so a GitHub
/// download whose server accepts and never answers held the install
/// silent to the install bound (`The install didn't finish within …`).
/// With smelt's `.curlrc` curl gives up after 60 s under 1 KB/s and the
/// install fails with curl's own error, in about 60 s.
#[tokio::test]
async fn test_a_github_download_ends_on_a_server_that_never_answers() {
    let install = never_answering(&release_install("http://127.0.0.1:8080/"));
    let ran = run_in_golang_pod("curlstall", install, std::time::Duration::from_secs(120)).await;
    let error = ran.result.expect_err("a download from a server that never answers finished");
    eprintln!("after {} s the install said: {error:?}", ran.took.as_secs());
    assert!(error.starts_with("The install failed (exit "), "{error}");
    assert!(error.contains("curl: (28) Operation too slow"), "{error}");
    assert!(ran.took < std::time::Duration::from_secs(110), "took {} s", ran.took.as_secs());
    assert!(ran.held.expect("read /tmp/held").lines().count() >= 1, "curl never reached the server");
}

/// SME-456: curl's connect bound is its 300 s default, the TLS handshake
/// included, so a GitHub download whose server takes the connection and
/// never answers the ClientHello held the install for 5 minutes. With
/// smelt's `.curlrc` (`connect-timeout = 30`) it fails with curl's own
/// error in about 30 s.
#[tokio::test]
async fn test_a_github_download_ends_on_a_tls_handshake_that_never_answers() {
    let install = never_answering(&release_install("https://127.0.0.1:8443/"));
    let ran = run_in_golang_pod("curltls", install, std::time::Duration::from_secs(90)).await;
    let error = ran.result.expect_err("a download from a TLS tarpit finished");
    eprintln!("after {} s the install said: {error:?}", ran.took.as_secs());
    assert!(error.starts_with("The install failed (exit "), "{error}");
    assert!(error.contains("curl: (28) Connection timed out after 30"), "{error}");
    assert!(ran.took < std::time::Duration::from_secs(80), "took {} s", ran.took.as_secs());
    assert!(ran.held.expect("read /tmp/held").lines().count() >= 1, "curl never reached the server");
}
