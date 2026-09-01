//! Server-wide settings: what they are, what they resolve to, and where each
//! value came from.
//!
//! The value alone is not enough to act on. A number shown without its origin
//! looks editable even when an environment variable is pinning it, and looks
//! stored even when it is only the built-in default — so every row says which
//! of the two it is, and offers a reset only when there is something stored to
//! reset.

use crate::dates::{Clock, DateOrder, DateParts, DateStyle, render};
use crate::listing::ListHeader;
use crate::routes::Route;
use aurcache_client::{ApplicationSettings, Setting, SettingSource};
use dioxus::prelude::*;

#[component]
pub fn Settings() -> Element {
    let mut settings = use_resource(|| async move {
        crate::api::client()?
            .settings(None)
            .await
            .map_err(|e| e.to_string())
    });

    // One banner for the whole page rather than one per row: a save either
    // worked or it did not, and the row already shows the value that resulted.
    let mut status = use_signal(|| Option::<(String, bool)>::None);

    // Every write goes through here so the page always re-reads afterwards.
    // A setting's stored value is not always the value that comes back — the
    // server parses and normalises it, and an environment variable outranks it
    // entirely — so echoing the input locally would show a value the server
    // does not hold.
    let save = move |(setting, value): (Setting, Option<String>)| async move {
        let key = setting.meta().key;
        let client = match crate::api::client() {
            Ok(client) => client,
            Err(e) => {
                status.set(Some((e, false)));
                return;
            }
        };
        let result = match &value {
            Some(value) => client.patch_setting(None, key, value).await,
            None => client.reset_setting(None, key).await,
        };
        match result {
            Ok(()) => {
                status.set(Some((
                    match value {
                        Some(_) => format!("Saved {key}."),
                        None => format!("Reset {key} to its default."),
                    },
                    true,
                )));
                settings.restart();
            }
            Err(e) => status.set(Some((format!("Could not save {key}: {e}"), false))),
        }
    };

    rsx! {
        div { class: "space-y-4",
            ListHeader { title: "Settings" }

            if let Some((message, ok)) = status() {
                div {
                    class: if ok { "alert alert-success text-sm" } else { "alert alert-error text-sm" },
                    span { "{message}" }
                }
            }

            match &*settings.read_unchecked() {
                None => rsx! { span { class: "loading loading-spinner" } },
                Some(Err(e)) => rsx! {
                    div { class: "alert alert-error", span { "Could not load settings: {e}" } }
                },
                Some(Ok(loaded)) => rsx! {
                    SettingsSections { settings: loaded.clone(), save }
                },
            }
        }
    }
}

