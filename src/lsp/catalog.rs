//! The catalog lookup (SME-35): a new language server's settings, suggested
//! from mason's registry (how to install it) and Helix's languages.toml
//! (how to run it, and which files it takes). Pure functions over the
//! fetched text; the page shows the result for the user to check and edit.

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

    let (install_command, command) = install(&purl, source, &bin, &mut notes);
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

/// Where the catalog's two files come from. `SMELT_MASON_REGISTRY_URL` and
/// `SMELT_HELIX_LANGUAGES_URL` point elsewhere (tests use a stand-in).
pub fn sources_from_env() -> (String, String) {
    let registry = std::env::var("SMELT_MASON_REGISTRY_URL")
        .unwrap_or_else(|_| "https://raw.githubusercontent.com/mason-org/mason-registry/main".to_string());
    let helix = std::env::var("SMELT_HELIX_LANGUAGES_URL")
        .unwrap_or_else(|_| "https://raw.githubusercontent.com/helix-editor/helix/master/languages.toml".to_string());
    (registry, helix)
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

/// The install command and the path it leaves the executable at.
fn install(purl: &Purl, source: &Source, bin: &str, notes: &mut Vec<String>) -> (String, String) {
    let home = POD_HOME;
    let extras = source.extra_packages.join(" ");
    let with_extras = |base: String| if extras.is_empty() { base } else { format!("{base} {extras}") };
    match purl.kind.as_str() {
        "npm" => (
            with_extras(format!("npm install --prefix {home}/lsp {}@{}", purl.path, purl.version)),
            format!("{home}/lsp/node_modules/.bin/{bin}"),
        ),
        "pypi" => (
            with_extras(format!("python -m venv {home}/venv && {home}/venv/bin/pip install {}=={}", purl.path, purl.version)),
            format!("{home}/venv/bin/{bin}"),
        ),
        // GOBIN: Go's image sets GOPATH to /go, so without it the binary
        // lands in /go/bin.
        "golang" => (format!("GOBIN={home}/go/bin go install {}@{}", purl.path, purl.version), format!("{home}/go/bin/{bin}")),
        "cargo" => (
            format!("cargo install {} --version {} --root {home}/cargo", purl.name(), purl.version),
            format!("{home}/cargo/bin/{bin}"),
        ),
        "github" => match github_asset(source) {
            Some((file, inner)) => {
                let file = file.replace("{{version}}", &purl.version);
                if file.contains("{{") {
                    notes.push(format!("The release asset's name ({file}) has a template smelt doesn't fill in: check the install command."));
                }
                let url = format!("https://github.com/{}/releases/download/{}/{file}", purl.path, purl.version);
                let target = format!("{home}/bin/{bin}");
                let inner = inner.unwrap_or_else(|| bin.to_string());
                let command = if file.ends_with(".tar.gz") || file.ends_with(".tgz") {
                    format!("mkdir -p {home}/bin {home}/opt && curl -fsSL {url} | tar -xz -C {home}/opt && ln -sf {home}/opt/{inner} {target}")
                } else if file.ends_with(".tar.xz") {
                    format!("mkdir -p {home}/bin {home}/opt && curl -fsSL {url} | tar -xJ -C {home}/opt && ln -sf {home}/opt/{inner} {target}")
                } else if file.ends_with(".zip") {
                    notes.push("The release is a .zip: the image needs unzip.".to_string());
                    format!("mkdir -p {home}/bin {home}/opt && curl -fsSL -o {home}/release.zip {url} && unzip -q {home}/release.zip -d {home}/opt && ln -sf {home}/opt/{inner} {target}")
                } else if file.ends_with(".gz") {
                    format!("mkdir -p {home}/bin && curl -fsSL {url} | gunzip > {target} && chmod +x {target}")
                } else {
                    format!("mkdir -p {home}/bin && curl -fsSL -o {target} {url} && chmod +x {target}")
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
    }
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
}
