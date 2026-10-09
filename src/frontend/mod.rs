pub(crate) mod clipboard;
mod pages;

use dioxus::prelude::*;
use pages::{
    Chat, GitSettingsPage, LanguageServerEdit, LanguageServerNew, LanguageServersIndex, McpServerEdit,
    McpServerNew, McpServersIndex, PodsIndex, ProviderEdit, ProviderNew, ProvidersIndex, SandboxVolumeNew,
    SandboxVolumesIndex,
};

#[derive(Routable, Clone, PartialEq, Debug)]
pub(crate) enum Route {
    #[route("/")]
    Home {},
    #[route("/conversation/:id")]
    ConversationRoute { id: i64 },
    #[route("/mcp-servers")]
    McpServersRoute {},
    #[route("/mcp-servers/new")]
    McpServerNewRoute {},
    #[route("/mcp-servers/:id")]
    McpServerEditRoute { id: i64 },
    #[route("/sandbox-volumes")]
    SandboxVolumesRoute {},
    #[route("/sandbox-volumes/new")]
    SandboxVolumeNewRoute {},
    #[route("/pods")]
    PodsRoute {},
    #[route("/git")]
    GitRoute {},
    #[route("/language-servers")]
    LanguageServersRoute {},
    #[route("/language-servers/new")]
    LanguageServerNewRoute {},
    #[route("/language-servers/:id")]
    LanguageServerEditRoute { id: i64 },
    #[route("/providers")]
    ProvidersRoute {},
    #[route("/providers/new")]
    ProviderNewRoute {},
    #[route("/providers/:id")]
    ProviderEditRoute { id: i64 },
    // Anything else, including a conversation id that isn't a number.
    #[route("/:..segments")]
    NotFound { segments: Vec<String> },
}

#[component]
fn Home() -> Element {
    rsx! { Chat {} }
}

#[component]
fn McpServersRoute() -> Element {
    rsx! { McpServersIndex {} }
}

#[component]
fn McpServerNewRoute() -> Element {
    rsx! { McpServerNew {} }
}

#[component]
fn McpServerEditRoute(id: i64) -> Element {
    rsx! { McpServerEdit { id } }
}

#[component]
fn SandboxVolumesRoute() -> Element {
    rsx! { SandboxVolumesIndex {} }
}

#[component]
fn SandboxVolumeNewRoute() -> Element {
    rsx! { SandboxVolumeNew {} }
}

#[component]
fn PodsRoute() -> Element {
    rsx! { PodsIndex {} }
}

#[component]
fn GitRoute() -> Element {
    rsx! { GitSettingsPage {} }
}

#[component]
fn LanguageServersRoute() -> Element {
    rsx! { LanguageServersIndex {} }
}

#[component]
fn LanguageServerNewRoute() -> Element {
    rsx! { LanguageServerNew {} }
}

#[component]
fn LanguageServerEditRoute(id: i64) -> Element {
    rsx! { LanguageServerEdit { id } }
}

#[component]
fn ProvidersRoute() -> Element {
    rsx! { ProvidersIndex {} }
}

#[component]
fn ProviderNewRoute() -> Element {
    rsx! { ProviderNew {} }
}

#[component]
fn ProviderEditRoute(id: i64) -> Element {
    rsx! { ProviderEdit { id } }
}

/// A URL that isn't one of smelt's pages. Without this the router showed
/// its raw "Failed to parse route" dump (SME-40 F10).
#[component]
fn NotFound(segments: Vec<String>) -> Element {
    let path = segments.join("/");
    rsx! {
        div { class: "not-found-page",
            h1 { "Page not found" }
            p { class: "muted", "There's no page at /{path}." }
            Link { to: Route::Home {}, class: "not-found-home-link", "\u{2190} Back to conversations" }
        }
    }
}

/// `id` only exists here to satisfy the `Routable` derive's requirement
/// that this component's props match the route's fields — `Chat` reads
/// the current conversation straight from the router itself (see its
/// `use_memo` over `router.current::<Route>()`) rather than through a
/// prop, since a plain prop change doesn't reliably re-trigger a
/// component's hooks without a full remount.
#[component]
fn ConversationRoute(id: i64) -> Element {
    let _ = id;
    rsx! { Chat {} }
}

#[component]
pub fn App() -> Element {
    rsx! {
        // Without this a phone lays the page out at desktop width and
        // shrinks it to fit (SME-40 F8).
        document::Meta { name: "viewport", content: "width=device-width, initial-scale=1" }
        document::Stylesheet { href: asset!("/assets/chat.css") }
        // Code highlighting in the model's replies (SME-30).
        document::Stylesheet { href: asset!("/assets/highlight.css") }
        StaleBundleBanner {}
        Router::<Route> {}
    }
}

/// Whether this page's bundle is older than the server it talks to
/// (SME-43): set by `check_build_id`, or by an event type the bundle
/// doesn't know. Only a reload clears it.
pub(crate) static STALE_BUNDLE: GlobalSignal<bool> = Signal::global(|| false);

/// Asks the server which build it is, and marks the page stale when that
/// isn't this bundle's. Called each time a live stream (re)connects: a
/// deploy restarts the server, which drops every stream. A failed request
/// leaves things as they were, and the next reconnect asks again.
#[cfg(feature = "web")]
pub(crate) async fn check_build_id() {
    if let Ok(id) = crate::api::version::get_build_id().await
        && id != crate::api::version::BUILD_ID
    {
        *STALE_BUNDLE.write() = true;
    }
}

/// Asks for a reload once the page is older than the server. Everything
/// the page knows how to show keeps working meanwhile, so it never reloads
/// by itself: a half-typed message is the user's to keep.
#[component]
fn StaleBundleBanner() -> Element {
    if !*STALE_BUNDLE.read() {
        return rsx! {};
    }
    rsx! {
        div { class: "stale-bundle-banner", role: "status",
            "smelt was updated. Reload to keep this page current."
            button {
                r#type: "button",
                onclick: move |_| {
                    spawn(async move {
                        let _ = document::eval("location.reload()").await;
                    });
                },
                "Reload"
            }
        }
    }
}