#[component]
fn SettingsSections(
    settings: ApplicationSettings,
    save: EventHandler<(Setting, Option<String>)>,
) -> Element {
    rsx! {
        // Two columns once there is genuinely room. A single column of settings
        // on a 1920-wide screen leaves each row a label at one edge and a
        // control at the other with a metre of nothing between — but splitting
        // earlier than `2xl` makes each column too narrow for a label and a
        // control side by side, which is worse than the stretch.
        //
        // Columns, not a grid: the sections are of very different heights, and
        // a two-column grid would pad every short card out to match the tall
        // one beside it.
        div { class: "2xl:columns-2 2xl:gap-4",
        Section { title: "General",
            SettingRow {
                setting: Setting::VersionCheckInterval,
                label: "Version check interval",
                description: "How often to look for new AUR and git versions, in seconds.",
                value: settings.version_check_interval.value.to_string(),
                source: settings.version_check_interval.source,
                editor: Editor::Number,
                save,
            }
            SettingRow {
                setting: Setting::AutoUpdateInterval,
                label: "Auto-update schedule",
                description: "Cron expression, including seconds, for scheduled rebuilds. Empty disables it.",
                // Distinct from a stored empty string, which is what
                // "disabled" is: the field renders empty either way, and the
                // source badge is what tells the two apart.
                value: settings.auto_update_interval.value.clone().unwrap_or_default(),
                source: settings.auto_update_interval.source,
                editor: Editor::Text { placeholder: "0 0 3 * * *".to_string(), wide: false },
                save,
            }
            SettingRow {
                setting: Setting::BuildOnNewVersion,
                label: "Build on new version",
                description: "Queue a rebuild the moment a new version is detected, without waiting for the schedule above.",
                value: settings.build_on_new_version.value.to_string(),
                source: settings.build_on_new_version.source,
                editor: Editor::Toggle,
                save,
            }
        }

        Section { title: "Default date format",
            DateFormatRows {
                value: settings.date_format.value.clone(),
                source: settings.date_format.source,
                save,
            }
        }

        Section { title: "Builder",
            div { class: "py-2",
                Link { class: "link link-primary text-sm", to: Route::ConfigFiles {},
                    "Config files"
                }
                p { class: "text-xs opacity-60",
                    "Edit the makepkg.conf and pacman.conf used by builds."
                }
            }
            SettingRow {
                setting: Setting::CpuLimit,
                label: "CPU limit",
                description: "µCPUs available to each build. 0 is unlimited.",
                value: settings.cpu_limit.value.to_string(),
                source: settings.cpu_limit.source,
                editor: Editor::Number,
                save,
            }
            SettingRow {
                setting: Setting::MemoryLimit,
                label: "Memory limit",
                description: "Bytes of memory each build may use. -1 is unlimited.",
                value: settings.memory_limit.value.to_string(),
                source: settings.memory_limit.source,
                editor: Editor::Number,
                save,
            }
            SettingRow {
                setting: Setting::MaxConcurrentBuilds,
                label: "Job concurrency",
                description: "How many builds may run at once.",
                value: settings.max_concurrent_builds.value.to_string(),
                source: settings.max_concurrent_builds.source,
                editor: Editor::Number,
                save,
            }
            SettingRow {
                setting: Setting::JobTimeout,
                label: "Job timeout",
                description: "How long a single build may run before it is abandoned, in seconds.",
                value: settings.job_timeout.value.to_string(),
                source: settings.job_timeout.source,
                editor: Editor::Number,
                save,
            }
        }

        Section { title: "Advanced",
            SettingRow {
                setting: Setting::BuilderImage,
                label: "Builder image",
                description: "Container image builds run in.",
                value: settings.builder_image.value.clone(),
                source: settings.builder_image.source,
                editor: Editor::Text { placeholder: String::new(), wide: true },
                save,
            }
        }

        ApiAccessSection {}
        crate::screens::backup::BackupSection {}
        }
    }
}

#[component]
pub fn Section(title: String, children: Element) -> Element {
    rsx! {
        // `break-inside-avoid` and a margin rather than a gap: the sections are
        // laid out in CSS columns (see `SettingsSections`), which flow rather
        // than grid, so a card must be told not to be split across the fold.
        div { class: "card bg-base-100 shadow-xl break-inside-avoid mb-4",
            div { class: "card-body",
                h2 { class: "card-title text-base", "{title}" }
                div { class: "divide-y divide-base-300", {children} }
            }
        }
    }
}

/// How a setting's value is edited.
///
/// Not how it is *typed*: the server parses every value from a string and owns
/// the rules, so this only decides which control a person is given.
#[derive(Clone, PartialEq)]
enum Editor {
    /// `wide` for values that are long by nature, such as an image reference:
    /// the default box shows only the registry and hides the tag.
    Text {
        placeholder: String,
        wide: bool,
    },
    Number,
    Toggle,
}

impl Editor {
    /// Whether this control wants a line to itself.
    fn is_stacked(&self) -> bool {
        matches!(self, Self::Text { wide: true, .. })
    }
}

/// How a row arranges its label and its control.
fn row_layout(stacked: bool) -> &'static str {
    if stacked {
        "flex-col"
    } else {
        "flex-wrap items-start justify-between"
    }
}

/// A text field wide enough for what it holds.
///
/// The wide case is on its own line already (see [`Editor::is_stacked`]), so it
/// takes the row rather than a fixed width.
fn text_width(wide: bool) -> &'static str {
    if wide { "w-full" } else { "w-64" }
}

