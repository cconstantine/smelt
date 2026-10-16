use super::*;

/// A stand-in for mason's registry and Helix's file, on a local port.
async fn stand_in() -> String {
    let router = axum::Router::new()
        .route(
            "/packages/pyright/package.yaml",
            axum::routing::get(|| async { include_str!("../fixtures/mason-pyright.yaml") }),
        )
        .route("/languages.toml", axum::routing::get(|| async { HELIX }))
        .route(
            "/packages/huge/package.yaml",
            axum::routing::get(|| async { "x".repeat(3 * 1024 * 1024) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, router).await.expect("serve") });
    format!("http://{addr}")
}

#[test]
fn test_empty_source_urls_fall_back_to_the_defaults() {
    assert_eq!(
        sources(Some(""), Some("")),
        (DEFAULT_MASON_REGISTRY_URL.to_string(), DEFAULT_HELIX_LANGUAGES_URL.to_string())
    );
}

#[test]
fn test_unset_source_urls_are_the_defaults() {
    assert_eq!(
        sources(None, None),
        (DEFAULT_MASON_REGISTRY_URL.to_string(), DEFAULT_HELIX_LANGUAGES_URL.to_string())
    );
}

#[test]
fn test_set_source_urls_are_used_as_given() {
    assert_eq!(
        sources(Some("http://registry.test"), Some("http://helix.test/languages.toml")),
        ("http://registry.test".to_string(), "http://helix.test/languages.toml".to_string())
    );
}

#[tokio::test]
async fn test_a_lookup_fetches_both_files_and_suggests() {
    let base = stand_in().await;
    let helix = format!("{base}/languages.toml");
    let found = lookup("pyright", &base, &helix).await.expect("found");
    assert_eq!(found.config.name, "pyright");

    let missing = lookup("no-such-server", &base, &helix).await.expect_err("404");
    assert!(missing.contains("no-such-server"), "{missing}");

    let huge = lookup("huge", &base, &helix).await.expect_err("too big");
    assert!(huge.contains("too big"), "{huge}");

    let odd = lookup("../etc/passwd", &base, &helix).await.expect_err("not a package name");
    assert!(odd.contains("package name"), "{odd}");
}

const HELIX: &str = include_str!("../fixtures/helix-languages-excerpt.toml");

fn suggest_for(package: &str) -> Suggestion {
    let yaml = std::fs::read_to_string(format!("{}/src/lsp/fixtures/mason-{package}.yaml", env!("CARGO_MANIFEST_DIR")))
        .expect("fixture");
    suggest(&yaml, HELIX).expect("a suggestion")
}

#[test]
fn test_rust_analyzer_comes_from_its_release_and_keeps_its_own_target_dir() {
    let s = suggest_for("rust-analyzer");
    let c = &s.config;
    assert_eq!(c.name, "rust-analyzer");
    assert_eq!(c.image, "rust:1", "rust-analyzer runs cargo, so it needs Rust's image");
    assert!(
        c.install_command.contains(
            "https://github.com/rust-lang/rust-analyzer/releases/download/2026-09-21/rust-analyzer-x86_64-unknown-linux-gnu.gz"
        ),
        "{}",
        c.install_command
    );
    assert_eq!(c.command, "/tmp/home/bin/rust-analyzer");
    assert!(c.args.is_empty());
    assert_eq!(c.file_types, [("rs".to_string(), "rust".to_string())].into());
    assert_eq!(c.root_markers, vec!["Cargo.toml", "Cargo.lock"]);
    let init = c.initialization_options.as_ref().expect("initialization options");
    assert_eq!(init["files"]["watcher"], "server", "Helix's config is kept");
    assert_eq!(init["cargo"]["targetDir"], true, "its own target dir, so it doesn't block the sandbox's cargo");
    assert!(c.enabled);
}

#[test]
fn test_pyright_is_an_npm_install_in_nodes_image_found_by_its_language() {
    let c = suggest_for("pyright").config;
    assert_eq!(c.image, "node:22-slim");
    assert_eq!(c.install_command, "npm install --prefix /tmp/home/lsp pyright@1.1.414");
    assert_eq!(c.command, "/tmp/home/lsp/node_modules/.bin/pyright-langserver");
    assert_eq!(c.args, vec!["--stdio"]);
    // Helix doesn't list pyright for Python; mason's `languages` does.
    assert_eq!(c.file_types.get("py").map(String::as_str), Some("python"));
    assert!(!c.file_types.keys().any(|k| k.contains('*') || k.starts_with('.')), "globs are left out");
    assert_eq!(c.root_markers[0], "pyproject.toml");
}

