//! SME-516 review 1, H1: the refresh's read of the recorded remote, run by
//! smelt against a fixture repository on the test's own disk (`file://`,
//! which only test builds allow).

use super::*;

/// A fixture remote: a bare repository whose `main` has `dev.md` at two
/// commits and then a symlink, and a `side` branch that widens `dev.md`.
pub(crate) struct Fixture {
    pub dir: std::path::PathBuf,
    pub url: String,
    /// `main`'s commits, oldest first: one, two, the symlink (the tip).
    pub main: Vec<String>,
    pub side: String,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn sh(dir: &std::path::Path, script: &str) -> String {
    let out = std::process::Command::new("sh").arg("-c").arg(script).current_dir(dir).output().expect("run sh");
    assert!(out.status.success(), "{script}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).expect("utf-8")
}

/// The fixture, with `first`, `second` and `wide` as the file's contents on
/// `main`'s first two commits and on `side`, at `.agents/personas/<file>`.
pub(crate) fn fixture(file: &str, first: &str, second: &str, wide: &str) -> Fixture {
    fixture_of(file, &[first, second], wide, true)
}

/// A fixture remote whose `main` has one commit per `main` content of
/// `.agents/personas/<file>` (oldest first), then a symlink commit when
/// `link`; and a `side` branch off its first commit holding `wide`.
pub(crate) fn fixture_of(file: &str, main: &[&str], wide: &str, link: bool) -> Fixture {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("smelt-remote-fixture-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src/.agents/personas")).expect("mkdir");
    let p = format!(".agents/personas/{file}");
    std::fs::write(dir.join("wide"), wide).expect("write");
    let mut script = format!(
        "set -e; cd src\n\
         c() {{ git -c user.name=t -c user.email=t@t commit -q --allow-empty -m \"$1\"; git rev-parse HEAD; }}\n\
         git init -q -b main .\n"
    );
    for (i, content) in main.iter().enumerate() {
        std::fs::write(dir.join(format!("main{i}")), content).expect("write");
        script.push_str(&format!("cp ../main{i} {p}; git add -A; c m{i}\n"));
        if i == 0 {
            script.push_str(&format!(
                "git checkout -q -b side; cp ../wide {p}; git add -A; c side >../side; git checkout -q main\n"
            ));
        }
    }
    if link {
        script.push_str(&format!("ln -s {file} .agents/personas/link.md; git add -A; c link\n"));
    }
    script.push_str("git clone -q --bare . ../origin.git\n");
    let main: Vec<String> = sh(&dir, &script).lines().map(str::to_string).collect();
    let side = std::fs::read_to_string(dir.join("side")).expect("side").trim().to_string();
    let url = format!("file://{}", dir.join("origin.git").display());
    Fixture { dir, url, main, side }
}

async fn read(f: &Fixture, rel: &str, commit: Option<&str>) -> Result<(String, String), String> {
    let key = remote_key(&f.url).expect("a key");
    persona_at_remote(&f.url, &key, rel, commit, &[]).await
}

const DEV: &str = ".agents/personas/dev.md";

/// The tip of the remote's default branch, or a commit on it (abbreviated
/// too), is read as committed.
#[tokio::test]
async fn test_a_remote_read_takes_the_default_branch_or_a_commit_on_it() {
    let f = fixture("dev.md", "one\n", "two\n", "wide\n");
    assert_eq!(read(&f, DEV, None).await, Ok((f.main[2].clone(), "two\n".to_string())), "the tip");
    assert_eq!(read(&f, DEV, Some(&f.main[0][..10])).await, Ok((f.main[0].clone(), "one\n".to_string())));
}

/// A side branch's commit, an unknown commit, a symlink and a missing file
/// are refused, each saying which.
#[tokio::test]
async fn test_a_remote_read_refuses_what_isnt_a_file_on_the_default_branch() {
    let f = fixture("dev.md", "one\n", "two\n", "wide\n");
    let on = format!("main at {}", f.url);
    let refused = read(&f, DEV, Some(&f.side)).await.expect_err("a side branch");
    assert_eq!(refused, format!("{} isn't on {on}: a refresh reads only commits on the remote's default branch", f.side));
    let unknown = "0123456789abcdef0123456789abcdef01234567";
    let refused = read(&f, DEV, Some(unknown)).await.expect_err("unknown");
    assert!(refused.starts_with(&format!("{unknown} isn't on {on}")), "{refused}");
    let refused = read(&f, ".agents/personas/link.md", None).await.expect_err("a symlink");
    assert!(refused.contains("isn't a regular file (a symlink is refused)"), "{refused}");
    let refused = read(&f, ".agents/personas/none.md", None).await.expect_err("missing");
    assert!(refused.starts_with(&format!("no file .agents/personas/none.md on {on} at ")), "{refused}");
}

/// The URL must be the remote whose trust was read, and a transport other
/// than ssh or https (here `git://`) is never fetched.
#[tokio::test]
async fn test_a_remote_read_reads_only_the_trusted_remote() {
    let f = fixture("dev.md", "one\n", "two\n", "wide\n");
    let refused = persona_at_remote(&f.url, "github.com/o/r", DEV, None, &[]).await.expect_err("another remote");
    assert!(refused.contains("isn't github.com/o/r, the remote whose trust was read"), "{refused}");
    let refused = persona_at_remote("/tmp/x.git", "file/tmp/x", DEV, None, &[]).await.expect_err("a local path");
    assert!(refused.contains("the remote whose trust was read"), "{refused}");
    let refused = persona_at_remote("git://h/o/r", "h/o/r", DEV, None, &[]).await.expect_err("git://");
    assert_eq!(refused, "a refresh reads a remote only over ssh or https, and git://h/o/r is neither");
}

/// Review 2 M1: git is given the string that was keyed, never the raw one,
/// and a URL the two parsers read as different remotes is refused before
/// anything is fetched.
#[tokio::test]
async fn test_a_remote_read_gives_git_the_url_it_keyed() {
    assert_eq!(fetch_url("https://GitHub.com/o/r").as_deref(), Some("https://github.com/o/r"));
    assert_eq!(fetch_url("ssh://git@GitHub.com/o/r").as_deref(), Some("ssh://git@GitHub.com/o/r"));
    assert_eq!(fetch_url("  git@github.com:o/r.git ").as_deref(), Some("git@github.com:o/r.git"));
    assert_eq!(fetch_url("http://[::1"), None);
    for (url, key) in [
        (r"https://github.com\@example.com/../cconstantine/smelt", "github.com/cconstantine/smelt"),
        ("ssh://git@github.com/x/../o/r", "github.com/o/r"),
        ("git@github.com:x/../o/r", "github.com/x/../o/r"),
    ] {
        let refused = persona_at_remote(url, key, DEV, None, &[]).await.expect_err(url);
        assert_eq!(refused, format!("the checkout's remote {url} isn't {key}, the remote whose trust was read; nothing was read"));
    }
}

/// Review 1 L1: `commit` is a commit id only; a ref name, even one on the
/// default branch, is refused before anything is fetched.
#[tokio::test]
async fn test_a_remote_read_takes_only_a_commit_id() {
    let f = fixture("dev.md", "one\n", "two\n", "wide\n");
    for r in ["HEAD", "main", "refs/heads/main", "FETCH_HEAD", "abc", "x^{tree}", &format!("{}^", &f.main[2][..8])] {
        let refused = read(&f, DEV, Some(r)).await.expect_err(r);
        assert_eq!(refused, format!("{r:?} isn't a commit id: a refresh takes 4 to 64 hex digits, or none for the default branch's tip"));
    }
    assert!(validate_refresh_commit(&f.main[1]).is_ok());
}

/// `ls-remote --symref`'s listing names the default branch; one that can't
/// be a plain branch name is refused.
#[test]
fn test_the_default_branch_comes_from_the_remotes_head() {
    assert_eq!(default_branch("ref: refs/heads/main\tHEAD\nabc\tHEAD\n").as_deref(), Some("main"));
    assert_eq!(default_branch("ref: refs/heads/a:b\tHEAD\n"), None);
    assert_eq!(default_branch("ref: refs/tags/v1\tHEAD\n"), None);
    assert_eq!(default_branch("abc\tHEAD\n"), None);
}