/// Whether this scope holds a stored value that a reset could clear.
///
/// `Default` has nothing stored, and `Env` is not stored here at all — an
/// environment variable outranks the database, so deleting the row would
/// change nothing while appearing to.
pub fn is_stored(source: SettingSource) -> bool {
    matches!(source, SettingSource::Global | SettingSource::Package)
}

/// The badge beside a setting's name.
///
/// Only `Default` gets one. `Env` says more than a badge has room for and is
/// rendered as [`EnvNote`] under the description instead; a stored value is the
/// ordinary case and needs no label.
#[component]
fn SourceNote(source: SettingSource) -> Element {
    match source {
        SettingSource::Default => rsx! {
            span { class: "badge badge-outline badge-sm whitespace-nowrap", "default" }
        },
        SettingSource::Env | SettingSource::Global | SettingSource::Package => rsx! {},
    }
}

/// A help marker naming the environment variable behind a setting.
///
/// Every row carries one, set or not: this page is where someone looks to find
/// out what a deployment *can* pin, and a name that only appears once the
/// variable is already set is documentation you cannot reach when you need it.
/// On hover rather than in the row, because eleven permanently visible variable
/// names crowd out the settings they annotate.
///
/// The name comes from the setting's own metadata rather than being written out
/// again here, so it cannot point at a variable the server does not read.
#[component]
fn EnvHelp(setting: Setting) -> Element {
    let Some(name) = setting.meta().env_name else {
        return rsx! {};
    };
    rsx! {
        span {
            class: "cursor-help text-xs opacity-40 hover:opacity-80 transition-opacity",
            title: "Set ${name} to pin this from the environment",
            "?"
        }
    }
}

/// What to do about a setting an environment variable has already taken over.
///
/// Stays visible rather than hiding behind [`EnvHelp`]: the field beside it is
/// disabled, and without this the row is a dead end. Saying *how* to take it
/// back matters more than saying it happened — the variable is in a compose
/// file or a unit file, nowhere this page can reach.
#[component]
fn EnvNote(setting: Setting, source: SettingSource) -> Element {
    if source != SettingSource::Env {
        return rsx! {};
    }
    let Some(name) = setting.meta().env_name else {
        return rsx! {};
    };
    rsx! {
        p { class: "text-xs text-warning max-w-prose",
            "unset ${name} to allow control here"
        }
    }
}

/// The parts of a row that do not depend on what is being edited: the name,
/// the source badge, the description, and the environment-variable notes.
///
/// Shared so that one facet of a date and a job timeout read as the same kind
/// of thing, which they are.
#[component]
fn RowShell(
    setting: Setting,
    label: String,
    description: String,
    source: SettingSource,
    /// Put the control on its own line under the label. For values too long to
    /// sit beside their name without squeezing it into a column of single
    /// words — a container image reference is the case that forced this.
    #[props(default = false)]
    stacked: bool,
    children: Element,
) -> Element {
    rsx! {
        div {
            class: "py-3 flex gap-3 {row_layout(stacked)}",
            div { class: "min-w-0 flex-1",
                div { class: "flex items-center gap-2 flex-wrap",
                    span { class: "font-medium text-sm", "{label}" }
                    EnvHelp { setting }
                    SourceNote { source }
                }
                if !description.is_empty() {
                    p { class: "text-xs opacity-60 max-w-prose", "{description}" }
                }
                EnvNote { setting, source }
            }
            div { class: "flex items-center gap-2 min-w-0", {children} }
        }
    }
}

