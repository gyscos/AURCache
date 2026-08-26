//! Editing a package's source files.

use crate::api::api_base;
use crate::routes::Route;
use aurcache_client::{AurCacheClient, SourceFileContent};
use dioxus::prelude::*;

/// The screen behind `/package/:pkgbase/source/:..path`.
///
/// The path arrives as segments because a source path contains slashes;
/// rejoining them is what makes a nested file linkable.
#[component]
pub fn PackageSource(pkgbase: String, path: Vec<String>) -> Element {
    let initial_path = (!path.is_empty()).then(|| path.join("/"));
    rsx! { SourceEditor { pkgbase, initial_path } }
}

// ---------------------------------------------------------------------------
// Source patch editor
//
// The server stores a package's local modifications as a diff against the
// pristine upstream source. The editor never shows that diff: it shows a file,
// you change it, and the server derives the patch. Writing the original
// content back is how a file is un-patched.
//
// The Dart original uses a plain text field — no syntax highlighting — so a
// textarea is a like-for-like replacement and needs no JS editor component.
// ---------------------------------------------------------------------------

#[component]
pub fn SourceEditor(pkgbase: String, initial_path: Option<String>) -> Element {
    let package = use_resource({
        let pkgbase = pkgbase.clone();
        move || {
            let pkgbase = pkgbase.clone();
            async move {
                crate::api::client()?
                    .get_package(&pkgbase)
                    .await
                    .map_err(|e| e.to_string())
            }
        }
    });

    let files = use_resource({
        let pkgbase = pkgbase.clone();
        move || {
            let pkgbase = pkgbase.clone();
            async move {
                let client = AurCacheClient::new(api_base(), None).map_err(|e| e.to_string())?;
                client
                    .list_source_files(&pkgbase)
                    .await
                    .map(|l| l.files)
                    .map_err(|e| e.to_string())
            }
        }
    });

    let mut selected = use_signal(|| Option::<String>::None);
    let mut loaded = use_signal(|| Option::<SourceFileContent>::None);
    // Edited text, kept separate from what was loaded so "dirty" and "revert"
    // are both expressible.
    let mut draft = use_signal(String::new);
    let mut status = use_signal(|| Option::<(String, bool)>::None);

    let open_file = move |pkgbase: String, path: String| async move {
        let Ok(client) = AurCacheClient::new(api_base(), None) else {
            return;
        };
        match client.get_source_file(&pkgbase, &path).await {
            Ok(content) => {
                // Show the patched content when there is some; otherwise the
                // pristine file. A patch that no longer applies falls back to
                // pristine and says so, rather than showing nothing.
                draft.set(
                    content
                        .patched_content
                        .clone()
                        .unwrap_or_else(|| content.original_content.clone()),
                );
                loaded.set(Some(content));
                selected.set(Some(path));
                status.set(None);
            }
            Err(e) => status.set(Some((e.to_string(), false))),
        }
    };

    // Open the file named in the URL, if any.
    use_effect({
        let pkgbase = pkgbase.clone();
        let initial_path = initial_path.clone();
        move || {
            if let Some(path) = initial_path.clone()
                && selected.peek().is_none()
            {
                let pkgbase = pkgbase.clone();
                spawn(async move { open_file(pkgbase, path).await });
            }
        }
    });

    let dirty = loaded.read().as_ref().is_some_and(|c| {
        let shown = c
            .patched_content
            .clone()
            .unwrap_or_else(|| c.original_content.clone());
        draft() != shown
    });
    let patched = loaded
        .read()
        .as_ref()
        .is_some_and(|c| c.patched_content.is_some());

    rsx! {
        div { class: "space-y-4",
        // The package's own header stays, so editing its sources happens in
        // sight of what is being edited rather than on a bare page. It carries
        // the trail, so the breadcrumb sits in the same place on every
        // package-scoped page.
        match &*package.read_unchecked() {
            Some(Ok(pkg)) => rsx! {
                crate::screens::PackageHeader {
                    pkg: pkg.clone(),
                    trail: vec![("Sources".to_string(), None)],
                }
            },
            // Its absence must not block the editor: the files are what this
            // page is for, and they load independently.
            _ => rsx! {},
        }
        crate::source_editor::SourcePane {
            title: pkgbase.clone(),
            files: files.read_unchecked().clone(),
            selected: selected(),
            onselect: {
                let pkgbase = pkgbase.clone();
                move |path: String| {
                    let pkgbase = pkgbase.clone();
                    spawn(async move { open_file(pkgbase, path).await });
                }
            },
            draft,
            dirty,
            patched,
            actions: rsx! {
                button {
                    class: "btn btn-ghost btn-sm",
                    disabled: !dirty,
                    onclick: move |_| {
                        if let Some(c) = loaded.read().as_ref() {
                            draft.set(c.original_content.clone());
                        }
                    },
                    "Revert to upstream"
                }
                button {
                    class: "btn btn-sm",
                    disabled: !dirty,
                    onclick: {
                        let pkgbase = pkgbase.clone();
                        move |_| {
                            let pkgbase = pkgbase.clone();
                            async move {
                                let Some(path) = selected() else { return };
                                let Ok(client) = AurCacheClient::new(api_base(), None) else { return };
                                match client.put_source_file(&pkgbase, &path, &draft()).await {
                                    Ok(()) => {
                                        navigator().push(Route::Package { pkgbase: pkgbase.clone() });
                                    }
                                    Err(e) => status.set(Some((e.to_string(), false))),
                                }
                            }
                        }
                    },
                    "Save"
                }
                button {
                    class: "btn btn-primary btn-sm",
                    disabled: !dirty,
                    // Editing a PKGBUILD is nearly always a prelude to building
                    // it; without this the next step is a save, a navigation
                    // back, and a second button.
                    onclick: {
                        let pkgbase = pkgbase.clone();
                        move |_| {
                            let pkgbase = pkgbase.clone();
                            async move {
                                let Some(path) = selected() else { return };
                                let Ok(client) = AurCacheClient::new(api_base(), None) else { return };
                                // The rebuild is only queued if the save
                                // worked: rebuilding the old source would
                                // report success for a change never stored.
                                match client.put_source_file(&pkgbase, &path, &draft()).await {
                                    Err(e) => status.set(Some((e.to_string(), false))),
                                    Ok(()) => match client
                                        .update_package(&pkgbase, &aurcache_client::UpdatePackageRequest { force: true })
                                        .await
                                    {
                                        // Back to the package, which is where
                                        // the build just queued will appear.
                                        Ok(_) => {
                                            navigator().push(Route::Package { pkgbase: pkgbase.clone() });
                                        }
                                        // Stay put on a partial failure: the
                                        // edit is saved but the build is not
                                        // queued, and leaving would hide that.
                                        Err(e) => status.set(Some((
                                            format!("Saved, but the rebuild could not be queued: {e}"),
                                            false,
                                        ))),
                                    },
                                }
                            }
                        }
                    },
                    "Save & Rebuild"
                }
            },
            notices: rsx! {
                // A patch that no longer applies is shown, not hidden: the
                // editor is falling back to pristine content and the user needs
                // to know before saving over it.
                if let Some(err) = loaded.read().as_ref().and_then(|c| c.patch_error.clone()) {
                    div { class: "alert alert-warning text-sm",
                        span { "Stored patch no longer applies: {err}. Showing upstream content." }
                    }
                }
                if let Some((msg, ok)) = status() {
                    div {
                        class: if ok { "alert alert-success text-sm" } else { "alert alert-error text-sm" },
                        span { "{msg}" }
                    }
                }
            },
        }
        }
    }
}
