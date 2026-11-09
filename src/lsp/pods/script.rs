//! The script a server pod runs (moved from `pods.rs`, SME-456): the
//! install with its stall bounds, its outcome recorded, then idle.

use super::{INSTALL_LOG, INSTALL_RC};

/// How long opening a connection may take during an install (SME-452's
/// mechanism): the DNS lookup, the TCP connect, an `http://` or
/// `https://` proxy's `CONNECT` reply (rounds 1 and 2) and the TLS
/// handshake alike. npm bounds none of them: `@npmcli/agent` hard-codes its connection
/// timeout to 0, and `fetch-timeout` (`NPM_STALL_BOUNDS`) starts only once
/// the agent hands over a connected socket. So a connection that opens
/// and then goes quiet before TLS completes (a peer that never answers
/// the ClientHello, a path that goes dark after the SYN-ACK) held npm
/// silent past the 600 s install bound, never retried, with 0 bytes of
/// log: measured through `test_pyright_from_the_catalog_answers` against
/// a k3s-node tarpit holding only the first connection, with a working
/// registry one retry away. The install preloads `connect_bound.js`,
/// which destroys such a connect with `ECONNECTIONTIMEOUT`; npm retries
/// that on a fresh connection and logs each attempt. Worst case for one
/// request: 4 tries × (30 s connect + 30 s fetch) + 3 × 10 s backoff =
/// 270 s, inside `INSTALL_TIMEOUT`. That's per request, not per install:
/// an install makes several (packument, tarball, dependencies), and
/// several that each stall a while could add up past 600 s. In practice a
/// dark registry or path fails the first request and ends the install
/// there. A real handshake takes well under 1 s.
pub const CONNECT_BOUND: std::time::Duration = std::time::Duration::from_secs(30);

/// Where the pod script writes the connect bound's preload.
const CONNECT_BOUND_JS: &str = "/tmp/smelt-connect-bound.js";

/// The script a server pod runs: the install, its outcome recorded, then
/// idle until deleted (the server itself is started over `pods/exec`).
/// The install's subshell alone preloads `CONNECT_BOUND`'s
/// `connect_bound.js` (it wraps node's `net.connect`/`createConnection`
/// and `tls.connect`) through `NODE_OPTIONS`, after any of the config's
/// own; installs that don't run node ignore it. If the preload can't be
/// written, the install runs without it.
pub(super) fn pod_script(install_command: &str) -> String {
    let install = if install_command.trim().is_empty() { "true" } else { install_command };
    let preload = include_str!("connect_bound.js")
        .trim_end()
        .replace("__CONNECT_BOUND_MS__", &CONNECT_BOUND.as_millis().to_string());
    format!(
        "cat > {CONNECT_BOUND_JS} <<'SMELT_EOF'\n{preload}\nSMELT_EOF\n\
         mkdir -p \"$HOME\" && ( [ -f {CONNECT_BOUND_JS} ] && \
         NODE_OPTIONS=\"${{NODE_OPTIONS:+$NODE_OPTIONS }}--require={CONNECT_BOUND_JS}\" && \
         export NODE_OPTIONS; {install} ) > {INSTALL_LOG} 2>&1; echo $? > {INSTALL_RC}; \
         trap 'exit 0' TERM; while :; do sleep 1 & wait; done"
    )
}

/// npm's own stall detection, bounded inside `INSTALL_TIMEOUT` (SME-452),
/// as `npm_config_*` variables in every server pod; npm and npx read
/// them, and pnpm and yarn classic too, harmlessly. npm 10's defaults (a 5-minute `fetch-timeout`, 2 retries, up to
/// 60 s apart) keep one registry request that never answers alive for up
/// to 970 s: measured against a registry that accepts and never answers,
/// `npm install pyright` gave up after 971 s, so the 600 s install bound
/// fired first and the pod went with npm's error and log. With these,
/// npm drops a stalled request after 30 s and retries on a fresh
/// connection, at most 4 × 30 s + 3 × 10 s = 150 s per request (an
/// install makes several, each with its own 150 s; measured for one:
/// 151 s, `npm error network timeout`); a transient stall recovers
/// inside the bound, and a dead registry fails with npm's own error. A
/// real `npm install pyright` was unchanged (`added 1 package in 4s`; its
/// slowest fetch, the tarball, about 3 s). Audit, fund and the update
/// notifier are each one more request that can stall, and an install
/// needs none of them. `loglevel=http` makes npm log each fetch as it
/// finishes (`npm http fetch GET 200 … 292ms`, 4 lines for pyright),
/// where by default it writes nothing to a file until it ends: a timed-out
/// install's log then says how far npm got (SME-452's recurrence). The
/// config's own `env` (in any of npm's spellings of the name) or the
/// install command's own flags win over these; a `~/.npmrc` doesn't,
/// since npm ranks its environment above it. These bound only what comes
/// after the socket: opening the connection is `CONNECT_BOUND`'s.
pub const NPM_STALL_BOUNDS: [(&str, &str); 7] = [
    ("npm_config_fetch_timeout", "30000"),
    ("npm_config_fetch_retries", "3"),
    ("npm_config_fetch_retry_maxtimeout", "10000"),
    ("npm_config_audit", "false"),
    ("npm_config_fund", "false"),
    ("npm_config_update_notifier", "false"),
    ("npm_config_loglevel", "http"),
];
