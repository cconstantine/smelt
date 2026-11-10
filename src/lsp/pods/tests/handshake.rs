use super::super::*;
use super::cluster::with_sandbox;

/// A throwaway self-signed certificate for `127.0.0.1` and its key, for
/// the TLS tarpit (`node:22-slim` has no `openssl` to make one). It
/// guards nothing: only the test's npm trusts it (`NODE_EXTRA_CA_CERTS`
/// in the test config's env; npm's `strict-ssl` overrides
/// `NODE_TLS_REJECT_UNAUTHORIZED`).
const TARPIT_TLS: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgTQ9cqopPkWQGzGIf
HCvC/OJQptxakhGE5QQ0fG2fsGmhRANCAAR0j1UBBCeUQhuKScSMwAGpSC8WBPIn
awGcWzHOZiE3e+5cQuhl2dxigFz9Rt43CY7jDgLYi+B11v5gJ5yNMc5f
-----END PRIVATE KEY-----
-----BEGIN CERTIFICATE-----
MIIBkDCCATagAwIBAgIUYpJQoUQ5DaMYToRxjsfFgdUEQoYwCgYIKoZIzj0EAwIw
FDESMBAGA1UEAwwJMTI3LjAuMC4xMCAXDTI2MTEwOTAxMzUwMloYDzIxMjYxMDE2
MDEzNTAyWjAUMRIwEAYDVQQDDAkxMjcuMC4wLjEwWTATBgcqhkjOPQIBBggqhkjO
PQMBBwNCAAR0j1UBBCeUQhuKScSMwAGpSC8WBPInawGcWzHOZiE3e+5cQuhl2dxi
gFz9Rt43CY7jDgLYi+B11v5gJ5yNMc5fo2QwYjAdBgNVHQ4EFgQUQ2d8YdxgWdeV
iVPGlY20/HQG0YUwHwYDVR0jBBgwFoAUQ2d8YdxgWdeViVPGlY20/HQG0YUwDwYD
VR0TAQH/BAUwAwEB/zAPBgNVHREECDAGhwR/AAABMAoGCCqGSM49BAMCA0gAMEUC
IQDsCD4b2Mc3f+JOAR8SCIaXphlS0BPiOseDwTUI7pfnAgIgYuFJP0Rls3xYSEV9
RoErN5Lo8Ywi+JaWoK5DXlm7dgQ=
-----END CERTIFICATE-----";

/// A tarpit on `127.0.0.1:<port>` inside the server pod: it accepts each
/// connection (with `tls`, completes the handshake first), appends a line
/// to `/tmp/held`, and never answers anything after, neither a
/// ClientHello nor a proxy's `CONNECT`. Then `npm install stalled@1.0.0
/// <npm_args>`. The tarpit's own output goes nowhere, so the log is npm's
/// alone.
fn tarpit_install(port: u16, tls: bool, npm_args: &str) -> String {
    let server = if tls {
        "require('tls').createServer({key: fs.readFileSync('/tmp/tarpit.pem'), cert: fs.readFileSync('/tmp/tarpit.pem')}, hold)"
    } else {
        "net.createServer(hold)"
    };
    format!(
        r#"set -e
cat > /tmp/tarpit.pem <<'EOF'
{TARPIT_TLS}
EOF
cat > /tmp/tarpit.js <<'EOF'
const net = require('net'), fs = require('fs');
const held = [];
const hold = s => {{
  held.push(s);
  fs.appendFileSync('/tmp/held', new Date().toISOString() + ' held ' + held.length + '\n');
}};
{server}.listen({port}, '127.0.0.1', () => fs.writeFileSync('/tmp/tarpit.ready', ''));
EOF
node /tmp/tarpit.js > /dev/null 2>&1 &
i=0
until [ -e /tmp/tarpit.ready ]; do i=$((i+1)); [ $i -lt 100 ] || exit 9; sleep 0.1; done
cd /tmp
npm install --prefix /tmp/home/lsp stalled@1.0.0 {npm_args}"#
    )
}

