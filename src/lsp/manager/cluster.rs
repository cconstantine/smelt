use super::*;
use crate::lsp::pods::tests::cluster::with_sandbox;

const HELIX: &str = include_str!("../fixtures/helix-languages-excerpt.toml");

async fn configure(pool: &PgPool, package_yaml: &str) -> LanguageServerConfig {
    let mut config = crate::lsp::catalog::suggest(package_yaml, HELIX).expect("a suggestion").config;
    config.memory_limit = "2Gi".to_string();
    crate::lsp::config::save(pool, None, &config).await.expect("saved");
    config
}

/// Writes `files` into the sandbox's workspace, their project (the
/// directory under `/workspace`) writable by the server.
async fn write_files(client: &kube::Client, sandbox: &SandboxRef, files: &[(&str, &str)]) {
    for (path, content) in files {
        let script = r#"mkdir -p "$(dirname "$1")" && cat > "$1" && chmod -R a+rwX "/workspace/$(echo "$1" | cut -d/ -f3)""#;
        let result = crate::sandbox::exec_with(client, &sandbox.pod_name, "sandbox", &["sh", "-c", script, "_", path], Some(content.as_bytes()))
            .await
            .expect("exec");
        assert_eq!(result.exit_code, 0, "{}", result.stderr);
    }
}

async fn read(client: &kube::Client, sandbox: &SandboxRef, path: &str) -> String {
    crate::sandbox::exec_with(client, &sandbox.pod_name, "sandbox", &["cat", path], None).await.expect("exec").stdout
}

fn at(path: &str, line: u32, character: u32) -> OperationInput {
    OperationInput { path: Some(path.to_string()), line: Some(line), character: Some(character), ..Default::default() }
}

