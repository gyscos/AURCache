//! The two config files every build runs against.
//!
//! `makepkg.conf` and `pacman.conf` are settings like any other — they resolve
//! through the same hierarchy, and are read and written through the same
//! endpoints — but they are files rather than values, so they get a page with
//! room to edit them instead of a row on the settings list.
//!
//! Neither has an environment variable behind it, so unlike everything on the
//! settings page these can only be stored or unset. "Unset" means the builder
//! image's own copy, which is why a reset is worth offering: it is the only way
//! back to whatever the image ships.

use crate::listing::ListHeader;
use crate::screens::settings::is_stored;
use aurcache_client::SettingSource;
use dioxus::prelude::*;

/// The files, in the order they are shown, as `(setting key, file name)`.
///
/// The key is the server's; the name is what the builder calls the file. They
/// differ, and showing the key would be showing an implementation detail.
const FILES: [(&str, &str); 2] = [
    ("makepkg_conf", "makepkg.conf"),
    ("pacman_conf", "pacman.conf"),
];

#[component]
pub fn ConfigFiles() -> Element {
    rsx! {
        div { class: "card bg-base-100 shadow-xl",
            div { class: "card-body",
                ListHeader { title: "Config files" }
                p { class: "text-xs opacity-60 max-w-prose -mt-1",
                    "Used by every build. Left unset, each falls back to the copy the builder image ships."
                }
                ConfigFileTabs { pkgbase: None }
            }
        }
    }
}

/// The two files, as tabs, for one scope.
///
/// Shared with the per-package settings page rather than reimplemented there:
/// both scopes are the same two settings behind the same endpoints, differing
/// only in which row is written. `pkgbase` of `None` is the server-wide file.
#[component]
pub fn ConfigFileTabs(pkgbase: Option<String>) -> Element {
    let mut showing = use_signal(|| FILES[0].0.to_string());

    rsx! {
        div { role: "tablist", class: "tabs tabs-bordered mt-2",
            for (key, name) in FILES {
                button {
                    key: "{key}",
                    role: "tab",
                    class: if showing() == key { "tab tab-active" } else { "tab" },
                    onclick: move |_| showing.set(key.to_string()),
                    "{name}"
                }
            }
        }

        // Keyed on the file so switching tabs remounts the editor.
        // Without that, the draft signal would survive the change and
        // one file's edits would appear under the other's name.
        for (key, name) in FILES {
            if showing() == key {
                ConfigFileEditor {
                    key: "{key}",
                    setting: key,
                    name,
                    pkgbase: pkgbase.clone(),
                }
            }
        }
    }
}

/// Whether *this* scope holds the value, as opposed to inheriting it.
///
/// Not the same question as [`is_stored`] once there is a package involved: a
/// package row inherits a `Global` value, so a globally stored file is stored
/// but not overridden here, and offering to "reset" it from the package page
/// would clear nothing.
fn overridden(source: SettingSource, scoped: bool) -> bool {
    if scoped {
        source == SettingSource::Package
    } else {
        is_stored(source)
    }
}