#[component]
fn SettingRow(
    setting: Setting,
    label: String,
    description: String,
    value: String,
    source: SettingSource,
    editor: Editor,
    save: EventHandler<(Setting, Option<String>)>,
) -> Element {
    // An environment variable wins over anything stored, so an editable field
    // would accept a change that has no effect.
    let locked = source == SettingSource::Env;
    let mut draft = use_signal(|| value.clone());

    // The row re-renders with a fresh value after every save, and the draft has
    // to follow it — otherwise a value the server normalised (or refused) keeps
    // showing the text that was typed.
    use_effect(use_reactive(&value, move |value| draft.set(value)));

    let dirty = draft() != value;

    rsx! {
        RowShell { setting, label, description, source, stacked: editor.is_stacked(),
            {
                let control = match editor {
                    Editor::Toggle => rsx! {
                        input {
                            r#type: "checkbox",
                            class: "toggle toggle-primary",
                            disabled: locked,
                            checked: value == "true",
                            onchange: move |e: FormEvent| {
                                save.call((setting, Some(e.checked().to_string())));
                            },
                        }
                    },
                    Editor::Number => rsx! {
                        input {
                            r#type: "number",
                            class: "input input-bordered input-sm w-40",
                            disabled: locked,
                            value: "{draft}",
                            oninput: move |e| draft.set(e.value()),
                            // Enter saves, because a field with a visible Save
                            // button next to it still gets Enter pressed at it.
                            onkeydown: move |e: KeyboardEvent| {
                                if e.key() == Key::Enter {
                                    save.call((setting, Some(draft())));
                                }
                            },
                        }
                    },
                    Editor::Text { placeholder, wide } => rsx! {
                        input {
                            r#type: "text",
                            class: "input input-bordered input-sm max-w-full font-mono text-xs {text_width(wide)}",
                            disabled: locked,
                            placeholder: "{placeholder}",
                            value: "{draft}",
                            oninput: move |e| draft.set(e.value()),
                            onkeydown: move |e: KeyboardEvent| {
                                if e.key() == Key::Enter {
                                    save.call((setting, Some(draft())));
                                }
                            },
                        }
                    },
                };
                rsx! {
                    {control}
                    // Only shown once there is something to save. A
                    // permanently visible Save button on every row reads as a
                    // page full of unsaved changes.
                    if dirty && !locked {
                        button {
                            class: "btn btn-primary btn-sm",
                            onclick: move |_| save.call((setting, Some(draft()))),
                            "Save"
                        }
                    }
                    ResetSlot { setting, source, save }
                }
            }
        }
    }
}

/// The Reset button, in a slot that is held open whether or not this row has
/// anything to reset — otherwise one stored setting knocks its own field out of
/// the column every other field lines up in.
#[component]
fn ResetSlot(
    setting: Setting,
    source: SettingSource,
    save: EventHandler<(Setting, Option<String>)>,
) -> Element {
    rsx! {
        div { class: "w-16 shrink-0 flex justify-end",
            if is_stored(source) {
                button {
                    class: "btn btn-ghost btn-sm",
                    title: "Discard the stored value and go back to the default",
                    onclick: move |_| save.call((setting, None)),
                    "Reset"
                }
            }
        }
    }
}

