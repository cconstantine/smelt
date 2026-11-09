use super::super::*;

/// The install's subshell in `script` (what runs between `( ` and the
/// log's redirection), and what comes after it.
fn install_subshell(script: &str) -> (&str, &str) {
    let (_, rest) = script.split_once("mkdir -p \"$HOME\" && ( ").unwrap_or_else(|| panic!("no install subshell: {script}"));
    let at = rest.find(") > /tmp/install.log").or_else(|| rest.find(") >> /tmp/install.log"));
    let at = at.unwrap_or_else(|| panic!("the subshell's output doesn't go to the log: {script}"));
    (&rest[..at], &rest[at..])
}

/// The line that points the install's curl at smelt's `.curlrc`, unless
/// the config's env names a `CURL_HOME` of its own.
const CURL_HOME_LINE: &str = "[ -n \"$CURL_HOME\" ] || [ -f \"$HOME/.curlrc\" ] || \
    [ -f \"${XDG_CONFIG_HOME:-$HOME/.config}/curlrc\" ] || { CURL_HOME=/tmp/smelt-curl; export CURL_HOME; };";

/// SME-456: curl bounds nothing after a connection opens, and its connect
/// default is 300 s. The pod script writes smelt's `.curlrc` (30 s to
/// connect, under 1 KB/s for 60 s ends it) and points the install's
/// curl at it; the language server, started later over `pods/exec`,
/// never sees it.
#[test]
fn test_an_install_gets_curls_stall_bounds_and_the_server_doesnt() {
    let curlrc = include_str!("../curlrc");
    assert!(!curlrc.lines().any(|line| line.trim() == "SMELT_EOF"), "the .curlrc would end its own heredoc");
    let script = pod_script("curl -fsSL x");
    let written = format!("cat > /tmp/smelt-curl/.curlrc <<'SMELT_EOF'\n{}\nSMELT_EOF\n", curlrc.trim_end());
    assert!(script.contains(&written), "no .curlrc written: {script}");
    for bound in ["connect-timeout = 30", "speed-limit = 1024", "speed-time = 60"] {
        assert!(curlrc.lines().any(|line| line == bound), "{bound} isn't in the .curlrc: {curlrc}");
    }
    let (subshell, tail) = install_subshell(&script);
    let at = subshell.find(CURL_HOME_LINE).unwrap_or_else(|| panic!("no CURL_HOME in the subshell: {subshell}"));
    assert!(subshell[at..].contains("curl -fsSL x"), "the install doesn't come after it: {subshell}");
    assert!(!tail.contains("CURL_HOME"), "outside the install's subshell: {tail}");
}

/// The line that makes the install's go log each fetch and build step,
/// after the config's own `GOFLAGS`.
const GOFLAGS_LINE: &str = "GOFLAGS=\"${GOFLAGS:+$GOFLAGS }-x\"; export GOFLAGS;";

/// SME-456: cmd/go writes nothing while it fetches, so a stalled module
/// proxy left an empty log. With `-x`, appended to the config's own
/// `GOFLAGS` (never replacing them), go logs each `# get <url>` and each
/// build step: the log is the watchdog's sign of progress, and a stall's
/// log names its URL. The language server never gets it.
#[test]
fn test_a_go_install_logs_its_fetches_and_the_server_doesnt() {
    let script = pod_script("go install x@v1");
    let (subshell, tail) = install_subshell(&script);
    let at = subshell.find(GOFLAGS_LINE).unwrap_or_else(|| panic!("no GOFLAGS in the subshell: {subshell}"));
    assert!(subshell[at..].contains("go install x@v1"), "the install doesn't come after it: {subshell}");
    assert!(!tail.contains("GOFLAGS"), "outside the install's subshell: {tail}");
}

