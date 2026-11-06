//! SME-440: makes a pod's execs stall the ways one can on a loaded node,
//! so a test can stall one on demand. `NoReply`: the command runs in the
//! pod, and `PodShell::run` never answers. `LateStart`: the command starts
//! in the pod only after a delay, on its own, while `run` never answers —
//! an abandoned exec that acts after its caller has moved on.
//! `ErrorAfterStart`: the command is running in the pod, acting after a
//! delay, while `run` answers at once with a broken stream (`Err(Io)`).

use super::{exec_with, ExecResult, SandboxError};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Stall {
    NoReply,
    LateStart(Duration),
    ErrorAfterStart(Duration),
}

struct Armed {
    matching: Vec<String>,
    count: usize,
    stall: Stall,
}

static ARMED: Mutex<Option<HashMap<String, Armed>>> = Mutex::new(None);
static LATE: Mutex<Option<HashMap<String, Vec<tokio::task::JoinHandle<i32>>>>> = Mutex::new(None);

/// The next `count` execs into `pod_name` whose command holds every one of
/// `matching` stall as `stall`. A count of 0 disarms.
pub(crate) fn arm(pod_name: &str, matching: &[&str], count: usize, stall: Stall) {
    let armed = Armed { matching: matching.iter().map(|m| m.to_string()).collect(), count, stall };
    ARMED.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(pod_name.to_string(), armed);
}

/// How many of the armed stalls for `pod_name` are still to come.
pub(crate) fn remaining(pod_name: &str) -> usize {
    ARMED.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|m| m.get(pod_name)).map_or(0, |a| a.count)
}

/// Waits for every late-started command into `pod_name` to finish, and
/// gives their exit codes (-1 for one whose exec failed).
pub(crate) async fn late_starts_done(pod_name: &str) -> Vec<i32> {
    let handles = LATE.lock().unwrap_or_else(|e| e.into_inner()).as_mut().and_then(|m| m.remove(pod_name)).unwrap_or_default();
    let mut codes = Vec::new();
    for handle in handles {
        codes.push(handle.await.unwrap_or(-1));
    }
    codes
}

pub(super) fn take(pod_name: &str, command: &[&str]) -> Option<Stall> {
    let joined = command.join(" ");
    let mut armed = ARMED.lock().unwrap_or_else(|e| e.into_inner());
    let entry = armed.as_mut()?.get_mut(pod_name)?;
    if entry.count == 0 || !entry.matching.iter().all(|m| joined.contains(m.as_str())) {
        return None;
    }
    entry.count -= 1;
    Some(entry.stall)
}

pub(super) async fn stall(
    client: &kube::Client,
    pod_name: &str,
    command: &[&str],
    stdin: Option<&[u8]>,
    stall: Stall,
) -> Result<ExecResult, SandboxError> {
    let delay = match stall {
        Stall::NoReply => {
            let _ = exec_with(client, pod_name, "sandbox", command, stdin).await;
            None
        }
        Stall::LateStart(delay) | Stall::ErrorAfterStart(delay) => Some(delay),
    };
    if let Some(delay) = delay {
        let (client, pod) = (client.clone(), pod_name.to_string());
        let command: Vec<String> = command.iter().map(|c| c.to_string()).collect();
        let stdin = stdin.map(<[u8]>::to_vec);
        let late = tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let command: Vec<&str> = command.iter().map(String::as_str).collect();
            exec_with(&client, &pod, "sandbox", &command, stdin.as_deref()).await.map_or(-1, |r| r.exit_code)
        });
        LATE.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).entry(pod_name.to_string()).or_default().push(late);
    }
    if let Stall::ErrorAfterStart(_) = stall {
        return Err(SandboxError::Io(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "the exec's stream broke (stall_hook)")));
    }
    std::future::pending().await
}
