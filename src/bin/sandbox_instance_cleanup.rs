//! Deletes the sandbox pods and claims a check server's scratch database
//! made, once that server is gone (SME-129).
//!
//! `scripts/check-server stop` runs this before it drops the scratch
//! database. The database's own `smelt_instance` row is the only record of
//! the instance id its server labelled every pod and claim it made with, and
//! once the database is dropped no server ever owns those objects again, so
//! they stay in the shared namespace for good (SME-126 left one behind).
//! This reads that row, deletes exactly that instance's objects — pods
//! first, then their claims — and exits non-zero while any of them is left,
//! so `stop` keeps the database and a later `stop` retries.
//!
//! Standalone rather than reusing `src/sandbox`: this crate has no `lib.rs`,
//! so a `src/bin/*.rs` binary is its own crate root and can't reach
//! `main.rs`'s modules (the same reason `sandbox_image_import.rs` is
//! standalone), so the few constants and names it needs — the namespace, the
//! `smelt/instance` label, the scratch database's name prefix — are copied
//! here, each next to a note naming its home.
//!
//! Usage:
//!
//! ```text
//! sandbox_instance_cleanup --database-url <scratch url> --dev-database-url <dev url> [--dry-run]
//! sandbox_instance_cleanup --list
//! ```
//!
//! Four guards refuse a `--database-url` that isn't a check server's scratch
//! database (exit status 2, deleting nothing): its name must start with
//! `smelt_scratch_`, it must not be `--dev-database-url`'s database, its
//! `smelt_instance` row must not own the objects made before SME-115
//! (`owns_unlabelled`), and its instance id must be a UUID. A fifth refuses
//! while anything else is still connected to it. Nothing without the exact
//! `smelt/instance` label is ever deleted.
//!
//! `--dry-run` does the same reads and lists what it would delete, deleting
//! nothing. `--list` connects to no database and is read-only: it prints the
//! namespace's pods and claims with their instance label, uid and age,
//! grouped by instance, which is how a stranded instance — one whose
//! database is gone — is found. Nothing is ever deleted without a database
//! to name the instance.
//!
//! The kube client talks to the same cluster `KUBECONFIG` names (or the
//! in-cluster config), with a connect and read timeout, so an unreachable
//! cluster fails the run rather than hanging `stop`. It reads no stdin.

use std::error::Error;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
use k8s_openapi::jiff::{SignedDuration, Timestamp};
use kube::api::{Api, DeleteParams, ListParams, ObjectMeta, Preconditions};
use kube::Resource;
use sqlx::postgres::PgConnectOptions;
use sqlx::{Connection, PgConnection, Row};
use tokio::time::sleep;

type BoxError = Box<dyn Error + Send + Sync>;

/// The namespace every smelt server shares, `src/sandbox/mod.rs`'s own
/// `NAMESPACE`: the tests' namespace under `cfg(test)`, so a test can never
/// reach a running server's objects.
#[cfg(not(test))]
const NAMESPACE: &str = "smelt-park";
#[cfg(test)]
const NAMESPACE: &str = "smelt-park-test";

/// `src/sandbox/spec.rs`'s `INSTANCE_LABEL`: the label naming the database an
/// object belongs to, carrying its `smelt_instance` id.
const INSTANCE_LABEL: &str = "smelt/instance";

/// The prefix `scripts/check-server`'s `scratch_db_url` gives a scratch
/// database's name.
const SCRATCH_PREFIX: &str = "smelt_scratch_";

/// How long the tool waits for the pods it deleted to disappear, and then
/// for their claims. A pod deleted with grace 0 goes at once; a claim waits
/// for `pvc-protection` to release it once its pod is gone.
const PODS_GONE_TIMEOUT: Duration = Duration::from_secs(120);
const CLAIMS_GONE_TIMEOUT: Duration = Duration::from_secs(60);

/// The cluster client's connect and read timeouts: an unreachable cluster
/// fails the run instead of hanging the `stop` that called it.
const CLIENT_TIMEOUT: Duration = Duration::from_secs(30);

const USAGE: &str = "usage: sandbox_instance_cleanup --database-url <scratch url> --dev-database-url <dev url> [--dry-run]\n       sandbox_instance_cleanup --list";

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("sandbox_instance_cleanup: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<ExitCode, BoxError> {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(why) => {
            eprintln!("sandbox_instance_cleanup: {why}\n{USAGE}");
            return Ok(ExitCode::from(2));
        }
    };
    // kube's rustls-tls stack only installs a default CryptoProvider when
    // built with aws-lc-rs; this project uses ring, so nothing else does
    // (the same call `sandbox_image_import.rs` makes).
    rustls::crypto::ring::default_provider().install_default().ok();
    let client = cluster_client().await?;
    if args.list {
        list_objects(&client).await?;
        return Ok(ExitCode::SUCCESS);
    }
    cleanup(&client, &args).await
}

// --- Arguments ---

