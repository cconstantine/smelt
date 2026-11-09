use super::super::*;
use super::cluster::with_sandbox;

/// A tarpit on `127.0.0.1:<port>` inside the server pod: it accepts each
/// connection, appends a line to `/tmp/held`, and never answers anything,
/// neither a ClientHello nor a proxy's `CONNECT`. Then `npm install
/// stalled@1.0.0 <npm_args>`. No certificate is needed: the handshake
/// never gets that far. The tarpit's own output goes nowhere, so the log
/// is npm's alone.
fn tarpit_install(port: u16, npm_args: &str) -> String {
    format!(
        r#"set -e
cat > /tmp/tarpit.js <<'EOF'
const net = require('net'), fs = require('fs');
const held = [];
net.createServer(s => {{
  held.push(s);
  fs.appendFileSync('/tmp/held', new Date().toISOString() + ' held ' + held.length + '\n');
}}).listen({port}, '127.0.0.1', () => fs.writeFileSync('/tmp/tarpit.ready', ''));
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
    let install = tarpit_install(8443, "--registry=https://127.0.0.1:8443/");
    assert_the_connect_bound_ends_it("handshake", install, &[]).await;
}

/// SME-452 round 1 (H1): behind a proxy (`HTTPS_PROXY` in the config's
/// env), npm opens the proxy's socket with `net.connect` and waits for
/// its `CONNECT` reply before any TLS starts, with no bound either. A
/// proxy that accepts and never answers held npm silent as the handshake
/// did; the connect bound now runs until the proxy's first byte.
#[tokio::test]
async fn test_an_npm_install_rides_past_a_proxy_that_never_answers_its_connect() {
    let install = tarpit_install(3128, "--registry=https://registry.npmjs.org/");
    assert_the_connect_bound_ends_it("proxy", install, &[("HTTPS_PROXY", "http://127.0.0.1:3128")]).await;
}
