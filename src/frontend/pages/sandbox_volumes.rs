use dioxus::prelude::*;

use crate::api::sandbox_volumes::{SandboxVolumeSummary, create_sandbox_volume, delete_sandbox_volume, list_sandbox_volumes};
use crate::frontend::Route;

/// Lists every configured volume — every sandbox pod smelt creates gets
/// every one of these mounted, unconditionally (see
/// docs/projects/plans/sandbox-native-environment.md's Phase 4). No edit
/// page: nothing to change once a volume exists besides deleting it, same
/// inline arm/confirm pattern `ConversationSidebar`'s delete already uses
/// — only one row armed at a time.
#[component]
pub fn SandboxVolumesIndex() -> Element {
    let initial_volumes = use_resource(list_sandbox_volumes);
    let mut volumes: Signal<Vec<SandboxVolumeSummary>> = use_signal(Vec::new);
    let mut loaded = use_signal(|| false);
    let mut list_error: Signal<Option<String>> = use_signal(|| None);

    use_effect(move || {
        if let Some(result) = initial_volumes() {
            match result {
                Ok(list) => volumes.set(list),
                Err(e) => list_error.set(Some(e.to_string())),
            }
            loaded.set(true);
        }
    });

    let mut pending_delete: Signal<Option<i64>> = use_signal(|| None);
    let mut delete_error: Signal<Option<String>> = use_signal(|| None);

    let mut request_delete = move |id: i64| {
        if pending_delete() == Some(id) {
            spawn(async move {
                match delete_sandbox_volume(id).await {
                    Ok(()) => {
                        volumes.write().retain(|v| v.id != id);
                        pending_delete.set(None);
                    }
                    Err(e) => delete_error.set(Some(e.to_string())),
                }
            });
        } else {
            pending_delete.set(Some(id));
        }
    };

    rsx! {
        div { class: "sandbox-volumes-page",
            div { class: "sandbox-volumes-header",
                Link { to: Route::Home {}, class: "sandbox-volumes-back-link", "\u{2190} Back to conversations" }
                h1 { "Sandbox volumes" }
                p { class: "muted",
                    "A volume is mounted into every sandbox pod smelt creates \u{2014} smelt doesn't need to know what it's for."
                }
                Link { to: Route::SandboxVolumeNewRoute {}, class: "sandbox-volumes-new-link", "+ Add a volume" }
            }

            if let Some(err) = list_error() {
                p { class: "error", "{err}" }
            }
            if let Some(err) = delete_error() {
                p { class: "error", "{err}" }
            }

            if !loaded() {
                p { class: "muted", "Loading..." }
            } else if volumes().is_empty() {
                p { class: "muted", "No volumes configured yet." }
            } else {
                div { class: "sandbox-volume-list",
                    for volume in volumes() {
                        div { key: "{volume.id}", class: "sandbox-volume-row",
                            div { class: "sandbox-volume-summary",
                                span { class: "sandbox-volume-name", "{volume.name}" }
                                span { class: "sandbox-volume-path", "{volume.mount_path}" }
                            }
                            button {
                                class: if pending_delete() == Some(volume.id) { "sandbox-volume-delete confirm" } else { "sandbox-volume-delete" },
                                r#type: "button",
                                onclick: move |_| request_delete(volume.id),
                                if pending_delete() == Some(volume.id) { "Confirm delete?" } else { "Delete" }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Creating a volume — name and mount path only. A leading `~` in the
/// path (e.g. `~/.ssh`) is expanded server-side to the sandbox user's
/// home directory; the placeholder hints at that rather than the page
/// doing its own resolution.
#[component]
pub fn SandboxVolumeNew() -> Element {
    let navigator = use_navigator();
    let mut name: Signal<String> = use_signal(String::new);
    let mut mount_path: Signal<String> = use_signal(String::new);
    let mut submit_error: Signal<Option<String>> = use_signal(|| None);
    let mut submitting = use_signal(|| false);

    let submit = move |evt: Event<FormData>| {
        evt.prevent_default();
        let name_value = name();
        let mount_path_value = mount_path();
        submitting.set(true);
        spawn(async move {
            match create_sandbox_volume(name_value, mount_path_value).await {
                Ok(_summary) => {
                    navigator.push(Route::SandboxVolumesRoute {});
                }
                Err(e) => {
                    submit_error.set(Some(e.to_string()));
                    submitting.set(false);
                }
            }
        });
    };

    rsx! {
        div { class: "sandbox-volumes-page",
            div { class: "sandbox-volumes-header",
                Link { to: Route::SandboxVolumesRoute {}, class: "sandbox-volumes-back-link", "\u{2190} Back to sandbox volumes" }
                h1 { "Add a volume" }
            }

            form { class: "sandbox-volume-add-form", onsubmit: submit,
                label { r#for: "sandbox-volume-new-name", "Name" }
                input {
                    id: "sandbox-volume-new-name",
                    r#type: "text",
                    required: true,
                    value: "{name}",
                    oninput: move |e| name.set(e.value()),
                }
                label { r#for: "sandbox-volume-new-mount-path", "Mount path" }
                input {
                    id: "sandbox-volume-new-mount-path",
                    r#type: "text",
                    required: true,
                    placeholder: "~/.ssh",
                    value: "{mount_path}",
                    oninput: move |e| mount_path.set(e.value()),
                }
                p { class: "muted", "A leading ~ is expanded to the sandbox user's home directory." }
                if let Some(err) = submit_error() {
                    p { class: "error", "{err}" }
                }
                button {
                    r#type: "submit",
                    disabled: submitting(),
                    if submitting() { "Adding\u{2026}" } else { "Add volume" }
                }
            }
        }
    }
}
