//! A Firefox page's commands, over WebDriver BiDi. Its page is the same
//! [`Page`] as Chrome's, made by [`open`] instead of a CDP target; what
//! isn't here yet is refused by name (see `Page::chrome_only`).

use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::{Value, json};

use super::{
    ContextRules, Cookie, KEPT, LOADING, Opening, Page, PageEnv, Response, Shared, Start, State,
    WaitUntil, lock_state, note_origin,
};
use crate::browser::{Guard, Inner, str_of};
use crate::conn::{GONE, Handler};
use crate::environment::Environment;
use crate::events::PageEvent;
use crate::network::{Decider, Request, bidi_headers, bidi_text, bidi_type};
use crate::{Error, Result};

/// The sandbox netweir's own scripts run in, apart from the page's.
const SANDBOX: &str = "netweir";

/// Events a page keeps its state by.
const EVENTS: &[&str] = &[
    // For the connection, which learns each frame's page from it, and
    // for each page, which hears of its popups by it.
    "browsingContext.contextCreated",
    "browsingContext.navigationStarted",
    "browsingContext.navigationCommitted",
    "browsingContext.navigationFailed",
    "browsingContext.domContentLoaded",
    "browsingContext.load",
    "browsingContext.fragmentNavigated",
    "browsingContext.historyUpdated",
    "network.beforeRequestSent",
    "network.responseStarted",
    "network.responseCompleted",
    "network.fetchError",
    "script.message",
    "browsingContext.userPromptOpened",
    "browsingContext.userPromptClosed",
    "browsingContext.downloadWillBegin",
    "browsingContext.downloadEnd",
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
    if let Some(what) = env_unsupported(&env) {
        return Err(unsupported(what));
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
        guard,
        shared_context,
        opener: None,
        env: PageEnv {
            env: env.clone(),
            ..PageEnv::default()
        },
    };
    let page = Page::start(browser.clone(), target.clone(), target.clone(), opening).await?;
    // Its environment and its context's rules (or its opener's) from
    // the first request on.
    // A page with a user context of its own (its popups apart) has its
    // environment there, so its popups have it from their first request.
    let scope = match owns_context {
        true => Scope::User(user_context.clone()),
        false => Scope::of(&page),
    };
    let ready = match apply_env(&page, &on_user_context(&env), &scope).await {
        Ok(()) if env.viewport.is_some() || env.device_scale_factor.is_some() => {
            set_viewport(&page, &env).await
        }
        r => r,
    };
    let ready = match ready {
        Ok(()) => page.refresh_fetch().await,
        Err(e) => Err(e),
    };
    if let Err(e) = ready {
        let _ = page.close().await;
        return Err(e);
    }
    lock_state(&page.inner.shared).url = "about:blank".into();
    Ok(page)
}

/// What every page of the browser needs from the start, a popup's first
/// document among them, which is under way before netweir hears of its
/// page: the events pages keep their state by, [`WATCH`] in each
/// document, and a data collector keeping response bodies for
/// Page::body, as Chrome does on its own, up to Chrome's limit for one.
/// None of it is anything a page can see. A request paused in a context
/// no page of netweir's holds goes on unchanged. Returns the collector.
pub(crate) async fn watch_browser(conn: &crate::conn::Connection) -> Result<String> {
    // Request bodies are kept too, for routes (a HAR file matching a
    // POST's, say): Firefox doesn't put them in its events.
    let collected = conn
        .call(
            "",
            "network.addDataCollector",
            json!({"dataTypes": ["request", "response"], "maxEncodedDataSize": BODY_KEPT}),
        )
        .await?;
    let collector = str_of(&collected, "collector");
    let (unheld, kept) = (conn.clone(), collector.clone());
    let handler: Handler = Arc::new(move |method, params| {
        if method == "network.beforeRequestSent" {
            let request = str_of(&params["request"], "request");
            if params["isBlocked"] == true {
                unheld.send("", "network.continueRequest", json!({"request": request}));
            }
            unheld_body(&unheld, &kept, params);
        }
    });
    conn.on("", handler);
    conn.call("", "session.subscribe", json!({"events": EVENTS}))
        .await?;
    conn.call(
        "",
        "script.addPreloadScript",
        json!({
            "functionDeclaration": WATCH,
            "arguments": [{"type": "channel", "value": {"channel": CHANNEL}}],
            "sandbox": SANDBOX,
        }),
    )
    .await?;
    Ok(collector)
}

/// Lets go of the body Firefox kept for a request no route will read
/// (`params`, its `network.beforeRequestSent`): only a paused one goes to
/// a route.
pub(crate) fn unheld_body(conn: &crate::conn::Connection, collector: &str, params: &Value) {
    if params["isBlocked"] != true && params["request"]["bodySize"].as_u64().unwrap_or(0) > 0 {
        conn.send(
            "",
            "network.disownData",
            json!({"dataType": "request", "collector": collector, "request": str_of(&params["request"], "request")}),
        );
    }
}

/// The body `request` sent, as `collector` kept it, let go of once read.
/// None when Firefox has it no longer.
pub(crate) async fn request_body(
    conn: &crate::conn::Connection,
    collector: &str,
    request: &str,
) -> Option<Vec<u8>> {
    let got = conn
        .call(
            "",
            "network.getData",
            json!({"dataType": "request", "collector": collector, "request": request, "disown": true}),
        )
        .await
        .ok()?;
    let value = got["bytes"]["value"].as_str()?;
    if got["bytes"]["type"] == "base64" {
        decoded("network.getData", &json!({"data": value})).ok()
    } else {
        Some(value.as_bytes().to_vec())
    }
}

/// The first part of `env` a Firefox page can't have, by the name its
/// error gives; None when Firefox can give it all. A phone's environment
/// and touch are Chrome's; the media features (colour scheme, reduced
/// motion, forced colours, contrast, print) Firefox has no command for.
pub(crate) fn env_unsupported(env: &Environment) -> Option<&'static str> {
    let unsupported = [
        (env.phone.is_some(), "a phone's environment"),
        (env.mobile == Some(true), "a phone's layout (mobile)"),
        (env.has_touch == Some(true), "touch"),
        (env.color_scheme.is_some(), "a colour scheme"),
        (env.reduced_motion.is_some(), "reduced motion"),
        (env.forced_colors.is_some(), "forced colours"),
        (env.contrast.is_some(), "a contrast preference"),
        (env.media.is_some(), "a media type"),
        (
            !env.permissions.is_empty(),
            "permissions for every origin (grant them to an origin)",
        ),
    ];
    unsupported
        .into_iter()
        .find_map(|(set, what)| set.then_some(what))
}

/// Where a Firefox page's environment goes: its own browsing context, or
/// its whole user context, whose popups have it from their first request
/// (Firefox gives a popup none of its opener's context's).
pub(super) enum Scope {
    Context(String),
    User(String),
}

impl Scope {
    fn of(page: &Page) -> Scope {
        Scope::Context(page.inner.target.clone())
    }

    fn on(&self, mut params: Value) -> Value {
        match self {
            Scope::Context(id) => params["contexts"] = json!([id]),
            Scope::User(id) => params["userContexts"] = json!([id]),
        }
        params
    }
}

