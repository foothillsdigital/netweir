//! A Firefox page's commands, over WebDriver BiDi. Its page is the same
//! [`Page`] as Chrome's, made by [`open`] instead of a CDP target; what
//! isn't here yet is refused by name (see `Page::chrome_only`).

use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::{Value, json};

use super::{
    ContextRules, KEPT, LOADING, Opening, Page, PageEnv, Response, Shared, Start, State, WaitUntil,
    lock_state, note_origin,
};
use crate::browser::{Guard, Inner, str_of};
use crate::conn::{GONE, Handler};
use crate::environment::Environment;
use crate::{Error, Result};

/// The sandbox netweir's own scripts run in, apart from the page's.
const SANDBOX: &str = "netweir";

/// Events a page keeps its state by.
const EVENTS: &[&str] = &[
    "browsingContext.navigationStarted",
    "browsingContext.navigationCommitted",
    "browsingContext.navigationFailed",
    "browsingContext.domContentLoaded",
    "browsingContext.load",
    "browsingContext.fragmentNavigated",
    "browsingContext.historyUpdated",
    "network.beforeRequestSent",
    "network.responseCompleted",
    "network.fetchError",
    "script.message",
];

/// The channel [`WATCH`] tells netweir on.
const CHANNEL: &str = "netweir";

/// Run in netweir's sandbox in each of the page's documents, as it
/// starts: what Firefox tells of by no event of its own, a move within
/// the document by the history (back to a #fragment, or to an entry
/// pushState made). Its listeners are on the document's window, behind
/// the sandbox, where page script can't find them.
const WATCH: &str = "(tell) => {
    const say = () => tell({url: location.href});
    addEventListener('hashchange', say);
    addEventListener('popstate', say);
}";

pub(super) async fn open(
    browser: Arc<Inner>,
    context: Option<String>,
    owns_context: bool,
    guard: Option<Guard>,
    shared_context: Option<Arc<ContextRules>>,
    env: Environment,
) -> Result<Page> {
    if guard.is_some() {
        return Err(unsupported("Browser.new_guarded_page"));
    }
    if env != Environment::default() {
        return Err(unsupported("a page's environment"));
    }
    let user_context = match context.clone().or_else(|| browser.default_context.clone()) {
        Some(id) => id,
        None => {
            return Err(Error::Launch(
                "Firefox's session has no user context".into(),
            ));
        }
    };
    // A window, not a tab, so every page is in front, as a person's one tab
    // is: a tab behind another is hidden, which its page can see.
    let created = create_window(&browser, &user_context).await?;
    let target = str_of(&created, "context");
    if let Err(e) = place(&browser, &target).await {
        let _ = browser
            .call("", "browsingContext.close", json!({"context": target}))
            .await;
        return Err(e);
    }
    let opening = Opening {
        context,
        owns_context,
        guard: None,
        shared_context,
        opener: None,
        env: PageEnv::default(),
    };
    let page = Page::start(browser.clone(), target.clone(), target.clone(), opening).await?;
    let subscribed = browser
        .call(
            "",
            "session.subscribe",
            json!({"events": EVENTS, "contexts": [target]}),
        )
        .await;
    let watched = match subscribed {
        Ok(_) => {
            browser
                .call(
                    "",
                    "script.addPreloadScript",
                    json!({
                        "functionDeclaration": WATCH,
                        "arguments": [{"type": "channel", "value": {"channel": CHANNEL}}],
                        "contexts": [target],
                        "sandbox": SANDBOX,
                    }),
                )
                .await
        }
        Err(e) => Err(e),
    };
    if let Err(e) = watched {
        let _ = page.close().await;
        return Err(e);
    }
    lock_state(&page.inner.shared).url = "about:blank".into();
    Ok(page)
}