#[test]
fn test_typescript_brings_its_extra_package_and_every_language_it_serves() {
    let c = suggest_for("typescript-language-server").config;
    assert_eq!(
        c.install_command,
        "npm install --prefix /tmp/home/lsp typescript-language-server@6.0.1 typescript@6.0.3"
    );
    assert_eq!(c.args, vec!["--stdio"]);
    for (ext, id) in [("ts", "typescript"), ("tsx", "typescriptreact"), ("js", "javascript"), ("jsx", "javascriptreact")] {
        assert_eq!(c.file_types.get(ext).map(String::as_str), Some(id), "{ext}");
    }
}

#[test]
fn test_gopls_is_a_go_install() {
    let c = suggest_for("gopls").config;
    assert_eq!(c.image, "golang:1");
    assert_eq!(c.install_command, "GOBIN=/tmp/home/go/bin go install golang.org/x/tools/gopls@v0.23.0");
    assert_eq!(c.command, "/tmp/home/go/bin/gopls");
    assert_eq!(c.file_types.get("go").map(String::as_str), Some("go"));
}

#[test]
fn test_a_package_it_cant_install_says_so_but_still_suggests_the_rest() {
    let yaml = "name: solargraph\nlanguages:\n  - Ruby\nsource:\n  id: pkg:gem/solargraph@0.50.0\nbin:\n  solargraph: gem:solargraph\n";
    let s = suggest(yaml, HELIX).expect("a partial suggestion");
    assert!(s.config.install_command.is_empty());
    assert!(s.notes.iter().any(|n| n.contains("pkg:gem")), "{:?}", s.notes);
    // Nothing in the Helix excerpt knows solargraph or Ruby.
    assert!(s.notes.iter().any(|n| n.contains("file types")), "{:?}", s.notes);
}

#[test]
fn test_something_that_isnt_a_mason_package_is_refused() {
    assert!(suggest("not: [a, package", HELIX).is_err());
    assert!(suggest("name: x\n", HELIX).is_err(), "no source");
}

/// A mason package with `source_id` and these `extra_packages`, each
/// written as a YAML double-quoted string.
fn package_with_extras(name: &str, source_id: &str, extras: &[&str]) -> String {
    let extras: String = extras
        .iter()
        .map(|e| format!("    - {}\n", serde_json::to_string(e).expect("a JSON string")))
        .collect();
    format!("name: {name}\nlanguages:\n  - Python\nsource:\n  id: {source_id}\n  extra_packages:\n{extras}bin:\n  {name}: npm:{name}\n")
}

