//! Which build the server is, so a tab can tell it's running an older
//! bundle than the server it talks to (SME-43).

use dioxus::prelude::*;

/// This build's id: a hash of its sources (`build.rs`), the same in the
/// server binary and the web bundle built from one tree.
pub const BUILD_ID: &str = env!("SMELT_BUILD_ID");

/// The server's build id. A tab calls it each time its live stream
/// (re)connects, and asks for a reload when it isn't its own `BUILD_ID`.
#[get("/api/build-id")]
pub async fn get_build_id() -> ServerFnResult<String> {
    #[cfg(feature = "browser-test")]
    if let Some(id) = test_override::get() {
        return Ok(id);
    }
    Ok(BUILD_ID.to_string())
}

/// Lets the browser tier play a server newer than the page's bundle.
#[cfg(feature = "browser-test")]
pub mod test_override {
    use std::sync::Mutex;

    static OVERRIDE: Mutex<Option<String>> = Mutex::new(None);

    pub fn set(id: Option<&str>) {
        *OVERRIDE.lock().unwrap_or_else(|e| e.into_inner()) = id.map(str::to_string);
    }

    pub(super) fn get() -> Option<String> {
        OVERRIDE.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}
