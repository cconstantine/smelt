use super::super::*;
use super::cluster::{echo_server, with_sandbox};

/// A registry on `127.0.0.1:8080` inside the server pod, serving one
/// package, `stallpkg@1.0.0`, packed in the pod first: it leaves the
/// first two `GET /stallpkg` unanswered (counted into `/tmp/stalls`),
/// then answers. npm's packument fetch has to give up on two stalled
/// attempts and win on its third.
const STALLING_REGISTRY_INSTALL: &str = r#"set -e
mkdir -p /tmp/stallpkg
cd /tmp/stallpkg
echo '{"name":"stallpkg","version":"1.0.0","bin":{"stallpkg":"bin.js"}}' > package.json
printf '#!/usr/bin/env node\nconsole.log("stallpkg")\n' > bin.js
npm pack --pack-destination /tmp --registry=http://127.0.0.1:8080/
cat > /tmp/registry.js <<'EOF'
const http = require('http'), fs = require('fs');
const tarball = fs.readFileSync('/tmp/stallpkg-1.0.0.tgz');
let stalls = 0;
http.createServer((req, res) => {
  console.log('registry: GET ' + req.url);
  if (req.url === '/stallpkg') {
    if (stalls < 2) {
      stalls += 1;
      fs.writeFileSync('/tmp/stalls', String(stalls));
      return; // never answered
    }
    res.setHeader('content-type', 'application/json');
    return res.end(JSON.stringify({
      name: 'stallpkg',
      'dist-tags': {latest: '1.0.0'},
      versions: {'1.0.0': {
        name: 'stallpkg', version: '1.0.0', bin: {stallpkg: 'bin.js'},
        dist: {tarball: 'http://127.0.0.1:8080/stallpkg/-/stallpkg-1.0.0.tgz'},
      }},
    }));
  }
  if (req.url === '/stallpkg/-/stallpkg-1.0.0.tgz') {
    res.setHeader('content-type', 'application/octet-stream');
    return res.end(tarball);
  }
  res.statusCode = 404;
  res.end('{}');
}).listen(8080, '127.0.0.1', () => fs.writeFileSync('/tmp/registry.ready', ''));
EOF
node /tmp/registry.js &
registry=$!
i=0
until [ -e /tmp/registry.ready ]; do i=$((i+1)); [ $i -lt 100 ] || exit 9; sleep 0.1; done
cd /tmp
npm install --prefix /tmp/home/lsp stallpkg@1.0.0 --registry=http://127.0.0.1:8080/
kill $registry"#;

/// SME-452: an npm install whose registry stalls twice on a request
/// still finishes inside the install bound. With npm's defaults its
/// second attempt ends at about 610 s, past the 600 s bound: the flake's
/// own signature, `The install didn't finish within 10 minutes`.
#[tokio::test]
async fn test_an_npm_install_rides_out_a_registry_that_stalls_twice() {
    with_sandbox(|client, sandbox| async move {
        let config = LanguageServerConfig {
            name: "stall".to_string(),
            image: "node:22-slim".to_string(),
            install_command: STALLING_REGISTRY_INSTALL.to_string(),
            command: "cat".to_string(),
            file_types: [("txt".to_string(), "plaintext".to_string())].into(),
            memory_limit: "512Mi".to_string(),
            cpu_limit: "1".to_string(),
            enabled: true,
            ..Default::default()
        };
        let name = server_pod_name(sandbox.pod_id, &config.name);
        let started = std::time::Instant::now();
        let result = start_with(&client, &sandbox, &config, "v1").await;
        let took = started.elapsed();
        let stalls = crate::sandbox::exec_with(&client, &name, "server", &["cat", "/tmp/stalls"], None)
            .await
            .map(|r| r.stdout.trim().to_string());
        let _ = crate::sandbox::pods_api(&client).delete(&name, &crate::sandbox::pod_delete_params()).await;
        assert_eq!(result, Ok(Started::Started), "after {} s", took.as_secs());
        assert_eq!(stalls.as_deref().ok(), Some("2"), "the registry didn't stall twice");
        eprintln!("installed through two stalls in {} s", took.as_secs());
    })
    .await;
}

/// SME-452: an install that runs out its bound says what it was doing:
/// its output so far comes back in the error, since its pod (and the log
/// in it) is deleted after. A 5 s bound stands in for the 10 minutes.
#[tokio::test]
async fn test_a_timed_out_install_says_what_it_was_doing() {
    with_sandbox(|client, sandbox| async move {
        let config = echo_server("hang", "echo fetching-marker-452; sleep 3600");
        let pods = crate::sandbox::pods_api(&client);
        let name = server_pod_name(sandbox.pod_id, &config.name);
        pods.create(&kube::api::PostParams::default(), &server_pod_spec(&sandbox, &config, "v1"))
            .await
            .expect("create the server pod");
        let running = wait_running(&pods, &name).await;
        let installed = match &running {
            Ok(uid) => Some(wait_installed_within(&client, &pods, &name, uid, std::time::Duration::from_secs(5)).await),
            Err(_) => None,
        };
        let _ = pods.delete(&name, &crate::sandbox::pod_delete_params()).await;
        running.expect("the server pod runs");
        let error = installed.expect("waited").expect_err("an install sleeping for an hour finished");
        assert!(error.contains("didn't finish within 5 s"), "{error}");
        assert!(error.contains("fetching-marker-452"), "{error}");
    })
    .await;
}
