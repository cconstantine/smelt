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
/// status and no `ExitCode` cause; the wait loop read that as \"still
/// running\" and the log read as an empty log.
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
