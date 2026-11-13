//! The Settings index (SME-512): every settings page, each with a line
//! saying what it holds, and the browser's own notifications setting.
//! The sidebar links here, from its bottom left; each settings page
//! links back through `SettingsCrumbs`.

use dioxus::prelude::*;

use crate::frontend::Route;
use crate::frontend::notifications::NotificationsToggle;

/// One settings page as the index lists it.
#[derive(Clone, PartialEq)]
pub(crate) struct SettingsPage {
    pub(crate) label: &'static str,
    pub(crate) description: &'static str,
    /// The link's own class, as the sidebar's link had it, so tests and
    /// styles keep their hook.
    pub(crate) class: &'static str,
    pub(crate) route: Route,
}

/// Every settings page, in the order the index lists them. A `fn`, not a
/// `const`: `Route` isn't const-constructible.
pub(crate) fn settings_pages() -> Vec<SettingsPage> {
    vec![
        SettingsPage {
            label: "Model providers",
            description: "Where the model runs: Anthropic, a llama.cpp or Ollama server, or another Anthropic-compatible server.",
            class: "providers-link",
            route: Route::ProvidersRoute {},
        },
        SettingsPage {
            label: "MCP servers",
            description: "Tool servers whose tools the model can use in every conversation.",
            class: "mcp-servers-link",
            route: Route::McpServersRoute {},
        },
        SettingsPage {
            label: "Sandboxes",
            description: "Every running sandbox, its usage, and Stop.",
            class: "pods-link",
            route: Route::PodsRoute {},
        },
        SettingsPage {
            label: "Sandbox volumes",
            description: "Volumes mounted into every sandbox pod.",
            class: "sandbox-volumes-link",
            route: Route::SandboxVolumesRoute {},
        },
        SettingsPage {
            label: "Git",
            description: "The SSH key, name and email every sandbox uses to clone and push.",
            class: "git-link",
            route: Route::GitRoute {},
        },
        SettingsPage {
            label: "Secrets",
            description: "Named secrets that personas hand to their agents' sandboxes.",
            class: "secrets-link",
            route: Route::SecretsRoute {},
        },
        SettingsPage {
            label: "Language servers",
            description: "Code intelligence for the model: diagnostics, definitions and references.",
            class: "language-servers-link",
            route: Route::LanguageServersRoute {},
        },
    ]
}

/// `/settings`: the index of every settings page.
#[component]
pub fn SettingsIndex() -> Element {
    rsx! {
        div { class: "pods-page settings-page settings-index",
            div { class: "pods-header",
                SettingsCrumbs { on_index: true }
                h1 { "Settings" }
            }
            ul { class: "settings-entries",
                for page in settings_pages() {
                    li { key: "{page.label}", class: "settings-entry",
                        Link { to: page.route.clone(), class: "settings-entry-link {page.class}", "{page.label}" }
                        p { class: "muted", "{page.description}" }
                    }
                }
            }
            section { class: "settings-browser",
                h2 { "This browser" }
                p { class: "muted", "Desktop notifications when a conversation needs you. Set per browser." }
                // The one place the browser's notification permission
                // prompt ever appears (SME-71).
                NotificationsToggle {}
            }
        }
    }
}

/// The breadcrumb over every settings page: back to the conversations,
/// and on to the Settings index. The page's own `h1` follows it as the
/// last crumb. On the index itself (`on_index`) only the first link shows.
#[component]
pub(crate) fn SettingsCrumbs(#[props(default)] on_index: bool) -> Element {
    rsx! {
        nav { class: "settings-crumbs", aria_label: "Breadcrumb",
            Link { to: Route::Home {}, class: "settings-home-link", "\u{2190} Conversations" }
            if !on_index {
                span { class: "settings-crumb-separator", aria_hidden: "true", "\u{203a}" }
                Link { to: Route::SettingsRoute {}, class: "settings-back-link", "Settings" }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_the_settings_index_lists_each_settings_page_once() {
        let pages = settings_pages();
        let mut paths: Vec<String> = pages.iter().map(|p| p.route.to_string()).collect();
        paths.sort();
        assert_eq!(
            paths,
            ["/git", "/language-servers", "/mcp-servers", "/pods", "/providers", "/sandbox-volumes", "/secrets"],
            "each settings index, once"
        );
        for page in &pages {
            assert!(!page.label.trim().is_empty(), "a page with no label: {}", page.route);
            assert!(!page.description.trim().is_empty(), "{} has no description", page.label);
            assert!(!page.class.trim().is_empty(), "{} has no class", page.label);
        }
    }
}