/// Opens a window whose one tab is in `user_context`. On Windows,
/// Firefox's create can fail with "BrowsingContext does no longer exist"
/// though the window opened: the window's first document moved to another
/// process while the create was waiting on the old one. The window is
/// fine, so the new one in `user_context` is taken as the answer once it
/// shows in the tree, which on a busy machine takes seconds. Only when
/// none has shown after [`WINDOW_SHOWS`] is the create asked again, three
/// times at most. Windows other pages are opening meanwhile, in other
/// user contexts, are left alone.
async fn create_window(browser: &Inner, user_context: &str) -> Result<Value> {
    // One create at a time, so the window that shows is this one's and
    // not another page's opening at the same moment.
    static CREATING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _one = CREATING.lock().await;
    let mut tries = 0;
    loop {
        let before: Vec<String> = top_level(browser)
            .await?
            .into_iter()
            .map(|(c, _)| c)
            .collect();
        let created = browser
            .call(
                "",
                "browsingContext.create",
                json!({"type": "window", "userContext": user_context}),
            )
            .await;
        // On a busy machine the tab Firefox opens the window with can be
        // replaced as it opens: Firefox then fails, or answers with no id,
        // while the window it opened shows in the tree.
        let message = match created {
            Ok(v) if !str_of(&v, "context").is_empty() => return Ok(v),
            Ok(_) => "Firefox answered with no context".to_string(),
            Err(Error::Protocol { message, .. })
                if message.contains("DiscardedBrowsingContextError") =>
            {
                message
            }
            Err(e) => return Err(e),
        };
        if tries == 3 {
            // Says what Firefox had open, which no failure has shown yet.
            let open = top_level(browser).await.map(|t| format!("{t:?}"));
            return Err(Error::Protocol {
                method: "browsingContext.create".into(),
                message: format!(
                    "{message} (4 tries, each waited {WINDOW_SHOWS:?} for the window; \
                     before the last: {before:?}; after: {open:?}; \
                     user context {user_context})"
                ),
            });
        }
        tries += 1;
        let gives_up = tokio::time::Instant::now() + WINDOW_SHOWS;
        while tokio::time::Instant::now() < gives_up {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let ours = top_level(browser)
                .await?
                .into_iter()
                .find(|(c, uc)| !c.is_empty() && !before.contains(c) && uc == user_context);
            if let Some((context, uc)) = ours {
                return Ok(json!({"context": context, "userContext": uc}));
            }
        }
    }
}

/// How long a window Firefox opened, though its create failed or gave no
/// id, is waited for to show in the tree.
const WINDOW_SHOWS: Duration = Duration::from_secs(5);

/// Every top-level browsing context, with its user context.
async fn top_level(browser: &Inner) -> Result<Vec<(String, String)>> {
    let tree = browser
        .call("", "browsingContext.getTree", json!({"maxDepth": 0}))
        .await?;
    Ok(tree["contexts"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| (str_of(c, "context"), str_of(c, "userContext")))
        .collect())
}

/// Puts the window of the new page `context` where Firefox opened its
/// first: Firefox cascades each new window from the last, somewhere a
/// person's one window isn't.
async fn place(browser: &Inner, context: &str) -> Result<()> {
    let Some(([x, y, width, height], maximized)) = browser.first_window else {
        return Ok(());
    };
    let tree = browser
        .call(
            "",
            "browsingContext.getTree",
            json!({"root": context, "maxDepth": 0}),
        )
        .await?;
    let window = str_of(&tree["contexts"][0], "clientWindow");
    // A maximized window has no frame above its tabs, which a page can
    // measure, so the page's window is maximized when the first was.
    let state = if maximized {
        json!({"clientWindow": window, "state": "maximized"})
    } else {
        json!({"clientWindow": window, "state": "normal",
            "x": x, "y": y, "width": width, "height": height})
    };
    browser
        .call("", "browser.setClientWindowState", state)
        .await
        .map(drop)
}

