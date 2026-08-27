//! Tests that click and type, against a real browser and a real server.
//!
//! The rest of the suite renders components in isolation and asserts on the
//! markup. That cannot see what a control *does* — a search box bound one way
//! instead of two renders identically on first paint — and it cannot see the
//! URL, which is not markup at all.
//!
//! These run against the stack `scripts/test-frontend.sh` brings up: a server
//! with the fixture data and the built frontend served beside it. They are
//! `#[ignore]`d so a bare `cargo test` stays fast and green without one; the
//! script passes `--ignored` once its ports are answering.
//!
//! WebDriver rather than the devtools protocol, after measuring both. Two
//! things decided it. `thirtyfour` manages the browser process itself —
//! resolving the local Chrome, fetching a matching chromedriver, and shutting
//! both down — where the CDP client needed the script to start a browser on a
//! fixed port, preflight it, and trap its cleanup. And `chromiumoxide`'s
//! `Page::goto` cannot do a fragment-only navigation: it waits for a
//! navigation lifecycle event that a same-document hash change never emits, so
//! `/packages#neofetch` timed out 30 times out of 30. Since this frontend puts
//! its search term in the fragment, that is not a corner we can avoid.
//!
//! Both were equally reliable once used correctly (30/30 each); this one is
//! reliable with less around it.

// Host only. The crate ships as wasm, and `--all-targets` would otherwise try
// to build this — and tokio's mio — for a target it cannot serve.
#![cfg(not(target_arch = "wasm32"))]

use std::time::Duration;
use thirtyfour::prelude::*;

/// How long to wait for the page to reach an expected state.
const PATIENCE: Duration = Duration::from_secs(10);

/// Where the frontend is served. The script exports this; the default matches
/// what it uses, so a stack brought up by hand works too.
fn ui_base() -> String {
    std::env::var("AURCACHE_UI").unwrap_or_else(|_| "http://localhost:8099".to_string())
}

/// A value as a JavaScript literal, so page scripts can be built by hand
/// without worrying about quoting.
fn json(value: &str) -> String {
    serde_json::to_string(value).expect("a string is always serialisable")
}

/// A browser session for the run.
struct Session {
    driver: WebDriver,
}

impl Session {
    /// Start Chrome. thirtyfour downloads and supervises the matching
    /// chromedriver itself, so nothing outside has to.
    async fn start() -> Self {
        let mut caps = DesiredCapabilities::chrome();
        caps.set_headless().expect("headless");
        caps.add_arg("--no-sandbox").expect("no-sandbox");
        caps.add_arg("--disable-gpu").expect("disable-gpu");
        let driver = WebDriver::managed(caps).await.expect("start chrome");
        Self { driver }
    }

    /// Go to a route and wait for the app to mount.
    ///
    /// Local storage is cleared afterwards: this frontend keeps its theme and
    /// date format there, and one scenario's preferences must not colour the
    /// next.
    async fn open(&self, path: &str) {
        self.driver
            .goto(format!("{}{path}", ui_base()))
            .await
            .expect("goto");
        self.wait_for(".drawer-side").await;
        let _: bool = self
            .eval("return (localStorage.clear(), true);".to_string())
            .await;
    }

    /// Evaluate JS in the page, returning `T`.
    ///
    /// Everything below goes through here rather than through element handles.
    /// A handle names a node, and this app re-renders as you type — a git URL
    /// makes the ref and subfolder fields appear — so a handle taken a moment
    /// ago can point at a node no longer in the document.
    async fn eval<T: serde::de::DeserializeOwned + Default>(&self, script: String) -> T {
        match self.driver.execute(script, Vec::new()).await {
            Ok(ret) => ret.convert().unwrap_or_default(),
            Err(_) => T::default(),
        }
    }