/// Gives a new Firefox page `env` at `scope` before anything loads in it,
/// all but its viewport and pixel ratio (see [`set_viewport`]).
async fn apply_env(page: &Page, env: &Environment, scope: &Scope) -> Result<()> {
    let browser = &page.inner.browser;
    let emulate = |method: &'static str, params: Value| browser.call("", method, scope.on(params));
    if let Some(locale) = &env.locale {
        emulate("emulation.setLocaleOverride", json!({"locale": locale})).await?;
    }
    if let Some(timezone) = &env.timezone {
        emulate(
            "emulation.setTimezoneOverride",
            json!({"timezone": timezone}),
        )
        .await?;
    }
    if let Some(user_agent) = &env.user_agent {
        emulate(
            "emulation.setUserAgentOverride",
            json!({"userAgent": user_agent}),
        )
        .await?;
    }
    if let Some(screen) = env.screen {
        emulate(
            "emulation.setScreenSettingsOverride",
            json!({"screenArea": {"width": screen.width, "height": screen.height}}),
        )
        .await?;
    }
    if let Some(at) = env.geolocation {
        geolocation_on(browser, scope, Some(at)).await?;
    }
    for script in &env.init_scripts {
        script_on(browser, scope, script).await?;
    }
    if !env.extra_headers.is_empty() {
        headers_on(browser, scope, &env.extra_headers).await?;
    }
    if env.offline == Some(true) {
        offline_on(browser, scope, true).await?;
    }
    Ok(())
}

/// The page's viewport and pixel ratio, as `env` has them; what it leaves
/// unset stays as the window has it.
async fn set_viewport(page: &Page, env: &Environment) -> Result<()> {
    let mut params = json!({"context": page.inner.target});
    if let Some(v) = env.viewport {
        params["viewport"] = json!({"width": v.width, "height": v.height});
    }
    if let Some(ratio) = env.device_scale_factor {
        params["devicePixelRatio"] = Value::from(ratio);
    }
    page.inner
        .browser
        .call("", "browsingContext.setViewport", params)
        .await?;
    Ok(())
}

/// What of `env` a page shares with every page of its user context: all
/// but its window's size.
pub(super) fn on_user_context(env: &Environment) -> Environment {
    Environment {
        viewport: None,
        device_scale_factor: None,
        ..env.clone()
    }
}

/// What of `env` isn't on the user context already, as `base` has it.
/// A change that takes something away goes to the whole user context
/// (see [`Change`]), so `env` only ever adds to `base`.
fn differences(env: &Environment, base: &Environment) -> Environment {
    let differs = |a, b| a != b;
    let mut init_scripts = env.init_scripts.clone();
    for script in &base.init_scripts {
        if let Some(at) = init_scripts.iter().position(|s| s == script) {
            init_scripts.remove(at);
        }
    }
    Environment {
        locale: env
            .locale
            .clone()
            .filter(|_| differs(&env.locale, &base.locale)),
        timezone: env
            .timezone
            .clone()
            .filter(|_| differs(&env.timezone, &base.timezone)),
        user_agent: env
            .user_agent
            .clone()
            .filter(|_| differs(&env.user_agent, &base.user_agent)),
        screen: env.screen.filter(|_| env.screen != base.screen),
        geolocation: env
            .geolocation
            .filter(|_| env.geolocation != base.geolocation),
        offline: env.offline.filter(|_| env.offline != base.offline),
        extra_headers: match env.extra_headers == base.extra_headers {
            true => Vec::new(),
            false => env.extra_headers.clone(),
        },
        init_scripts,
        ..Environment::default()
    }
}

/// What's on a page's user context: what it was opened with, if it
/// has the context to itself and its popups, and nothing otherwise.
fn base_of(page: &Page) -> Environment {
    match &page.inner.user_env {
        Some(base) => base.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        None => Environment::default(),
    }
}

/// The pages sharing `page`'s user context's environment, `page` aside:
/// its opener and its popups, and theirs.
fn sharing(page: &Page) -> Vec<Page> {
    let Some(base) = &page.inner.user_env else {
        return Vec::new();
    };
    let pages = page
        .inner
        .browser
        .pages
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    pages
        .iter()
        .filter_map(std::sync::Weak::upgrade)
        .filter(|p| !Arc::ptr_eq(p, &page.inner))
        .filter(|p| p.user_env.as_ref().is_some_and(|b| Arc::ptr_eq(b, base)))
        .map(|inner| Page { inner })
        .collect()
}

/// A change to one setting of a page's environment: one that sets
/// something goes on the page's own browsing context, as on Chrome, where
/// it reaches neither its opener nor its popups. One that takes it away
/// can't go there: Firefox reads that as the user context's setting, so
/// it's taken off the user context, and each other page sharing it that
/// had its own is given that back on its own context. `held_for_popups`
/// follows either.
async fn change<F, S>(page: &Page, sets: bool, clear: F, send: S) -> Result<()>
where
    F: Fn(&mut Environment),
    S: AsyncFn(&Inner, &Scope, &Environment) -> Result<()>,
{
    let browser = &page.inner.browser;
    let env = super::lock_env(&page.inner.env).env.clone();
    match (&page.inner.user_env, sets) {
        (Some(base), false) => {
            let user_context = user_context_of(browser, page.inner.context.as_deref());
            send(browser, &Scope::User(user_context), &env).await?;
            clear(&mut base.lock().unwrap_or_else(|e| e.into_inner()));
            send(browser, &Scope::of(page), &env).await?;
            for other in sharing(page) {
                let theirs = super::lock_env(&other.inner.env).env.clone();
                let mut cleared = theirs.clone();
                clear(&mut cleared);
                if theirs != cleared {
                    send(browser, &Scope::of(&other), &theirs).await?;
                }
            }
        }
        _ => send(browser, &Scope::of(page), &env).await?,
    }
    held_for_popups(page).await
}

/// Where the page says it is, or where it really is for None.
pub(super) async fn set_geolocation(
    page: &Page,
    at: Option<crate::environment::Geolocation>,
) -> Result<()> {
    change(
        page,
        at.is_some(),
        |e| e.geolocation = None,
        async |b, scope, env| geolocation_on(b, scope, env.geolocation).await,
    )
    .await
}

async fn geolocation_on(
    browser: &Inner,
    scope: &Scope,
    at: Option<crate::environment::Geolocation>,
) -> Result<()> {
    let coordinates = match at {
        Some(g) => {
            json!({"latitude": g.latitude, "longitude": g.longitude, "accuracy": g.accuracy})
        }
        None => Value::Null,
    };
    browser
        .call(
            "",
            "emulation.setGeolocationOverride",
            scope.on(json!({"coordinates": coordinates})),
        )
        .await?;
    Ok(())
}

/// Grants `permissions` to `origin` in `context`, a Firefox user context
/// (the session's own when None). Firefox grants only to an origin.
pub(crate) async fn grant(
    browser: &Inner,
    context: Option<&str>,
    permissions: &[String],
    origin: Option<&str>,
) -> Result<()> {
    if permissions.is_empty() {
        return Ok(());
    }
    let Some(origin) = origin else {
        return Err(unsupported(
            "permissions for every origin (grant them to an origin)",
        ));
    };
    let user_context = user_context_of(browser, context);
    for name in permissions {
        let descriptor = crate::environment::permission(name)?;
        set_permission(browser, &user_context, origin, &descriptor, "granted")
            .await
            .map_err(|e| match e {
                Error::Protocol { message, .. } => {
                    Error::Invalid(format!("Firefox wouldn't grant {name:?}: {message}"))
                }
                e => e,
            })?;
        {
            let mut granted = browser.granted.lock().unwrap_or_else(|e| e.into_inner());
            granted.retain(|(c, o, d)| !(*c == user_context && o == origin && *d == descriptor));
            granted.push((user_context.clone(), origin.to_string(), descriptor.clone()));
        }
        seen_by_pages(browser, &user_context, origin, &descriptor, "granted").await;
    }
    Ok(())
}

