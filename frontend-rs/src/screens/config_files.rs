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

        // Keyed on the scope and the file so switching either remounts the
        // editor. Without that, the draft signal would survive the change:
        // one file's edits under the other's name, or one package's file
        // read — and written — as the package left behind.
        for (key, name) in FILES {
            if showing() == key {
                ConfigFileEditor {
                    key: "{pkgbase.as_deref().unwrap_or(\"global\")}-{key}",
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

/// Write one config file: `Some` stores, `None` resets to inherited.
/// Split out of [`ConfigFileEditor`] so both buttons can share it; see the
/// `save` binding there for why it is not a closure.
#[allow(clippy::too_many_arguments)]
async fn run_save(
    mut busy: Signal<bool>,
    mut status: Signal<Option<(String, bool)>>,
    mut stored: Signal<String>,
    mut reload: Signal<u32>,
    scoped: bool,
    setting: String,
    pkgbase: Option<String>,
    value: Option<String>,
) {
    busy.set(true);
    status.set(None);
    let outcome = match crate::api::client() {
        Err(e) => Err(e),
        Ok(client) => match &value {
            Some(value) => {
                client
                    .patch_setting(pkgbase.as_deref(), &setting, value)
                    .await
            }
            None => client.reset_setting(pkgbase.as_deref(), &setting).await,
        }
        .map_err(|e| e.to_string()),
    };
    busy.set(false);
    match outcome {
        Ok(()) => {
            status.set(Some((
                match (&value, scoped) {
                    (Some(_), _) => "Saved.".to_string(),
                    (None, true) => "Reset to the server-wide file.".to_string(),
                    (None, false) => "Reset to the builder's own copy.".to_string(),
                },
                true,
            )));
            // The box already shows what was just saved, so it stays clean
            // through the re-read below — and anything typed during the
            // flight stays dirty, correctly. A reset leaves both alone: the
            // box still matches, so it counts as clean and the re-read
            // reseeds the inherited file.
            if let Some(value) = value {
                stored.set(value);
            }
            reload += 1;
        }
        Err(e) => status.set(Some((e, false))),
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
    // Props read directly, not mirrored into signals: the call site keys on
    // scope and file, so a new scope or file remounts rather than reusing a
    // draft — and a `patch_setting` can never write the package left behind.
    let scoped = pkgbase.is_some();
    let reload = use_signal(|| 0u32);
    // Its own pair: the resource closure is `FnMut`, so it clones per run
    // instead of moving the props, which the save buttons still need.
    let res_props = (setting.clone(), pkgbase.clone());
    let loaded = use_resource(move || {
        let (setting, pkgbase) = res_props.clone();
        async move {
            // Read so a save re-fetches: the server owns the value, and the
            // source badge has to follow what it actually stored.
            let _ = reload();
            crate::api::client()?
                .get_setting(pkgbase.as_deref(), &setting)
                .await
                .map_err(|e| e.to_string())
        }
    });

    let mut draft = use_signal(String::new);
    let mut stored = use_signal(String::new);
    let mut source = use_signal(|| SettingSource::Default);
    let busy = use_signal(|| false);
    let status = use_signal(|| Option::<(String, bool)>::None);

    // Seed the editor once the value arrives, and again after each save — but
    // only while clean. A refetch landing mid-edit today overwrites
    // in-progress typing; with the gate, typing wins and the refetch only
    // refreshes the source badge. After a save or reset the box matches what
    // was just written, so it counts as clean and the re-read still lands.
    use_effect(move || {
        if let Some(Ok(response)) = &*loaded.read_unchecked() {
            source.set(response.source);
            // `peek` borrows through a guard, so compare through it.
            if draft.peek().as_str() == stored.peek().as_str() {
                draft.set(response.value.clone());
                stored.set(response.value.clone());
            }
        }
    });

    let dirty = draft() != stored();
    let reset_title = if scoped {
        "Discard this package's copy and use the server-wide file"
    } else {
        "Discard the stored file and use the builder's own copy"
    };

    // Owns its own scope pair, cloned per call, so both buttons can share it
    // without either moving the props out. The signals are all `Copy`.
    let save_props = (setting, pkgbase);
    let save = move |value: Option<String>| {
        let (setting, pkgbase) = save_props.clone();
        run_save(
            busy, status, stored, reload, scoped, setting, pkgbase, value,
        )
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
                                onclick: {
                                    // Cloned, not moved: the Save button below
                                    // needs its own.
                                    let save = save.clone();
                                    move |_| save(None)
                                },
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