/// Runs `install` in a `node:22-slim` server pod with `env` and npm's
/// `fetch_retries=1` (two tries), waits up to 90 s for it (standing in for
/// the 10 minutes, through the real wait), and asserts it failed with
/// the connect bound's logged reason after two connections to the
/// tarpit: the retry was a fresh connection.
async fn assert_the_connect_bound_ends_it(server: &str, install: String, env: &[(&str, &str)]) {
    with_sandbox(|client, sandbox| async move {
        let mut config_env: std::collections::BTreeMap<String, String> =
            env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        config_env.insert("npm_config_fetch_retries".to_string(), "1".to_string());
        let config = LanguageServerConfig {
            name: server.to_string(),
            image: "node:22-slim".to_string(),
            install_command: install,
            command: "cat".to_string(),
            file_types: [("txt".to_string(), "plaintext".to_string())].into(),
            env: config_env,
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
            Ok(uid) => Some(wait_installed_within(&client, &pods, &name, uid, std::time::Duration::from_secs(90)).await),
            Err(_) => None,
        };
        let took = started.elapsed();
        let held = match &running {
            Ok(_) => Some(
                crate::sandbox::exec_with(&client, &name, "server", &["cat", "/tmp/held"], None)
                    .await
                    .map(|r| r.stdout),
            ),
            Err(_) => None,
        };
        let _ = pods.delete(&name, &crate::sandbox::pod_delete_params()).await;
        running.expect("the server pod runs");
        let error = installed.expect("waited").expect_err("an install from a tarpit finished");
        eprintln!("after {} s the install said: {error:?}", took.as_secs());
        assert!(error.starts_with("The install failed (exit 1)"), "{error}");
        assert!(error.contains("attempt 1 failed with ECONNECTIONTIMEOUT"), "{error}");
        assert!(error.contains("didn't finish within 30000 ms"), "{error}");
        let held = held.expect("ran").expect("read /tmp/held");
        assert_eq!(held.lines().count(), 2, "the retry wasn't a fresh connection: {held}");
    })
    .await;
}

/// SME-452's mechanism, on demand: npm bounds no part of opening a
/// connection, so a TLS handshake that never answers held `npm install`
/// silent past the install bound (CI's `The install didn't finish within
/// 10 minutes` with no output, `npm install` still running). With the
/// install's connect bound (`CONNECT_BOUND`, 30 s) the stalled connect
/// fails with `ECONNECTIONTIMEOUT`, npm retries it on a fresh connection,
/// and the install fails with npm's own logged reason, in about 70 s.
#[tokio::test]
async fn test_an_npm_install_rides_past_a_tls_handshake_that_never_answers() {
    let install = tarpit_install(8443, false, "--registry=https://127.0.0.1:8443/");
    assert_the_connect_bound_ends_it("handshake", install, &[]).await;
}

/// SME-452 round 1 (H1): behind a proxy (`HTTPS_PROXY` in the config's
/// env), npm opens the proxy's socket with `net.connect` and waits for
/// its `CONNECT` reply before any TLS starts, with no bound either. A
/// proxy that accepts and never answers held npm silent as the handshake
/// did; the connect bound now runs until the tunnel is handed to TLS.
#[tokio::test]
async fn test_an_npm_install_rides_past_a_proxy_that_never_answers_its_connect() {
    let install = tarpit_install(3128, false, "--registry=https://registry.npmjs.org/");
    assert_the_connect_bound_ends_it("proxy", install, &[("HTTPS_PROXY", "http://127.0.0.1:3128")]).await;
}

/// SME-452 round 2 (H1): an `https://` proxy's socket is a TLS one, made
/// by `tls.connect`, and its handshake with the proxy completes; the
/// `CONNECT` after it had no bound. A proxy that finishes the handshake
/// and never answers held npm silent; the bound now runs past
/// `secureConnect` on a tunnel, until it's handed to TLS.
#[tokio::test]
async fn test_an_npm_install_rides_past_an_https_proxy_that_never_answers_its_connect() {
    let install = tarpit_install(3129, true, "--registry=https://registry.npmjs.org/");
    let env = [("HTTPS_PROXY", "https://127.0.0.1:3129"), ("NODE_EXTRA_CA_CERTS", "/tmp/tarpit.pem")];
    assert_the_connect_bound_ends_it("tlsproxy", install, &env).await;
}

