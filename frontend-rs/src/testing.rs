//! Driving components with real events, without a browser.
//!
//! The SSR tests elsewhere render a component once and assert on the markup.
//! That covers what a component *shows*, and nothing about what it *does* — a
//! button wired to the wrong handler, or one that passes the wrong argument,
//! renders identically to a correct one. This is that missing half: click a
//! button, type in a field, and assert on what came out.
//!
//! It works because `VirtualDom::handle_event` is public. Three things have to
//! line up for it:
//!
//! 1. **Finding the element.** Events are addressed by `ElementId`, which is
//!    handed out in the mutation stream rather than being visible in the
//!    markup. [`Harness`] indexes every *dynamic* attribute it sees there, so
//!    an element can be named by one of its attributes.
//!
//!    Only dynamic ones: `class: "btn"` is baked into the compiled template and
//!    never appears as a mutation, while `class: "btn {kind}"` does. So an
//!    element is addressable when it already carries an interpolated attribute
//!    — `aria-label: "Remove {label}"` and the like — and otherwise needs one.
//!
//! 2. **Building the payload.** Listeners receive a `PlatformEventData`, which
//!    a platform-registered converter turns into `MouseData`, `FormData` and
//!    friends. There is no platform here, so [`install`] registers a converter
//!    that yields the stubs below. Only the events these tests fire are
//!    implemented; the rest panic rather than pretend.
//!
//! 3. **Flushing.** An event marks the component dirty; the re-render happens
//!    on the next `render_immediate_to_vec`, which [`Harness`] does for you so
//!    a click is followed by fresh markup and a fresh element index.
//!
//! This does not replace the browser test. Nothing here lays anything out, so
//! it cannot see a clipped column or an invisible badge — it sees wiring.

use dioxus::html::geometry::{ClientPoint, ElementPoint, PagePoint, ScreenPoint};
use dioxus::html::input_data::{MouseButton, MouseButtonSet};
use dioxus::html::point_interaction::{
    InteractionElementOffset, InteractionLocation, ModifiersInteraction, PointerInteraction,
};
use dioxus::html::{
    AnimationData, CancelData, ClipboardData, CompositionData, DragData, FileData, FocusData,
    FormData, FormValue, HasFileData, HasFormData, HasMouseData, HtmlEventConverter, ImageData,
    KeyboardData, MediaData, Modifiers, MountedData, MouseData, PlatformEventData, PointerData,
    ResizeData, ScrollData, SelectionData, ToggleData, TouchData, TransitionData, VisibleData,
    WheelData,
};
use dioxus::prelude::*;
use dioxus_core::{ComponentFunction, ElementId, Mutation};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Once;

// ---------------------------------------------------------------------------
// Event payloads
// ---------------------------------------------------------------------------

/// A click. Position and modifiers are zeroed: nothing here reads them, and a
/// stub that invented values would be inventing test input.
#[derive(Debug, Clone, Default)]
struct TestMouse;