/// One file: what it currently is, and a way to change it.
///
/// `pkgbase` picks the scope. In the package scope the file is an override:
/// unset means the server-wide file applies, not the builder image's — the
/// hierarchy has one more rung, which is why the badges below say "inherited"
/// rather than naming a specific fallback they cannot see from here.
#[component]
fn ConfigFileEditor(setting: String, name: String, pkgbase: Option<String>) -> Element {
    // Held in signals so the closures below stay `Copy`; two buttons share the
    // save path, and a captured `String` would let only one of them have it.
    //
    // Synced from the props rather than seeded once: this component stays
    // mounted when the route moves from one package's config files to
    // another's, and a signal initialised on the first mount would keep
    // reading -- and `patch_setting` writing -- the package left behind.
    let setting_prop = setting.clone();
    let setting = use_signal(|| setting);
    use_effect(use_reactive(&setting_prop, move |setting_prop: String| {
        let mut setting = setting;
        setting.set(setting_prop);
    }));

    let scoped = pkgbase.is_some();
    let pkgbase_prop = pkgbase.clone();
    let pkgbase = use_signal(|| pkgbase);
    use_effect(use_reactive(
        &pkgbase_prop,
        move |pkgbase_prop: Option<String>| {
            let mut pkgbase = pkgbase;
            pkgbase.set(pkgbase_prop);
        },
    ));
    let mut reload = use_signal(|| 0u32);
    let loaded = use_resource(move || async move {
        // Read so a save re-fetches: the server owns the value, and the source
        // badge has to follow what it actually stored.
        let _ = reload();
        crate::api::client()?
            .get_setting(pkgbase().as_deref(), &setting())
            .await
            .map_err(|e| e.to_string())
    });

    let mut draft = use_signal(String::new);
    let mut stored = use_signal(String::new);
    let mut source = use_signal(|| SettingSource::Default);
    let mut busy = use_signal(|| false);
    let mut status = use_signal(|| Option::<(String, bool)>::None);

    // Seed the editor once the value arrives, and again after each save.
    use_effect(move || {
        if let Some(Ok(response)) = &*loaded.read_unchecked() {
            draft.set(response.value.clone());
            stored.set(response.value.clone());
            source.set(response.source);
        }
    });

    let dirty = draft() != stored();
    let reset_title = if scoped {
        "Discard this package's copy and use the server-wide file"
    } else {
        "Discard the stored file and use the builder's own copy"
    };

    let save = move |value: Option<String>| async move {
        busy.set(true);
        status.set(None);
        let outcome = match crate::api::client() {
            Err(e) => Err(e),
            Ok(client) => match &value {
                Some(value) => {
                    client
                        .patch_setting(pkgbase().as_deref(), &setting(), value)
                        .await
                }
                None => client.reset_setting(pkgbase().as_deref(), &setting()).await,
            }
            .map_err(|e| e.to_string()),
        };
        busy.set(false);
        match outcome {
            Ok(()) => {
                status.set(Some((
                    match (value, scoped) {
                        (Some(_), _) => "Saved.".to_string(),
                        (None, true) => "Reset to the server-wide file.".to_string(),
                        (None, false) => "Reset to the builder's own copy.".to_string(),
                    },
                    true,
                )));
                reload += 1;
            }
            Err(e) => status.set(Some((e, false))),
        }
    };

    rsx! {
        div { class: "pt-3 space-y-2",
            match &*loaded.read_unchecked() {
                None => rsx! {
                    div { class: "flex justify-center p-8",
                        span { class: "loading loading-spinner loading-lg" }
                    }
                },
                Some(Err(e)) => rsx! {
                    div { class: "alert alert-error", span { "Could not load {name}: {e}" } }
                },
                Some(Ok(_)) => rsx! {
                    div { class: "flex items-center gap-2 flex-wrap",
                        span { class: "font-mono text-sm", "{name}" }
                        // Says which copy is on screen. Without it an unset
                        // file and a stored one that happens to match the
                        // image's are indistinguishable.
                        if overridden(source(), scoped) {
                            span { class: "badge badge-outline badge-sm",
                                if scoped { "package override" } else { "stored" }
                            }
                        } else {
                            span { class: "badge badge-outline badge-sm",
                                if scoped { "inherited" } else { "builder default" }
                            }
                        }
                        if dirty {
                            span { class: "badge badge-info badge-sm", "unsaved" }
                        }
                        div { class: "flex-1" }
                        if overridden(source(), scoped) {
                            button {
                                class: "btn btn-ghost btn-sm",
                                disabled: busy(),
                                title: reset_title,
                                onclick: move |_| save(None),
                                "Reset"
                            }
                        }
                        button {
                            class: "btn btn-primary btn-sm",
                            disabled: !dirty || busy(),
                            onclick: move |_| save(Some(draft())),
                            if busy() {
                                span { class: "loading loading-spinner loading-xs" }
                            }
                            "Save"
                        }
                    }

                    if let Some((message, ok)) = status() {
                        div {
                            class: if ok { "alert alert-success text-sm" } else { "alert alert-error text-sm" },
                            span { "{message}" }
                        }
                    }

                    textarea {
                        class: "textarea textarea-bordered font-mono text-xs w-full h-[60vh] leading-snug",
                        spellcheck: "false",
                        value: "{draft}",
                        oninput: move |e| draft.set(e.value()),
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::FILES;
    use aurcache_client::Setting;

    /// Every file here must be a setting the server knows, or saving it is a
    /// 404 that only shows up by clicking Save.
    #[test]
    fn each_file_is_a_setting_the_server_accepts() {
        for (key, _) in FILES {
            assert!(
                Setting::from_key(key).is_some(),
                "{key} is not a setting key"
            );
        }
    }

    /// These two are settings with no environment variable, which is why this
    /// page offers only stored-or-default and no "unset $VAR" note.
    #[test]
    fn neither_file_can_be_pinned_from_the_environment() {
        for (key, _) in FILES {
            let setting = Setting::from_key(key).expect("a known setting");
            assert_eq!(
                setting.meta().env_name,
                None,
                "{key} gained an env var; this page needs to handle it"
            );
        }
    }
}
