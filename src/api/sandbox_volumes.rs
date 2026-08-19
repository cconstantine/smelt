use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

#[cfg(feature = "server")]
use crate::{db, sandbox};

/// Read-only summary of a configured generic volume for the browser — see
/// docs/projects/plans/sandbox-native-environment.md's Phase 4. Deliberately
/// minimal: this pass has nothing to edit or view beyond name/mount path
/// (no upload, no browsing — see the plan's "What").
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SandboxVolumeSummary {
    pub id: i64,
    pub name: String,
    pub mount_path: String,
}

#[cfg(feature = "server")]
impl From<db::SandboxVolume> for SandboxVolumeSummary {
    fn from(volume: db::SandboxVolume) -> Self {
        Self { id: volume.id, name: volume.name, mount_path: volume.mount_path }
    }
}

#[get("/api/sandbox-volumes")]
pub async fn list_sandbox_volumes() -> ServerFnResult<Vec<SandboxVolumeSummary>> {
    let volumes = db::list_sandbox_volumes(db::get()).await.map_err(ServerFnError::new)?;
    Ok(volumes.into_iter().map(SandboxVolumeSummary::from).collect())
}

/// Creates a volume and its backing PVC (`sandbox::create_volume` — see
/// its own doc comment for the rollback-on-PVC-failure behavior) — a
/// leading `~` in `mount_path` is expanded to the sandbox user's home
/// directory before anything is persisted, so the browser never needs to
/// know that resolution happened.
#[post("/api/sandbox-volumes")]
pub async fn create_sandbox_volume(name: String, mount_path: String) -> ServerFnResult<SandboxVolumeSummary> {
    let id = sandbox::create_volume(db::get(), &name, &mount_path).await.map_err(ServerFnError::new)?;
    let volume = db::get_sandbox_volume(db::get(), id)
        .await
        .map_err(ServerFnError::new)?
        .ok_or_else(|| ServerFnError::new("volume vanished immediately after creation"))?;
    Ok(volume.into())
}

#[delete("/api/sandbox-volumes/{id}")]
pub async fn delete_sandbox_volume(id: i64) -> ServerFnResult<()> {
    sandbox::delete_volume(db::get(), id).await.map_err(ServerFnError::new)?;
    Ok(())
}