impl InteractionLocation for TestMouse {
    fn client_coordinates(&self) -> ClientPoint {
        ClientPoint::zero()
    }
    fn screen_coordinates(&self) -> ScreenPoint {
        ScreenPoint::zero()
    }
    fn page_coordinates(&self) -> PagePoint {
        PagePoint::zero()
    }
}
impl InteractionElementOffset for TestMouse {
    fn element_coordinates(&self) -> ElementPoint {
        ElementPoint::zero()
    }
}
impl ModifiersInteraction for TestMouse {
    fn modifiers(&self) -> Modifiers {
        Modifiers::empty()
    }
}
impl PointerInteraction for TestMouse {
    fn trigger_button(&self) -> Option<MouseButton> {
        Some(MouseButton::Primary)
    }
    fn held_buttons(&self) -> MouseButtonSet {
        MouseButtonSet::empty()
    }
}
impl HasMouseData for TestMouse {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// A form event: the text in a field, or whether a box is ticked.
///
/// Both travel as `value`, because that is how dioxus carries them —
/// `FormData::checked` parses the value as a bool rather than reading a
/// separate field.
#[derive(Debug, Clone, Default)]
struct TestForm {
    value: String,
}

impl HasFileData for TestForm {
    fn files(&self) -> Vec<FileData> {
        Vec::new()
    }
}

impl HasFormData for TestForm {
    fn value(&self) -> String {
        self.value.clone()
    }
    fn valid(&self) -> bool {
        true
    }
    fn values(&self) -> Vec<(String, FormValue)> {
        Vec::new()
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ---------------------------------------------------------------------------
// Converter
// ---------------------------------------------------------------------------

struct TestConverter;

/// Every event this harness does not fire. Panicking beats returning a default:
/// a component that started listening for something new should fail loudly
/// here rather than silently receive a zeroed event.
macro_rules! unsupported {
    ($($method:ident -> $ty:ty),* $(,)?) => {
        $(
            fn $method(&self, _: &PlatformEventData) -> $ty {
                unimplemented!(
                    concat!(stringify!($method), ": add it to crate::testing when a test needs it")
                )
            }
        )*
    };
}

impl HtmlEventConverter for TestConverter {
    fn convert_mouse_data(&self, event: &PlatformEventData) -> MouseData {
        MouseData::new(event.downcast::<TestMouse>().cloned_or_default())
    }

    fn convert_form_data(&self, event: &PlatformEventData) -> FormData {
        FormData::new(event.downcast::<TestForm>().cloned_or_default())
    }

    unsupported! {
        convert_animation_data -> AnimationData,
        convert_cancel_data -> CancelData,
        convert_clipboard_data -> ClipboardData,
        convert_composition_data -> CompositionData,
        convert_drag_data -> DragData,
        convert_focus_data -> FocusData,
        convert_image_data -> ImageData,
        convert_keyboard_data -> KeyboardData,
        convert_media_data -> MediaData,
        convert_mounted_data -> MountedData,
        convert_pointer_data -> PointerData,
        convert_resize_data -> ResizeData,
        convert_scroll_data -> ScrollData,
        convert_selection_data -> SelectionData,
        convert_toggle_data -> ToggleData,
        convert_touch_data -> TouchData,
        convert_transition_data -> TransitionData,
        convert_visible_data -> VisibleData,
        convert_wheel_data -> WheelData,
    }
}

/// Small helper so a missing payload is a default rather than a panic.
trait ClonedOrDefault<T> {
    fn cloned_or_default(self) -> T;
}
impl<T: Clone + Default> ClonedOrDefault<T> for Option<&T> {
    fn cloned_or_default(self) -> T {
        self.cloned().unwrap_or_default()
    }
}

/// Register the converter, once per process.
///
/// The registry is global and the test runner is threaded, so this cannot be a
/// plain call at the top of each test.
fn install() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| dioxus::html::set_event_converter(Box::new(TestConverter)));
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A mounted component that can be clicked and typed into.
pub struct Harness {
    dom: VirtualDom,
    /// `(attribute, value)` to the element carrying it. Rebuilt after every
    /// render, because ids are handed out again as the tree changes.
    elements: HashMap<(String, String), ElementId>,
}

impl Harness {
    /// Mount a component that takes no props.
    pub fn new(component: fn() -> Element) -> Self {
        Self::mount(VirtualDom::new(component))
    }

    /// Mount a component with props.
    pub fn new_with_props<P: Clone + 'static, M: 'static>(
        component: impl ComponentFunction<P, M>,
        props: P,
    ) -> Self {
        Self::mount(VirtualDom::new_with_props(component, props))
    }

    fn mount(mut dom: VirtualDom) -> Self {
        install();
        let mutations = dom.rebuild_to_vec();
        let mut harness = Self {
            dom,
            elements: HashMap::new(),
        };
        harness.index(&mutations.edits);
        harness
    }