/// What a Firefox page hears, kept in the same [`State`] Chrome's events
/// keep: a navigation's id is its loader, as a document's loader is on
/// Chrome. Only the page's own context navigates the page; requests from
/// any of its frames count towards network idle.
pub(super) fn handler(shared: Arc<Shared>, runtime: tokio::runtime::Handle) -> Handler {
    Arc::new(move |method, params| {
        let mut state = lock_state(&shared);
        let own = params["context"] == state.frame.as_str();
        match method {
            GONE => {
                state.gone = true;
                state.listeners.clear();
                state.dialogs = None;
            }
            "browsingContext.navigationStarted" if own => {
                state.started += 1;
                let start = Start {
                    number: state.started,
                    kind: state.next_kind.take().unwrap_or("differentDocument").into(),
                    loader: str_of(params, "navigation"),
                    url: str_of(params, "url"),
                    navigations: state.navigations,
                };
                state.starts.push(start);
                if state.starts.len() > KEPT {
                    state.starts.remove(0);
                }
            }
            "browsingContext.navigationCommitted" if own => {
                let url = str_of(params, "url");
                note_origin(&state.origins, &url);
                let loader = str_of(params, "navigation");
                // Firefox's own page for one it couldn't load, at its own
                // URL: the document is the one that couldn't be.
                if url.starts_with("about:neterror") || url.starts_with("about:certerror") {
                    let wanted = state
                        .starts
                        .iter()
                        .rfind(|st| st.loader == loader)
                        .map(|st| st.url.clone())
                        .unwrap_or_else(|| url.clone());
                    state.unreachable.insert(loader.clone(), wanted);
                }
                state.url = url;
                state.loader = loader.clone();
                state.navigations += 1;
                state.within = false;
                state.committed.retain(|l| *l != loader);
                state.committed.push(loader);
                if state.committed.len() > KEPT {
                    let old = state.committed.remove(0);
                    state.events.remove(&old);
                    state.responses.remove(&old);
                    state.unreachable.remove(&old);
                }
                // What the last document left loading is no more.
                state.in_flight.clear();
                state.quiet += 1;
            }
            "browsingContext.navigationCommitted" => {
                note_origin(&state.origins, params["url"].as_str().unwrap_or_default());
            }
            "browsingContext.domContentLoaded" | "browsingContext.load" if own => {
                let loader = str_of(params, "navigation");
                let event = if method == "browsingContext.load" {
                    "load"
                } else {
                    "DOMContentLoaded"
                };
                state
                    .events
                    .entry(loader.clone())
                    .or_default()
                    .push(event.into());
                if event == "DOMContentLoaded" {
                    quiet_soon(&shared, &runtime, &mut state, loader);
                }
            }
            "browsingContext.fragmentNavigated" | "browsingContext.historyUpdated" if own => {
                let url = str_of(params, "url");
                if state.within && state.url == url {
                    return;
                }
                state.url = url;
                state.navigations += 1;
                state.within = true;
                state.moved_within = state.navigations;
            }
            "script.message" if params["channel"] == CHANNEL => {
                if params["source"]["context"] != state.frame.as_str() {
                    return;
                }
                let Ok(said) = to_json(&params["data"]) else {
                    return;
                };
                // A move to a #fragment fires hashchange and popstate
                // both: one move.
                let url = str_of(&said, "url");
                if state.within && state.url == url {
                    return;
                }
                state.url = url;
                state.navigations += 1;
                state.within = true;
                state.moved_within = state.navigations;
            }
            "browsingContext.navigationFailed" if own => {
                let loader = str_of(params, "navigation");
                let url = str_of(params, "url");
                state.unreachable.insert(loader, url);
            }
            "network.beforeRequestSent" => {
                if state.in_flight.len() >= LOADING {
                    state.in_flight.clear();
                }
                state
                    .in_flight
                    .insert(str_of(&params["request"], "request"));
                state.quiet += 1;
            }
            "network.fetchError" => {
                let request = str_of(&params["request"], "request");
                if own && let Some(navigation) = params["navigation"].as_str() {
                    if state.failures.len() >= KEPT {
                        state.failures.clear();
                    }
                    let error = chrome_error(params["errorText"].as_str().unwrap_or_default());
                    state.failures.insert(navigation.to_string(), error);
                }
                state.in_flight.remove(&request);
                state.quiet += 1;
                let loader = state.loader.clone();
                quiet_soon(&shared, &runtime, &mut state, loader);
            }
            "network.responseCompleted" => {
                let request = &params["request"];
                state.in_flight.remove(&str_of(request, "request"));
                state.quiet += 1;
                let loader = state.loader.clone();
                quiet_soon(&shared, &runtime, &mut state, loader);
                // Only a navigation's own response: its document's.
                let Some(navigation) = params["navigation"].as_str().filter(|_| own) else {
                    drop(state);
                    shared.changed.notify_waiters();
                    return;
                };
                let response = &params["response"];
                let headers = response["headers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|h| (str_of(h, "name"), str_of(&h["value"], "value")))
                    .collect();
                state.responses.insert(
                    navigation.to_string(),
                    Response {
                        url: str_of(response, "url"),
                        status: response["status"].as_u64().unwrap_or(0) as u16,
                        headers,
                        loader: navigation.to_string(),
                        request_id: str_of(request, "request"),
                        session: String::new(),
                        method: str_of(request, "method"),
                        resource_type: "document".into(),
                    },
                );
            }
            _ => return,
        }
        drop(state);
        shared.changed.notify_waiters();
    })
}

/// How long a document's network must be quiet to be idle: Chrome's
/// networkIdle, nothing loading for half a second once its
/// DOMContentLoaded has fired.
const QUIET: Duration = Duration::from_millis(500);

/// Marks `loader`'s document network-idle once nothing has loaded for
/// [`QUIET`], unless something starts loading first. Only once its
/// DOMContentLoaded has fired, and only once a document.
fn quiet_soon(
    shared: &Arc<Shared>,
    runtime: &tokio::runtime::Handle,
    state: &mut State,
    loader: String,
) {
    let events = state.events.get(&loader);
    let ready = events.is_some_and(|e| e.iter().any(|n| n == "DOMContentLoaded"));
    let idle = events.is_some_and(|e| e.iter().any(|n| n == "networkIdle"));
    if !ready || idle || !state.in_flight.is_empty() {
        return;
    }
    let (shared, quiet) = (shared.clone(), state.quiet);
    runtime.spawn(async move {
        tokio::time::sleep(QUIET).await;
        let mut state = lock_state(&shared);
        if state.quiet != quiet || !state.in_flight.is_empty() {
            return;
        }
        state
            .events
            .entry(loader)
            .or_default()
            .push("networkIdle".into());
        drop(state);
        shared.changed.notify_waiters();
    });
}

/// Firefox's name for why a document couldn't load, as Chrome names it
/// (`net::ERR_CONNECTION_REFUSED`), so an error reads the same whichever
/// browser met it; Firefox's own name follows in brackets. One Chrome has
/// no name for stays as Firefox gave it.
pub(crate) fn chrome_error(firefox: &str) -> String {
    const NAMES: &[(&str, &str)] = &[
        ("NS_ERROR_CONNECTION_REFUSED", "ERR_CONNECTION_REFUSED"),
        ("NS_ERROR_UNKNOWN_HOST", "ERR_NAME_NOT_RESOLVED"),
        ("NS_ERROR_NET_TIMEOUT", "ERR_TIMED_OUT"),
        ("NS_ERROR_NET_RESET", "ERR_CONNECTION_RESET"),
        ("NS_ERROR_NET_INTERRUPT", "ERR_CONNECTION_CLOSED"),
        ("NS_ERROR_NET_EMPTY_RESPONSE", "ERR_EMPTY_RESPONSE"),
        ("NS_ERROR_OFFLINE", "ERR_INTERNET_DISCONNECTED"),
        (
            "NS_ERROR_PROXY_CONNECTION_REFUSED",
            "ERR_PROXY_CONNECTION_FAILED",
        ),
        ("NS_ERROR_UNKNOWN_PROXY_HOST", "ERR_PROXY_CONNECTION_FAILED"),
        ("NS_ERROR_MALFORMED_URI", "ERR_INVALID_URL"),
        ("NS_ERROR_UNKNOWN_PROTOCOL", "ERR_UNKNOWN_URL_SCHEME"),
        ("NS_ERROR_FILE_NOT_FOUND", "ERR_FILE_NOT_FOUND"),
        ("NS_BINDING_ABORTED", "ERR_ABORTED"),
        ("NS_ERROR_ABORT", "ERR_ABORTED"),
        ("NS_ERROR_CORRUPTED_CONTENT", "ERR_INVALID_RESPONSE"),
        ("SEC_ERROR_UNKNOWN_ISSUER", "ERR_CERT_AUTHORITY_INVALID"),
        ("SEC_ERROR_EXPIRED_CERTIFICATE", "ERR_CERT_DATE_INVALID"),
        ("SSL_ERROR_BAD_CERT_DOMAIN", "ERR_CERT_COMMON_NAME_INVALID"),
        (
            "MOZILLA_PKIX_ERROR_SELF_SIGNED_CERT",
            "ERR_CERT_AUTHORITY_INVALID",
        ),
    ];
    match NAMES.iter().find(|(name, _)| firefox.contains(name)) {
        Some((name, chrome)) => format!("net::{chrome} ({name})"),
        None => firefox.to_string(),
    }
}

pub(super) async fn goto(
    page: &Page,
    url: &str,
    wait: WaitUntil,
    timeout: Option<Duration>,
) -> Result<Response> {
    if page.inner.closed.load(Ordering::Relaxed) {
        return Err(Error::PageClosed);
    }
    let deadline = page.deadline(timeout);
    let moved = page.state().moved_within;
    let navigate = page.inner.browser.conn.call(
        "",
        "browsingContext.navigate",
        json!({"context": page.inner.target, "url": url, "wait": "none"}),
    );
    let navigated = tokio::time::timeout_at(deadline, navigate)
        .await
        .map_err(|_| Error::Timeout(format!("navigating to {url}")))?
        .map_err(|e| match e {
            // Closed while it waited: its context is gone.
            _ if page.inner.closed.load(Ordering::Relaxed) => Error::PageClosed,
            Error::Protocol { message, .. } => page.navigation_error(url, &chrome_error(&message)),
            e => e,
        })?;
    let navigation = str_of(&navigated, "navigation");
    // Within the same document (a #fragment), it starts, commits and
    // loads nothing, and Firefox says nothing of it: the sandbox's
    // hashchange says it has moved, and there's nothing more to wait for.
    let event = wait.event();
    let left = deadline.saturating_duration_since(tokio::time::Instant::now());
    let within =
        |s: &State| s.moved_within > moved && !s.starts.iter().any(|st| st.loader == navigation);
    page.until(
        || format!("{event} of {url}"),
        left,
        |s| {
            within(s)
                || s.committed.contains(&navigation)
                || s.unreachable.contains_key(&navigation)
        },
    )
    .await?;
    if within(&page.state()) {
        return Ok(page.response_for(None));
    }
    page.landed(&navigation, wait, deadline, url).await
}

/// Goes `by` entries through the page's history; None when there's no
/// entry there. See [`Page::go_forward`].
pub(super) async fn go_by(
    page: &Page,
    by: i64,
    wait: WaitUntil,
    timeout: Option<Duration>,
) -> Result<Option<Response>> {
    let deadline = page.deadline(timeout);
    // The document showing now is marked in netweir's sandbox, so one
    // back from the back-forward cache, which commits nothing, can be
    // told from it: Firefox makes its sandbox again.
    let mark = format!(
        "{:x}",
        std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish()
    );
    call(page, &format!("globalThis.__netweirLeft = '{mark}';"), true).await?;
    let (navigations, started) = {
        let mut s = page.state();
        s.next_kind = Some("historyDifferentDocument");
        (s.navigations, s.started)
    };
    let what = || {
        format!(
            "going {by} through the page's history (Firefox may not have committed a \
             navigation: the page asked before being left, or the answer was a 204 or a download)"
        )
    };
    let traverse = page.call(
        "browsingContext.traverseHistory",
        json!({"context": page.inner.target, "delta": by}),
    );
    let traversed = match tokio::time::timeout_at(deadline, traverse).await {
        Ok(r) => r,
        Err(_) => Err(Error::Timeout(what())),
    };
    match traversed {
        // Firefox stops waiting for the step after a second of its own,
        // though its navigation may still be on its way (a refused
        // connection takes longer than that on Windows): the wait below
        // is for that, to the caller's own deadline.
        Err(Error::Protocol { message, .. }) if message.starts_with("History was not updated") => {}
        Err(e) => {
            page.state().next_kind = None;
            return match e {
                // "no such history entry", which Firefox says as this.
                Error::Protocol { message, .. }
                    if message.starts_with("History entry with delta")
                        || message.contains("no such history entry") =>
                {
                    Ok(None)
                }
                e => Err(e),
            };
        }
        Ok(_) => {}
    }
    let left = || deadline.saturating_duration_since(tokio::time::Instant::now());
    let ours = |s: &State| s.starts.iter().find(|st| st.number > started).cloned();
    // Another document, which starts a navigation, or the same one at
    // another entry, which only moves within it.
    page.until(what, left(), |s| {
        ours(s).is_some() || s.navigations > navigations
    })
    .await?;
    let Some(start) = ours(&page.state()) else {
        let mut s = page.state();
        s.next_kind = None;
        return Ok(Some(Response::bare(s.url.clone(), s.loader.clone())));
    };
    // A document from the back-forward cache commits nothing: it's back,
    // loaded already, once the sandbox is one without the mark, at the
    // entry's URL. Firefox's error page for one that can't be loaded is a
    // new document too, at its own URL, and does commit.
    let back = format!(
        "return globalThis.__netweirLeft !== '{mark}' && document.readyState === 'complete' \
         && location.href === {};",
        Value::from(start.url.as_str())
    );
    loop {
        if page.state().committed.contains(&start.loader) {
            return page
                .landed(&start.loader, wait, deadline, &start.url)
                .await
                .map(Some);
        }
        // Between documents, the sandbox may not answer.
        if call(page, &back, true).await.ok() == Some(Value::Bool(true))
            && !page.state().committed.contains(&start.loader)
        {
            let loader = {
                let mut s = page.state();
                restore(&mut s, start.url.clone());
                s.loader.clone()
            };
            page.inner.shared.changed.notify_waiters();
            // Its sandbox is new, without the listeners the preload
            // script gave the old one.
            page.call(
                "script.callFunction",
                json!({
                    "functionDeclaration": WATCH,
                    "arguments": [{"type": "channel", "value": {"channel": CHANNEL}}],
                    "awaitPromise": false,
                    "target": {"context": page.inner.target, "sandbox": SANDBOX},
                    "resultOwnership": "none",
                }),
            )
            .await?;
            return Ok(Some(page.response_for(Some(&loader))));
        }
        if left().is_zero() {
            return Err(Error::Timeout(what()));
        }
        tokio::time::sleep(Duration::from_millis(20).min(left())).await;
    }
}

/// The page's document is one Firefox kept whole in its back-forward
/// cache, at `url`, come back: it's under the loader it was first
/// committed with, which its response was kept under.
fn restore(state: &mut State, url: String) {
    let loader = state
        .committed
        .iter()
        .rev()
        .find(|l| state.responses.get(*l).is_some_and(|r| r.url == url))
        .cloned()
        .unwrap_or_default();
    state.url = url;
    state.loader = loader.clone();
    state.navigations += 1;
    state.within = false;
    state.restored = (state.navigations, loader.clone());
    state.committed.retain(|l| *l != loader);
    state.committed.push(loader);
}

/// Loads the page's document again, as the reload button does.
pub(super) async fn reload(
    page: &Page,
    wait: WaitUntil,
    timeout: Option<Duration>,
) -> Result<Response> {
    let deadline = page.deadline(timeout);
    let url = page.url();
    let r = page
        .call(
            "browsingContext.reload",
            json!({"context": page.inner.target, "wait": "none"}),
        )
        .await?;
    page.landed(&str_of(&r, "navigation"), wait, deadline, &url)
        .await
}

/// Brings the page's window in front, as a person does before using it,
/// when it hasn't the focus: each Firefox page is a window of its own,
/// and another's may have it. Asked in netweir's sandbox, where page
/// script can't see the asking.
pub(super) async fn to_front(page: &Page) -> Result<()> {
    let focused = call(page, "return document.hasFocus();", true).await?;
    if focused == true {
        return Ok(());
    }
    page.call(
        "browsingContext.activate",
        json!({"context": page.inner.target}),
    )
    .await?;
    tokio::time::sleep(crate::events::pause(page.inner.browser.pause)).await;
    Ok(())
}

/// Runs `body` as an async function in the page's main frame; see
/// [`super::firefox_call`].
pub(super) async fn call(page: &Page, body: &str, sandboxed: bool) -> Result<Value> {
    if page.inner.closed.load(Ordering::Relaxed) {
        return Err(Error::PageClosed);
    }
    let mut target = json!({"context": page.inner.target});
    if sandboxed {
        target["sandbox"] = Value::from(SANDBOX);
    }
    let r = page
        .inner
        .browser
        .conn
        .call(
            "",
            "script.callFunction",
            json!({
                "functionDeclaration": format!("async () => {{ {body}\n}}"),
                "awaitPromise": true,
                "target": target,
                "resultOwnership": "none",
            }),
        )
        .await?;
    if r["type"] == "exception" {
        let details = &r["exceptionDetails"];
        let message = details["text"]
            .as_str()
            .filter(|t| !t.is_empty())
            .unwrap_or("script error");
        return Err(Error::Script(message.to_string()));
    }
    to_json(&r["result"])
}

pub(super) async fn close(page: &Page) -> Result<()> {
    let inner = &page.inner;
    let browser = &inner.browser;
    let closed = browser
        .call(
            "",
            "browsingContext.close",
            json!({"context": inner.target}),
        )
        .await;
    browser.conn.off(&inner.session);
    if inner.owns_context
        && let Some(id) = &inner.context
    {
        let _ = browser
            .call("", "browser.removeUserContext", json!({"userContext": id}))
            .await;
    }
    match closed {
        Ok(_) | Err(Error::Closed) | Err(Error::Protocol { .. }) => Ok(()),
        Err(e) => Err(e),
    }
}

fn unsupported(feature: &str) -> Error {
    Error::Unsupported {
        engine: "Firefox",
        feature: feature.to_string(),
    }
}

/// A BiDi remote value as the JSON Chrome's evaluate gives for the same
/// value: numbers JSON can't hold, and BigInts, as text; objects as their
/// own properties, undefined ones left out; what has no JSON form of its
/// own (a function, a date, a map, a node) as an empty object.
pub(crate) fn to_json(remote: &Value) -> Result<Value> {
    Ok(match remote["type"].as_str().unwrap_or_default() {
        "undefined" | "null" => Value::Null,
        "string" | "boolean" => remote["value"].clone(),
        "number" => match &remote["value"] {
            // NaN, -0, Infinity and -Infinity come as text.
            Value::String(text) => Value::from(text.as_str()),
            n => n.clone(),
        },
        "bigint" => remote["value"].clone(),
        "array" => Value::Array(
            remote["value"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|v| match jsonless(v) {
                    true => Ok(Value::Null),
                    false => to_json(v),
                })
                .collect::<Result<_>>()?,
        ),
        "object" => {
            let mut object = serde_json::Map::new();
            for pair in remote["value"].as_array().into_iter().flatten() {
                let (Some(key), Some(value)) = (pair[0].as_str(), pair.get(1)) else {
                    continue;
                };
                if !jsonless(value) {
                    object.insert(key.to_string(), to_json(value)?);
                }
            }
            Value::Object(object)
        }
        _ => Value::Object(serde_json::Map::new()),
    })
}

/// Values left out of an object, and written as null in an array.
fn jsonless(remote: &Value) -> bool {
    remote["type"] == "undefined"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(v: Value) -> Value {
        json!({"type": "number", "value": v})
    }

    #[test]
    fn firefox_errors_read_as_chrome_names_them() {
        assert_eq!(
            chrome_error("NS_ERROR_CONNECTION_REFUSED"),
            "net::ERR_CONNECTION_REFUSED (NS_ERROR_CONNECTION_REFUSED)"
        );
        assert_eq!(
            chrome_error("Error: NS_ERROR_UNKNOWN_HOST"),
            "net::ERR_NAME_NOT_RESOLVED (NS_ERROR_UNKNOWN_HOST)"
        );
        // One Chrome has no name for stays as Firefox said it.
        assert_eq!(
            chrome_error("NS_ERROR_SOMETHING_NEW"),
            "NS_ERROR_SOMETHING_NEW"
        );
    }

    #[test]
    fn remote_values_read_as_chrome_gives_them() {
        assert_eq!(to_json(&n(json!(1))).unwrap(), json!(1));
        for text in ["NaN", "-0", "Infinity", "-Infinity"] {
            assert_eq!(to_json(&n(json!(text))).unwrap(), json!(text));
        }
        assert_eq!(
            to_json(&json!({"type": "bigint", "value": "10"})).unwrap(),
            json!("10")
        );
        assert_eq!(to_json(&json!({"type": "undefined"})).unwrap(), Value::Null);
        let object = json!({"type": "object", "value": [
            ["a", n(json!(2))],
            ["f", {"type": "function"}],
            ["u", {"type": "undefined"}],
            ["list", {"type": "array", "value": [
                {"type": "string", "value": "x"},
                {"type": "function"},
                {"type": "null"},
            ]}],
        ]});
        assert_eq!(
            to_json(&object).unwrap(),
            json!({"a": 2, "f": {}, "list": ["x", {}, null]})
        );
        assert_eq!(
            to_json(&json!({"type": "date", "value": "1970-01-01T00:00:00.000Z"})).unwrap(),
            json!({})
        );
        assert_eq!(
            to_json(&json!({"type": "map", "value": [[n(json!(1)), n(json!(2))]]})).unwrap(),
            json!({})
        );
    }
}