/// SME-456: the go watchdog runs next to the install, which runs in the
/// background, its log opened for append: the watchdog's line about why
/// it stopped go survives the install's next write. The record is
/// written once the install and its watchdog have both ended.
#[test]
fn test_the_install_log_is_appended_to() {
    let watchdog = include_str!("../go_idle.sh");
    assert!(!watchdog.lines().any(|line| line.trim() == "SMELT_EOF"), "the watchdog would end its own heredoc");
    let filled = watchdog.trim_end().replace("__GO_IDLE_S__", "120");
    assert!(!filled.contains("__"), "a placeholder left unfilled: {filled}");
    let script = pod_script("go install x@v1");
    let written = format!("cat > /tmp/smelt-go-idle.sh <<'SMELT_EOF'\n{filled}\nSMELT_EOF\n");
    assert!(script.contains(&written), "no watchdog written: {script}");
    let mut rest = script.as_str();
    for step in [
        ": > /tmp/install.log; mkdir -p \"$HOME\" && ( ",
        ") >> /tmp/install.log 2>&1 & install=$!;",
        "sh /tmp/smelt-go-idle.sh \"$install\" /tmp/install.log & watch=$!;",
        "wait \"$install\"; rc=$?;",
        "kill \"$watch\" 2>/dev/null; wait \"$watch\";",
        "echo $rc > /tmp/install.rc;",
        "trap 'exit 0' TERM;",
    ] {
        let at = rest.find(step).unwrap_or_else(|| panic!("no {step:?} after what came before: {script}"));
        rest = &rest[at + step.len()..];
    }
}

/// The pod script's paths in a server pod are the ones the install's
/// waiters read.
#[test]
fn test_the_pod_scripts_record_and_log_are_the_ones_read() {
    let at = &super::super::script::IN_THE_POD;
    assert_eq!(format!("{}/{}", at.tmp, super::super::script::INSTALL_LOG_FILE), INSTALL_LOG);
    assert_eq!(format!("{}/{}", at.tmp, super::super::script::INSTALL_RC_FILE), INSTALL_RC);
    assert_eq!(at.go_idle, std::time::Duration::from_secs(120));
}

/// What the catalog suggested for rust-analyzer before SME-360
/// (`config.rs`'s `OLD_INSTALL`, copied): the x86-64 asset on every node.
const OLD_INSTALL: &str = "mkdir -p /tmp/home/bin && curl -fsSL \
    https://github.com/rust-lang/rust-analyzer/releases/download/2026-09-21/rust-analyzer-x86_64-unknown-linux-gnu.gz \
    | gunzip > /tmp/home/bin/rust-analyzer && chmod +x /tmp/home/bin/rust-analyzer";

/// The catalog's suggested install for `package`, from its fixture.
fn suggested(package: &str) -> String {
    let yaml = std::fs::read_to_string(format!("{}/src/lsp/fixtures/mason-{package}.yaml", env!("CARGO_MANIFEST_DIR")))
        .expect("fixture");
    let helix = include_str!("../../fixtures/helix-languages-excerpt.toml");
    crate::lsp::catalog::suggest(&yaml, helix).expect("a suggestion").config.install_command
}

/// SME-456 (the Driver's Q1): the bounds live in the pod script, so the
/// catalog's suggestions don't change and every saved install runs as
/// written, under them: the pre-SME-360 text, today's per-arch
/// suggestion, a user's edit and the gopls suggestion alike. The saved
/// old text still re-derives to today's suggestion (SME-386 A3).
#[test]
fn test_saved_and_edited_installs_run_with_the_stall_bounds_as_written() {
    let rust_analyzer = suggested("rust-analyzer");
    let installs = [
        OLD_INSTALL.to_string(),
        rust_analyzer.clone(),
        "curl -fsSL --max-time 900 https://example.invalid/x -o /tmp/x".to_string(),
        suggested("gopls"),
    ];
    for install in &installs {
        let script = pod_script(install);
        let (subshell, _) = install_subshell(&script);
        let at = subshell.find(CURL_HOME_LINE).unwrap_or_else(|| panic!("no CURL_HOME before {install}: {subshell}"));
        let after = subshell[at + CURL_HOME_LINE.len()..].trim_start();
        let after = after.strip_prefix(GOFLAGS_LINE).unwrap_or_else(|| panic!("no GOFLAGS before {install}: {subshell}"));
        assert_eq!(after.trim_start(), format!("{install} "), "the install isn't run as written");
    }
    assert_eq!(
        crate::lsp::catalog::rederived_install(OLD_INSTALL, &rust_analyzer).as_deref(),
        Some(rust_analyzer.as_str()),
        "the saved pre-SME-360 install no longer re-derives"
    );
}
