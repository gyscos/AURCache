//! Which architectures a package is built for.
//!
//! Shared because the same list is picked twice — once when a package is added
//! and again when it is edited — and two copies would drift the moment an
//! architecture is added.

use dioxus::prelude::*;

/// The architectures a build can target.
///
/// Whether a given package *can* build for one of these depends entirely on its
/// PKGBUILD, which is why the picker says so rather than pretending to know.
pub const ALL: [&str; 3] = ["x86_64", "aarch64", "armv7h"];

/// The default for a new package: what almost every PKGBUILD supports, and the
/// only one that needs no emulation.
pub const DEFAULT: &str = "x86_64";

/// A checkbox per architecture, reporting the whole selection on every change.
///
/// Reports the set rather than the toggle so callers hold one value; a caller
/// that had to apply add/remove events would be re-implementing this list.
#[component]
pub fn PlatformChecklist(
    selected: Vec<String>,
    onchange: EventHandler<Vec<String>>,
    #[props(default = String::from("checkbox-xs"))] size: String,
    #[props(default = false)] disabled: bool,
) -> Element {
    rsx! {
        div { class: "flex flex-col gap-1",
            for platform in ALL {
                label { key: "{platform}", class: "flex items-center gap-2 cursor-pointer",
                    input {
                        r#type: "checkbox",
                        class: "checkbox {size}",
                        disabled,
                        checked: selected.iter().any(|p| p == platform),
                        onchange: {
                            let selected = selected.clone();
                            move |e: FormEvent| {
                                let mut next = selected.clone();
                                if e.checked() {
                                    if !next.iter().any(|p| p == platform) {
                                        next.push(platform.to_string());
                                    }
                                } else {
                                    next.retain(|p| p != platform);
                                }
                                onchange.call(next);
                            }
                        },
                    }
                    span { class: "font-mono text-xs", "{platform}" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ALL, DEFAULT};

    /// The default has to be one of the offered architectures, or a new package
    /// starts out asking for something the picker cannot show.
    #[test]
    fn the_default_platform_is_one_of_the_offered_ones() {
        assert!(ALL.contains(&DEFAULT));
    }
}