/// What an install in a `node:22-slim` server pod came to, waited on
/// through the real wait with 90 s standing in for the 10 minutes — the
/// success-side sibling of `assert_the_connect_bound_ends_it` (which
/// asserts failure), modelled on `installers.rs`'s `Ran`/`run_in_*_pod`.
struct Ran {
    result: Result<(), String>,
    took: std::time::Duration,
    /// One per name: that pod file, read before the pod was deleted.
    files: Vec<Result<String, String>>,
}

async fn run_in_node_pod(server: &str, install: String, env: &[(&str, &str)], files: &[&'static str]) -> Ran {
    let ran = std::sync::Arc::new(std::sync::Mutex::new(None));
    let out = ran.clone();
    with_sandbox(|client, sandbox| async move {
        let config_env: std::collections::BTreeMap<String, String> =
            env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let config = LanguageServerConfig {
            name: server.to_string(),
            image: "node:22-slim".to_string(),
            install_command: install,
            command: "cat".to_string(),
            file_types: [("txt".to_string(), "plaintext".to_string())].into(),
            env: config_env,
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
            Ok(uid) => Some(wait_installed_within(&client, &pods, &name, uid, std::time::Duration::from_secs(90)).await),
            Err(_) => None,
        };
        let took = started.elapsed();
        let read = |file: &'static str| {
            let (client, name) = (client.clone(), name.clone());
            async move {
                crate::sandbox::exec_with(&client, &name, "server", &["cat", file], None)
                    .await
                    .map(|r| r.stdout)
                    .map_err(|e| e.to_string())
            }
        };
        let mut read_files = Vec::new();
        if running.is_ok() {
            for file in files {
                read_files.push(read(*file).await);
            }
        } else {
            for _ in files {
                read_files.push(Err("the server pod didn't run".to_string()));
            }
        }
        let _ = pods.delete(&name, &crate::sandbox::pod_delete_params()).await;
        running.expect("the server pod runs");
        let ran = Ran { result: installed.expect("waited"), took, files: read_files };
        *out.lock().unwrap_or_else(|e| e.into_inner()) = Some(ran);
    })
    .await;
    let ran = ran.lock().unwrap_or_else(|e| e.into_inner()).take();
    ran.expect("the install ran")
}