/// The server-wide date default, as three settings rather than one.
///
/// They are stored as a single string — `ymd-pad-24` — but nobody thinks in
/// composite ids: the field order, the clock and the zero padding are three
/// independent choices, and presenting them as one control with three knobs
/// inside made them look like a sub-menu of something else. Each row writes the
/// whole id back with its own facet replaced.
///
/// This is the *default*: what someone sees before they choose. A browser that
/// has already chosen keeps its choice, which is what the note says — changing
/// this and seeing nothing happen is otherwise indistinguishable from a bug.
#[component]
fn DateFormatRows(
    value: String,
    source: SettingSource,
    save: EventHandler<(Setting, Option<String>)>,
) -> Element {
    let locked = source == SettingSource::Env;
    let style = DateStyle::from_id(&value);
    // One reset for the three rows, because there is one stored value behind
    // them. Three would each undo all three.
    let resettable = is_stored(source);

    // Rendered against a fixed instant so the preview reads as an example
    // rather than as a pattern.
    let sample = DateParts {
        year: 2026,
        month: 8,
        day: 6,
        hour: 14,
        minute: 5,
    };

    rsx! {
        RowShell {
            setting: Setting::DateFormat,
            label: "Date order",
            description: "",
            source,
            select {
                class: "select select-bordered select-sm w-48",
                disabled: locked,
                onchange: move |e: FormEvent| {
                    let order = DateOrder::ALL
                        .into_iter()
                        .find(|o| o.id() == e.value())
                        .unwrap_or(DateOrder::Ymd);
                    save.call((Setting::DateFormat, Some(DateStyle { order, ..style }.id())));
                },
                for order in DateOrder::ALL {
                    option {
                        key: "{order.id()}",
                        value: order.id(),
                        selected: style.order == order,
                        "{order.label()}"
                    }
                }
            }
        }

        RowShell {
            setting: Setting::DateFormat,
            label: "Time",
            description: "",
            source,
            select {
                class: "select select-bordered select-sm w-48",
                disabled: locked,
                onchange: move |e: FormEvent| {
                    let clock = Clock::ALL
                        .into_iter()
                        .find(|c| c.id() == e.value())
                        .unwrap_or(Clock::H24);
                    save.call((Setting::DateFormat, Some(DateStyle { clock, ..style }.id())));
                },
                for clock in Clock::ALL {
                    option {
                        key: "{clock.id()}",
                        value: clock.id(),
                        selected: style.clock == clock,
                        "{clock.label()}"
                    }
                }
            }
        }

        RowShell {
            setting: Setting::DateFormat,
            label: "Leading zeros",
            description: "Pad the day and month to two digits.",
            source,
            input {
                r#type: "checkbox",
                class: "toggle toggle-primary",
                disabled: locked,
                checked: style.pad,
                onchange: move |e: FormEvent| {
                    let pad = e.checked();
                    save.call((Setting::DateFormat, Some(DateStyle { pad, ..style }.id())));
                },
            }
        }

        div { class: "py-3 flex items-center justify-between gap-3 flex-wrap",
            div { class: "min-w-0",
                p { class: "text-xs opacity-60 max-w-prose",
                    "Used for anyone who has not picked a format of their own. Your browser's choice, under Preferences, overrides this for you."
                }
                span { class: "text-xs opacity-50 font-mono", {render(sample, style)} }
            }
            if resettable && !locked {
                button {
                    class: "btn btn-ghost btn-sm",
                    title: "Discard the stored date format and go back to the default",
                    onclick: move |_| save.call((Setting::DateFormat, None)),
                    "Reset"
                }
            }
        }
    }
}