/// The parsed command line. `database_url` and `dev_database_url` are
/// required unless `list` is set.
#[derive(Debug, Default, PartialEq)]
struct Args {
    database_url: Option<String>,
    dev_database_url: Option<String>,
    dry_run: bool,
    list: bool,
}

/// The line's own flags. `--list` takes no others: it opens no database, so
/// the URLs it isn't given can't be checked.
fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--database-url" => parsed.database_url = Some(value_for(&arg, &mut args)?),
            "--dev-database-url" => parsed.dev_database_url = Some(value_for(&arg, &mut args)?),
            "--dry-run" => parsed.dry_run = true,
            "--list" => parsed.list = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if parsed.list {
        if parsed.database_url.is_some() || parsed.dev_database_url.is_some() || parsed.dry_run {
            return Err("--list takes no other arguments".to_string());
        }
        return Ok(parsed);
    }
    if parsed.database_url.is_none() {
        return Err("--database-url is required".to_string());
    }
    if parsed.dev_database_url.is_none() {
        return Err("--dev-database-url is required".to_string());
    }
    Ok(parsed)
}

/// `flag`'s value, the next argument; an error naming `flag` when there is
/// none.
fn value_for(flag: &str, args: &mut impl Iterator<Item = String>) -> Result<String, String> {
    args.next().ok_or_else(|| format!("{flag} needs a value"))
}

// --- The target: the scratch database and its instance ---

/// The database name in `url` (`postgres://user@host/name?query`), or
/// `None` when it names none.
fn database_name(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let path = after_scheme.split(['?', '#']).next()?;
    // No `/` after the authority: the URL names no database.
    let name = path.rsplit_once('/')?.1;
    (!name.is_empty()).then(|| name.to_string())
}

/// Guards 1 and 2: `--database-url` must name a `smelt_scratch_*` database
/// that isn't `--dev-database-url`'s. The scratch database's name is what
/// both of `check-server`'s callers key the scratch lock on, and a name that
/// isn't a check server's is never this tool's to clean up. The names only,
/// never the URLs (they may carry a password).
fn scratch_database(scratch_url: &str, dev_url: &str) -> Result<String, String> {
    let Some(name) = database_name(scratch_url) else {
        return Err("refusing: --database-url names no database".to_string());
    };
    if !name.starts_with(SCRATCH_PREFIX) {
        return Err(format!(
            "refusing to clean up the database {name}: it doesn't start with {SCRATCH_PREFIX}, so it isn't a check server's scratch database"
        ));
    }
    if database_name(dev_url).as_deref() == Some(name.as_str()) {
        return Err(format!(
            "refusing to clean up {name}: it is --dev-database-url's database"
        ));
    }
    Ok(name)
}

/// Guards 1 to 4, over what the scratch database's `smelt_instance` row
/// says. A scratch database migrates empty, so `owns_unlabelled` is false
/// there; the dev and production databases had conversations when they
/// migrated, so it is true there (SME-115), which also refuses a copy of one
/// renamed `smelt_scratch_*`.
fn check_target(
    scratch_url: &str,
    dev_url: &str,
    instance: &str,
    owns_unlabelled: bool,
) -> Result<(), String> {
    let name = scratch_database(scratch_url, dev_url)?;
    if owns_unlabelled {
        return Err(format!(
            "refusing to clean up {name}: its smelt_instance row owns the objects made before SME-115 (owns_unlabelled = true)"
        ));
    }
    if !is_uuid(instance) {
        return Err(format!(
            "refusing to clean up {name}: its instance id {instance:?} isn't a UUID, so no selector can be built from it"
        ));
    }
    Ok(())
}