/// SME-464 (PR #307 round 3's M1): a tunnel's bound ran until the tunnel
/// was handed to `tls.connect({socket})` or closed, but yarn berry's
/// hpagent `HttpProxyAgent` CONNECT-tunnels a plain-`http://` registry and
/// never hands it to TLS — every such tunnel died 30 s after its CONNECT,
/// mid-download or idle. Now a complete reply header ends the bound too.
/// The M1 repro, hermetic: an hpagent-shaped client (a `CONNECT`, its
/// reply awaited by a flowing `data` listener, then an origin-form `GET`
/// on the raw socket, no TLS anywhere) against an in-pod proxy that
/// answers its CONNECT at once and an in-pod registry that replies 40 s
/// after the request — past the 30 s bound, inside the 90 s wait. The
/// client runs in the install's foreground, so it carries the preload and
/// the install's exit code is the client's.
const BERRY_TUNNEL_INSTALL: &str = r#"set -e
cat > /tmp/registry.js <<'EOF'
const http = require('http'), fs = require('fs');
http.createServer((req, res) => {
  fs.appendFileSync('/tmp/held', 'GET ' + req.url + '\n');
  setTimeout(() => {
    res.writeHead(200, {'content-type': 'text/plain', 'connection': 'close'});
    res.end('berry-464');
  }, 40000);
}).listen(8080, '127.0.0.1', () => fs.writeFileSync('/tmp/registry.ready', ''));
EOF
cat > /tmp/proxy.js <<'EOF'
const net = require('net'), fs = require('fs');
net.createServer(client => {
  // head grows until the CONNECT header is complete; every byte after it
  // is buffered too (they arrive before the upstream connect completes)
  // and flushed once it pipes, so nothing written in that window is lost.
  let head = Buffer.alloc(0), open = false;
  client.on('data', d => {
    head = Buffer.concat([head, d]);
    if (open) return;
    const i = head.indexOf('\r\n\r\n');
    if (i < 0) return;
    const line = head.subarray(0, head.indexOf('\r\n')).toString();
    if (!line.startsWith('CONNECT 127.0.0.1:8080')) { console.log('not our CONNECT: ' + line); process.exit(1); }
    fs.appendFileSync('/tmp/held', line + '\n');
    client.write('HTTP/1.1 200 Connection established\r\n\r\n');
    open = true;
    const up = net.connect(8080, '127.0.0.1', () => {
      const rest = head.subarray(i + 4);
      if (rest.length) up.write(rest);
      client.pipe(up);
      up.pipe(client);
    });
    up.on('error', () => client.destroy());
    client.on('error', () => up.destroy());
  });
}).listen(3132, '127.0.0.1', () => fs.writeFileSync('/tmp/proxy.ready', ''));
EOF
cat > /tmp/client.js <<'EOF'
// hpagent's HttpProxyAgent for a plain-http target: net.connect to the
// proxy, a CONNECT, its reply awaited by a data listener (its
// socketOnData, flowing), then an origin-form GET on the raw socket.
const net = require('net'), fs = require('fs');
let t0 = 0, head = Buffer.alloc(0), resp = Buffer.alloc(0), got = 'header';
const s = net.connect(3132, '127.0.0.1');
s.on('error', e => {
  console.log('client died: ' + (e.code || '?') + ': ' + e.message + ' after ' + (Date.now() - t0) + ' ms');
  process.exit(1);
});
s.on('connect', () => {
  t0 = Date.now();
  s.write('CONNECT 127.0.0.1:8080 HTTP/1.1\r\nHost: 127.0.0.1:8080\r\n\r\n');
});
s.on('data', d => {
  if (got === 'header') {
    head = Buffer.concat([head, d]);
    const i = head.indexOf('\r\n\r\n');
    if (i < 0) return;
    if (!head.subarray(0, 12).toString().startsWith('HTTP/1.1 200')) {
      console.log('proxy refused: ' + head.subarray(0, i).toString());
      process.exit(1);
    }
    s.write('GET /x HTTP/1.1\r\nHost: 127.0.0.1:8080\r\n\r\n');
    got = 'body';
    d = head.subarray(i + 4);
  }
  resp = Buffer.concat([resp, d]);
});
s.on('end', () => {
  // The registry's own reply only: node's 60 s headers timeout would
  // also end this socket, with a 408 — accepting that would be a green
  // that never crossed the bound.
  const ms = Date.now() - t0;
  if (!resp.subarray(0, 12).toString().startsWith('HTTP/1.1 200') || !resp.includes('berry-464')) {
    console.log('bad reply after ' + ms + ' ms: ' + JSON.stringify(resp.subarray(0, 60).toString()));
    process.exit(1);
  }
  fs.writeFileSync('/tmp/elapsed', String(ms));
  console.log('reply after ' + ms + ' ms: ' + JSON.stringify(resp.subarray(0, 12).toString()));
  process.exit(0);
});
EOF
node /tmp/registry.js > /dev/null 2>&1 &
node /tmp/proxy.js > /dev/null 2>&1 &
i=0
until [ -e /tmp/registry.ready ] && [ -e /tmp/proxy.ready ]; do i=$((i+1)); [ $i -lt 100 ] || exit 9; sleep 0.1; done
node /tmp/client.js"#;