/// The personal API token, which the CLI and any script authenticate with.
#[component]
fn ApiAccessSection() -> Element {
    let user = use_resource(|| async move {
        crate::api::client()?
            .user_info()
            .await
            .map_err(|e| e.to_string())
    });

    // Held only until the page is left. The server stores a hash, so this is
    // the one and only time the token can be read.
    let mut issued = use_signal(|| Option::<String>::None);
    let mut error = use_signal(|| Option::<String>::None);

    let username = match &*user.read_unchecked() {
        Some(Ok(info)) => info.username.clone(),
        _ => None,
    };

    // Signed out there is no token to hold, so the section would be a button
    // that cannot work.
    let Some(username) = username else {
        return rsx! {};
    };

    rsx! {
        Section { title: "API access",
            div { class: "py-3 space-y-2",
                p { class: "text-sm",
                    "Personal API token for "
                    span { class: "font-mono", "{username}" }
                    "."
                }
                p { class: "text-xs opacity-60",
                    "Generating a token replaces any existing one, which stops working immediately."
                }
                button {
                    class: "btn btn-sm",
                    onclick: move |_| async move {
                        error.set(None);
                        match crate::api::client() {
                            Err(e) => error.set(Some(e)),
                            Ok(client) => match client.regenerate_api_token().await {
                                Ok(response) => issued.set(Some(response.token)),
                                Err(e) => error.set(Some(e.to_string())),
                            },
                        }
                    },
                    "Generate new token"
                }

                if let Some(token) = issued() {
                    div { class: "alert alert-warning text-sm flex-col items-start gap-1",
                        span { "Copy this now — it is not shown again." }
                        code { class: "font-mono text-xs break-all", "{token}" }
                    }
                }
                if let Some(e) = error() {
                    div { class: "alert alert-error text-sm", span { "{e}" } }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Editor, SettingRow, is_stored};
    use aurcache_client::{Setting, SettingSource};
    use dioxus::prelude::*;

    /// An `EventHandler` can only be built inside a running runtime, so the row
    /// is rendered through a host component rather than by handing it props
    /// from outside.
    #[component]
    fn Harness(source: SettingSource, editor: Editor) -> Element {
        rsx! {
            SettingRow {
                setting: Setting::CpuLimit,
                label: "CPU limit",
                description: "µCPUs",
                value: "4",
                source,
                editor,
                save: move |_| {},
            }
        }
    }

    fn row(source: SettingSource, editor: Editor) -> String {
        let mut dom = VirtualDom::new_with_props(Harness, HarnessProps { source, editor });
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    /// Reset clears a stored row. `Default` has no row to clear, and `Env` is
    /// not stored in the database at all — offering Reset there would delete
    /// nothing while implying the value would change.
    #[test]
    fn only_a_stored_value_can_be_reset() {
        assert!(is_stored(SettingSource::Global));
        assert!(is_stored(SettingSource::Package));
        assert!(!is_stored(SettingSource::Default));
        assert!(!is_stored(SettingSource::Env));
    }

    /// An environment variable outranks anything stored, so an editable field
    /// would take a change that the next read silently discards.
    #[test]
    fn an_env_locked_setting_cannot_be_edited() {
        let html = row(SettingSource::Env, Editor::Number);
        assert!(
            html.contains("disabled"),
            "input should be disabled: {html}"
        );
        assert!(
            !html.contains(">Reset<"),
            "reset would clear nothing: {html}"
        );
        // Named from the setting's own metadata, so the banner cannot point at
        // a variable the server does not read.
        assert!(
            html.contains("CPU_LIMIT"),
            "should name the variable: {html}"
        );
    }

    /// The two unstored sources look identical in the value column — the badge
    /// is the only thing that separates "nobody set this" from "the deployment
    /// set this".
    #[test]
    fn an_unset_setting_says_it_is_the_default() {
        let html = row(SettingSource::Default, Editor::Number);
        assert!(html.contains("default"), "{html}");
        assert!(!html.contains(">Reset<"), "nothing stored to reset: {html}");
    }

    /// The variable a deployment would use to pin this setting is documented
    /// on the row whether or not anything has set it — a name that appears only
    /// once it is too late to be useful is not documentation. It lives in hover
    /// text, so it is present in the markup without occupying the row.
    #[test]
    fn a_settings_environment_variable_is_always_named() {
        for source in [
            SettingSource::Default,
            SettingSource::Global,
            SettingSource::Package,
        ] {
            let html = row(source, Editor::Number);
            assert!(
                html.contains(r#"title="Set $CPU_LIMIT to pin this from the environment""#),
                "{source:?}: {html}"
            );
            assert!(
                !html.contains("unset $CPU_LIMIT"),
                "{source:?} is not pinned, so nothing needs unsetting: {html}"
            );
        }
    }

    /// A stored value is the only case with something to undo.
    #[test]
    fn a_stored_setting_offers_a_reset() {
        let html = row(SettingSource::Global, Editor::Number);
        assert!(html.contains(">Reset<"), "{html}");
        assert!(!html.contains("disabled"), "{html}");
    }

    /// Each editor renders its own control, so a row cannot silently fall back
    /// to a text box for a boolean.
    #[test]
    fn each_editor_renders_its_own_control() {
        let text = row(
            SettingSource::Global,
            Editor::Text {
                placeholder: "0 0 3 * * *".to_string(),
                wide: false,
            },
        );
        assert!(text.contains("0 0 3 * * *"), "{text}");

        let number = row(SettingSource::Global, Editor::Number);
        assert!(number.contains(r#"type="number""#), "{number}");

        let toggle = row(SettingSource::Global, Editor::Toggle);
        assert!(toggle.contains("toggle"), "{toggle}");
        assert!(toggle.contains(r#"type="checkbox""#), "{toggle}");
    }
}