/// Whether `id` has a UUID's shape, lower-case 8-4-4-4-12 hex, as Postgres
/// prints one. `uuid` isn't a dependency of this crate, and only this check
/// needs it.
fn is_uuid(id: &str) -> bool {
    const LENGTHS: [usize; 5] = [8, 4, 4, 4, 12];
    let parts: Vec<&str> = id.split('-').collect();
    parts.len() == LENGTHS.len()
        && parts.iter().zip(LENGTHS).all(|(part, length)| {
            part.len() == length
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

/// What reading the scratch database said. A database that isn't there is
/// `open_scratch`'s `None`, not a state here: nothing can be read from it.
#[derive(Debug, PartialEq)]
enum ScratchState {
    /// It exists but has no `smelt_instance` table: the server never
    /// migrated, so nothing can carry its instance.
    Unmigrated,
    /// Its `smelt_instance` row.
    Instance { id: String, owns_unlabelled: bool },
}

/// Connects to the scratch database, or `None` when it isn't there. A
/// database `stop` already dropped is the ordinary retry case (its objects
/// are `--list`'s business then), not a failure.
async fn open_scratch(options: &PgConnectOptions) -> Result<Option<PgConnection>, BoxError> {
    match PgConnection::connect_with(options).await {
        Ok(conn) => Ok(Some(conn)),
        Err(error) if missing_database(&error) => Ok(None),
        Err(error) => Err(format!("connecting to the scratch database: {error}").into()),
    }
}

/// Whether `error` is Postgres refusing a database that isn't there
/// (`3D000`, "database ... does not exist").
fn missing_database(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(e) if e.code().as_deref() == Some("3D000"))
}

/// The database's `smelt_instance` row (the query `db::smelt_instance`
/// runs), or that it was never migrated. Reads only.
async fn read_instance(conn: &mut PgConnection) -> Result<ScratchState, BoxError> {
    let row = sqlx::query("SELECT instance_id::text, owns_unlabelled FROM smelt_instance")
        .fetch_optional(&mut *conn)
        .await;
    match row {
        Ok(Some(row)) => Ok(ScratchState::Instance {
            id: row.try_get::<String, _>(0)?,
            owns_unlabelled: row.try_get::<bool, _>(1)?,
        }),
        Ok(None) => Ok(ScratchState::Unmigrated),
        // 42P01: there is no `smelt_instance` table.
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("42P01") => {
            Ok(ScratchState::Unmigrated)
        }
        Err(error) => Err(format!("reading smelt_instance: {error}").into()),
    }
}

/// Guard 5: the pids of every other backend connected to `database`. A live
/// check server could make new objects between this tool's list and its
/// deletes, so its absence is checked rather than assumed.
async fn other_connections(conn: &mut PgConnection, database: &str) -> Result<Vec<i32>, BoxError> {
    let rows: Vec<(i32,)> = sqlx::query_as(
        "SELECT pid FROM pg_stat_activity WHERE datname = $1 AND pid <> pg_backend_pid()",
    )
    .bind(database)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().map(|(pid,)| pid).collect())
}

// --- Selecting and deleting ---

/// The name and uid of each object in `listed` labelled exactly `instance`,
/// in name order. The label must match exactly (the same check
/// `sandbox::spec::ownership` makes): an unlabelled object, another
/// database's, or an empty or merely similar label is never this tool's to
/// delete, whatever selector listed it. An object with no uid is skipped:
/// its delete can't be held to the object that was listed.
fn to_delete(listed: &[ObjectMeta], instance: &str) -> Vec<(String, String)> {
    // An empty instance (never read) is nobody's: no selector can be built
    // from it, and it must never match an object with an empty label.
    if instance.is_empty() {
        return Vec::new();
    }
    let mut chosen: Vec<(String, String)> = listed
        .iter()
        .filter(|meta| {
            meta.labels
                .as_ref()
                .and_then(|labels| labels.get(INSTANCE_LABEL))
                .is_some_and(|label| label == instance)
        })
        .filter_map(|meta| Some((meta.name.clone()?, meta.uid.clone()?)))
        .collect();
    chosen.sort();
    chosen
}

/// The metadata of `items`, for `to_delete`.
fn metas<K: Resource>(items: &[K]) -> Vec<ObjectMeta> {
    items.iter().map(|item| item.meta().clone()).collect()
}

/// The objects of `instance` in `api`: listed with the instance selector,
/// then checked again object by object (`to_delete`).
async fn list_instance<K>(api: &Api<K>, instance: &str) -> Result<Vec<ObjectMeta>, BoxError>
where
    K: Resource<DynamicType = ()> + serde::de::DeserializeOwned + Clone + std::fmt::Debug,
{
    let selector = format!("{INSTANCE_LABEL}={instance}");
    let listed = api.list(&ListParams::default().labels(&selector)).await?;
    Ok(metas(&listed.items))
}

/// What deleting one kind left behind: the uids the API accepted a delete
/// for, and the names it couldn't delete (made again under the same name
/// since the listing, or a failed delete).
#[derive(Debug, Default, PartialEq)]
struct Deleted {
    uids: Vec<String>,
    left: Vec<String>,
}

/// Deletes each `(name, uid)`, held to the uid it was listed with, so an
/// object made again under the same name is left alone. Prints each one it
/// deletes. A 404 counts as gone; a 409 (the uid precondition failed) is
/// skipped, and the next listing sees the new object if it carries the
/// instance.
async fn delete_each<K>(api: &Api<K>, kind: &str, targets: &[(String, String)]) -> Deleted
where
    K: Resource<DynamicType = ()> + serde::de::DeserializeOwned + Clone + std::fmt::Debug,
{
    let mut deleted = Deleted::default();
    for (name, uid) in targets {
        let params = DeleteParams {
            preconditions: Some(Preconditions {
                uid: Some(uid.clone()),
                resource_version: None,
            }),
            // A sandbox pod is disposable; a claim's own grace (if any)
            // still applies.
            grace_period_seconds: (kind == "pod").then_some(0),
            ..DeleteParams::default()
        };
        match api.delete(name, &params).await {
            Ok(_) => {
                println!("deleted {kind} {name} uid={uid}");
                deleted.uids.push(uid.clone());
            }
            Err(kube::Error::Api(e)) if e.code == 404 => {
                println!("{kind} {name} was already gone");
            }
            Err(kube::Error::Api(e)) if e.code == 409 => {
                eprintln!(
                    "{kind} {name} was made again under the same name since it was listed; left alone"
                );
                deleted.left.push(name.clone());
            }
            Err(error) => {
                eprintln!("couldn't delete {kind} {name}: {error}");
                deleted.left.push(name.clone());
            }
        }
    }
    deleted
}

/// Waits until the instance's selector lists none of `uids`, or `timeout`
/// passes. An object made again under one of those names has a new uid and
/// so doesn't hold this wait: it is one the next run sees.
async fn wait_gone<K>(
    api: &Api<K>,
    instance: &str,
    uids: &[String],
    timeout: Duration,
) -> Result<bool, BoxError>
where
    K: Resource<DynamicType = ()> + serde::de::DeserializeOwned + Clone + std::fmt::Debug,
{
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = list_instance(api, instance).await?.into_iter().any(|meta| {
            meta.uid
                .as_ref()
                .is_some_and(|uid| uids.iter().any(|gone| gone == uid))
        });
        if !remaining {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        sleep(Duration::from_millis(500)).await;
    }
}

/// Deletes `instance`'s pods, waits for them, then its claims, and waits for
/// those. `Ok(false)` when something is left (a delete was skipped, or a
/// wait timed out), so the caller keeps the database for a retry. No claim
/// is deleted while a pod that carried the instance may still be there: one
/// deleted under a still-running pod is SME-134's bug.
async fn delete_instance(
    client: &kube::Client,
    instance: &str,
    dry_run: bool,
) -> Result<bool, BoxError> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), NAMESPACE);
    let claims: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), NAMESPACE);

    let pod_targets = to_delete(&list_instance(&pods, instance).await?, instance);
    let claim_targets = to_delete(&list_instance(&claims, instance).await?, instance);
    if dry_run {
        for (name, uid) in &pod_targets {
            println!("would delete pod {name} uid={uid}");
        }
        for (name, uid) in &claim_targets {
            println!("would delete claim {name} uid={uid}");
        }
        return Ok(true);
    }

    let pods_deleted = delete_each(&pods, "pod", &pod_targets).await;
    if !pods_deleted.left.is_empty() {
        eprintln!(
            "{} of the instance's pods couldn't be deleted; deleting no claims",
            pods_deleted.left.len()
        );
        return Ok(false);
    }
    if !wait_gone(&pods, instance, &pods_deleted.uids, PODS_GONE_TIMEOUT).await? {
        eprintln!(
            "some of the instance's pods are still there after {}s; deleting no claims",
            PODS_GONE_TIMEOUT.as_secs()
        );
        return Ok(false);
    }

    let claims_deleted = delete_each(&claims, "claim", &claim_targets).await;
    if !claims_deleted.left.is_empty() {
        eprintln!(
            "{} of the instance's claims couldn't be deleted",
            claims_deleted.left.len()
        );
        return Ok(false);
    }
    if !wait_gone(&claims, instance, &claims_deleted.uids, CLAIMS_GONE_TIMEOUT).await? {
        eprintln!(
            "some of the instance's claims are still there after {}s",
            CLAIMS_GONE_TIMEOUT.as_secs()
        );
        return Ok(false);
    }

    // Nothing is left: the same selector that found them lists none. A
    // leftover here would be an object made again since the deletes.
    let left =
        list_instance(&pods, instance).await?.len() + list_instance(&claims, instance).await?.len();
    if left > 0 {
        eprintln!("{left} object(s) labelled {INSTANCE_LABEL}={instance} are still there");
        return Ok(false);
    }
    Ok(true)
}