/// SME-464: the hpagent-shaped tunnel above survives past the connect
/// bound — the install succeeds through the 40 s registry reply — and
/// really tunnelled to really cross the bound's 30 s: `/tmp/held` holds
/// one `CONNECT 127.0.0.1:8080` line and one `GET /x` line (a client that
/// regressed to a direct connection would pass the success vacuously),
/// and the client's own clock says the reply arrived ≥ 30 s after its
/// CONNECT (a scenario that stopped crossing the bound fails here).
#[tokio::test]
async fn test_a_yarn_berry_style_plain_http_tunnel_survives_past_the_connect_bound() {
    let ran = run_in_node_pod("berry", BERRY_TUNNEL_INSTALL.to_string(), &[], &["/tmp/held", "/tmp/elapsed"]).await;
    let held = ran.files[0].as_ref().expect("read /tmp/held");
    eprintln!("after {} s the install said: {:?}; the servers saw:\n{held}", ran.took.as_secs(), ran.result);
    ran.result.expect("an install whose tunnel dies 30 s after its CONNECT finished");
    assert_eq!(held.lines().collect::<Vec<_>>(), ["CONNECT 127.0.0.1:8080 HTTP/1.1", "GET /x"], "the scenario didn't tunnel once to the registry: {held}");
    let ms: u64 = ran.files[1].as_ref().expect("read /tmp/elapsed").trim().parse().expect("a whole number of ms");
    assert!(ms >= 30_000, "the reply arrived at {ms} ms, before the bound would have fired — the scenario doesn't cross it");
}

/// A proxy on `127.0.0.1:<port>` inside the server pod that answers each
/// `CONNECT` in part — `HTTP/1.1 200 Conn`, no terminator, nothing after —
/// and then stalls, appending `held N` to `/tmp/held` at accept and
/// `replied N` after that partial write. Then `npm install stalled@1.0.0
/// <npm_args>`. The proxy's own output goes nowhere, so the log is npm's
/// alone. The sibling of `tarpit_install` SME-464's T2 needs: that one
/// never answers, this one answers a header's prefix and stalls.
fn partial_reply_install(port: u16, npm_args: &str) -> String {
    format!(
        r#"set -e
cat > /tmp/partial.js <<'EOF'
const net = require('net'), fs = require('fs');
const held = [];
const serve = s => {{
  held.push(s);
  const k = held.length;
  fs.appendFileSync('/tmp/held', new Date().toISOString() + ' held ' + k + '\n');
  let first = true;
  s.on('data', d => {{
    if (!first) return;
    first = false;
    const line = d.toString().split('\r\n')[0];
    if (!line.startsWith('CONNECT ')) {{ console.log('not a CONNECT: ' + line); process.exit(1); }}
    s.write('HTTP/1.1 200 Conn');
    fs.appendFileSync('/tmp/held', new Date().toISOString() + ' replied ' + k + '\n');
  }});
}};
net.createServer(serve).listen({port}, '127.0.0.1', () => fs.writeFileSync('/tmp/partial.ready', ''));
EOF
node /tmp/partial.js > /dev/null 2>&1 &
i=0
until [ -e /tmp/partial.ready ]; do i=$((i+1)); [ $i -lt 100 ] || exit 9; sleep 0.1; done
cd /tmp
npm install --prefix /tmp/home/lsp stalled@1.0.0 {npm_args}"#
    )
}

/// SME-464, the fix's boundary: a complete reply header ends a tunnel's
/// bound, and a *partial* one must not — round 2's L1 (answer in part,
/// then stall) stays bounded exactly as before, or this fix would have
/// silently un-bounded the case round 2 shipped it for. Round 2 proved
/// that in a run matrix with no cluster test; this is that test.
///
/// It runs under `assert_the_connect_bound_ends_it`'s assertion set —
/// exit 1, both tries bounded with ECONNECTIONTIMEOUT at 30000 ms, two
/// held connections (the retry fresh) — duplicated in this runner rather
/// than called through the helper: the helper deletes its pod inside
/// itself, so no extra in-pod assertion could be layered on it. That
/// extra one is T2's own: `/tmp/held` must hold the 2 `replied` lines —
/// without it a regression into the never-answers case would pass every
/// other assertion here and quietly duplicate
/// `test_an_npm_install_rides_past_a_proxy_that_never_answers_its_connect`.
#[tokio::test]
async fn test_an_npm_install_rides_past_a_proxy_that_answers_its_connect_in_part_then_stalls() {
    let install = partial_reply_install(3131, "--registry=https://registry.npmjs.org/");
    let env = [("HTTPS_PROXY", "http://127.0.0.1:3131"), ("npm_config_fetch_retries", "1")];
    let ran = run_in_node_pod("partial", install, &env, &["/tmp/held"]).await;
    let held = ran.files[0].as_ref().expect("read /tmp/held");
    eprintln!("after {} s the install said: {:?}; the proxy saw:\n{held}", ran.took.as_secs(), ran.result);
    let error = ran.result.expect_err("an install through a proxy that answers in part and stalls finished");
    assert!(error.starts_with("The install failed (exit 1)"), "{error}");
    assert!(error.contains("attempt 1 failed with ECONNECTIONTIMEOUT"), "{error}");
    assert!(error.contains("didn't finish within 30000 ms"), "{error}");
    assert!(error.matches("ECONNECTIONTIMEOUT").count() >= 2, "both tries weren't bounded: {error}");
    assert_eq!(held.lines().filter(|l| l.contains(" held ")).count(), 2, "the retry wasn't a fresh connection: {held}");
    assert_eq!(held.lines().filter(|l| l.contains(" replied ")).count(), 2, "the proxy never answered in part: {held}");
}

