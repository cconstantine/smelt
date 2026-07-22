mod pages;

use dioxus::prelude::*;
use pages::Chat;

#[derive(Routable, Clone, PartialEq, Debug)]
enum Route {
    #[route("/")]
    Chat {},
}

#[component]
pub fn App() -> Element {
    rsx! {
        document::Stylesheet { href: asset!("/assets/chat.css") }
        Router::<Route> {}
    }
}