/// The whole cleanup: the guards, the read, and the deletes.
async fn cleanup(client: &kube::Client, args: &Args) -> Result<ExitCode, BoxError> {
    let scratch_url = args.database_url.as_deref().unwrap_or_default();
    let dev_url = args.dev_database_url.as_deref().unwrap_or_default();

    // Guards 1 and 2 first, before anything connects: a wrong target is
    // refused without touching any database.
    let database = match scratch_database(scratch_url, dev_url) {
        Ok(database) => database,
        Err(why) => {
            eprintln!("sandbox_instance_cleanup: {why}");
            return Ok(ExitCode::from(2));
        }
    };

    let options: PgConnectOptions = match scratch_url.parse() {
        Ok(options) => options,
        Err(error) => {
            return Err(format!("--database-url isn't a connection string: {error}").into());
        }
    };
    let Some(mut conn) = open_scratch(&options).await? else {
        println!("{database} doesn't exist: its objects, if any, can't be identified; see --list");
        return Ok(ExitCode::SUCCESS);
    };
    let state = read_instance(&mut conn).await?;
    let (instance, owns_unlabelled) = match state {
        ScratchState::Unmigrated => {
            println!(
                "{database} was never migrated, so nothing carries its instance; nothing to delete"
            );
            conn.close().await.ok();
            return Ok(ExitCode::SUCCESS);
        }
        ScratchState::Instance {
            id,
            owns_unlabelled,
        } => (id, owns_unlabelled),
    };
    // Guards 3 and 4.
    if let Err(why) = check_target(scratch_url, dev_url, &instance, owns_unlabelled) {
        eprintln!("sandbox_instance_cleanup: {why}");
        conn.close().await.ok();
        return Ok(ExitCode::from(2));
    }
    // Guard 5, while this tool's own connection is the only one it can
    // exclude.
    let others = other_connections(&mut conn, &database).await?;
    conn.close().await.ok();
    if !others.is_empty() {
        let pids = others
            .iter()
            .map(|pid| pid.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let line =
            format!("{database} has other open connections (pids {pids}): a server may still be running");
        if args.dry_run {
            println!("{line}");
        } else {
            eprintln!("sandbox_instance_cleanup: refusing: {line}");
            return Ok(ExitCode::from(2));
        }
    }

    let note = if args.dry_run { ", dry run" } else { "" };
    println!("cleaning up the objects of instance {instance} ({database}{note})");
    if delete_instance(client, &instance, args.dry_run).await? {
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!(
            "the instance's objects weren't all deleted; the database is kept so a later stop can retry"
        );
        Ok(ExitCode::FAILURE)
    }
}

// --- --list ---

/// Prints the namespace's pods and claims, grouped by instance label, as
/// `<name> uid=<uid> age=<age>`. Read-only, and opens no database: a
/// stranded instance has none to open.
async fn list_objects(client: &kube::Client) -> Result<(), BoxError> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), NAMESPACE);
    let claims: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), NAMESPACE);
    let now = Timestamp::now();
    let listed_pods = metas(&pods.list(&ListParams::default()).await?.items);
    let listed_claims = metas(&claims.list(&ListParams::default()).await?.items);
    println!("pods in {NAMESPACE}:");
    print_groups(&listed_pods, now);
    println!("claims in {NAMESPACE}:");
    print_groups(&listed_claims, now);
    Ok(())
}