    /// Poll a page script until it reports true, or fail showing the page.
    async fn wait_for_script(&self, what: &str, script: String) {
        let deadline = std::time::Instant::now() + PATIENCE;
        loop {
            if self.eval::<bool>(script.clone()).await {
                return;
            }
            if std::time::Instant::now() > deadline {
                panic!(
                    "timed out waiting for {what}. Page was:\n{}",
                    self.text().await
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn wait_for(&self, selector: &str) {
        self.wait_for_script(
            &format!("{selector:?} to appear"),
            format!(
                "return document.querySelector({}) !== null;",
                json(selector)
            ),
        )
        .await;
    }

    /// Wait until `check` holds for the page's text, or fail showing it.
    async fn wait_until(&self, what: &str, check: impl Fn(&str) -> bool) {
        let deadline = std::time::Instant::now() + PATIENCE;
        loop {
            let text = self.text().await;
            if check(&text) {
                return;
            }
            if std::time::Instant::now() > deadline {
                panic!("timed out waiting for {what}. Page was:\n{text}");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn text(&self) -> String {
        self.eval("return document.body.innerText;".to_string())
            .await
    }

    /// Put `text` in a field, the way a person would as far as the app can tell.
    ///
    /// Assigning the value and dispatching `input` is what the handler reads —
    /// it takes the value off the event's target — and unlike synthesised key
    /// presses it cannot be interrupted half way by a re-render.
    async fn type_into(&self, selector: &str, text: &str) {
        self.wait_for(selector).await;
        let script = format!(
            "const el = document.querySelector({}); if (!el) return false; \
              el.focus(); el.value = {}; \
              el.dispatchEvent(new Event('input', {{ bubbles: true }})); return true;",
            json(selector),
            json(text),
        );
        assert!(
            self.eval::<bool>(script).await,
            "could not type into {selector:?}"
        );
    }

    async fn click(&self, selector: &str) {
        self.wait_for_script(
            &format!("{selector:?} to click"),
            format!(
                "const el = document.querySelector({}); if (!el) return false; \
                  el.click(); return true;",
                json(selector)
            ),
        )
        .await;
    }

    /// Click the element matching `selector` whose text is `text`.
    ///
    /// CSS cannot select on text, and the alternative — labelling buttons for
    /// the tests' benefit — would put scaffolding in the markup people use.
    async fn click_labelled(&self, selector: &str, text: &str) {
        self.wait_for_script(
            &format!("a {selector} labelled {text:?}"),
            format!(
                "const el = [...document.querySelectorAll({})] \
                  .find(e => e.textContent.trim() === {}); \
                  if (!el) return false; el.click(); return true;",
                json(selector),
                json(text),
            ),
        )
        .await;
    }

    /// The value of an input, for checking that typing landed where intended.
    async fn value_of(&self, selector: &str) -> String {
        self.eval(format!(
            "return document.querySelector({})?.value ?? \"\";",
            json(selector)
        ))
        .await
    }

    async fn url(&self) -> String {
        self.driver
            .current_url()
            .await
            .map(|u| u.to_string())
            .unwrap_or_default()
    }

    /// End the session, which stops both Chrome and the driver.
    async fn stop(self) {
        let _ = self.driver.quit().await;
    }
}

/// The scenarios, in one run over one session.
///
/// One `#[tokio::test]` rather than several: starting a browser per test is
/// both slower and, in the CDP client, where the flakiness lived. The trade is
/// coarser reporting — a failure names the scenario in its message rather than
/// in the test name.
#[tokio::test]
#[ignore = "needs the stack from scripts/test-frontend.sh"]
async fn interactions() {
    let session = Session::start().await;

    filtering_narrows_the_list_and_updates_the_url(&session).await;
    a_linked_search_arrives_applied(&session).await;
    one_queued_package_can_be_taken_back(&session).await;

    session.stop().await;
}

/// Typing in the filter narrows the list *and* updates the address bar.
///
/// Both halves are invisible to a rendering test: a box bound one way still
/// shows what was typed, and the URL is not part of the markup.
async fn filtering_narrows_the_list_and_updates_the_url(session: &Session) {
    session.open("/packages").await;
    session
        .wait_until("the list to load", |t| t.contains("neofetch"))
        .await;

    session.type_into("input[type=search]", "neofetch").await;

    session
        .wait_until("the list to narrow", |t| {
            t.contains("neofetch") && !t.contains("visual-studio-code-bin")
        })
        .await;

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if session.url().await.ends_with("#neofetch") {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "URL never picked up the search: {}",
            session.url().await
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A search in the URL is applied on arrival, which is what makes one
/// shareable. The counterpart to the scenario above.
async fn a_linked_search_arrives_applied(session: &Session) {
    session.open("/packages#neofetch").await;
    session
        .wait_until("the linked search to apply", |t| {
            t.contains("neofetch") && !t.contains("visual-studio-code-bin")
        })
        .await;
}

/// Queueing two packages and taking one back off again.
///
/// Two, deliberately: every chip's ✕ looks the same and sits in the same place,
/// so a handler capturing the wrong label is invisible in the markup — and with
/// one chip queued, removing "the first" and removing "that one" are the same
/// act. This test slept through exactly that until a mutation caught it.
async fn one_queued_package_can_be_taken_back(session: &Session) {
    session.open("/packages/add").await;

    let entry = ".modal-box input[type=text]";
    for repo in ["one", "two"] {
        let url = format!("https://github.com/user/{repo}.git");
        session.type_into(entry, &url).await;
        assert_eq!(
            session.value_of(entry).await,
            url,
            "typing did not land in the entry field"
        );
        session
            .wait_until("the git fields", |t| t.contains("Subfolder"))
            .await;
        session
            .click_labelled(".modal-box button", "Add to list")
            .await;
        session
            .wait_until("the queued chip", |t| t.contains(&format!("{repo}.git")))
            .await;
    }

    session
        .click("button[aria-label^='Remove https://github.com/user/two.git']")
        .await;

    session
        .wait_until("the second chip to go", |t| !t.contains("two.git"))
        .await;
    assert!(
        session.text().await.contains("one.git"),
        "removing one chip took its neighbour with it"
    );
}