/// SME-464 round 1's L1: the plan required the reply-watch guard to let any
/// unexpected chunk shape fall through to the original `push` — and
/// `chunk !== null` let `undefined` through to `typeof chunk.byteLength`,
/// which throws where bare node `push` returns cleanly (reviewer's run:
/// `push(undefined)` → false, `push()` → undefined). Fast by construction,
/// no bound wait: an hpagent-shaped client writes a CONNECT to a silent
/// in-pod proxy (arming the bound and installing the watch), then pushes
/// `undefined` and nothing on the tunnel socket and compares the returns
/// against a never-written oracle socket — node owns those values, so none
/// are hardcoded. Pre-guard-fix the client dies with the TypeError; that is
/// the red.
const PUSH_SHAPE_INSTALL: &str = r#"set -e
cat > /tmp/silent.js <<'EOF'
const net = require('net'), fs = require('fs');
// a proxy that accepts and never answers: the watch stays installed and the
// bound armed for the whole run of the client's pushes
net.createServer(() => {}).listen(3134, '127.0.0.1', () => fs.writeFileSync('/tmp/silent.ready', ''));
EOF
cat > /tmp/shapes.js <<'EOF'
const net = require('net'), fs = require('fs');
const bare = net.connect(3134, '127.0.0.1'); // oracle: never writes CONNECT, so its push is the prototype's
bare.on('error', () => {});
const s = net.connect(3134, '127.0.0.1');
s.on('error', () => {});
s.on('connect', () => {
  s.write('CONNECT 127.0.0.1:8080 HTTP/1.1\r\nHost: 127.0.0.1:8080\r\n\r\n');
  const b1 = String(bare.push(undefined)), b2 = String(bare.push());
  const r1 = String(s.push(undefined)), r2 = String(s.push());
  const match = r1 === b1 && r2 === b2;
  fs.writeFileSync('/tmp/shapes', `tunnel push(undefined)=${r1} push()=${r2}; bare push(undefined)=${b1} push()=${b2}; match=${match}\n`);
  process.exit(match ? 0 : 1);
});
EOF
node /tmp/silent.js > /dev/null 2>&1 &
i=0
until [ -e /tmp/silent.ready ]; do i=$((i+1)); [ $i -lt 100 ] || exit 9; sleep 0.1; done
node /tmp/shapes.js"#;

/// L1's assertion: the client exits 0 and the watched tunnel socket's
/// `push(undefined)`/`push()` returned exactly what the never-written oracle
/// socket's returned.
#[tokio::test]
async fn test_a_tunnel_socket_push_of_undefined_or_nothing_behaves_like_bare_push() {
    let ran = run_in_node_pod("shapes", PUSH_SHAPE_INSTALL.to_string(), &[], &["/tmp/shapes"]).await;
    let shapes = ran.files[0].as_ref().expect("read /tmp/shapes");
    eprintln!("after {} s the install said: {:?}; shapes: {shapes}", ran.took.as_secs(), ran.result);
    ran.result.expect("a client that pushed only undefined and nothing died — the shape guard must fall through to the original push, not throw");
    assert!(shapes.contains("match=true"), "the watched push didn't return what bare push returns: {shapes}");
}

