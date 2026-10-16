//! The catalog lookup (SME-35): a new language server's settings, suggested
//! from mason's registry (how to install it) and Helix's languages.toml
//! (how to run it, and which files it takes). Pure functions over the
//! fetched text; the page shows the result for the user to check and edit.

use std::borrow::Cow;

use crate::models::LanguageServerConfig;

/// Where a server pod's installs go: its HOME (SME-35's spike).
pub const POD_HOME: &str = "/tmp/home";

pub use crate::models::LanguageServerSuggestion as Suggestion;

/// Suggests a config for mason package `package_yaml`, run as Helix's
/// `languages_toml` describes it.
pub fn suggest(package_yaml: &str, languages_toml: &str) -> Result<Suggestion, String> {
    let package: Package =
        serde_norway::from_str(package_yaml).map_err(|e| format!("That isn't a mason package: {e}"))?;
    let source = package.source.as_ref().ok_or("That mason package has no source to install from.")?;
    let purl = Purl::parse(&source.id)?;
    let helix = toml::Value::Table(
        toml::from_str(languages_toml).map_err(|e| format!("Helix's languages.toml couldn't be read: {e}"))?,
    );
    let mut notes = Vec::new();

    // How to run it: Helix's entry for a server of this name.
    let helix_server = helix.get("language-server").and_then(|servers| servers.get(&package.name));
    let helix_command = helix_server.and_then(|s| s.get("command")).and_then(toml::Value::as_str);
    let args: Vec<String> = helix_server
        .and_then(|s| s.get("args"))
        .and_then(toml::Value::as_array)
        .map(|args| args.iter().filter_map(|a| a.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    if helix_server.is_none() {
        notes.push(format!(
            "Helix doesn't say how to run {}: check the command's arguments (many servers need --stdio).",
            package.name
        ));
    }

    // Which executable: the one Helix runs, else one named after the
    // package, else the first.
    let bin = helix_command
        .filter(|command| package.bin.contains_key(*command))
        .map(str::to_string)
        .or_else(|| package.bin.contains_key(&package.name).then(|| package.name.clone()))
        .or_else(|| package.bin.keys().next().cloned())
        .unwrap_or_else(|| package.name.clone());

    let (install_command, command) = match install(&purl, source, &bin, &mut notes) {
        Ok(install) => install,
        Err(note) => {
            notes.push(note);
            (String::new(), bin.clone())
        }
    };
    let image = image_for(&purl, &package.languages);

    let (file_types, root_markers) = languages(&helix, &package.name, &package.languages);
    if file_types.is_empty() {
        notes.push(format!("No file types are known for {}: add the extensions it handles.", package.name));
    }

    let mut initialization_options = helix_server
        .and_then(|s| s.get("config"))
        .and_then(|config| serde_json::to_value(config).ok())
        .filter(|config| config.as_object().is_some_and(|o| !o.is_empty()));
    for (server, extra) in ADJUSTMENTS {
        if *server == package.name {
            #[expect(clippy::expect_used, reason = "ADJUSTMENTS is a constant, and its JSON is covered by the catalog's tests")]
            let extra: serde_json::Value = serde_json::from_str(extra).expect("the adjustments are valid JSON");
            let merged = initialization_options.get_or_insert_with(|| serde_json::json!({}));
            merge(merged, &extra);
        }
    }

    Ok(Suggestion {
        config: LanguageServerConfig {
            name: package.name.to_ascii_lowercase().replace(|c: char| !(c.is_ascii_alphanumeric() || c == '-'), "-"),
            image,
            install_command,
            command,
            args,
            env: Default::default(),
            file_types,
            root_markers,
            initialization_options,
            settings: None,
            memory_limit: "2Gi".to_string(),
            cpu_limit: "1".to_string(),
            enabled: true,
        },
        notes,
    })
}

/// Mason's registry, unless `SMELT_MASON_REGISTRY_URL` says otherwise.
pub const DEFAULT_MASON_REGISTRY_URL: &str = "https://raw.githubusercontent.com/mason-org/mason-registry/main";

/// Helix's `languages.toml`, unless `SMELT_HELIX_LANGUAGES_URL` says
/// otherwise.
pub const DEFAULT_HELIX_LANGUAGES_URL: &str =
    "https://raw.githubusercontent.com/helix-editor/helix/master/languages.toml";

/// Where the catalog's two files come from. `SMELT_MASON_REGISTRY_URL` and
/// `SMELT_HELIX_LANGUAGES_URL` point elsewhere (tests use a stand-in).
pub fn sources_from_env() -> (String, String) {
    sources(
        std::env::var("SMELT_MASON_REGISTRY_URL").ok().as_deref(),
        std::env::var("SMELT_HELIX_LANGUAGES_URL").ok().as_deref(),
    )
}

/// `sources_from_env` on given values, with set-but-empty treated as unset
/// (as everywhere else in smelt).
fn sources(registry: Option<&str>, helix: Option<&str>) -> (String, String) {
    let given = |v: Option<&str>, default: &str| v.filter(|v| !v.is_empty()).unwrap_or(default).to_string();
    (given(registry, DEFAULT_MASON_REGISTRY_URL), given(helix, DEFAULT_HELIX_LANGUAGES_URL))
}

/// Looks `package` up in mason's registry at `registry` and suggests a
/// config, run as Helix's file at `helix_url` says.
pub async fn lookup(package: &str, registry: &str, helix_url: &str) -> Result<Suggestion, String> {
    let package = package.trim();
    let name_ok = !package.is_empty()
        && package.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        && !package.starts_with('.');
    if !name_ok {
        return Err(format!("{package:?} isn't a mason package name (e.g. rust-analyzer, pyright)."));
    }
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .build()
        .map_err(|e| format!("couldn't make an HTTP client: {e}"))?;
    let yaml_url = format!("{}/packages/{package}/package.yaml", registry.trim_end_matches('/'));
    let (yaml, helix) = tokio::join!(fetch(&client, &yaml_url), fetch(&client, helix_url));
    let yaml = match yaml {
        Ok(Some(text)) => text,
        Ok(None) => return Err(format!("mason's registry has no package named {package}.")),
        Err(e) => return Err(format!("Couldn't reach mason's registry: {e}")),
    };
    let helix = match helix {
        Ok(Some(text)) => text,
        Ok(None) => return Err("Helix's languages.toml wasn't found.".to_string()),
        Err(e) => return Err(format!("Couldn't reach Helix's languages.toml: {e}")),
    };
    suggest(&yaml, &helix)
}

/// Each catalog fetch's time limit and size cap (Helix's file is about
/// 250 KB).
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const FETCH_MAX_BYTES: usize = 2 * 1024 * 1024;

/// The body at `url`, `None` for a 404.
async fn fetch(client: &reqwest::Client, url: &str) -> Result<Option<String>, String> {
    let mut response = client.get(url).send().await.map_err(|e| e.to_string())?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !response.status().is_success() {
        return Err(format!("{url} answered {}", response.status()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        body.extend_from_slice(&chunk);
        if body.len() > FETCH_MAX_BYTES {
            return Err(format!("{url} is too big (over {} MB)", FETCH_MAX_BYTES / 1024 / 1024));
        }
    }
    String::from_utf8(body).map(Some).map_err(|_| format!("{url} isn't text"))
}

/// Settings a server needs to share `/workspace` with the sandbox, beyond
/// what Helix gives. rust-analyzer's `cargo check` would otherwise use the
/// project's own `target/` and block the sandbox's builds on its lock
/// (SME-35's spike).
const ADJUSTMENTS: &[(&str, &str)] = &[("rust-analyzer", r#"{"cargo": {"targetDir": true}}"#)];

#[derive(serde::Deserialize)]
struct Package {
    name: String,
    #[serde(default)]
    languages: Vec<String>,
    source: Option<Source>,
    #[serde(default)]
    bin: std::collections::BTreeMap<String, String>,
}

#[derive(serde::Deserialize)]
struct Source {
    id: String,
    #[serde(default)]
    extra_packages: Vec<String>,
    #[serde(default)]
    asset: Option<serde_norway::Value>,
}

/// A package URL: `pkg:npm/pyright@1.1.414`.
struct Purl {
    kind: String,
    path: String,
    version: String,
}

impl Purl {
    fn parse(id: &str) -> Result<Purl, String> {
        let rest = id.strip_prefix("pkg:").ok_or_else(|| format!("{id:?} isn't a package URL"))?;
        let (kind, rest) = rest.split_once('/').ok_or_else(|| format!("{id:?} isn't a package URL"))?;
        let rest = rest.split(['?', '#']).next().unwrap_or(rest);
        let (path, version) = rest.rsplit_once('@').ok_or_else(|| format!("{id:?} has no version"))?;
        Ok(Purl { kind: kind.to_string(), path: path.replace("%40", "@"), version: version.to_string() })
    }

    /// The package's own name: the last part of its path.
    fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

/// One shell word for `value`, a value from the catalog (SME-182): the
/// saved install command runs in the server pod's `sh -c` as written, so
/// this is the only place it can be quoted. As is when every character is
/// in CPython `shlex.quote`'s safe set (every real registry value but the
/// unfilled templates), else in single quotes, POSIX style. Refused, with
/// the note to show, when quoting can't make it safe: empty, starting with
/// `-` (quoted, it's still an option to npm, pip, go or cargo), or holding
/// a control character.
fn shell_word(value: &str) -> Result<Cow<'_, str>, String> {
    let refuse = |shown: String, why: &str| {
        Err(format!(
            "The catalog's install details include {shown}, which {why}, so smelt won't put it in a command: fill in the install command."
        ))
    };
    if value.is_empty() {
        return refuse("an empty value".to_string(), "is empty");
    }
    // The text around the first control character, so it shows however
    // far into a long word (a URL) it is.
    let first_bad = value.chars().position(char::is_control);
    let shown = |from: usize| {
        const SHOWN: usize = 80;
        let text: String = value.chars().skip(from).take(SHOWN).collect();
        let before = if from > 0 { "…" } else { "" };
        let after = if value.chars().nth(from + SHOWN).is_some() { "…" } else { "" };
        format!("{before}{text:?}{after}")
    };
    if let Some(at) = first_bad {
        return refuse(shown(at.saturating_sub(20)), "holds a control character");
    }
    if value.starts_with('-') {
        return refuse(shown(0), "starts with \"-\" (an option to the installer)");
    }
    if value.chars().all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c)) {
        Ok(Cow::Borrowed(value))
    } else {
        Ok(Cow::Owned(format!("'{}'", value.replace('\'', r"'\''"))))
    }
}

/// The install command and the path it leaves the executable at, or the
/// note to show when a catalog value can't go in a command.
fn install(purl: &Purl, source: &Source, bin: &str, notes: &mut Vec<String>) -> Result<(String, String), String> {
    let home = POD_HOME;
    let with_extras = |base: String| -> Result<String, String> {
        let mut command = base;
        for extra in &source.extra_packages {
            command.push(' ');
            command.push_str(&shell_word(extra)?);
        }
        Ok(command)
    };
    let versioned = |separator: &str| shell_word(&format!("{}{separator}{}", purl.path, purl.version)).map(Cow::into_owned);
    Ok(match purl.kind.as_str() {
        "npm" => (
            with_extras(format!("npm install --prefix {home}/lsp {}", versioned("@")?))?,
            format!("{home}/lsp/node_modules/.bin/{bin}"),
        ),
        "pypi" => (
            with_extras(format!("python -m venv {home}/venv && {home}/venv/bin/pip install {}", versioned("==")?))?,
            format!("{home}/venv/bin/{bin}"),
        ),
        // GOBIN: Go's image sets GOPATH to /go, so without it the binary
        // lands in /go/bin.
        "golang" => (format!("GOBIN={home}/go/bin go install {}", versioned("@")?), format!("{home}/go/bin/{bin}")),
        "cargo" => (
            format!(
                "cargo install {} --version {} --root {home}/cargo",
                shell_word(purl.name())?,
                shell_word(&purl.version)?
            ),
            format!("{home}/cargo/bin/{bin}"),
        ),
        "github" => match github_asset(source) {
            Some((file, inner)) => {
                let file = file.replace("{{version}}", &purl.version);
                if file.contains("{{") {
                    notes.push(format!("The release asset's name ({file}) has a template smelt doesn't fill in: check the install command."));
                }
                let target = format!("{home}/bin/{bin}");
                let url = shell_word(&format!("https://github.com/{}/releases/download/{}/{file}", purl.path, purl.version))?
                    .into_owned();
                let to = shell_word(&target)?;
                let inner = shell_word(&format!("{home}/opt/{}", inner.as_deref().unwrap_or(bin)))?.into_owned();
                let command = if file.ends_with(".tar.gz") || file.ends_with(".tgz") {
                    format!("mkdir -p {home}/bin {home}/opt && curl -fsSL {url} | tar -xz -C {home}/opt && ln -sf {inner} {to}")
                } else if file.ends_with(".tar.xz") {
                    format!("mkdir -p {home}/bin {home}/opt && curl -fsSL {url} | tar -xJ -C {home}/opt && ln -sf {inner} {to}")
                } else if file.ends_with(".zip") {
                    notes.push("The release is a .zip: the image needs unzip.".to_string());
                    format!("mkdir -p {home}/bin {home}/opt && curl -fsSL -o {home}/release.zip {url} && unzip -q {home}/release.zip -d {home}/opt && ln -sf {inner} {to}")
                } else if file.ends_with(".gz") {
                    format!("mkdir -p {home}/bin && curl -fsSL {url} | gunzip > {to} && chmod +x {to}")
                } else {
                    format!("mkdir -p {home}/bin && curl -fsSL -o {to} {url} && chmod +x {to}")
                };
                (command, target)
            }
            None => {
                notes.push("The package has no Linux x86-64 release to download: fill in the install command.".to_string());
                (String::new(), bin.to_string())
            }
        },
        other => {
            notes.push(format!("smelt can't suggest an install for pkg:{other} packages: fill in the install command."));
            (String::new(), bin.to_string())
        }
    })
}

/// The Linux x86-64 release asset: its file, and the executable's path
/// inside it when it's an archive.
fn github_asset(source: &Source) -> Option<(String, Option<String>)> {
    let assets = source.asset.as_ref()?.as_sequence()?;
    let for_linux = |asset: &&serde_norway::Value| {
        let targets: Vec<&str> = match asset.get("target") {
            Some(serde_norway::Value::String(t)) => vec![t.as_str()],
            Some(serde_norway::Value::Sequence(ts)) => ts.iter().filter_map(|t| t.as_str()).collect(),
            _ => vec![],
        };
        targets.iter().any(|t| *t == "linux_x64_gnu" || *t == "linux_x64")
    };
    let asset = assets.iter().find(for_linux)?;
    let file = asset.get("file")?.as_str()?;
    // `file` can name the archive and, after a colon, a directory in it.
    let file = file.split(':').next().unwrap_or(file).to_string();
    let inner = asset.get("bin").and_then(|b| b.as_str()).map(str::to_string);
    Some((file, inner))
}

/// The image a server's pod runs: what its install needs, else what its
/// language's tools need (rust-analyzer runs cargo, gopls runs go).
fn image_for(purl: &Purl, languages: &[String]) -> String {
    let by_install = match purl.kind.as_str() {
        "npm" => Some("node:22-slim"),
        "pypi" => Some("python:3-slim"),
        "golang" => Some("golang:1"),
        "cargo" => Some("rust:1"),
        _ => None,
    };
    let by_language = languages.iter().find_map(|l| match l.to_ascii_lowercase().as_str() {
        "rust" => Some("rust:1"),
        "go" => Some("golang:1"),
        "python" => Some("python:3-slim"),
        "typescript" | "javascript" => Some("node:22-slim"),
        _ => None,
    });
    let fallback = if purl.kind == "github" { "buildpack-deps:trixie-curl" } else { "debian:trixie-slim" };
    by_install.or(by_language).unwrap_or(fallback).to_string()
}

/// File types (extension → language id) and root markers: from the Helix
/// languages that list this server, else the ones mason names.
fn languages(
    helix: &toml::Value,
    server: &str,
    mason_languages: &[String],
) -> (std::collections::BTreeMap<String, String>, Vec<String>) {
    let all: Vec<&toml::Value> = helix
        .get("language")
        .and_then(toml::Value::as_array)
        .map(|l| l.iter().collect())
        .unwrap_or_default();
    let serves = |language: &&toml::Value| {
        language.get("language-servers").and_then(toml::Value::as_array).is_some_and(|servers| {
            servers.iter().any(|s| {
                s.as_str() == Some(server) || s.get("name").and_then(toml::Value::as_str) == Some(server)
            })
        })
    };
    let mut chosen: Vec<&toml::Value> = all.iter().copied().filter(serves).collect();
    if chosen.is_empty() {
        let wanted: Vec<String> = mason_languages.iter().map(|l| l.to_ascii_lowercase()).collect();
        chosen = all
            .iter()
            .copied()
            .filter(|l| l.get("name").and_then(toml::Value::as_str).is_some_and(|n| wanted.iter().any(|w| w == n)))
            .collect();
    }
    let mut file_types = std::collections::BTreeMap::new();
    let mut roots: Vec<String> = Vec::new();
    for language in chosen {
        let Some(name) = language.get("name").and_then(toml::Value::as_str) else { continue };
        let id = language.get("language-id").and_then(toml::Value::as_str).unwrap_or(name);
        for extension in language.get("file-types").and_then(toml::Value::as_array).into_iter().flatten() {
            // Globs (`{ glob = ... }`) name files, not extensions.
            if let Some(extension) = extension.as_str() {
                file_types.entry(extension.to_string()).or_insert_with(|| id.to_string());
            }
        }
        for root in language.get("roots").and_then(toml::Value::as_array).into_iter().flatten() {
            if let Some(root) = root.as_str()
                && !roots.iter().any(|r| r == root)
            {
                roots.push(root.to_string());
            }
        }
    }
    (file_types, roots)
}

/// Merges `extra` into `base`, object by object.
fn merge(base: &mut serde_json::Value, extra: &serde_json::Value) {
    match (base, extra) {
        (serde_json::Value::Object(base), serde_json::Value::Object(extra)) => {
            for (key, value) in extra {
                merge(base.entry(key.clone()).or_insert(serde_json::Value::Null), value);
            }
        }
        (base, extra) => *base = extra.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for mason's registry and Helix's file, on a local port.
    async fn stand_in() -> String {
        let router = axum::Router::new()
            .route(
                "/packages/pyright/package.yaml",
                axum::routing::get(|| async { include_str!("fixtures/mason-pyright.yaml") }),
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

    const HELIX: &str = include_str!("fixtures/helix-languages-excerpt.toml");

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
}
