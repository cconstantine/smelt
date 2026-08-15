mod pages;

use dioxus::prelude::*;
use pages::Chat;

#[derive(Routable, Clone, PartialEq, Debug)]
pub(crate) enum Route {
    #[route("/")]
    Home {},
    #[route("/conversation/:id")]
    ConversationRoute { id: i64 },
}

#[component]
fn Home() -> Element {
    rsx! { Chat {} }
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
        document::Stylesheet { href: asset!("/assets/chat.css") }
        Router::<Route> {}
    }
}