/// One group per `smelt/instance` label (or its absence), each object's name,
/// uid and age under it, instances in name order.
fn print_groups(listed: &[ObjectMeta], now: Timestamp) {
    let mut groups: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
    for meta in listed {
        let instance = meta
            .labels
            .as_ref()
            .and_then(|labels| labels.get(INSTANCE_LABEL))
            .cloned()
            .unwrap_or_else(|| "(no smelt/instance label)".to_string());
        let name = meta.name.clone().unwrap_or_default();
        let uid = meta.uid.clone().unwrap_or_default();
        let age = meta
            .creation_timestamp
            .as_ref()
            .map(|created| age_string(now.duration_since(created.0)))
            .unwrap_or_else(|| "?".to_string());
        groups
            .entry(instance)
            .or_default()
            .push(format!("{name} uid={uid} age={age}"));
    }
    if groups.is_empty() {
        println!("  (none)");
    }
    for (instance, objects) in groups {
        println!("  {instance}:");
        for object in objects {
            println!("    {object}");
        }
    }
}

/// A rough age for a person reading `--list`: days and hours, hours and
/// minutes, minutes and seconds, or seconds.
fn age_string(age: SignedDuration) -> String {
    let seconds = age.as_secs().max(0);
    let (days, hours, minutes, secs) = (
        seconds / 86_400,
        (seconds % 86_400) / 3_600,
        (seconds % 3_600) / 60,
        seconds % 60,
    );
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m{secs}s")
    } else {
        format!("{secs}s")
    }
}