/// Waits, for a few seconds at most, until each open page of
/// `user_context` at `origin` reads the permission `descriptor` as
/// `state`. Firefox tells a page's process of a change on its own time,
/// so a page asking at once could still read the old one; on Chrome the
/// change is the page's when the call returns, and so it is here.
async fn seen_by_pages(
    browser: &Inner,
    user_context: &str,
    origin: &str,
    descriptor: &Value,
    state: &str,
) {
    let pages: Vec<Page> = browser
        .pages
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter_map(std::sync::Weak::upgrade)
        .map(|inner| Page { inner })
        .filter(|page| {
            user_context_of(browser, page.inner.context.as_deref()) == user_context
                && page.url().starts_with(origin)
        })
        .collect();
    let ask = format!("navigator.permissions.query({descriptor}).then((p) => p.state)");
    let gives_up = tokio::time::Instant::now() + Duration::from_secs(5);
    for page in pages {
        while tokio::time::Instant::now() < gives_up {
            match helper(&page, &page.inner.target, &ask).await {
                Ok(v) if v != state => tokio::time::sleep(Duration::from_millis(50)).await,
                // Seen, or a permission the page can't ask about.
                _ => break,
            }
        }
    }
}

/// Takes back every permission granted in `context`: each is asked for
/// again, as it is in a new profile.
pub(crate) async fn clear_permissions(browser: &Inner, context: Option<&str>) -> Result<()> {
    let user_context = user_context_of(browser, context);
    let taken: Vec<(String, String, Value)> = {
        let mut granted = browser.granted.lock().unwrap_or_else(|e| e.into_inner());
        let (taken, kept) = granted.drain(..).partition(|(c, _, _)| *c == user_context);
        *granted = kept;
        taken
    };
    for (_, origin, descriptor) in taken {
        set_permission(browser, &user_context, &origin, &descriptor, "prompt").await?;
        seen_by_pages(browser, &user_context, &origin, &descriptor, "prompt").await;
    }
    Ok(())
}

async fn set_permission(
    browser: &Inner,
    user_context: &str,
    origin: &str,
    descriptor: &Value,
    state: &str,
) -> Result<()> {
    browser
        .call(
            "",
            "permissions.setPermission",
            json!({
                "descriptor": descriptor,
                "state": state,
                "origin": origin,
                "userContext": user_context,
            }),
        )
        .await?;
    Ok(())
}

/// A Firefox user context: `context`, or the session's own for None.
fn user_context_of(browser: &Inner, context: Option<&str>) -> String {
    context
        .map(str::to_string)
        .or_else(|| browser.default_context.clone())
        .unwrap_or_else(|| "default".into())
}

/// Runs `script` in every document the page and its frames load from now
/// on, in the page's own world, before its own scripts: only the page's,
/// as on Chrome. A preload script is a function, so the script is run by
/// indirect eval, which keeps its top-level declarations global, as a
/// script's are.
pub(super) async fn add_init_script(page: &Page, script: &str) -> Result<()> {
    script_on(&page.inner.browser, &Scope::of(page), script).await
}

async fn script_on(browser: &Inner, scope: &Scope, script: &str) -> Result<()> {
    let source = serde_json::to_string(script).unwrap_or_default();
    browser
        .call(
            "",
            "script.addPreloadScript",
            scope.on(json!({"functionDeclaration": format!("() => {{ (0, eval)({source}); }}")})),
        )
        .await?;
    Ok(())
}

/// Sends every request of the page and its frames with `headers` too.
pub(super) async fn set_extra_headers(page: &Page, headers: &[(String, String)]) -> Result<()> {
    change(
        page,
        !headers.is_empty(),
        |e| e.extra_headers.clear(),
        async |b, scope, env| headers_on(b, scope, &env.extra_headers).await,
    )
    .await
}

async fn headers_on(browser: &Inner, scope: &Scope, headers: &[(String, String)]) -> Result<()> {
    let headers: Vec<Value> = headers
        .iter()
        .map(|(name, value)| json!({"name": name, "value": {"type": "string", "value": value}}))
        .collect();
    browser
        .call(
            "",
            "network.setExtraHeaders",
            scope.on(json!({"headers": headers})),
        )
        .await?;
    Ok(())
}

/// Takes the page and its frames off the network, or puts them back.
pub(super) async fn set_offline(page: &Page, offline: bool) -> Result<()> {
    change(
        page,
        offline,
        |e| e.offline = None,
        async |b, scope, env| offline_on(b, scope, env.offline == Some(true)).await,
    )
    .await
}

async fn offline_on(browser: &Inner, scope: &Scope, offline: bool) -> Result<()> {
    let conditions = if offline {
        json!({"type": "offline"})
    } else {
        Value::Null
    };
    browser
        .call(
            "",
            "emulation.setNetworkConditions",
            scope.on(json!({"networkConditions": conditions})),
        )
        .await?;
    Ok(())
}

/// The most of one response's body a Firefox page keeps: Chrome's
/// default for one resource.
const BODY_KEPT: u64 = 10 * 1024 * 1024;

/// The body of a response a Firefox page got, once it has all arrived.
pub(super) async fn body(page: &Page, response: &Response) -> Result<Vec<u8>> {
    let id = response.request_id.clone();
    page.until(
        || format!("the body of {}", response.url),
        page.timeout(None),
        |s| !s.in_flight.contains(&id),
    )
    .await?;
    let (collector, current) = {
        let state = lock_state(&page.inner.shared);
        let current = state.made_for.contains_key(&id);
        (state.collector.clone(), current)
    };
    if !current {
        return Err(Error::Gone(format!(
            "the body of {} isn't available any more (the page may have navigated away, or \
             the request failed)",
            response.url
        )));
    }
    let got = page
        .inner
        .browser
        .call(
            "",
            "network.getData",
            json!({"dataType": "response", "request": id, "collector": collector}),
        )
        .await;
    let bytes = match got {
        Ok(r) => r["bytes"].clone(),
        Err(Error::Protocol { message, .. }) => {
            return Err(Error::Gone(format!(
                "the body of {} isn't available any more (the page may have navigated \
                 away, or the request failed): {message}",
                response.url
            )));
        }
        Err(e) => return Err(e),
    };
    let value = bytes["value"].as_str().unwrap_or_default();
    if bytes["type"] == "base64" {
        decoded("network.getData", &json!({"data": value}))
    } else {
        Ok(value.as_bytes().to_vec())
    }
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

/// Takes on the popup `target` that `opener` opened, showing `url`, as a
/// page with the opener's rules and environment, and tells the opener's
/// listeners. Its first request is its own to decide from the start: the
/// popup's handler is in place within this future's first poll, which
/// the reader thread makes. One whose setup fails is closed, with a
/// warning, rather than left to fetch without its rules.
async fn adopt(opener: Page, target: String, url: String) {
    let o = &opener.inner;
    let env = super::lock_env(&o.env).clone();
    let opening = Opening {
        context: o.context.clone(),
        owns_context: false,
        guard: o.decider.guard.clone(),
        shared_context: o.shared_context.clone(),
        opener: Some(opener.clone()),
        env: env.clone(),
    };
    let started = Page::start(o.browser.clone(), target.clone(), target, opening).await;
    let popup = match started {
        Ok(popup) => popup,
        Err(e) => return popup_failed(&opener, e),
    };
    // What the opener has beyond its user context's environment, the
    // popup gets now, and its first requests, which the opener's
    // intercept paused (see `holds_popups`), wait for it.
    let own = differences(&on_user_context(&env.env), &base_of(&opener));
    let apart = holds_popups(&own);
    {
        let mut state = lock_state(&popup.inner.shared);
        state.url = url;
        state.held = apart.then(Vec::new);
    }
    let ready = async {
        if apart {
            apply_env(&popup, &own, &Scope::of(&popup)).await?;
        }
        popup.refresh_fetch().await
    };
    let ready = ready.await;
    if apart {
        release(&popup, &env.env);
    }
    if let Err(e) = ready {
        let _ = popup.close().await;
        return popup_failed(&opener, e);
    }
    o.popups
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Arc::downgrade(&popup.inner));
    behind(&opener, &popup).await;
    opener.state().tell(PageEvent::Popup(popup));
}

