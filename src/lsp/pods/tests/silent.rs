use super::super::*;
use super::cluster::{echo_server, with_sandbox};

/// The exec protocol's terminal `Status` for `command` in pod `name`'s
/// server container, as the API server sent it: what `exec_with` reads its
/// exit code from.
async fn raw_status(client: &kube::Client, name: &str, command: &[&str]) -> String {
    let attached = crate::sandbox::pods_api(client)
        .exec(name, command.iter().copied(), &kube::api::AttachParams::default().container("server"))
        .await;
    match attached {
        Ok(mut attached) => {
            let status = attached.take_status();
            let status = match status {
                Some(status) => tokio::time::timeout(std::time::Duration::from_secs(30), status).await.ok().flatten(),
                None => None,
            };
            let _ = attached.join().await;
            serde_json::to_string(&status).unwrap_or_default()
        }
        Err(e) => format!("the exec upgrade failed: {e}"),
    }
}

/// SME-452 (B): an exec the container runtime can't start (here a
/// binary that doesn't exist) is a failure that says why, not exit 0
/// with nothing on stderr. The kubelet ends such an exec with a `Failure`
/// status and no `ExitCode` cause; the wait loop read that as "still
/// running" and the log read as an empty log.
#[tokio::test]
async fn test_an_exec_the_runtime_cant_start_reads_as_a_failure() {
    with_sandbox(|client, sandbox| async move {
        let config = echo_server("refuse", "true");
        let pods = crate::sandbox::pods_api(&client);
        let name = server_pod_name(sandbox.pod_id, &config.name);
        pods.create(&kube::api::PostParams::default(), &server_pod_spec(&sandbox, &config, "v1"))
            .await
            .expect("create the server pod");
        let running = wait_running(&pods, &name).await;
        let command = ["/nonexistent-452"];
        let (status, result) = match &running {
            Ok(_) => (
                raw_status(&client, &name, &command).await,
                Some(crate::sandbox::exec_with(&client, &name, "server", &command, None).await),
            ),
            Err(_) => (String::new(), None),
        };
        let _ = pods.delete(&name, &crate::sandbox::pod_delete_params()).await;
        running.expect("the server pod runs");
        let result = result.expect("ran").expect("the exec upgrade went through");
        eprintln!("the runtime's status: {status}");
        assert!(
            result.exit_code != 0 && result.stderr.contains("nonexistent-452"),
            "exit_code: {}, stderr: {:?}, status: {status}",
            result.exit_code,
            result.stderr
        );
    })
    .await;
}

/// A registry on `127.0.0.1:8080` inside the server pod that answers
/// `GET /trickle`'s packument with its `content-length` set and then one
/// byte every 20 s: never idle long enough for npm's 30 s fetch timeout,
/// never done. npm has no bound on a request's total time, and writes
/// nothing to a file until it ends, so the install sits silent, the
/// recurrence's mechanism (A) (the Planner's M2). The registry's own
/// output goes nowhere, so the log is npm's alone.
const TRICKLING_REGISTRY_INSTALL: &str = r#"set -e
cat > /tmp/registry.js <<'EOF'
const http = require('http'), fs = require('fs');
const body = JSON.stringify({name: 'trickle', 'dist-tags': {latest: '1.0.0'}, versions: {}});
http.createServer((req, res) => {
  if (req.url !== '/trickle') { res.statusCode = 404; return res.end('{}'); }
  res.writeHead(200, {'content-type': 'application/json', 'content-length': Buffer.byteLength(body)});
  let sent = 0;
  const next = () => { if (sent < body.length) res.write(body[sent++]); };
  next();
  const timer = setInterval(next, 20000);
  res.on('close', () => clearInterval(timer));
}).listen(8080, '127.0.0.1', () => fs.writeFileSync('/tmp/registry.ready', ''));
EOF
node /tmp/registry.js > /dev/null 2>&1 &
i=0
until [ -e /tmp/registry.ready ]; do i=$((i+1)); [ $i -lt 100 ] || exit 9; sleep 0.1; done
cd /tmp
npm install --prefix /tmp/home/lsp trickle@1.0.0 --registry=http://127.0.0.1:8080/"#;

/// SME-452's recurrence, on demand: an npm install still running, and
/// silent, when its bound runs out gave CI's `The install didn't finish
/// within 10 minutes. Its output so far:` with nothing after it. It now
/// says it wrote no output and names what was still running then (npm
/// sets its process title to `npm install …`). A 90 s bound stands in
/// for the 10 minutes, through the real wait.
#[tokio::test]
async fn test_a_silent_npm_install_that_runs_out_its_bound_names_what_was_running() {
    with_sandbox(|client, sandbox| async move {
        let config = LanguageServerConfig {
            name: "trickle".to_string(),
            image: "node:22-slim".to_string(),
            install_command: TRICKLING_REGISTRY_INSTALL.to_string(),
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
        let installed = match &running {
            Ok(uid) => Some(wait_installed_within(&client, &pods, &name, uid, std::time::Duration::from_secs(90)).await),
            Err(_) => None,
        };
        let _ = pods.delete(&name, &crate::sandbox::pod_delete_params()).await;
        running.expect("the server pod runs");
        let error = installed.expect("waited").expect_err("an install from a trickling registry finished");
        eprintln!("the timed-out install said: {error:?}");
        assert!(error.starts_with("The install didn't finish within 1 minute"), "{error}");
        assert!(error.contains("It wrote no output"), "{error}");
        assert!(error.lines().any(|line| line.contains("] npm install")), "no npm process named: {error}");
    })
    .await;
}