/// The cluster client: the same `KUBECONFIG` the server used (or the
/// in-cluster config), with a connect and read timeout so an unreachable
/// cluster fails the run rather than hanging `stop`.
async fn cluster_client() -> Result<kube::Client, BoxError> {
    let mut config = kube::Config::infer().await?;
    config.connect_timeout = Some(CLIENT_TIMEOUT);
    config.read_timeout = Some(CLIENT_TIMEOUT);
    Ok(kube::Client::try_from(config)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::PgPool;

    // --- Pure: selection ---

    fn meta(name: &str, labels: Option<&str>, uid: Option<&str>) -> ObjectMeta {
        ObjectMeta {
            name: Some(name.to_string()),
            uid: uid.map(str::to_string),
            labels: labels.map(|value| [(INSTANCE_LABEL.to_string(), value.to_string())].into()),
            ..Default::default()
        }
    }

    /// An object labelled exactly with the instance is kept for deletion.
    /// Unlabelled, another instance, an empty label, a label that merely
    /// starts with or contains the instance, and one with no uid are all
    /// left.
    #[test]
    fn test_to_delete_keeps_only_an_object_labelled_exactly_with_the_instance() {
        let instance = "0f8e2a5c-1b2d-4e6f-8a9b-0c1d2e3f4a5b";
        let listed = [
            meta("ours", Some(instance), Some("uid-ours")),
            meta(
                "theirs",
                Some("11111111-2222-3333-4444-555555555555"),
                Some("uid-theirs"),
            ),
            meta("unlabelled", None, Some("uid-unlabelled")),
            meta("empty", Some(""), Some("uid-empty")),
            meta("prefix", Some(&format!("{instance}-x")), Some("uid-prefix")),
            meta("contains", Some(&format!("x{instance}")), Some("uid-contains")),
            meta("no-uid", Some(instance), None),
        ];

        assert_eq!(
            to_delete(&listed, instance),
            vec![("ours".to_string(), "uid-ours".to_string())]
        );
    }

    /// Every object of the instance, in name order, whatever order they were
    /// listed in.
    #[test]
    fn test_to_delete_returns_every_object_of_the_instance_in_name_order() {
        let instance = "0f8e2a5c-1b2d-4e6f-8a9b-0c1d2e3f4a5b";
        let listed = [
            meta("b", Some(instance), Some("uid-b")),
            meta("a", Some(instance), Some("uid-a")),
        ];

        assert_eq!(
            to_delete(&listed, instance),
            vec![
                ("a".to_string(), "uid-a".to_string()),
                ("b".to_string(), "uid-b".to_string())
            ]
        );
    }

    /// An empty instance (never read) matches nothing, not even an empty
    /// label: no selector can be built from it.
    #[test]
    fn test_to_delete_matches_nothing_for_an_empty_instance() {
        let listed = [
            meta("empty", Some(""), Some("uid-empty")),
            meta("ours", Some("abc"), Some("uid-abc")),
        ];

        assert!(to_delete(&listed, "").is_empty());
    }

    // --- Pure: guards ---

    const SCRATCH: &str = "postgres://smelt:smelt@postgres/smelt_scratch_worktree";
    const DEV: &str = "postgres://smelt:smelt@postgres/smelt";
    const UUID: &str = "0f8e2a5c-1b2d-4e6f-8a9b-0c1d2e3f4a5b";

    /// A real scratch case is accepted.
    #[test]
    fn test_check_target_accepts_a_scratch_database_with_a_uuid_instance() {
        assert_eq!(check_target(SCRATCH, DEV, UUID, false), Ok(()));
        assert_eq!(
            scratch_database(SCRATCH, DEV),
            Ok("smelt_scratch_worktree".to_string())
        );
    }

    /// Guard 1: a database that isn't a check server's scratch database.
    #[test]
    fn test_check_target_refuses_a_database_that_isnt_smelt_scratch() {
        let why = check_target(DEV, DEV, UUID, false).expect_err("the dev database must be refused");
        assert!(why.contains("smelt_scratch_"), "{why}");
    }

    /// Guard 2: the dev URL's database, whatever its name.
    #[test]
    fn test_check_target_refuses_the_dev_databases_name() {
        let same = "postgres://smelt:smelt@postgres/smelt_scratch_dev";
        let dev = "postgres://other:x@elsewhere/smelt_scratch_dev";
        let why = check_target(same, dev, UUID, false).expect_err("the dev database must be refused");
        assert!(why.contains("--dev-database-url"), "{why}");
    }

    /// Guard 3: a database that owns the objects made before SME-115.
    #[test]
    fn test_check_target_refuses_a_database_that_owns_unlabelled_objects() {
        let why =
            check_target(SCRATCH, DEV, UUID, true).expect_err("an owning database must be refused");
        assert!(why.contains("owns_unlabelled"), "{why}");
    }

    /// Guard 4: an instance that isn't a UUID, an empty one included.
    #[test]
    fn test_check_target_refuses_a_non_uuid_instance() {
        for instance in [
            "",
            "not-a-uuid",
            "0F8E2A5C-1B2D-4E6F-8A9B-0C1D2E3F4A5B",
            "0f8e2a5c1b2d4e6f8a9b0c1d2e3f4a5b",
        ] {
            assert!(
                check_target(SCRATCH, DEV, instance, false).is_err(),
                "{instance:?} must be refused"
            );
        }
    }

    /// The database name is read from the URL's path, query and all; a URL
    /// naming no database has none.
    #[test]
    fn test_database_name_reads_the_urls_path() {
        assert_eq!(database_name("postgres://u@h/db"), Some("db".to_string()));
        assert_eq!(
            database_name("postgres://u@h/db?sslmode=disable"),
            Some("db".to_string())
        );
        assert_eq!(database_name("postgres://u@h/"), None);
        assert_eq!(database_name("postgres://u@h"), None);
    }

    #[test]
    fn test_is_uuid_takes_only_a_lower_case_uuid() {
        assert!(is_uuid(UUID));
        assert!(!is_uuid(""));
        assert!(!is_uuid("0f8e2a5c-1b2d-4e6f-8a9b-0c1d2e3f4a5"));
        assert!(!is_uuid("0f8e2a5c-1b2d-4e6f-8a9b-0c1d2e3f4a5b-"));
        assert!(!is_uuid("0f8e2a5c-1b2d-4e6f-8a9b-0c1d2e3f4a5g"));
    }

    // --- Pure: arguments ---

    #[test]
    fn test_parse_args_requires_both_urls() {
        let both =
            parse_args(["--database-url", SCRATCH, "--dev-database-url", DEV].map(str::to_string))
                .expect("both URLs parse");
        assert_eq!(both.database_url.as_deref(), Some(SCRATCH));
        assert_eq!(both.dev_database_url.as_deref(), Some(DEV));
        assert!(!both.dry_run && !both.list);

        assert!(parse_args(["--database-url".to_string(), SCRATCH.to_string()]).is_err());
        assert!(parse_args(["--dev-database-url".to_string(), DEV.to_string()]).is_err());
        assert!(parse_args(["--database-url".to_string()]).is_err());
    }

    #[test]
    fn test_parse_args_takes_list_alone_and_rejects_unknown_flags() {
        let listed = parse_args(["--list".to_string()]).expect("--list parses");
        assert!(listed.list);
        assert!(parse_args(["--list".to_string(), "--dry-run".to_string()]).is_err());
        assert!(parse_args(["--nope".to_string()]).is_err());
    }

    /// `--dry-run` is a flag, and a URL's value is the argument after its
    /// flag, never a flag itself.
    #[test]
    fn test_parse_args_takes_dry_run_with_the_urls() {
        let args = parse_args(
            ["--dry-run", "--database-url", SCRATCH, "--dev-database-url", DEV].map(str::to_string),
        )
        .expect("parses");
        assert!(args.dry_run && !args.list);
    }

    // --- Against a real Postgres ---

    /// A unique number for a test's own object and database names.
    fn unique() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    }

    /// A connection of our own to the test database, the pool's own closed
    /// first so guard 5's count is only what the test makes.
    async fn connection(pool: &PgPool) -> PgConnection {
        let options: PgConnectOptions = pool.connect_options().as_ref().clone();
        pool.close().await;
        PgConnection::connect_with(&options)
            .await
            .expect("a connection to the test database")
    }

    /// A migrated database gives its instance: a UUID, owning nothing from
    /// before SME-115.
    #[sqlx::test]
    async fn test_a_migrated_database_reads_its_instance(pool: PgPool) {
        let mut conn = connection(&pool).await;
        match read_instance(&mut conn).await.expect("the instance") {
            ScratchState::Instance {
                id,
                owns_unlabelled,
            } => {
                assert!(
                    is_uuid(&id),
                    "a migrated database's instance id must be a UUID: {id}"
                );
                assert!(
                    !owns_unlabelled,
                    "a scratch database owns no objects from before SME-115"
                );
            }
            other => panic!("a migrated database must read its instance, not {other:?}"),
        }
    }

    /// A database without `smelt_instance` was never migrated: nothing can
    /// carry its instance, so the cleanup succeeds deleting nothing.
    #[sqlx::test]
    async fn test_a_database_without_smelt_instance_reads_as_never_migrated(pool: PgPool) {
        sqlx::query("DROP TABLE smelt_instance")
            .execute(&pool)
            .await
            .expect("drop the table");
        let mut conn = connection(&pool).await;
        assert_eq!(
            read_instance(&mut conn).await.expect("the state"),
            ScratchState::Unmigrated
        );
    }

    /// A database that isn't there reads as absent, not as a failure.
    #[sqlx::test]
    async fn test_a_missing_database_reads_as_absent(pool: PgPool) {
        let mut options: PgConnectOptions = pool.connect_options().as_ref().clone();
        options = options.database(&format!("smelt_instance_cleanup_missing_{}", unique()));
        assert!(
            open_scratch(&options).await.expect("connecting").is_none(),
            "a database that isn't there must read as absent"
        );
    }

    /// Guard 5 counts every other connection to the database, and never this
    /// one's own.
    #[sqlx::test]
    async fn test_other_connections_counts_every_other_connection(pool: PgPool) {
        let database = pool
            .connect_options()
            .get_database()
            .unwrap_or_default()
            .to_string();
        let options: PgConnectOptions = pool.connect_options().as_ref().clone();
        let mut ours = connection(&pool).await;
        assert_eq!(
            other_connections(&mut ours, &database)
                .await
                .expect("count"),
            Vec::<i32>::new(),
            "this tool's own connection must not count"
        );
        let other = PgConnection::connect_with(&options)
            .await
            .expect("a second connection");
        let pids = other_connections(&mut ours, &database)
            .await
            .expect("count");
        assert_eq!(
            pids.len(),
            1,
            "another open connection to {database} must be counted: {pids:?}"
        );
        other.close().await.ok();
    }

    // --- Against the real cluster, in `smelt-park-test` only ---

    mod cluster {
        use super::*;

        fn labels(instance: Option<&str>) -> serde_json::Map<String, serde_json::Value> {
            let mut labels = serde_json::Map::new();
            if let Some(instance) = instance {
                labels.insert(INSTANCE_LABEL.to_string(), instance.to_string().into());
            }
            labels
        }

        /// A pod that never starts (an image no node has, never pulled),
        /// which is enough to delete.
        fn pod(name: &str, instance: Option<&str>) -> Pod {
            serde_json::from_value(serde_json::json!({
                "metadata": {"name": name, "labels": labels(instance)},
                "spec": {
                    "containers": [{
                        "name": "sandbox",
                        "image": "smelt.invalid/none:0",
                        "imagePullPolicy": "Never",
                    }],
                },
            }))
            .expect("a pod spec")
        }

        fn claim(name: &str, instance: Option<&str>) -> PersistentVolumeClaim {
            serde_json::from_value(serde_json::json!({
                "metadata": {"name": name, "labels": labels(instance)},
                "spec": {
                    "accessModes": ["ReadWriteOnce"],
                    "resources": {"requests": {"storage": "1Mi"}},
                },
            }))
            .expect("a claim spec")
        }

        /// Cleanup deletes exactly the instance it is given: everything of
        /// that instance and nothing else. Another instance's objects and
        /// the unlabelled ones keep their uids.
        #[tokio::test]
        async fn test_cleanup_deletes_only_the_instance_it_is_given() {
            let client = kube::Client::try_default().await.expect("a cluster client");
            let pods: Api<Pod> = Api::namespaced(client.clone(), NAMESPACE);
            let claims: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), NAMESPACE);
            let nonce = unique();
            let ours = format!("cleanup-a-{nonce}");
            let theirs = format!("cleanup-b-{nonce}");
            let named = |kind: &str| format!("cleanup-{kind}-{nonce}");

            let ours_pods = [named("a1"), named("a2")];
            let ours_claims = [named("aclaim1"), named("aclaim2")];
            let theirs_pod = named("b1");
            let theirs_claim = named("bclaim1");
            let unlabelled_pod = named("unlabelled1");
            let unlabelled_claim = named("unlabelledclaim1");

            let mut made_pods: Vec<String> = Vec::new();
            for (name, instance) in ours_pods
                .iter()
                .map(|name| (name.as_str(), Some(ours.as_str())))
                .chain([
                    (theirs_pod.as_str(), Some(theirs.as_str())),
                    (unlabelled_pod.as_str(), None),
                ])
            {
                pods.create(&Default::default(), &pod(name, instance))
                    .await
                    .expect("create a pod");
                made_pods.push(name.to_string());
            }
            let mut made_claims: Vec<String> = Vec::new();
            for (name, instance) in ours_claims
                .iter()
                .map(|name| (name.as_str(), Some(ours.as_str())))
                .chain([
                    (theirs_claim.as_str(), Some(theirs.as_str())),
                    (unlabelled_claim.as_str(), None),
                ])
            {
                claims
                    .create(&Default::default(), &claim(name, instance))
                    .await
                    .expect("create a claim");
                made_claims.push(name.to_string());
            }

            let theirs_pod_uid = pods.get(&theirs_pod).await.expect("their pod").metadata.uid;
            let theirs_claim_uid = claims
                .get(&theirs_claim)
                .await
                .expect("their claim")
                .metadata
                .uid;
            let unlabelled_pod_uid = pods
                .get(&unlabelled_pod)
                .await
                .expect("the unlabelled pod")
                .metadata
                .uid;
            let unlabelled_claim_uid = claims
                .get(&unlabelled_claim)
                .await
                .expect("the unlabelled claim")
                .metadata
                .uid;

            let finished = delete_instance(&client, &ours, false)
                .await
                .expect("the cleanup");

            assert!(
                finished,
                "the cleanup must finish with only the given instance's objects there"
            );
            for name in ours_pods.iter().chain(ours_claims.iter()) {
                assert!(
                    pods.get_opt(name).await.expect("get").is_none()
                        && claims.get_opt(name).await.expect("get").is_none(),
                    "{name} should have been deleted"
                );
            }
            assert_eq!(
                pods.get(&theirs_pod).await.expect("their pod").metadata.uid,
                theirs_pod_uid,
                "another instance's pod must be kept"
            );
            assert_eq!(
                claims.get(&theirs_claim).await.expect("their claim").metadata.uid,
                theirs_claim_uid,
                "another instance's claim must be kept"
            );
            assert_eq!(
                pods.get(&unlabelled_pod)
                    .await
                    .expect("the unlabelled pod")
                    .metadata
                    .uid,
                unlabelled_pod_uid,
                "an unlabelled pod must be kept"
            );
            assert_eq!(
                claims.get(&unlabelled_claim)
                    .await
                    .expect("the unlabelled claim")
                    .metadata
                    .uid,
                unlabelled_claim_uid,
                "an unlabelled claim must be kept"
            );

            // Last, so the assertions above saw what was kept: delete
            // everything this test made. A killed run leaves them, which is
            // harmless — every name is unique to this run.
            for name in &made_pods {
                let _ = pods
                    .delete(
                        name,
                        &DeleteParams {
                            grace_period_seconds: Some(0),
                            ..Default::default()
                        },
                    )
                    .await;
            }
            for name in &made_claims {
                let _ = claims.delete(name, &DeleteParams::default()).await;
            }
        }
    }
}