/// How long Firefox gets to show a popup's tab in front of its opener.
const SHOWN: Duration = Duration::from_secs(2);

/// Waits until a popup opened as a tab of its opener's window is in front
/// of it, as Chrome's is by the time it tells of one: Firefox shows the
/// tab a moment after. A popup in a window of its own leaves its opener
/// showing.
async fn behind(opener: &Page, popup: &Page) {
    let window = |page: &Page| {
        let (page, target) = (page.clone(), page.inner.target.clone());
        async move {
            page.call(
                "browsingContext.getTree",
                json!({"root": target, "maxDepth": 0}),
            )
            .await
            .map(|t| str_of(&t["contexts"][0], "clientWindow"))
        }
    };
    let (Ok(theirs), Ok(its)) = (window(opener).await, window(popup).await) else {
        return;
    };
    if theirs != its {
        return;
    }
    let deadline = tokio::time::Instant::now() + SHOWN;
    while tokio::time::Instant::now() < deadline {
        match opener.main_frame().helper("document.visibilityState").await {
            Ok(v) if v == "visible" => tokio::time::sleep(Duration::from_millis(10)).await,
            _ => return,
        }
    }
}

/// Has the intercept that holds a page's popups' first requests (see
/// [`holds_popups`]) follow a change to its environment.
pub(super) async fn held_for_popups(page: &Page) -> Result<()> {
    page.refresh_fetch().await
}

/// Whether a page's popups need something of `env`, what it has beyond
/// its user context's environment, before their first request: Firefox
/// gives a popup only the user context's, so while a page has more, its
/// requests are paused, its popups' first among them.
pub(super) fn holds_popups(env: &Environment) -> bool {
    env.locale.is_some()
        || env.timezone.is_some()
        || env.user_agent.is_some()
        || env.screen.is_some()
        || env.geolocation.is_some()
        || !env.init_scripts.is_empty()
        || !env.extra_headers.is_empty()
        || env.offline == Some(true)
}

/// Lets go of what a popup's handler held while the popup was set up, in
/// `env`: each request is told of and decided as it would have been,
/// with the headers `env` gives it, which Firefox set before it paused
/// it; offline, it fails, as it would have.
fn release(popup: &Page, env: &Environment) {
    let (held, decider) = (
        lock_state(&popup.inner.shared)
            .held
            .take()
            .unwrap_or_default(),
        &popup.inner.decider,
    );
    for (mut params, guarded) in held {
        if env.offline == Some(true) {
            let request = str_of(&params["request"], "request");
            decider
                .conn
                .send("", "network.failRequest", json!({"request": request}));
            continue;
        }
        let headers = opened_with(bidi_headers(&params["request"]["headers"]), env);
        params["request"]["headers"] = headers
            .iter()
            .map(|(name, value)| json!({"name": name, "value": {"type": "string", "value": value}}))
            .collect();
        let navigation = params["navigation"].is_string();
        let request = Request::from_bidi(&params["request"], navigation);
        popup.state().tell(PageEvent::Request(request));
        decide(decider, &decider.runtime, &params, guarded, Some(headers));
    }
}

/// `headers` as `env` would have had Firefox send them: its user agent,
/// its locale's language and its extra headers.
fn opened_with(mut headers: Vec<(String, String)>, env: &Environment) -> Vec<(String, String)> {
    let mut set = |name: &str, value: &str| match headers
        .iter_mut()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
    {
        Some((_, v)) => *v = value.to_string(),
        None => headers.push((name.to_string(), value.to_string())),
    };
    if let Some(user_agent) = &env.user_agent {
        set("User-Agent", user_agent);
    }
    if let Some(locale) = &env.locale {
        set("Accept-Language", locale);
    }
    for (name, value) in &env.extra_headers {
        set(name, value);
    }
    headers
}

fn popup_failed(opener: &Page, e: Error) {
    if !super::went_away(&e) {
        crate::warn(&format!(
            "a popup of the page at {} was closed: it couldn't be set up to fetch as its \
             opener does: {e}",
            opener.url()
        ));
    }
}