/// An empty directory of the test's own to run `sh` in.
fn scratch(test: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("smelt-catalog-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// Runs `script` with a real `sh -c` in `dir`.
fn sh(dir: &std::path::Path, script: &str) -> std::process::Output {
    std::process::Command::new("sh").arg("-c").arg(script).current_dir(dir).output().expect("sh runs")
}

fn lines(path: std::path::PathBuf) -> Vec<String> {
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn test_catalog_values_reach_the_shell_as_single_words() {
    let extras = ["$(touch MARKER)", "a;touch MARKER", "`touch MARKER`", "it's", "{{ x | y }}"];
    let yaml = package_with_extras("pkg", "pkg:npm/pkg@1.0.0", &extras);
    let install = suggest(&yaml, HELIX).expect("a suggestion").config.install_command;
    assert!(install.starts_with("npm install --prefix /tmp/home/lsp pkg@1.0.0 "), "{install}");

    let dir = scratch("single-words");
    let out = sh(&dir, &format!("npm() {{ printf '%s\\n' \"$@\" > ARGS; }}\n{install}"));
    assert!(!dir.join("MARKER").exists(), "a catalog value ran a command: {install}\n{out:?}");
    let mut expected = vec!["install", "--prefix", "/tmp/home/lsp", "pkg@1.0.0"];
    expected.extend(extras);
    assert_eq!(lines(dir.join("ARGS")), expected, "{install}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_each_install_kind_quotes_what_it_takes_from_the_catalog() {
    let install = |yaml: &str| suggest(yaml, HELIX).expect("a suggestion").config;

    let pypi = install(&package_with_extras("pkg", "pkg:pypi/pkg@1.0", &["x y"]));
    assert_eq!(pypi.install_command, "python -m venv /tmp/home/venv && /tmp/home/venv/bin/pip install pkg==1.0 'x y'");

    let golang = install("name: tool\nsource:\n  id: pkg:golang/example.com/a b/tool@v1.0.0\n");
    assert_eq!(golang.install_command, "GOBIN=/tmp/home/go/bin go install 'example.com/a b/tool@v1.0.0'");

    let cargo = install("name: tool\nsource:\n  id: pkg:cargo/a;b@1.0\n");
    assert_eq!(cargo.install_command, "cargo install 'a;b' --version 1.0 --root /tmp/home/cargo");

    let archive = install(
        "name: tool\nsource:\n  id: pkg:github/owner/tool@v1\n  asset:\n    - target: linux_x64\n      file: tool-$HOME.tar.gz\n      bin: dir with space/tool\nbin:\n  tool: \"{{source.asset.bin}}\"\n",
    );
    assert_eq!(
        archive.install_command,
        "mkdir -p /tmp/home/bin /tmp/home/opt && curl -fsSL 'https://github.com/owner/tool/releases/download/v1/tool-$HOME.tar.gz' | tar -xz -C /tmp/home/opt && ln -sf '/tmp/home/opt/dir with space/tool' /tmp/home/bin/tool"
    );

    // No `bin` map: the target is named after the package.
    let plain = install("name: a;b\nsource:\n  id: pkg:github/owner/tool@v1\n  asset:\n    - target: linux_x64_gnu\n      file: tool\n");
    assert_eq!(
        plain.install_command,
        "mkdir -p /tmp/home/bin && curl -fsSL -o '/tmp/home/bin/a;b' https://github.com/owner/tool/releases/download/v1/tool && chmod +x '/tmp/home/bin/a;b'"
    );
    assert_eq!(plain.command, "/tmp/home/bin/a;b", "the command is an argv, run with no shell: unquoted");
}

#[test]
fn test_a_value_quoting_cant_make_safe_leaves_the_install_to_the_user() {
    let long = format!("-{}", "a".repeat(200));
    let long_shown = format!("{:?}…", &long[..80]);
    let cases = [
        (package_with_extras("pkg", "pkg:npm/pkg@1.0.0", &["--registry=http://evil.test"]), r#""--registry=http://evil.test""#, "node:22-slim"),
        (package_with_extras("pkg", "pkg:npm/pkg@1.0.0", &["a\nb"]), r#""a\nb""#, "node:22-slim"),
        (package_with_extras("pkg", "pkg:npm/pkg@1.0.0", &[""]), "an empty value", "node:22-slim"),
        (package_with_extras("pkg", "pkg:pypi/pkg@1.0", &[long.as_str()]), long_shown.as_str(), "python:3-slim"),
        ("name: pkg\nlanguages:\n  - Python\nsource:\n  id: pkg:cargo/pkg@-x\n".to_string(), r#""-x""#, "rust:1"),
    ];
    for (yaml, shown, image) in cases {
        let s = suggest(&yaml, HELIX).expect("a partial suggestion");
        let c = &s.config;
        assert_eq!(c.install_command, "", "{shown}");
        assert!(
            s.notes.iter().any(|n| n.contains(shown) && n.contains("fill in the install command")),
            "{shown}: {:?}",
            s.notes
        );
        assert!(!s.notes.iter().any(|n| n.contains(&long)), "a long value is cut: {:?}", s.notes);
        assert_eq!(c.image, image, "{shown}");
        assert_eq!(c.command, "pkg", "{shown}");
        assert_eq!(c.file_types.get("py").map(String::as_str), Some("python"), "{shown}");
    }
}

#[test]
fn test_a_refusal_note_says_why_and_shows_where() {
    let note = |yaml: &str| {
        let s = suggest(yaml, HELIX).expect("a partial suggestion");
        assert_eq!(s.config.install_command, "", "{yaml}");
        s.notes.into_iter().find(|n| n.contains("won't put")).expect("a refusal note")
    };
    // A control character past the first 80 characters of a long word
    // (the URL) is still shown.
    let late = note(
        "name: tool\nsource:\n  id: pkg:github/owner/tool@v1\n  asset:\n    - target: linux_x64\n      file: \"tool_linux_amd64_with_a_long_enough_name_to_push_it_past_eighty\\n.tar.gz\"\n",
    );
    assert!(late.contains(r"\n.tar.gz"), "{late}");
    assert!(late.contains("control character"), "{late}");

    let option = note(&package_with_extras("pkg", "pkg:npm/pkg@1.0.0", &["--registry=http://evil.test"]));
    assert!(option.contains("starts with \"-\""), "{option}");
    let empty = note(&package_with_extras("pkg", "pkg:npm/pkg@1.0.0", &[""]));
    assert!(empty.contains("an empty value"), "{empty}");
}

/// A github package with one Linux asset.
fn github_package(file: &str, asset_bin: Option<&str>, bin_key: &str) -> String {
    let asset_bin = asset_bin.map(|b| format!("      bin: {}\n", serde_json::to_string(b).expect("JSON"))).unwrap_or_default();
    let file = serde_json::to_string(file).expect("JSON");
    let bin_key = serde_json::to_string(bin_key).expect("JSON");
    format!("name: tool\nsource:\n  id: pkg:github/owner/tool@v1\n  asset:\n    - target: linux_x64\n      file: {file}\n{asset_bin}bin:\n  {bin_key}: \"{{{{source.asset.bin}}}}\"\n")
}

#[test]
fn test_every_github_download_shape_quotes_and_every_kind_refuses() {
    let install = |yaml: &str| suggest(yaml, HELIX).expect("a suggestion").config.install_command;
    let release = "https://github.com/owner/tool/releases/download/v1";
    assert_eq!(
        install(&github_package("tool $X.gz", None, "tool")),
        format!("mkdir -p /tmp/home/bin && curl -fsSL '{release}/tool $X.gz' | gunzip > /tmp/home/bin/tool && chmod +x /tmp/home/bin/tool")
    );
    assert_eq!(
        install(&github_package("tool.zip", Some("a b/tool"), "tool")),
        format!("mkdir -p /tmp/home/bin /tmp/home/opt && curl -fsSL -o /tmp/home/release.zip {release}/tool.zip && unzip -q /tmp/home/release.zip -d /tmp/home/opt && ln -sf '/tmp/home/opt/a b/tool' /tmp/home/bin/tool")
    );
    assert_eq!(
        install(&github_package("tool;x.tar.xz", None, "tool")),
        format!("mkdir -p /tmp/home/bin /tmp/home/opt && curl -fsSL '{release}/tool;x.tar.xz' | tar -xJ -C /tmp/home/opt && ln -sf /tmp/home/opt/tool /tmp/home/bin/tool")
    );

    for (yaml, why) in [
        ("name: tool\nsource:\n  id: pkg:golang/-toolexec=evil@v1\n".to_string(), "starts with"),
        (github_package("tool", None, "t\no"), "control character"),
    ] {
        let s = suggest(&yaml, HELIX).expect("a partial suggestion");
        assert_eq!(s.config.install_command, "", "{yaml}");
        assert!(s.notes.iter().any(|n| n.contains("won't put") && n.contains(why)), "{why}: {:?}", s.notes);
    }
}

#[test]
fn test_a_value_that_would_show_as_something_else_is_refused() {
    // A right-to-left override shows `evil` the other way round in the
    // form; a zero-width space or a soft hyphen doesn't show at all.
    for (extra, escaped) in [("pkg\u{202E}live", r"\u{202e}"), ("type\u{200B}script", r"\u{200b}"), ("a\u{2066}b", r"\u{2066}"), ("x\u{AD}y", r"\u{ad}")] {
        let s = suggest(&package_with_extras("pkg", "pkg:npm/pkg@1.0.0", &[extra]), HELIX).expect("a partial suggestion");
        assert_eq!(s.config.install_command, "", "{escaped}");
        assert!(
            s.notes.iter().any(|n| n.contains("won't put") && n.contains(escaped) && n.contains("invisible")),
            "{escaped}: {:?}",
            s.notes
        );
    }
    // Other non-ASCII text is quoted, not refused.
    let s = suggest(&package_with_extras("pkg", "pkg:npm/pkg@1.0.0", &["café"]), HELIX).expect("a suggestion");
    assert_eq!(s.config.install_command, "npm install --prefix /tmp/home/lsp pkg@1.0.0 'café'");
}

#[test]
fn test_a_refused_install_leaves_no_notes_about_the_command_it_dropped() {
    // An unfilled template in the file (noted first), then a refused
    // archive path.
    let yaml = "name: tool\nsource:\n  id: pkg:github/owner/tool@v1\n  asset:\n    - target: linux_x64\n      file: tool_{{ version | x }}.tar.gz\n      bin: \"dir\\n/tool\"\nbin:\n  tool: \"{{source.asset.bin}}\"\n";
    let s = suggest(yaml, HELIX).expect("a partial suggestion");
    assert_eq!(s.config.install_command, "");
    assert!(!s.notes.iter().any(|n| n.contains("template")), "no note about a command that isn't there: {:?}", s.notes);
    let refusal = s.notes.iter().find(|n| n.contains("won't put")).expect("a refusal note");
    assert!(refusal.contains("and the command to run"), "the command is only the bare name now: {refusal}");
    assert_eq!(s.config.command, "tool");
}

#[test]
fn test_shell_word_round_trips_through_sh() {
    let dir = scratch("round-trip");
    // Something for an unquoted `*` to match, so a missing quote shows.
    std::fs::write(dir.join("a-file"), "").expect("a file to glob");
    for value in ["'", "\"", "$HOME", "`", "\\", "a b", "*", "~", "~x", "#x", "{a,b}", "é", "it's \"$(x)\""] {
        let word = shell_word(value).expect("quotable");
        // `sh` may be dash, which has no brace expansion, and `~x` names
        // no user: unquoted, those would come back unchanged too.
        assert!(word.starts_with('\''), "{value} needs quoting, got {word}");
        let out = sh(&dir, &format!("printf %s {word}"));
        assert!(out.status.success(), "{value}: {out:?}");
        assert_eq!(String::from_utf8_lossy(&out.stdout), value, "{value} as {word}");
    }
    for value in ["@scope/pkg@1.2.3", "pkg==1.0", "v0.0.0-20260918162853-acb8859b9031", "4.39.0%2B79757"] {
        assert_eq!(shell_word(value).expect("safe"), value, "a safe value is left as it is");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_actionlints_unfilled_template_is_one_quoted_word() {
    let s = suggest_for("actionlint");
    let install = &s.config.install_command;
    let url = r#"https://github.com/rhysd/actionlint/releases/download/v1.7.12/actionlint_{{ version | strip_prefix "v" }}_linux_amd64.tar.gz"#;
    assert_eq!(
        install,
        &format!("mkdir -p /tmp/home/bin /tmp/home/opt && curl -fsSL '{url}' | tar -xz -C /tmp/home/opt && ln -sf /tmp/home/opt/actionlint /tmp/home/bin/actionlint")
    );
    assert!(s.notes.iter().any(|n| n.contains("template")), "the template note still fires: {:?}", s.notes);

    let dir = scratch("actionlint");
    let stubs = "curl() { printf '%s\\n' \"$@\" > CURL_ARGS; }\ntar() { :; }\nmkdir() { :; }\nln() { :; }\nstrip_prefix() { touch MARKER; }\n";
    let out = sh(&dir, &format!("{stubs}{install}"));
    assert!(!dir.join("MARKER").exists(), "the template ran as a command: {out:?}");
    assert_eq!(lines(dir.join("CURL_ARGS")), vec!["-fsSL", url]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Guards: real packages whose values need no quoting read as before.
/// (The pypi and cargo installs are wrong for SME-215's reasons: they
/// drop the purl's qualifiers. Pinned as unchanged, not as right.)
#[test]
fn test_real_packages_that_need_no_quoting_are_unchanged() {
    assert_eq!(
        suggest_for("commitlint").config.install_command,
        "npm install --prefix /tmp/home/lsp @commitlint/cli@21.2.3 @commitlint/config-conventional commitlint-format-json"
    );
    assert_eq!(
        suggest_for("python-lsp-server").config.install_command,
        "python -m venv /tmp/home/venv && /tmp/home/venv/bin/pip install python-lsp-server==1.15.0"
    );
    assert_eq!(suggest_for("nil").config.install_command, "cargo install nil --version 2025-06-13 --root /tmp/home/cargo");
}
