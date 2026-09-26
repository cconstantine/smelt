mod pages;

use dioxus::prelude::*;
use pages::{
    Chat, McpServerEdit, McpServerNew, McpServersIndex, PodsIndex, SandboxVolumeNew,
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
        Router::<Route> {}
    }
}