/// What a Firefox page hears, kept in the same [`State`] Chrome's events
/// keep: a navigation's id is its loader, as a document's loader is on
/// Chrome. Only the page's own context navigates the page; requests from
/// any of its frames count towards network idle.
pub(super) fn handler(
    shared: Arc<Shared>,
    browser: &Inner,
    decider: Arc<Decider>,
    context: Option<String>,
    runtime: tokio::runtime::Handle,
) -> Handler {
    let (conn, timeout, pause) = (browser.conn.clone(), browser.timeout, browser.pause);
    let (downloads, pages) = (browser.downloads.clone(), browser.pages.clone());
    Arc::new(move |method, params| {
        // A popup the page or one of its frames opened, which the
        // connection hands the opener: it's taken on now, before
        // Firefox's next message is read, so it hears its own first
        // request.
        if method == "browsingContext.contextCreated"
            && params["parent"].is_null()
            && params["originalOpener"].is_string()
        {
            let opener = {
                let pages = pages.lock().unwrap_or_else(|e| e.into_inner());
                pages
                    .iter()
                    .filter_map(std::sync::Weak::upgrade)
                    .find(|inner| Arc::ptr_eq(&inner.shared, &shared))
                    .map(|inner| Page { inner })
            };
            if let Some(opener) = opener {
                crate::browser::adopt_now(
                    &runtime,
                    adopt(opener, str_of(params, "context"), str_of(params, "url")),
                );
            }
            return;
        }
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
                // The bodies of the document going go with it.
                if let Some(collector) = state.collector.clone() {
                    let gone: Vec<String> = state
                        .made_for
                        .iter()
                        .filter(|(_, l)| **l != loader)
                        .map(|(r, _)| r.clone())
                        .collect();
                    for request in gone {
                        state.made_for.remove(&request);
                        conn.send(
                            "",
                            "network.disownData",
                            json!({"dataType": "response", "collector": collector, "request": request}),
                        );
                    }
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
                let url = str_of(params, "url");
                note_origin(&state.origins, &url);
                if state.frame_urls.len() >= KEPT * 4 {
                    state.frame_urls.clear();
                }
                state.frame_urls.insert(str_of(params, "context"), url);
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
            // A script writing into a popup's first, empty document (what
            // `window.open('')` gives it) makes it a document of its own,
            // with the writer's URL, which Firefox tells of only by this.
            // Chrome loads the empty document first, and so it is here.
            "browsingContext.historyUpdated"
                if own && state.committed.is_empty() && state.starts.is_empty() =>
            {
                let loader = format!("{}.written", state.frame);
                state.url = str_of(params, "url");
                state.loader = loader.clone();
                state.navigations += 1;
                state.committed.push(loader.clone());
                state
                    .events
                    .insert(loader, vec!["DOMContentLoaded".into(), "load".into()]);
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
            // In the page's own frame or any of its others: the page's.
            "browsingContext.userPromptOpened" => {
                let url = match own {
                    true => state.url.clone(),
                    false => state
                        .frame_urls
                        .get(params["context"].as_str().unwrap_or_default())
                        .cloned()
                        .unwrap_or_default(),
                };
                let dialog = crate::events::Dialog::opened_bidi(
                    &conn, params, url, timeout, pause, &runtime,
                );
                state.prompts += 1;
                state.open_prompts += 1;
                // Nobody listening drops it, which answers it.
                if let Some(to) = &state.dialogs
                    && to.send(dialog).is_err()
                {
                    state.dialogs = None;
                }
            }
            "browsingContext.downloadWillBegin" => {
                if let Some(download) =
                    downloads.begin_bidi(&conn, timeout, context.as_deref(), params)
                {
                    state.tell(PageEvent::Download(download));
                }
            }
            "browsingContext.downloadEnd" => downloads.end_bidi(params),
            "browsingContext.userPromptClosed" => {
                state.open_prompts = state.open_prompts.saturating_sub(1);
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
                if let Some(collector) = &decider.collector {
                    unheld_body(&conn, collector, params);
                }
                if state.in_flight.len() >= LOADING {
                    state.in_flight.clear();
                }
                state
                    .in_flight
                    .insert(str_of(&params["request"], "request"));
                state.quiet += 1;
                let navigation = params["navigation"].as_str();
                let request = Request::from_bidi(&params["request"], navigation.is_some());
                if state.made_for.len() >= LOADING {
                    state.made_for.clear();
                }
                // The page's own document request belongs to that
                // document, not to the one it replaces; a frame's, and
                // everything else, to the document showing.
                let loader = match navigation {
                    Some(n) if own => n.to_string(),
                    _ => state.loader.clone(),
                };
                state.made_for.insert(request.id.clone(), loader);
                let guarded = own && navigation.is_some();
                if params["isBlocked"] == true
                    && let Some(held) = &mut state.held
                {
                    held.push((params.clone(), guarded));
                    return;
                }
                state.tell(PageEvent::Request(request));
                if params["isBlocked"] == true {
                    decide(&decider, &runtime, params, guarded, None);
                }
            }
            // Its headers are in: when Chrome tells of a response too.
            "network.responseStarted" => {
                let response = response_of(params);
                state.tell(PageEvent::Response(response));
            }
            "network.fetchError" => {
                let request = str_of(&params["request"], "request");
                let refused = decider.refused.take(&request);
                if own && let Some(navigation) = params["navigation"].as_str() {
                    if state.failures.len() >= KEPT {
                        state.failures.clear();
                    }
                    let error = if refused {
                        "net::ERR_BLOCKED_BY_CLIENT".to_string()
                    } else {
                        chrome_error(params["errorText"].as_str().unwrap_or_default())
                    };
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
                state
                    .responses
                    .insert(navigation.to_string(), response_of(params));
            }
            _ => return,
        }
        drop(state);
        shared.changed.notify_waiters();
    })
}

/// Puts a request the page's intercept paused to the guard first, if
/// it's a document for the page itself and there's a guard; then to the
/// rules, which let it go on unless they block or route it. The guard is
/// async and the reader thread can't wait, so it's asked on a task.
fn decide(
    decider: &Arc<Decider>,
    runtime: &tokio::runtime::Handle,
    params: &Value,
    guarded: bool,
    tidied: Option<Vec<(String, String)>>,
) {
    let Some(guard) = decider.guard.clone().filter(|_| guarded) else {
        decider.decide_bidi(params, tidied);
        return;
    };
    let (decider, params) = (decider.clone(), params.clone());
    runtime.spawn(async move {
        if guard(str_of(&params["request"], "url")).await {
            decider.decide_bidi(&params, tidied);
        } else {
            let request = str_of(&params["request"], "request");
            decider.refused.add(request.clone());
            decider
                .conn
                .send("", "network.failRequest", json!({"request": request}));
        }
    });
}

/// Has Firefox pause the page's requests while its rules, its context's,
/// its guard or its proxy's login need them, and none otherwise. The
/// intercept is the browser's: one on the page's context would miss its
/// popups, and Firefox takes no user context for one. Every other page's
/// paused requests go on at once, by its own rules or the browser's
/// handler. Page script can't see an intercept.
pub(super) async fn refresh_intercept(page: &Page) -> Result<()> {
    let decider = &page.inner.decider;
    let _one = decider.fetching.lock().await;
    let wanted = !decider.patterns().is_empty()
        || holds_popups(&differences(
            &super::lock_env(&page.inner.env).env,
            &base_of(page),
        ));
    let held = decider
        .intercept
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let browser = &page.inner.browser;
    let now = match (wanted, held) {
        (true, None) => {
            let r = browser
                .call(
                    "",
                    "network.addIntercept",
                    json!({"phases": ["beforeRequestSent"]}),
                )
                .await?;
            Some(str_of(&r, "intercept"))
        }
        (false, Some(id)) => {
            browser
                .call("", "network.removeIntercept", json!({"intercept": id}))
                .await?;
            None
        }
        (_, held) => held,
    };
    *decider.intercept.lock().unwrap_or_else(|e| e.into_inner()) = now;
    Ok(())
}

/// The response a `network.responseStarted` or `responseCompleted` event
/// tells of, as Chrome's would read.
fn response_of(params: &Value) -> Response {
    let (request, response) = (&params["request"], &params["response"]);
    let navigation = params["navigation"].as_str();
    Response {
        url: str_of(response, "url"),
        status: response["status"].as_u64().unwrap_or(0) as u16,
        headers: bidi_headers(&response["headers"]),
        loader: navigation.unwrap_or_default().to_string(),
        request_id: str_of(request, "request"),
        session: String::new(),
        method: str_of(request, "method"),
        resource_type: bidi_type(request, navigation.is_some()).into(),
    }
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

/// The failure netweir gave a navigation started after `begun`, when it
/// blocked it (its guard, its blocks, a route): Firefox fails `navigate`
/// with only "aborted", and tells the request's own failure as an event,
/// a moment before or after.
async fn refused_start(page: &Page, begun: u64) -> Option<String> {
    let failure = |s: &State| {
        s.starts
            .iter()
            .filter(|st| st.number > begun)
            .find_map(|st| s.failures.get(&st.loader))
            .filter(|e| e.contains("BLOCKED_BY_CLIENT"))
            .cloned()
    };
    // The event comes on the same connection, so it's due at once.
    let _ = page
        .until(
            || "the failure of a navigation".into(),
            Duration::from_millis(500),
            |s| failure(s).is_some(),
        )
        .await;
    failure(&page.state())
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
        ("NS_ERROR_DOCUMENT_NOT_CACHED", "ERR_CACHE_MISS"),
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
    let (moved, begun) = {
        let state = page.state();
        (state.moved_within, state.started)
    };
    let navigate = page.inner.browser.conn.call(
        "",
        "browsingContext.navigate",
        json!({"context": page.inner.target, "url": url, "wait": "none"}),
    );
    let navigated = match tokio::time::timeout_at(deadline, navigate)
        .await
        .map_err(|_| Error::Timeout(format!("navigating to {url}")))?
    {
        Ok(navigated) => navigated,
        // Closed while it waited: its context is gone.
        Err(_) if page.inner.closed.load(Ordering::Relaxed) => return Err(Error::PageClosed),
        Err(Error::Protocol { message, .. }) => {
            // A request netweir failed reads to Firefox as aborted.
            let refused = match message.contains("NS_ERROR_ABORT") {
                true => refused_start(page, begun).await,
                false => None,
            };
            let error = refused.unwrap_or_else(|| chrome_error(&message));
            return Err(page.navigation_error(url, &error));
        }
        Err(e) => return Err(e),
    };
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
    let moved = page
        .until(what, left(), |s| {
            ours(s).is_some() || s.navigations > navigations
        })
        .await;
    if moved.is_err() {
        // Not left to name a navigation the page starts itself later.
        page.state().next_kind = None;
    }
    moved?;
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

/// Waits until none of the page's dialogs is open. Chrome runs nothing in
/// a page's own world while it has one open, and Firefox would: a script
/// would see the page from before its dialog was answered.
pub(crate) async fn no_dialog_open(page: &Page) -> Result<()> {
    loop {
        let changed = page.inner.shared.changed.notified();
        {
            let s = page.state();
            if s.open_prompts == 0 {
                return Ok(());
            }
            if s.gone || page.inner.closed.load(Ordering::Relaxed) {
                return Err(Error::PageClosed);
            }
        }
        changed.await;
    }
}

/// Runs `body` (a function body; `await` allowed) as an async function in
/// the page's main frame: in the page's own world, or with `sandboxed`,
/// in netweir's, where page script can't see it.
pub(super) async fn call(page: &Page, body: &str, sandboxed: bool) -> Result<Value> {
    let r = remote(page, &page.inner.target, body, sandboxed, false).await?;
    to_json(&r)
}

/// Runs `body` as an async function in the frame `context`, in its own
/// world or netweir's sandbox, and gives what it returns as BiDi has it.
/// `bounded` holds it to the browser's timeout, as netweir's own calls
/// are; the caller's own script may take as long as it likes.
pub(crate) async fn remote(
    page: &Page,
    context: &str,
    body: &str,
    sandboxed: bool,
    bounded: bool,
) -> Result<Value> {
    if page.inner.closed.load(Ordering::Relaxed) {
        return Err(Error::PageClosed);
    }
    let mut target = json!({"context": context});
    if sandboxed {
        target["sandbox"] = Value::from(SANDBOX);
    }
    let params = json!({
        "functionDeclaration": format!("async () => {{ {body}\n}}"),
        "awaitPromise": true,
        "target": target,
        "resultOwnership": "none",
    });
    let browser = &page.inner.browser;
    let r = if bounded {
        browser.call("", "script.callFunction", params).await?
    } else {
        browser.conn.call("", "script.callFunction", params).await?
    };
    if r["type"] == "exception" {
        let details = &r["exceptionDetails"];
        let message = details["text"]
            .as_str()
            .filter(|t| !t.is_empty())
            .unwrap_or("script error");
        return Err(Error::Script(message.to_string()));
    }
    Ok(r["result"].clone())
}

/// Runs `expression` in netweir's sandbox of the frame `context`, with
/// its helpers, which go into a document's sandbox the first time they're
/// needed there, and awaits it. A document that went while it ran, which
/// takes its sandbox with it, has it run once more in the one after.
pub(crate) async fn helper(page: &Page, context: &str, expression: &str) -> Result<Value> {
    to_json(&helper_remote(page, context, expression).await?)
}

/// [`helper`], giving what it returns as BiDi has it: a node as one BiDi
/// can name in other commands.
pub(crate) async fn helper_remote(page: &Page, context: &str, expression: &str) -> Result<Value> {
    let body = format!("return await ({expression}\n);");
    // A document that goes between the helpers' loading and the call
    // leaves them behind, and the next is loaded again: a few times, for a
    // page that moves on quickly.
    let (mut loads, mut again) = (0, false);
    loop {
        match remote(page, context, &body, true, true).await {
            Err(Error::Script(m)) if m.contains("__netweir is not defined") && loads < 3 => {
                remote(page, context, crate::frame::HELPERS, true, true).await?;
                loads += 1;
            }
            Err(Error::Protocol { message, .. }) if gone_under(&message) && !again => {
                again = true;
                loads = 0;
            }
            r => return r,
        }
    }
}

/// Gives the `<input type=file>` `selector` finds in the frame `context`
/// these files, as choosing them would.
pub(crate) async fn set_files(
    page: &Page,
    context: &str,
    selector: &str,
    files: &[String],
) -> Result<()> {
    let input = helper_remote(
        page,
        context,
        &format!("__netweir.find({})", Value::from(selector)),
    )
    .await?;
    let Some(shared) = input["sharedId"].as_str() else {
        return Err(Error::Script(format!(
            "set_input_files {selector}: it went away"
        )));
    };
    page.call(
        "input.setFiles",
        json!({"context": context, "element": {"sharedId": shared}, "files": files}),
    )
    .await
    .map(drop)
}

/// Whether Firefox said a script's realm went while it ran: its document
/// was replaced.
fn gone_under(message: &str) -> bool {
    ["realm", "has been discarded", "got discarded"]
        .iter()
        .any(|m| message.contains(m))
}

/// The page's frames, its own first, each as the frame tree had it. A
/// frame's name is its window's, which the tree doesn't carry; a frame
/// gone before its name was read is left out.
pub(super) async fn frames(page: &Page) -> Result<Vec<crate::Frame>> {
    let tree = page
        .call(
            "browsingContext.getTree",
            json!({"root": page.inner.target}),
        )
        .await?;
    let mut frames = Vec::new();
    let mut walk: Vec<(&Value, Option<String>)> = vec![(&tree["contexts"][0], None)];
    while let Some((node, parent)) = walk.pop() {
        let id = str_of(node, "context");
        let name = match &parent {
            None => String::new(),
            Some(_) => match helper(page, &id, "window.name").await {
                Ok(name) => name.as_str().unwrap_or_default().to_string(),
                Err(Error::Protocol { .. }) => continue,
                Err(e) => return Err(e),
            },
        };
        if let Some(children) = node["children"].as_array() {
            walk.extend(children.iter().rev().map(|c| (c, Some(id.clone()))));
        }
        frames.push(crate::Frame::new(
            page.clone(),
            page.inner.session.clone(),
            id,
            parent,
            name,
            str_of(node, "url"),
        ));
    }
    Ok(frames)
}

/// Which of the `<iframe>`s and `<frame>`s in the frame `parent` holds
/// the frame `child`, by its place among them, found by its window, which
/// BiDi names by its context: from another site too.
pub(crate) async fn holder(page: &Page, parent: &str, child: &str) -> Result<Option<usize>> {
    let windows = remote(
        page,
        parent,
        "return [...document.querySelectorAll('iframe, frame')].map(f => f.contentWindow);",
        true,
        true,
    )
    .await?;
    Ok(windows["value"]
        .as_array()
        .into_iter()
        .flatten()
        .position(|w| w["value"]["context"] == child))
}

/// Sends `sources`, BiDi's input sources each with its actions, to the
/// page. Firefox keeps each source's state between calls: a key or button
/// that went down stays down until it comes up.
///
/// Firefox doesn't answer until the page has handled what the actions
/// did, and a dialog that opens meanwhile (a click's alert) holds that
/// up until it's answered. Chrome answers once it opens, and so does this:
/// the caller may be the one to answer it.
async fn perform(page: &Page, context: &str, sources: Value) -> Result<Value> {
    let opened = page.state().prompts;
    let performed = page.call(
        "input.performActions",
        json!({"context": context, "actions": sources}),
    );
    let asked = async {
        loop {
            let changed = page.inner.shared.changed.notified();
            if page.state().prompts != opened {
                return;
            }
            changed.await;
        }
    };
    tokio::select! {
        r = performed => r,
        () = asked => Ok(Value::Null),
    }
}

/// The key Chrome's `event` names, going down or up, as one keyboard
/// action. Firefox makes the key's code from it and does what the key
/// does in a field (Control-A selects all) itself, so Chrome's edit
/// commands aren't needed.
pub(super) async fn key_event(page: &Page, event: &Value) -> Result<Value> {
    let kind = match event["type"].as_str() {
        Some("keyUp") => "keyUp",
        _ => "keyDown",
    };
    let value = key_value(
        event["key"].as_str().unwrap_or_default(),
        event["code"].as_str().unwrap_or_default(),
    );
    // Keys go to the frame that has the focus: Firefox sends those for
    // one from another site no further than its own frame.
    let context = focused(page).await?;
    perform(
        page,
        &context,
        json!([{"type": "key", "id": "keyboard", "actions": [{"type": kind, "value": value}]}]),
    )
    .await
}

/// WebDriver's value for a key: the character it types, or for a key that
/// types none, its code point in WebDriver's table.
fn key_value(key: &str, code: &str) -> String {
    let right = code.ends_with("Right");
    let point = match key {
        "Unidentified" => '\u{E000}',
        "Cancel" => '\u{E001}',
        "Help" => '\u{E002}',
        "Backspace" => '\u{E003}',
        "Tab" => '\u{E004}',
        "Clear" => '\u{E005}',
        "Enter" if code == "NumpadEnter" => '\u{E007}',
        "Enter" => '\u{E006}',
        "Shift" if right => '\u{E050}',
        "Shift" => '\u{E008}',
        "Control" if right => '\u{E051}',
        "Control" => '\u{E009}',
        "Alt" if right => '\u{E052}',
        "Alt" => '\u{E00A}',
        "Meta" if right => '\u{E053}',
        "Meta" => '\u{E03D}',
        "Pause" => '\u{E00B}',
        "Escape" => '\u{E00C}',
        "PageUp" => '\u{E00E}',
        "PageDown" => '\u{E00F}',
        "End" => '\u{E010}',
        "Home" => '\u{E011}',
        "ArrowLeft" => '\u{E012}',
        "ArrowUp" => '\u{E013}',
        "ArrowRight" => '\u{E014}',
        "ArrowDown" => '\u{E015}',
        "Insert" => '\u{E016}',
        "Delete" => '\u{E017}',
        f if f.len() > 1
            && f.starts_with('F')
            && f[1..].parse::<u32>().is_ok_and(|n| (1..=12).contains(&n)) =>
        {
            char::from_u32(0xE031 + f[1..].parse::<u32>().unwrap_or(1) - 1).unwrap_or('\u{E000}')
        }
        // A character: the key that types it.
        other => return other.to_string(),
    };
    point.to_string()
}

/// Chrome's mouse `event` as the mouse's own action: a move, a button
/// going down or up where the mouse is, or a turn of the wheel.
pub(super) async fn mouse_event(page: &Page, event: &Value) -> Result<Value> {
    // The point is in the page's viewport. Firefox sends what reaches a
    // frame from another site only from that frame's own context, at its
    // point there, so the mouse of an action on a frame goes that way.
    let (context, dx, dy) = page
        .state()
        .input_frame
        .clone()
        .unwrap_or_else(|| (page.inner.target.clone(), 0.0, 0.0));
    let x = event["x"].as_f64().unwrap_or(0.0) - dx;
    let y = event["y"].as_f64().unwrap_or(0.0) - dy;
    let button = match event["button"].as_str() {
        Some("middle") => 1,
        Some("right") => 2,
        _ => 0,
    };
    let mouse = |action: Value| json!([{"type": "pointer", "id": "mouse", "parameters": {"pointerType": "mouse"}, "actions": [action]}]);
    let sources = match event["type"].as_str().unwrap_or_default() {
        "mouseMoved" => mouse(json!({"type": "pointerMove", "x": x, "y": y, "duration": 0})),
        "mousePressed" => mouse(json!({"type": "pointerDown", "button": button})),
        "mouseReleased" => mouse(json!({"type": "pointerUp", "button": button})),
        // The wheel's place and turn are whole pixels to WebDriver.
        "mouseWheel" => json!([{"type": "wheel", "id": "wheel", "actions": [{
            "type": "scroll",
            "x": x.round() as i64,
            "y": y.round() as i64,
            "deltaX": event["deltaX"].as_f64().unwrap_or(0.0).round() as i64,
            "deltaY": event["deltaY"].as_f64().unwrap_or(0.0).round() as i64,
            "duration": 0,
        }]}]),
        other => {
            return Err(Error::Invalid(format!("no mouse event {other:?}")));
        }
    };
    perform(page, &context, sources).await
}

/// Clicks twice where the mouse is, as one double click. Firefox counts
/// clicks only within one chain of actions, and starts again at a move,
/// so both clicks go in one chain, with `pauses` (the first button's hold,
/// the gap, the second's hold) between their presses and releases.
pub(super) async fn mouse_double(page: &Page, pauses: [Duration; 3]) -> Result<Value> {
    let context = page
        .state()
        .input_frame
        .clone()
        .map_or_else(|| page.inner.target.clone(), |(c, _, _)| c);
    let wait = |d: Duration| json!({"type": "pause", "duration": d.as_millis() as u64});
    let actions = json!([
        {"type": "pointerDown", "button": 0},
        wait(pauses[0]),
        {"type": "pointerUp", "button": 0},
        wait(pauses[1]),
        {"type": "pointerDown", "button": 0},
        wait(pauses[2]),
        {"type": "pointerUp", "button": 0},
    ]);
    let sources = json!([{"type": "pointer", "id": "mouse", "parameters": {"pointerType": "mouse"}, "actions": actions}]);
    perform(page, &context, sources).await
}

/// Puts `text` in at the caret of the frame with the focus, as an OS's
/// character picker does: no key, and an input event the page sees as
/// trusted, as with Chrome's insertText.
pub(super) async fn insert_text(page: &Page, text: &str) -> Result<Value> {
    let context = focused(page).await?;
    let insert = format!(
        "document.execCommand('insertText', false, {})",
        Value::from(text)
    );
    helper(page, &context, &insert).await?;
    Ok(Value::Null)
}

/// The context of the frame with the focus: the one whose document has it
/// and whose focused element isn't a frame within it. The frame last acted
/// on is asked first, as it usually is the one; the page's own when none
/// has it.
async fn focused(page: &Page) -> Result<String> {
    const HERE: &str =
        "document.hasFocus() && !['IFRAME', 'FRAME'].includes(document.activeElement?.tagName)";
    let has = |context: String| async move {
        (helper(page, &context, HERE).await.ok() == Some(Value::Bool(true))).then_some(context)
    };
    let last = page.state().input_frame.clone().map(|(c, _, _)| c);
    let first = last.unwrap_or_else(|| page.inner.target.clone());
    if let Some(context) = has(first).await {
        return Ok(context);
    }
    for frame in frames(page).await?.iter().rev() {
        if let Some(context) = has(frame.id().to_string()).await {
            return Ok(context);
        }
    }
    Ok(page.inner.target.clone())
}

/// Drags with the left button from `from`, where the mouse already is, to
/// `to`, as [`Page::mouse_drag`] does. An HTML5 drag the page begins on
/// the way is Firefox's own to carry on, as with a person's mouse.
pub(super) async fn mouse_drag(page: &Page, from: (f64, f64), to: (f64, f64)) -> Result<()> {
    let at =
        |(x, y): (f64, f64), kind: &str| json!({"type": kind, "x": x, "y": y, "button": "left"});
    page.mouse_event(at(from, "mousePressed")).await?;
    let mut now = from;
    let dragged = async {
        tokio::time::sleep(crate::events::pause(super::BUTTON_HOLD)).await;
        // As Chrome's: a step about every 20 pixels, eased at both ends.
        let distance = (to.0 - from.0).hypot(to.1 - from.1);
        let steps = (distance / 20.0).ceil().clamp(5.0, 40.0) as u32;
        for i in 1..=steps {
            let t = f64::from(i) / f64::from(steps);
            let t = 0.5 - 0.5 * (std::f64::consts::PI * t).cos();
            now = (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t);
            page.mouse_event(at(now, "mouseMoved")).await?;
            tokio::time::sleep(Duration::from_millis(16)).await;
        }
        tokio::time::sleep(crate::events::pause(super::BUTTON_HOLD)).await;
        Ok::<_, Error>(())
    }
    .await;
    // Let go wherever the mouse got to, even after a failure: a button
    // left down would change every click after.
    let released = page.mouse_event(at(now, "mouseReleased")).await;
    dragged?;
    released.map(drop)
}

/// A PNG of the viewport or the whole page, or with a `quality` (1 to
/// 100), a JPEG of that quality.
pub(super) async fn capture(page: &Page, full_page: bool, quality: Option<u8>) -> Result<Vec<u8>> {
    let format = match quality {
        Some(q) => json!({"type": "image/jpeg", "quality": f64::from(q.clamp(1, 100)) / 100.0}),
        None => json!({"type": "image/png"}),
    };
    let r = page
        .call(
            "browsingContext.captureScreenshot",
            json!({
                "context": page.inner.target,
                "origin": if full_page { "document" } else { "viewport" },
                "format": format,
            }),
        )
        .await?;
    decoded("browsingContext.captureScreenshot", &r)
}

/// Centimetres in an inch: `PdfOptions` is in inches, as Chrome's
/// printing is, and BiDi's in centimetres.
const CM: f64 = 2.54;

/// The page as a PDF, laid out for printing as [`Page::pdf`] says.
pub(super) async fn pdf(page: &Page, options: &super::PdfOptions) -> Result<Vec<u8>> {
    let mut params = json!({
        "context": page.inner.target,
        "background": options.print_background,
        "orientation": if options.landscape { "landscape" } else { "portrait" },
        // As Chrome prints: at the scale asked for, not shrunk to fit.
        "shrinkToFit": false,
    });
    if let Some(scale) = options.scale {
        params["scale"] = json!(scale);
    }
    if let Some((width, height)) = options.paper {
        params["page"] = json!({"width": width * CM, "height": height * CM});
    }
    if let Some(m) = options.margin {
        let m = m * CM;
        params["margin"] = json!({"top": m, "bottom": m, "left": m, "right": m});
    }
    if let Some(ranges) = &options.page_ranges {
        // "1-5, 8" is a list to BiDi, of page numbers and ranges.
        let ranges: Vec<Value> = ranges
            .split(',')
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .map(|r| match r.parse::<u64>() {
                Ok(n) => Value::from(n),
                Err(_) => Value::from(r),
            })
            .collect();
        params["pageRanges"] = Value::Array(ranges);
    }
    let r = page.call("browsingContext.print", params).await?;
    decoded("browsingContext.print", &r)
}

/// The base64 `data` of `method`'s answer, as bytes.
fn decoded(method: &str, answer: &Value) -> Result<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(answer["data"].as_str().unwrap_or_default())
        .map_err(|e| Error::Protocol {
            method: method.into(),
            message: e.to_string(),
        })
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
        browser.downloads.remove(Some(id));
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

/// The cookies of `context`, a Firefox user context (the session's own
/// when None), as Chrome's would read.
pub(crate) async fn cookies(browser: &Inner, context: Option<&str>) -> Result<Vec<Cookie>> {
    let r = browser
        .call(
            "",
            "storage.getCookies",
            json!({"partition": partition(browser, context)}),
        )
        .await?;
    Ok(r["cookies"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| Cookie {
            name: str_of(c, "name"),
            value: bidi_text(&c["value"]),
            domain: str_of(c, "domain"),
            path: str_of(c, "path"),
            expires: c["expiry"].as_f64().unwrap_or(-1.0),
            http_only: c["httpOnly"].as_bool().unwrap_or(false),
            secure: c["secure"].as_bool().unwrap_or(false),
            same_site: match c["sameSite"].as_str() {
                Some("strict") => Some("Strict".into()),
                Some("lax") => Some("Lax".into()),
                Some("none") => Some("None".into()),
                _ => None,
            },
            partition_key: None,
        })
        .collect())
}

/// Sets `cookies` in `context`, a Firefox user context (the session's
/// own when None). A domain with a leading dot is the domain's and its
/// subdomains', one without it the host's alone, as Firefox keeps them.
pub(crate) async fn set_cookies(
    browser: &Inner,
    context: Option<&str>,
    cookies: &[Cookie],
) -> Result<()> {
    if let Some(c) = cookies.iter().find(|c| c.partition_key.is_some()) {
        return Err(Error::Unsupported {
            engine: "Firefox",
            feature: format!("the partitioned cookie {:?}", c.name),
        });
    }
    for c in cookies {
        let mut cookie = json!({
            "name": c.name,
            "value": {"type": "string", "value": c.value},
            "domain": c.domain,
            "path": if c.path.is_empty() { "/" } else { &c.path },
            "httpOnly": c.http_only,
            "secure": c.secure,
        });
        if c.expires >= 0.0 {
            cookie["expiry"] = Value::from(c.expires as u64);
        }
        if let Some(s) = &c.same_site {
            cookie["sameSite"] = Value::from(s.to_ascii_lowercase());
        }
        browser
            .call(
                "",
                "storage.setCookie",
                json!({"cookie": cookie, "partition": partition(browser, context)}),
            )
            .await?;
    }
    Ok(())
}

/// The storage partition of a Firefox user context.
fn partition(browser: &Inner, context: Option<&str>) -> Value {
    json!({"type": "storageKey", "userContext": user_context_of(browser, context)})
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
