//! The pod work a child's start does, behind one seam (`ChildCheckout`):
//! cloning the child's repo and reading files in its checkout.
//! `PodCheckout` does it in the child's pod; a test registers a fake per
//! parent conversation, so the start's database behaviour is tested
//! without a cluster (`sandbox::tests::child_checkout` tests `PodCheckout`'s
//! git work against a real pod).

use super::*;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

type Boxed<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The pod work of a child's start.
pub(crate) trait ChildCheckout: Send + Sync {
    /// Clones `repo` (the child's, recorded `cloning`) at `target` and
    /// records it ready, with its AGENTS.md and persona files.
    fn clone_repo<'a>(
        &'a self,
        pool: &'a PgPool,
        child: i64,
        repo: &'a db::ConversationRepo,
        target: &'a crate::git::CloneTarget,
    ) -> Boxed<'a, Result<(), String>>;

    /// Reads `rel` in the child's checkout `dir`, symlink-safe as an
    /// AGENTS.md is read; `None` when there's no such file.
    fn read_file<'a>(
        &'a self,
        pool: &'a PgPool,
        child: i64,
        dir: &'a str,
        rel: &'a str,
    ) -> Boxed<'a, Result<Option<db::InstructionsFile>, String>>;
}

/// The real checkout: the child's own pod.
pub(crate) struct PodCheckout;

impl ChildCheckout for PodCheckout {
    fn clone_repo<'a>(
        &'a self,
        pool: &'a PgPool,
        child: i64,
        repo: &'a db::ConversationRepo,
        target: &'a crate::git::CloneTarget,
    ) -> Boxed<'a, Result<(), String>> {
        Box::pin(async move { crate::git::clone_child_repo(pool, child, repo, target).await.map(|_| ()) })
    }

    fn read_file<'a>(
        &'a self,
        pool: &'a PgPool,
        child: i64,
        dir: &'a str,
        rel: &'a str,
    ) -> Boxed<'a, Result<Option<db::InstructionsFile>, String>> {
        Box::pin(async move {
            let shell = crate::sandbox::PodShell::for_conversation(pool, child)
                .await
                .map_err(|e| format!("couldn't reach the agent's sandbox: {e}"))?;
            let checkout = format!("{}/{dir}", crate::sandbox::WORKSPACE_DIR);
            crate::git::read_instructions_file(&shell, &checkout, &format!("{checkout}/{rel}")).await
        })
    }
}

/// The checkout a start of a child of `parent` uses: a test's fake if one
/// is registered for that parent, else the child's pod.
pub(crate) fn checkout_for(parent: i64) -> Arc<dyn ChildCheckout> {
    #[cfg(test)]
    if let Some(fake) = test_fakes::lookup(parent) {
        return fake;
    }
    let _ = parent;
    Arc::new(PodCheckout)
}

/// Fakes registered by tests, per parent conversation.
#[cfg(test)]
pub(crate) mod test_fakes {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};

    static FAKES: LazyLock<Mutex<HashMap<i64, Arc<dyn ChildCheckout>>>> = LazyLock::new(Default::default);

    /// Every start of a child of `parent` uses `fake` from now on.
    pub fn register(parent: i64, fake: Arc<dyn ChildCheckout>) {
        FAKES.lock().unwrap_or_else(|e| e.into_inner()).insert(parent, fake);
    }

    pub(super) fn lookup(parent: i64) -> Option<Arc<dyn ChildCheckout>> {
        FAKES.lock().unwrap_or_else(|e| e.into_inner()).get(&parent).cloned()
    }
}