/// Runs `operation` until its answer contains `expected` (the server
/// may still be indexing), for up to three minutes.
async fn until_contains(
    pool: &PgPool,
    client: &kube::Client,
    sandbox: &SandboxRef,
    operation: Operation,
    input: &OperationInput,
    expected: &str,
) -> String {
    let mut last = Err(String::new());
    for _ in 0..60 {
        last = operate(pool, client, sandbox, operation, input).await;
        if last.as_ref().is_ok_and(|answer| answer.contains(expected)) {
            return last.unwrap();
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    panic!("{} never answered with {expected:?}; last: {last:?}", operation.name());
}

const MAIN: &str = "/workspace/demo/src/main.rs";
const SHAPES: &str = "/workspace/demo/src/shapes.rs";
const MAIN_RS: &str = "mod shapes;
use shapes::{Shape, Square};

fn total(shapes: &[&dyn Shape]) -> f64 {
    shapes.iter().map(|s| s.area()).sum()
}

fn main() {
    let square = Square { side: 2.0 };
    println!(\"{}\", total(&[&square]));
}
";
const SHAPES_RS: &str = "pub trait Shape {
    fn area(&self) -> f64;
}

pub struct Square {
    pub side: f64,
}

impl Shape for Square {
    fn area(&self) -> f64 {
        self.side * self.side
    }
}
";

/// rust-analyzer, installed as the catalog suggests: navigation across
/// files, calls, diagnostics (after an edit too), a rename's edits, and
/// a session that comes back after its server process dies.
#[sqlx::test]
async fn test_rust_analyzer_from_the_catalog_answers_across_files(pool: PgPool) {
    let config = configure(&pool, include_str!("../fixtures/mason-rust-analyzer.yaml")).await;
    with_sandbox(|client, sandbox| async move {
        write_files(&client, &sandbox, &[
            ("/workspace/demo/Cargo.toml", "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
            (MAIN, MAIN_RS),
            (SHAPES, SHAPES_RS),
        ])
        .await;

        let started = start(&pool, &client, &sandbox, &config.name).await.expect("started");
        assert!(started.contains("is running"), "{started}");
        let listed = servers_summary(&pool, &client, Some(&sandbox)).await.expect("summary");
        assert!(listed.contains("rust-analyzer: .rs (running)"), "{listed}");

        // `Square` in main.rs is defined in shapes.rs.
        let found = until_contains(&pool, &client, &sandbox, Operation::Definition, &at(MAIN, 9, 18), "shapes.rs").await;
        assert!(found.contains("pub struct Square"), "{found}");

        let refs = until_contains(&pool, &client, &sandbox, Operation::References, &at(SHAPES, 2, 8), "main.rs").await;
        assert!(refs.contains("shapes.rs:10:"), "{refs}");

        until_contains(&pool, &client, &sandbox, Operation::Implementation, &at(SHAPES, 1, 11), "impl Shape for Square").await;

        until_contains(&pool, &client, &sandbox, Operation::IncomingCalls, &at(MAIN, 4, 4), "main.rs").await;

        // A type error, seen by diagnostics, and after an edit.
        let broken = MAIN_RS.replace("let square", "let wrong: i32 = \"no\";\n    let square");
        write_files(&client, &sandbox, &[(MAIN, &broken)]).await;
        let problems = until_contains(&pool, &client, &sandbox, Operation::Diagnostics, &at(MAIN, 1, 1), "error").await;
        assert!(problems.contains("let wrong"), "the problem's line is shown: {problems}");
        let after_edit = edit_diagnostics(&[config.clone()], &client, &sandbox, MAIN).await.expect("a running server answers");
        assert!(after_edit.starts_with("rust-analyzer"), "{after_edit}");
        write_files(&client, &sandbox, &[(MAIN, MAIN_RS)]).await;

        // A rename edits both files.
        let renamed = operate(&pool, &client, &sandbox, Operation::Rename, &OperationInput { new_name: Some("sum_areas".to_string()), ..at(MAIN, 4, 4) })
            .await
            .expect("rename");
        assert!(renamed.contains("Renamed to sum_areas"), "{renamed}");
        let main = read(&client, &sandbox, MAIN).await;
        assert!(main.contains("fn sum_areas(") && main.contains("sum_areas(&[&square])"), "{main}");

        // The server process dies; the next request reconnects.
        let pod_name = pods::server_pod_name(sandbox.pod_id, &config.name);
        let killed = crate::sandbox::exec_with(&client, &pod_name, "server", &["sh", "-c", "kill $(pidof rust-analyzer)"], None).await.expect("kill");
        assert_eq!(killed.exit_code, 0, "{}", killed.stderr);
        let session = SESSIONS.lock().unwrap().get(&pod_name).cloned().expect("a session");
        for _ in 0..50 {
            if session.is_closed() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(session.is_closed(), "the session sees its server die");
        until_contains(&pool, &client, &sandbox, Operation::Definition, &at(MAIN, 9, 18), "shapes.rs").await;
    })
    .await;
}

/// A server that can't be connected to isn't promised: after a failed
/// attempt, an edit says why rather than "connecting" again.
#[tokio::test]
async fn test_an_edit_says_when_a_server_cannot_connect() {
    with_sandbox(|client, sandbox| async move {
        let config = LanguageServerConfig {
            name: "gone".to_string(),
            image: "debian:trixie-slim".to_string(),
            command: "false".to_string(),
            file_types: [("txt".to_string(), "plaintext".to_string())].into(),
            memory_limit: "128Mi".to_string(),
            cpu_limit: "1".to_string(),
            enabled: true,
            ..Default::default()
        };
        pods::start_with(&client, &sandbox, &config, "v1").await.expect("start");
        let path = "/workspace/notes/a.txt";
        write_files(&client, &sandbox, &[(path, "hello\n")]).await;
        let first = edit_diagnostics(&[config.clone()], &client, &sandbox, path).await.expect("a note");
        assert!(first.contains("gone is connecting"), "{first}");
        tokio::time::sleep(Duration::from_secs(5)).await;
        let second = edit_diagnostics(&[config], &client, &sandbox, path).await.expect("a note");
        assert!(second.contains("gone couldn't connect"), "{second}");
    })
    .await;
}

/// Starting a server that's already running gives it a fresh reconnect
/// budget: that's what the error for a spent budget tells the model
/// to do, and the pod itself is still ready.
#[sqlx::test]
async fn test_starting_a_running_server_resets_its_reconnects(pool: PgPool) {
    let config = LanguageServerConfig {
        name: "budget".to_string(),
        image: "debian:trixie-slim".to_string(),
        command: "cat".to_string(),
        file_types: [("txt".to_string(), "plaintext".to_string())].into(),
        memory_limit: "128Mi".to_string(),
        cpu_limit: "1".to_string(),
        enabled: true,
        ..Default::default()
    };
    crate::lsp::config::save(&pool, None, &config).await.expect("saved");
    with_sandbox(|client, sandbox| async move {
        start(&pool, &client, &sandbox, "budget").await.expect("started");
        let pod_name = pods::server_pod_name(sandbox.pod_id, "budget");
        OPENED.lock().unwrap().insert(pod_name.clone(), vec![Instant::now(); RECONNECTS]);
        let again = start(&pool, &client, &sandbox, "budget").await.expect("started again");
        assert!(again.contains("already running"), "{again}");
        let spent = OPENED.lock().unwrap().get(&pod_name).map_or(0, Vec::len);
        assert_eq!(spent, 0, "the reconnect budget should be fresh");
    })
    .await;
}

/// An edit never waits for a server to start talking: with no session
/// open, it comes back at once saying the server is connecting, even
/// with a server that never answers `initialize`.
#[tokio::test]
async fn test_an_edit_does_not_wait_for_a_server_to_connect() {
    with_sandbox(|client, sandbox| async move {
        let config = LanguageServerConfig {
            name: "silent".to_string(),
            image: "debian:trixie-slim".to_string(),
            command: "sh".to_string(),
            args: vec!["-c".to_string(), "cat > /dev/null".to_string()],
            file_types: [("txt".to_string(), "plaintext".to_string())].into(),
            memory_limit: "128Mi".to_string(),
            cpu_limit: "1".to_string(),
            enabled: true,
            ..Default::default()
        };
        pods::start_with(&client, &sandbox, &config, "v1").await.expect("start");
        let path = "/workspace/notes/a.txt";
        write_files(&client, &sandbox, &[(path, "hello\n")]).await;
        let started = Instant::now();
        let found = edit_diagnostics(&[config], &client, &sandbox, path).await;
        assert!(started.elapsed() < Duration::from_secs(10), "the edit waited {:?}", started.elapsed());
        let found = found.expect("a note");
        assert!(found.contains("silent is connecting"), "{found}");
    })
    .await;
}

/// pyright, installed with npm as the catalog suggests: a definition
/// in another module, and a type error.
#[sqlx::test]
async fn test_pyright_from_the_catalog_answers(pool: PgPool) {
    let config = configure(&pool, include_str!("../fixtures/mason-pyright.yaml")).await;
    with_sandbox(|client, sandbox| async move {
        let app = "/workspace/py/app.py";
        write_files(&client, &sandbox, &[
            ("/workspace/py/pyproject.toml", "[project]\nname = \"py\"\n"),
            ("/workspace/py/shapes.py", "def area(side: float) -> float:\n    return side * side\n"),
            (app, "from shapes import area\n\nprint(area(2.0))\nwrong: int = \"no\"\n"),
        ])
        .await;
        start(&pool, &client, &sandbox, &config.name).await.expect("started");

        let found = until_contains(&pool, &client, &sandbox, Operation::Definition, &at(app, 3, 7), "shapes.py").await;
        assert!(found.contains("def area"), "{found}");
        let problems = until_contains(&pool, &client, &sandbox, Operation::Diagnostics, &at(app, 1, 1), "error").await;
        assert!(problems.contains("wrong: int"), "{problems}");
    })
    .await;
}

/// gopls, installed with `go install` as the catalog suggests: a
/// definition in another file of the package.
#[sqlx::test]
async fn test_gopls_from_the_catalog_answers(pool: PgPool) {
    let config = configure(&pool, include_str!("../fixtures/mason-gopls.yaml")).await;
    with_sandbox(|client, sandbox| async move {
        let main = "/workspace/hello/main.go";
        write_files(&client, &sandbox, &[
            ("/workspace/hello/go.mod", "module hello\n\ngo 1.21\n"),
            ("/workspace/hello/area.go", "package main\n\nfunc area(side float64) float64 {\n\treturn side * side\n}\n"),
            (main, "package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(area(2))\n}\n"),
        ])
        .await;
        start(&pool, &client, &sandbox, &config.name).await.expect("started");
        let found = until_contains(&pool, &client, &sandbox, Operation::Definition, &at(main, 6, 14), "area.go").await;
        assert!(found.contains("func area"), "{found}");
    })
    .await;
}