    fn index(&mut self, edits: &[Mutation]) {
        for edit in edits {
            if let Mutation::SetAttribute {
                name, value, id, ..
            } = edit
            {
                // The debug form is stable enough for a lookup key and avoids
                // matching on every `AttributeValue` variant.
                let value = format!("{value:?}");
                let value = value
                    .strip_prefix("Text(\"")
                    .and_then(|v| v.strip_suffix("\")"))
                    .map_or(value.clone(), ToString::to_string);
                self.elements.insert(((*name).to_string(), value), *id);
            }
        }
    }

    /// Apply pending work and re-index.
    fn settle(&mut self) {
        let mutations = self.dom.render_immediate_to_vec();
        self.index(&mutations.edits);
    }

    /// The element carrying `attribute="value"`.
    ///
    /// Panics with the available options, because the usual cause is an
    /// attribute that is static and therefore invisible here.
    fn find(&self, attribute: &str, value: &str) -> ElementId {
        self.elements
            .get(&(attribute.to_string(), value.to_string()))
            .copied()
            .unwrap_or_else(|| {
                let mut known: Vec<_> = self
                    .elements
                    .keys()
                    .map(|(a, v)| format!("{a}={v:?}"))
                    .collect();
                known.sort();
                panic!(
                    "no element with {attribute}={value:?}. \
                     Interpolated attributes present: {known:#?}"
                )
            })
    }

    fn fire(&mut self, event: &str, id: ElementId, data: Rc<dyn std::any::Any>) {
        self.dom
            .runtime()
            .handle_event(event, dioxus_core::Event::new(data, true), id);
        self.settle();
    }

    /// Click the element carrying `attribute="value"`.
    pub fn click(&mut self, attribute: &str, value: &str) {
        let id = self.find(attribute, value);
        self.fire(
            "click",
            id,
            Rc::new(PlatformEventData::new(Box::new(TestMouse))),
        );
    }

    /// Type into the element carrying `attribute="value"`.
    pub fn input(&mut self, attribute: &str, value: &str, text: &str) {
        let id = self.find(attribute, value);
        self.fire(
            "input",
            id,
            Rc::new(PlatformEventData::new(Box::new(TestForm {
                value: text.to_string(),
            }))),
        );
    }

    /// Tick or untick the element carrying `attribute="value"`.
    pub fn set_checked(&mut self, attribute: &str, value: &str, checked: bool) {
        let id = self.find(attribute, value);
        self.fire(
            "change",
            id,
            Rc::new(PlatformEventData::new(Box::new(TestForm {
                value: checked.to_string(),
            }))),
        );
    }

    /// The markup as it currently stands.
    pub fn html(&self) -> String {
        dioxus_ssr::render(&self.dom)
    }
}

#[cfg(test)]
mod tests {
    use super::Harness;
    use dioxus::prelude::*;

    #[component]
    fn Counter() -> Element {
        let mut n = use_signal(|| 0);
        let bump = "bump";
        let reset = "reset";
        rsx! {
            button { "aria-label": "{bump}", onclick: move |_| n += 1, "bump" }
            button { "aria-label": "{reset}", onclick: move |_| n.set(0), "reset" }
            span { "count={n}" }
        }
    }

    /// The harness's own reason to exist: a click reaches the handler it is
    /// wired to, and the markup that follows reflects it.
    #[test]
    fn a_click_reaches_its_own_handler() {
        let mut app = Harness::new(Counter);
        assert!(app.html().contains("count=0"));

        app.click("aria-label", "bump");
        assert!(app.html().contains("count=1"), "{}", app.html());

        app.click("aria-label", "bump");
        assert!(app.html().contains("count=2"), "{}", app.html());

        // The other button, to prove the lookup distinguishes them rather than
        // firing at whichever it found last.
        app.click("aria-label", "reset");
        assert!(app.html().contains("count=0"), "{}", app.html());
    }
}
