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
    std::env::var("AURCACHE_UI").unwrap_or_else(|_| "http://localhost:8080".to_string())
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

    /// How many nodes match, for checking how much of a list is on screen.
    async fn count(&self, selector: &str) -> i64 {
        self.eval(format!(
            "return document.querySelectorAll({}).length;",
            json(selector)
        ))
        .await
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
    a_stored_config_file_is_loaded_into_the_editor(&session).await;
    a_linked_search_arrives_applied(&session).await;
    one_queued_package_can_be_taken_back(&session).await;
    a_queue_is_added_as_one_job(&session).await;
    the_export_dialog_warns_only_when_secrets_are_asked_for(&session).await;
    the_restore_dialog_offers_its_options(&session).await;
    approving_a_worker_lets_it_build(&session).await;
    a_per_package_file_leaves_the_server_wide_one_alone(&session).await;
    a_build_flag_survives_a_reload_and_can_be_taken_off(&session).await;
    dependencies_stay_out_of_the_list_until_asked_for(&session).await;
    following_a_dependency_loads_that_package(&session).await;
    a_second_page_holds_different_builds(&session).await;
    a_build_can_be_found_by_the_name_the_list_shows(&session).await;
    // Last: it deletes a row the others would otherwise still be looking at.
    removing_a_package_takes_it_out_of_the_list(&session).await;

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

/// The stored `makepkg.conf` reaches the editor.
///
/// Not checkable from a DOM dump: a textarea's contents are a property, not
/// serialised markup, so an editor that renders but never fills looks
/// identical there.
async fn a_stored_config_file_is_loaded_into_the_editor(session: &Session) {
    session.open("/config-files").await;
    session.wait_for("textarea").await;

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let value = session.value_of("textarea").await;
        if value.contains("MAKEFLAGS") {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the seeded makepkg.conf never reached the editor; textarea held {value:?}"
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

/// Approving a worker moves it out of the queue of machines waiting.
///
/// The approval gate is what the workers page is for, and it is the one flow
/// here that changes server state through a button rather than a form. A
/// rendering test sees the button; only this sees what pressing it does.
async fn approving_a_worker_lets_it_build(session: &Session) {
    session.open("/workers").await;
    session
        .wait_until("the fleet to load", |t| t.contains("new-machine"))
        .await;
    assert!(
        session
            .text()
            .await
            .contains("1 worker is waiting for approval"),
        "the fixture should start with exactly one pending worker"
    );

    session.click_labelled("table button", "Approve").await;

    session
        .wait_until("the approval to land", |t| t.contains("Worker approved"))
        .await;
    // The list refetches, so the notice about waiting machines should be gone
    // rather than merely stale.
    session
        .wait_until("the pending notice to clear", |t| {
            !t.contains("waiting for approval")
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

/// A queue of several packages is submitted as one bulk add, the dialog closes
/// at once, and each one's outcome arrives on the progress card.
///
/// The failure this guards against is invisible without a browser and a server:
/// the add is started by a dialog that then unmounts, and the polling is owned
/// by the card that outlives it. A mistake in that handover -- never starting,
/// starting twice, losing the offset, not noticing the job finished -- leaves a
/// spinner in the corner forever while the server has long since finished. Two
/// unreachable remotes are used deliberately: what is being tested is that both
/// outcomes are reported, and a failure is the outcome this fixture can produce
/// without network.
async fn a_queue_is_added_as_one_job(session: &Session) {
    session.open("/packages/add").await;

    let entry = ".modal-box input[type=text]";
    for repo in ["bulk-one", "bulk-two"] {
        let url = format!("https://github.com/user/{repo}.git");
        session.type_into(entry, &url).await;
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
        .click_labelled(".modal-action button", "Add 2 packages")
        .await;

    // The dialog goes as soon as the work is handed over -- the whole point of
    // the change -- so the rest of the site is usable while the add runs.
    session
        .wait_until("the dialog to close", |t| {
            !t.contains("Type a package name to search the AUR")
        })
        .await;

    // Both are named on the card, so the job's per-package outcomes reached the
    // screen -- not just the first, and not a single collapsed error for the
    // batch. That they arrive at all is what proves the card took over the
    // polling from the dialog that started it.
    session
        .wait_until("both packages to be reported", |t| {
            t.contains("bulk-one.git") && t.contains("bulk-two.git")
        })
        .await;
}

/// The export dialog only warns about secrets once they are asked for.
///
/// Invisible to a render test, which sees one paint: the warning is the point
/// of the checkbox, and a warning that is always on screen is one nobody reads
/// by the time it matters. The download itself is a plain link, so what is
/// checked here is that ticking the box changes what the link offers.
async fn the_export_dialog_warns_only_when_secrets_are_asked_for(session: &Session) {
    session.open("/settings").await;
    session.click_labelled(".card button", "Export…").await;
    session
        .wait_until("the export dialog", |t| t.contains("Include secrets"))
        .await;
    assert!(
        !session.text().await.contains("This file is a credential"),
        "the warning is shown before secrets are asked for"
    );

    session.click("input[aria-label='Include secrets']").await;
    session
        .wait_until("the credential warning", |t| {
            t.contains("This file is a credential")
        })
        .await;

    // And the link now asks for them.
    session
        .wait_for_script(
            "the download link to carry the flag",
            "const a = [...document.querySelectorAll('.modal-open a')] \
                 .find(e => e.getAttribute('href')?.includes('include_secrets=true')); \
             return !!a;"
                .to_string(),
        )
        .await;
}

/// The restore dialog offers a drop target and the three choices an import has
/// to make, and disables the one that would be meaningless.
async fn the_restore_dialog_offers_its_options(session: &Session) {
    session.open("/settings").await;
    session.click_labelled(".card button", "Restore…").await;
    session
        .wait_until("the restore dialog", |t| {
            t.contains("Drop a dump here") && t.contains("Replace everything")
        })
        .await;

    // Nothing chosen yet, so there is nothing to preview or restore.
    session
        .wait_for_script(
            "Preview to be disabled with no file",
            "const b = [...document.querySelectorAll('.modal-open .modal-action button')] \
                 .find(e => e.textContent.trim() === 'Preview'); \
             return !!b && b.disabled;"
                .to_string(),
        )
        .await;

    // Replacing everything leaves the per-package policy with nothing to decide.
    session
        .click("input[aria-label='Replace everything']")
        .await;
    session
        .wait_until("the policy to be explained away", |t| {
            t.contains("Nothing will already be here")
        })
        .await;
}

/// Saving a package's config file writes the package's row, not the server's.
///
/// The failure this exists for is invisible to every other kind of test: pass
/// the scope as `None` and the page still renders correctly, still reports a
/// successful save, and quietly edits the file every other package builds
/// against. Only reading the global file back afterwards can tell.
async fn a_per_package_file_leaves_the_server_wide_one_alone(session: &Session) {
    const MARKER: &str = "# only-for-hello";

    session.open("/package/hello/config-files").await;
    // `hello` seeds nothing of its own, so what loads is the server-wide file.
    session
        .wait_until("the inherited file", |t| t.contains("inherited"))
        .await;

    session.type_into("textarea", MARKER).await;
    session.click_labelled("button", "Save").await;
    session
        .wait_until("the save to land on the package", |t| {
            t.contains("package override")
        })
        .await;

    // The actual assertion. A textarea's contents are a property rather than
    // markup, so this is also the only way to read the file back at all.
    session.open("/config-files").await;
    session.wait_for("textarea").await;
    let global = session.value_of("textarea").await;
    assert!(
        !global.contains(MARKER),
        "a per-package save was written to the server-wide makepkg.conf: {global:?}"
    );
    assert!(
        global.contains("MAKEFLAGS"),
        "the server-wide makepkg.conf lost its seeded contents: {global:?}"
    );
}

/// A build flag is stored, comes back on a fresh load, and can be removed.
///
/// The reload is the point. Adding a chip to a local list renders identically
/// to one that reached the server, and this page is the only way to set flags
/// at all.
async fn a_build_flag_survives_a_reload_and_can_be_taken_off(session: &Session) {
    // Deliberately not the placeholder's own text: a field that reported its
    // placeholder as its value would pass if the two matched.
    const FLAG: &str = "--skipinteg";

    session.open("/package/hello").await;
    session
        .wait_until("the empty flag list", |t| t.contains("No build flags"))
        .await;

    session
        .type_into("input[placeholder='--nocheck']", FLAG)
        .await;
    session.click_labelled("button", "Add").await;
    session
        .wait_until("the flag to appear", |t| t.contains(FLAG))
        .await;

    session.open("/package/hello").await;
    session
        .wait_until("the flag to have persisted", |t| t.contains(FLAG))
        .await;

    session
        .click(&format!("button[aria-label='Remove {FLAG}']"))
        .await;
    session
        .wait_until("the flag to be gone", |t| t.contains("No build flags"))
        .await;
}

/// Removing a package takes it out of the repository.
///
/// `2048.c` because nothing else in this suite or in the route list looks at
/// it, and it has no dependency edges — so it is deleted outright rather than
/// demoted to a dependency, which is the case worth asserting.
async fn removing_a_package_takes_it_out_of_the_list(session: &Session) {
    session.open("/package/2048.c").await;
    session
        .wait_until("the remove section", |t| t.contains("Remove package"))
        .await;

    // The dialog is in the document whether or not it is open, so clicking the
    // confirm button without opening it would "pass" without confirming
    // anything. Everything below is scoped to `.modal-open` for that reason.
    session.click_labelled("button", "Remove package").await;
    session.wait_for(".modal-open").await;
    session
        .wait_until("the confirmation to name the package", |t| {
            t.contains("Remove 2048.c?")
        })
        .await;

    session.click(".modal-open .modal-action .btn-error").await;

    session
        .wait_until("the package to leave the list", |t| !t.contains("2048.c"))
        .await;
    assert!(
        session.url().await.ends_with("/packages"),
        "removal did not land back on the list: {}",
        session.url().await
    );
}

/// The list shows what was asked for; the toggle adds what was pulled in.
///
/// The half a rendering test cannot reach is the absence: a marker can only
/// assert that something is on the page, and the whole point of the default is
/// that `libfoo` is not.
async fn dependencies_stay_out_of_the_list_until_asked_for(session: &Session) {
    session.open("/packages").await;
    session
        .wait_until("the list to load", |t| t.contains("neofetch"))
        .await;

    let listed = session.text().await;
    assert!(
        !listed.contains("libfoo"),
        "a dependency was listed without being asked for: {listed}"
    );

    session
        .click_labelled("button", "Show dependencies (2)")
        .await;
    session
        .wait_until("the dependency to appear", |t| t.contains("libfoo"))
        .await;

    // Revealed, but still distinguishable from a package somebody chose.
    let shown = session.text().await;
    assert!(
        shown.contains("dependency"),
        "a revealed dependency was not marked as one: {shown}"
    );

    // And back, so the next scenario sees the list as it found it.
    session
        .click_labelled("button", "Hide dependencies (2)")
        .await;
    session
        .wait_until("the dependency to go again", |t| !t.contains("libfoo"))
        .await;
}

/// Paging shows rows the previous page did not, and filtering starts over.
///
/// A rendering test can see the controls but not what they do: a Next button
/// that renders and does nothing looks exactly like one that works, and a page
/// that never resets only shows itself once a filter has been typed.
async fn a_second_page_holds_different_builds(session: &Session) {
    session.open("/builds").await;
    session
        .wait_until("the list to load", |t| t.contains("Page 1 of"))
        .await;

    // Counted, not compared as text: the pager's own label differs between
    // pages, so comparing the rendered page against itself would pass even if
    // the table below it never changed — or never paged at all.
    let on_first = session.count("tbody tr").await;
    assert_eq!(
        on_first, 100,
        "a page should hold exactly PAGE_SIZE rows, got {on_first}"
    );

    session.click("button[aria-label='Next page']").await;
    session
        .wait_until("the second page", |t| t.contains("Page 2 of"))
        .await;

    let on_second = session.count("tbody tr").await;
    assert!(
        on_second > 0 && on_second < 100,
        "the second page should hold the remainder, got {on_second}"
    );

    let second_page = session.text().await;
    assert!(
        second_page.contains("Showing 101"),
        "the second page did not start where the first ended: {second_page}"
    );

    // Filtering has to put you back at the start, or a search run from page 2
    // reports matches it is not showing.
    session.type_into("input[type=search]", "paru").await;
    session
        .wait_until("the filter to reset the page", |t| {
            t.contains("Showing 1\u{2013}") || !t.contains("Page 2 of")
        })
        .await;
}

/// A build is findable by what the row calls it, not only by its package.
async fn a_build_can_be_found_by_the_name_the_list_shows(session: &Session) {
    session.open("/builds").await;
    session
        .wait_until("the list to load", |t| t.contains("hello"))
        .await;

    // `hello/2` is the failed second build the fixture gives it.
    session.type_into("input[type=search]", "hello/2").await;
    session
        .wait_until("the list to narrow to one build", |t| {
            t.contains("hello/2") && !t.contains("neofetch")
        })
        .await;
}

/// Clicking a dependency actually opens it.
///
/// Invisible to every rendering check: both pages render correctly on their
/// own, and the URL updates either way. What broke was that navigating between
/// two `/package/:pkgbase` routes reuses the component, so a fetch that
/// captured the first name never ran again -- the address bar said one package
/// while the page showed the other, with no request in flight to explain it.
async fn following_a_dependency_loads_that_package(session: &Session) {
    // `yay` depends on `hello`, so its dependency list has something to click.
    session.open("/package/yay").await;
    session
        .wait_until("the package to load", |t| t.contains("yay"))
        .await;
    assert!(
        session.text().await.contains("hello"),
        "expected yay's dependency list to name hello"
    );

    session.click_labelled("a", "hello").await;

    // The breadcrumb, not the body: `yay` appears on hello's page too, in its
    // dependents list, so "does the text mention hello" is true either way.
    // The heading is what says which package the page is actually about.
    session
        .wait_until("the dependency's own page", |t| {
            t.contains("Packages/hello")
        })
        .await;
    assert!(
        session.url().await.ends_with("/package/hello"),
        "url did not follow the click: {}",
        session.url().await
    );
}
