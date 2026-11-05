//! A frame of a page: the page's own document, or one in an `<iframe>`.
//! Everything that finds elements by selector runs in a frame, in
//! netweir's isolated world for that frame's document.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::Instant;

use crate::browser::str_of;
use crate::page::{NOTCH, Page};
use crate::{Error, Result};

/// netweir's helpers, installed in each document's isolated world. The
/// page can't see them: the world shares the DOM, not the globals.
pub(crate) const HELPERS: &str = include_str!("helpers.js");

/// How many isolated worlds a page keeps track of. Past that (a page that
/// has had hundreds of frames), the record starts again, and worlds are
/// made afresh as they're needed.
const WORLDS: usize = 64;

/// How long a person takes to see the caret, or the selection, before
/// typing.
const REACTION: (Duration, Duration) = (Duration::from_millis(150), Duration::from_millis(400));

/// At most how long typing `text` with `delay` between keys takes: each
/// key held its longest, Shift and all, the longest pause between keys,
/// and Chrome's own time for each event, as well as the click and the
/// moments to react.
pub fn typing_time(text: &str, delay: (Duration, Duration)) -> Duration {
    let keys = u32::try_from(text.chars().count()).unwrap_or(u32::MAX);
    // A held key, Shift's lead either side of it, and four events.
    let per_key = crate::events::KEY_HOLD_MAX + Duration::from_millis(180 + 4 * 25);
    per_key
        .saturating_add(delay.1)
        .saturating_mul(keys)
        .saturating_add(Duration::from_secs(2))
}

/// Which options [`Frame::select_option`] picks.
#[derive(Debug, Clone, PartialEq)]
pub enum Pick {
    Values(Vec<String>),
    /// By the text the option shows (its `label`, or its text).
    Labels(Vec<String>),
    Indexes(Vec<u32>),
}

/// A frame of a page. Cheap to clone. Its name and URL are as they were
/// when it was found; its actions work on the document it shows now.
#[derive(Clone)]
pub struct Frame {
    page: Page,
    /// The session that reached it when it was found: the page's, or, for
    /// a frame Chrome runs in a process of its own, that frame's. A frame
    /// that moves to another site, or back, moves process and session, so
    /// [`Frame::session`] looks again.
    session: String,
    id: String,
    parent: Option<String>,
    name: String,
    url: String,
}

impl Frame {
    pub(crate) fn new(
        page: Page,
        session: String,
        id: String,
        parent: Option<String>,
        name: String,
        url: String,
    ) -> Frame {
        Frame {
            page,
            session,
            id,
            parent,
            name,
            url,
        }
    }

    /// Chrome's id for the frame.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The frame's `name` attribute; empty for the page's own.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// The id of the frame it's in; None for the page's own.
    pub fn parent_id(&self) -> Option<&str> {
        self.parent.as_deref()
    }

    pub fn page(&self) -> &Page {
        &self.page
    }

    fn is_main(&self) -> bool {
        self.parent.is_none() && self.session == self.page.inner.session
    }

    /// The session that reaches the frame now. A frame Chrome has just
    /// moved to a process of its own has a session named for it; one whose
    /// session has gone (it came back to the page's process, say) is
    /// looked for in the frame tree.
    async fn session(&self) -> Result<String> {
        if self.is_main() {
            return Ok(self.session.clone());
        }
        {
            let state = self.page.state();
            // The newest, if a frame moved from one other site to another.
            if let Some((s, _)) = state.children.iter().rfind(|(_, f)| *f == self.id) {
                return Ok(s.clone());
            }
            let alive = self.session == self.page.inner.session
                || state.children.iter().any(|(s, _)| *s == self.session);
            if alive {
                return Ok(self.session.clone());
            }
        }
        self.find().await
    }

    /// The session the frame tree has the frame in now; Gone once it has
    /// been removed from the page.
    async fn find(&self) -> Result<String> {
        let frames = self.page.frames().await?;
        match frames.into_iter().find(|f| f.id == self.id) {
            Some(f) => Ok(f.session),
            None => Err(Error::Gone(format!(
                "the frame at {} isn't in the page any more",
                self.url
            ))),
        }
    }

    /// A Firefox frame's context Firefox no longer has is a frame gone
    /// from the page.
    fn gone_if_lost(&self, e: Error) -> Error {
        match e {
            Error::Protocol { message, .. }
                if message.contains("Browsing Context with id")
                    && message.contains("not found") =>
            {
                Error::Gone(format!(
                    "the frame at {} isn't in the page any more",
                    self.url
                ))
            }
            e => e,
        }
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let session = self.session().await?;
        self.page.call_on(&session, method, params).await
    }

    async fn call_unbounded(&self, method: &str, params: Value) -> Result<Value> {
        let session = self.session().await?;
        self.page.call_unbounded_on(&session, method, params).await
    }

    /// The isolated world of the frame's document, made (with the helpers
    /// in it) the first time it's needed. The main frame's goes with its
    /// document; another frame's is made again once it's found gone.
    async fn world(&self, fresh: bool) -> Result<i64> {
        let mut worlds = self.page.inner.worlds.lock().await;
        let loader = if self.is_main() {
            self.page.state().loader.clone()
        } else {
            String::new()
        };
        let mut session = self.session().await?;
        if let Some((l, id)) = worlds.get(&(session.clone(), self.id.clone()))
            && *l == loader
            && !fresh
        {
            return Ok(*id);
        }
        let create = json!({"frameId": self.id});
        let r = match self
            .page
            .call_on(&session, "Page.createIsolatedWorld", create.clone())
            .await
        {
            Ok(r) => r,
            // Between processes, or gone: the frame tree says which.
            Err(Error::Protocol { .. }) if !self.is_main() => {
                let found = self.find().await?;
                if found == session {
                    return Err(Error::Gone(format!(
                        "the frame at {} isn't in the page any more",
                        self.url
                    )));
                }
                session = found;
                self.page
                    .call_on(&session, "Page.createIsolatedWorld", create)
                    .await?
            }
            Err(e) => return Err(e),
        };
        let key = (session, self.id.clone());
        let id = r["executionContextId"].as_i64().unwrap_or_default();
        self.run(Some(id), HELPERS, true).await?;
        if worlds.len() >= WORLDS {
            worlds.clear();
        }
        worlds.insert(key, (loader, id));
        Ok(id)
    }

    /// Runs `expression` in netweir's isolated world, awaiting a promise.
    pub(crate) async fn helper(&self, expression: &str) -> Result<Value> {
        if self.page.is_firefox() {
            return crate::page::bidi::helper(&self.page, &self.id, expression)
                .await
                .map_err(|e| self.gone_if_lost(e));
        }
        let id = self.world(false).await?;
        match self.run(Some(id), expression, true).await {
            // The document changed under it, or the frame moved to another
            // process; once more in the new one.
            Err(Error::Protocol { message, .. })
                if message.contains("context") || !self.is_main() =>
            {
                let id = self.world(true).await?;
                self.run(Some(id), expression, true).await
            }
            other => other,
        }
    }

    /// `Runtime.evaluate`, which works without `Runtime.enable`. With no
    /// context, it runs in the main world of the session's own frame.
    /// `bounded` holds it to the browser's timeout.
    async fn run(&self, context: Option<i64>, expression: &str, bounded: bool) -> Result<Value> {
        let mut params = json!({
            "expression": expression,
            "awaitPromise": true,
            "returnByValue": true,
        });
        if let Some(id) = context {
            params["contextId"] = Value::from(id);
        }
        let r = if bounded {
            self.call("Runtime.evaluate", params).await?
        } else {
            self.call_unbounded("Runtime.evaluate", params).await?
        };
        value_of(&r)
    }

    /// Lets go of an object Chrome holds for netweir.
    async fn release(&self, object: &str) {
        let _ = self
            .call("Runtime.releaseObject", json!({"objectId": object}))
            .await;
    }

    /// Runs `js` in the frame's main world, where the page's own variables
    /// are: an expression, or a function, which is called. A promise is
    /// awaited. The result comes back as JSON.
    pub async fn evaluate(&self, js: &str) -> Result<Value> {
        let body =
            format!("const v = ({js}\n); return typeof v === 'function' ? await v() : await v;");
        if self.page.is_firefox() {
            crate::page::bidi::no_dialog_open(&self.page).await?;
            let r = crate::page::bidi::remote(&self.page, &self.id, &body, false, false)
                .await
                .map_err(|e| self.gone_if_lost(e))?;
            return crate::page::bidi::to_json(&r);
        }
        if self.is_main() {
            // No timeout of its own, as documented: the caller's script may
            // rightly take as long as it likes.
            return self
                .run(None, &format!("(async () => {{ {body} }})()"), false)
                .await;
        }
        // Only a session's own frame has a main world Runtime.evaluate can
        // name without Runtime.enable. Any other is reached through its
        // document: found in netweir's world, and resolved again in the
        // main one, where the function is called on it.
        let document = self.main_world_document().await?;
        let r = self
            .call_unbounded(
                "Runtime.callFunctionOn",
                json!({
                    "objectId": document,
                    "functionDeclaration": format!("async function() {{ {body} }}"),
                    "awaitPromise": true,
                    "returnByValue": true,
                }),
            )
            .await;
        self.release(&document).await;
        value_of(&r?)
    }

    /// The frame's document, as an object in its main world.
    async fn main_world_document(&self) -> Result<String> {
        let mut fresh = false;
        loop {
            let id = self.world(fresh).await?;
            let found = self
                .call(
                    "Runtime.evaluate",
                    json!({"expression": "document", "contextId": id}),
                )
                .await;
            let object = match found {
                Ok(r) => str_of(&r["result"], "objectId"),
                Err(Error::Protocol { .. }) if !fresh => {
                    fresh = true;
                    continue;
                }
                Err(e) => return Err(e),
            };
            let node = self
                .call("DOM.describeNode", json!({"objectId": object}))
                .await;
            self.release(&object).await;
            let backend = node?["node"]["backendNodeId"].clone();
            let r = self
                .call("DOM.resolveNode", json!({"backendNodeId": backend}))
                .await?;
            return Ok(str_of(&r["object"], "objectId"));
        }
    }

    /// The document as Chrome now has it, doctype included.
    pub async fn content(&self) -> Result<String> {
        if self.page.is_firefox() {
            let html = crate::page::bidi::remote(&self.page, &self.id, CONTENT, true, true)
                .await
                .map_err(|e| self.gone_if_lost(e))?;
            return Ok(str_of(&html, "value"));
        }
        let html = self.helper("__netweir.content()").await?;
        Ok(html.as_str().unwrap_or_default().to_string())
    }

    pub async fn title(&self) -> Result<String> {
        Ok(self
            .helper("document.title")
            .await?
            .as_str()
            .unwrap_or_default()
            .to_string())
    }

    /// The frames between this one and the page's own, innermost first:
    /// each by its id, with the frame whose `<iframe>` holds it.
    async fn ancestry(&self) -> Result<Vec<(String, Frame)>> {
        if self.parent.is_none() {
            return Ok(Vec::new());
        }
        let frames = self.page.frames().await?;
        let mut chain: Vec<(String, Frame)> = Vec::new();
        let mut child = self.clone();
        while let Some(parent) = child.parent.clone() {
            let Some(found) = frames.iter().find(|f| f.id == parent) else {
                return Err(Error::Gone(format!(
                    "the frame at {} isn't in the page any more",
                    self.url
                )));
            };
            // A frame can't be its own ancestor; Chrome would never say so,
            // but a loop here would never end.
            if chain.len() > frames.len() {
                break;
            }
            chain.push((child.id.clone(), found.clone()));
            child = found.clone();
        }
        Ok(chain)
    }

    /// Where the point (x, y) of this frame's viewport is in the page's,
    /// where the mouse goes, through each `<iframe>` holding it; None if,
    /// with `hit`, one of those is covered there.
    async fn to_page(
        chain: &[(String, Frame)],
        mut x: f64,
        mut y: f64,
        hit: bool,
    ) -> Result<Option<(f64, f64)>> {
        for (child, parent) in chain {
            if parent.page.is_firefox() {
                let Some(at) = crate::page::bidi::holder(&parent.page, &parent.id, child).await?
                else {
                    return Err(Error::Gone(format!(
                        "the frame at {} isn't in the page any more",
                        parent.url
                    )));
                };
                let r = parent
                    .helper(&format!(
                        "__netweir.offset(document.querySelectorAll('iframe, frame')[{at}], {x}, {y}, {hit})"
                    ))
                    .await?;
                if r.get("failed").is_some() {
                    return Ok(None);
                }
                x = r["x"].as_f64().unwrap_or(0.0);
                y = r["y"].as_f64().unwrap_or(0.0);
                continue;
            }
            let owner = parent.owner(child).await?;
            let r = parent
                .call(
                    "Runtime.callFunctionOn",
                    json!({
                        "objectId": owner,
                        "functionDeclaration": "function(x, y, hit) { return __netweir.offset(this, x, y, hit); }",
                        "arguments": [{"value": x}, {"value": y}, {"value": hit}],
                        "returnByValue": true,
                    }),
                )
                .await;
            parent.release(&owner).await;
            let r = value_of(&r?)?;
            if r.get("failed").is_some() {
                return Ok(None);
            }
            x = r["x"].as_f64().unwrap_or(0.0);
            y = r["y"].as_f64().unwrap_or(0.0);
        }
        Ok(Some((x, y)))
    }

    /// The `<iframe>` in this frame that holds the frame `child`, as an
    /// object in netweir's world, where its helpers can work on it.
    async fn owner(&self, child: &str) -> Result<String> {
        let owner = self
            .call("DOM.getFrameOwner", json!({"frameId": child}))
            .await?;
        let mut fresh = false;
        loop {
            let id = self.world(fresh).await?;
            match self
                .call(
                    "DOM.resolveNode",
                    json!({"backendNodeId": owner["backendNodeId"], "executionContextId": id}),
                )
                .await
            {
                Ok(r) => return Ok(str_of(&r["object"], "objectId")),
                Err(Error::Protocol { .. }) if !fresh => fresh = true,
                Err(e) => return Err(e),
            }
        }
    }

    /// Runs the helper `call` until it stops reporting a check that failed,
    /// and returns what it says then. One that reports an error fails at
    /// once. With `point`, its x and y are moved into the page's viewport,
    /// where the mouse goes, and the point must reach this frame if
    /// `point` is true; that's a check too.
    async fn attempt(
        &self,
        action: &str,
        selector: &str,
        call: &str,
        point: Option<bool>,
        timeout: Option<Duration>,
    ) -> Result<Value> {
        let deadline = Instant::now() + self.page.timeout(timeout);
        let chain = match point {
            Some(_) => self.ancestry().await?,
            None => Vec::new(),
        };
        let mut pause = Duration::from_millis(20);
        loop {
            let mut r = self.helper(call).await?;
            if let Some(e) = r["error"].as_str() {
                return Err(Error::Script(format!("{action} {selector}: {e}")));
            }
            let mut failed = r["failed"].as_str().map(str::to_string);
            if failed.is_none()
                && let Some(hit) = point
            {
                let (x, y) = (
                    r["x"].as_f64().unwrap_or(0.0),
                    r["y"].as_f64().unwrap_or(0.0),
                );
                match Frame::to_page(&chain, x, y, hit).await? {
                    Some((px, py)) => {
                        if self.page.is_firefox() {
                            self.page.state().input_frame =
                                (!chain.is_empty()).then(|| (self.id.clone(), px - x, py - y));
                        }
                        r["x"] = Value::from(px);
                        r["y"] = Value::from(py);
                    }
                    None => failed = Some("receives events".into()),
                }
            }
            let Some(failed) = failed else {
                return Ok(r);
            };
            if Instant::now() + pause >= deadline {
                let why = match r["why"].as_str() {
                    Some(why) => why.to_string(),
                    None => never(&failed).to_string(),
                };
                return Err(Error::Timeout(format!("{action} {selector}: {why}")));
            }
            tokio::time::sleep(pause).await;
            pause = (pause * 2).min(Duration::from_millis(200));
        }
    }

    /// Waits until the element `selector` finds passes `checks`, scrolling
    /// it into view first if `scroll`. Returns its centre in the page's
    /// viewport.
    async fn actionable(
        &self,
        action: &str,
        selector: &str,
        checks: &[&str],
        scroll: bool,
        timeout: Option<Duration>,
    ) -> Result<(f64, f64)> {
        let call = format!(
            "__netweir.check({}, {}, {scroll})",
            Value::from(selector),
            json!(checks)
        );
        let hit = checks.contains(&"receives events");
        let r = self
            .attempt(action, selector, &call, Some(hit), timeout)
            .await?;
        Ok((
            r["x"].as_f64().unwrap_or(0.0),
            r["y"].as_f64().unwrap_or(0.0),
        ))
    }

    /// Clicks the element's centre with the mouse, once it's visible,
    /// still, enabled and not covered.
    pub async fn click(&self, selector: &str, timeout: Option<Duration>) -> Result<()> {
        self.click_as("click", selector, timeout).await
    }

    /// `click`, its errors naming `action`, the action it's part of.
    async fn click_as(
        &self,
        action: &str,
        selector: &str,
        timeout: Option<Duration>,
    ) -> Result<()> {
        let checks = [
            "attached",
            "visible",
            "stable",
            "enabled",
            "receives events",
        ];
        // The button goes down only once the page has seen the mouse come
        // over the element. The checks ask the frame's own renderer, but
        // Chrome's browser process decides which frame gets the mouse, and
        // for a frame from another site it goes by what the page around
        // the frame last drew: a click there could land on that page
        // instead, return, and do nothing.
        let deadline = self.page.deadline(timeout);
        self.page.to_front().await?;
        let (x, y) = self.over(action, selector, &checks, deadline).await?;
        if action == "double_click" {
            self.page.mouse_double(x, y).await
        } else {
            self.page.mouse_press(x, y).await
        }
    }

    /// Double-clicks the element's centre (taps it twice on a phone), once
    /// it's visible, still, enabled and not covered.
    pub async fn dblclick(&self, selector: &str, timeout: Option<Duration>) -> Result<()> {
        self.click_as("double_click", selector, timeout).await
    }

    /// Moves the mouse to the element's centre, once it's visible, still
    /// and not covered, so the page sees it come over the element.
    pub async fn hover(&self, selector: &str, timeout: Option<Duration>) -> Result<()> {
        if self.page.touches() {
            return Err(Error::Invalid(format!(
                "hover {selector}: the page is a phone's, which has no mouse to hover with; a \
                 mouse moving over a phone's page is something no person's phone sends"
            )));
        }
        let checks = ["attached", "visible", "stable", "receives events"];
        let deadline = Instant::now() + self.page.timeout(timeout);
        self.page.to_front().await?;
        self.over("hover", selector, &checks, deadline).await?;
        Ok(())
    }

    /// Drags the element `source` onto `target` with the mouse: the mouse
    /// comes over the source's centre once it's visible, still and not
    /// covered, the button goes down, and the mouse moves to the centre
    /// of the target, which must be visible and still and show in the
    /// window with the source, and lets go there. A drag that fails
    /// partway is called off, the button let go.
    /// That drags anything that follows the mouse (a slider's handle, a
    /// sortable list's row) and, from a draggable element, makes an
    /// HTML5 drag and drop, dataTransfer and all.
    pub async fn drag(&self, source: &str, target: &str, timeout: Option<Duration>) -> Result<()> {
        if self.page.touches() {
            return Err(Error::Invalid(format!(
                "drag {source}: the page is a phone's, which has no mouse to drag with"
            )));
        }
        let deadline = self.page.deadline(timeout);
        self.page.to_front().await?;
        if self.page.is_firefox() {
            self.html5_drag_refused(source, deadline).await?;
        }
        // Nothing need be under the mouse at the target but the target.
        let landing = ["attached", "visible", "stable"];
        self.actionable("drag", target, &landing, true, left(deadline))
            .await?;
        let checks = ["attached", "visible", "stable", "receives events"];
        let from = self.over("drag", source, &checks, deadline).await?;
        // Again, where bringing the source into view left it.
        let to = self
            .actionable("drag", target, &landing, false, left(deadline))
            .await?;
        // A mouse can't go past the window's edge, and nothing scrolls the
        // page while it's held, so both must show at once.
        let size = self
            .page
            .main_frame()
            .helper("[innerWidth, innerHeight]")
            .await?;
        let inside = |at: f64, i: usize| at >= 0.0 && at < size[i].as_f64().unwrap_or(0.0);
        if !inside(to.0, 0) || !inside(to.1, 1) {
            return Err(Error::Invalid(format!(
                "drag {source} to {target}: the target isn't in view with the source; \
                 scroll so both show, or make the window bigger"
            )));
        }
        self.page.mouse_drag(from, to).await
    }

    /// Firefox doesn't begin an HTML5 drag from the mouse WebDriver moves,
    /// so a drag from what the page could drag that way (a draggable
    /// element, a link, an image) is refused before anything is pressed,
    /// rather than doing something else.
    async fn html5_drag_refused(&self, source: &str, deadline: Instant) -> Result<()> {
        let sel = Value::from(source);
        let call = format!(
            "(() => {{ const el = __netweir.find({sel}); if (!el) return {{failed: 'attached'}}; \
             const d = el.closest('[draggable]'); \
             const html5 = d ? d.getAttribute('draggable') === 'true' \
               : !!el.closest('a[href], img'); return {{html5}}; }})()"
        );
        let r = self
            .attempt("drag", source, &call, None, left(deadline))
            .await?;
        if r["html5"] == true {
            return Err(Error::Unsupported {
                engine: "Firefox",
                feature: format!("drag {source}: an HTML5 drag and drop"),
            });
        }
        Ok(())
    }

    /// Moves the mouse to the element's centre once it passes `checks`,
    /// and returns that point once the page has sent mouseover for it,
    /// heard from netweir's world. A move to where the mouse already is
    /// sends nothing, and the page may not have noticed it there (after a
    /// scroll, or the element coming back under it), so a move that
    /// brings no mouseover is tried again from just outside, as a person
    /// moves off and back on.
    async fn over(
        &self,
        action: &str,
        selector: &str,
        checks: &[&str],
        deadline: Instant,
    ) -> Result<(f64, f64)> {
        if self.page.touches() {
            // A finger comes down where it taps, from nowhere: there's no
            // pointer to bring over the element first.
            return self
                .actionable(action, selector, checks, true, left(deadline))
                .await;
        }
        let sel = Value::from(selector);
        let mut from_outside = false;
        loop {
            let (x, y) = self
                .actionable(action, selector, checks, true, left(deadline))
                .await?;
            self.helper(&format!("__netweir.armOver({sel})")).await?;
            if from_outside {
                self.come_from_beside(action, selector, deadline).await?;
            }
            self.page.mouse_move(x, y).await?;
            if self.helper("__netweir.overSeen()").await? == true {
                return Ok((x, y));
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout(format!(
                    "{action} {selector}: the page never sent mouseover for it"
                )));
            }
            from_outside = true;
        }
    }

    /// Moves the mouse to just outside the element, unless it fills the
    /// window, and listens again for it coming over.
    async fn come_from_beside(
        &self,
        action: &str,
        selector: &str,
        deadline: Instant,
    ) -> Result<()> {
        let sel = Value::from(selector);
        let beside = self
            .attempt(
                action,
                selector,
                &format!("__netweir.beside({sel})"),
                Some(false),
                left(deadline),
            )
            .await?;
        if beside["none"] != true {
            let (bx, by) = (
                beside["x"].as_f64().unwrap_or(0.0),
                beside["y"].as_f64().unwrap_or(0.0),
            );
            self.page.mouse_move(bx, by).await?;
        }
        // Listening starts again here, so only coming back on counts.
        self.helper(&format!("__netweir.armOver({sel})")).await?;
        Ok(())
    }

    /// Replaces the element's contents with `text`, as typing would.
    pub async fn fill(&self, selector: &str, text: &str, timeout: Option<Duration>) -> Result<()> {
        self.focus_field("fill", selector, timeout).await?;
        if text.is_empty() {
            self.page.press("Delete").await
        } else {
            self.page.insert_text(text).await?;
            Ok(())
        }
    }

    /// Types `text` into the element a key at a time, as a person does.
    /// The field is clicked (tapped on a phone), and after a moment to
    /// see the caret, what's in it is selected with Control-A (Command-A
    /// on a Mac), so the first key replaces it. Each character is a key
    /// pressed and held for a person's time; capitals and symbols are
    /// typed with Shift as on a US keyboard, a newline is Enter and a tab
    /// is Tab, and one that isn't on the keyboard arrives as text, as an
    /// OS's character picker sends it. Between keys there's a pause
    /// within `delay`, more often near its middle. On a phone, each key
    /// comes as its on-screen keyboard sends it, and the field's contents
    /// are selected from script, as a phone has no Control-A.
    ///
    /// `timeout` is for the field to be ready; the typing takes as long
    /// as it takes (see [`typing_time`]).
    pub async fn type_text(
        &self,
        selector: &str,
        text: &str,
        delay: (Duration, Duration),
        timeout: Option<Duration>,
    ) -> Result<()> {
        let deadline = self.page.deadline(timeout);
        let sel = Value::from(selector);
        let checks = ["attached", "visible", "enabled", "editable"];
        let ready = format!("__netweir.check({sel}, {}, true)", json!(checks));
        self.attempt("type", selector, &ready, None, timeout)
            .await?;
        let left = || Some(deadline.saturating_duration_since(Instant::now()));
        // Clicked into, as a person gets there, through a floating label
        // laid over the field too. Something covering it (a loading
        // overlay) is waited out for most of what's left of the time, as
        // a click waits; a field still covered then (one styled away under
        // a widget of its own) is focused from script with the rest.
        let rest = deadline.saturating_duration_since(Instant::now());
        let clicked = match self
            .click_as("type", selector, Some(rest.mul_f64(0.8)))
            .await
        {
            Ok(()) => true,
            Err(Error::Timeout(m)) if m.contains(never("receives events")) => false,
            Err(e) => return Err(e),
        };
        if !clicked {
            self.focus_field("type", selector, left()).await?;
        } else {
            // A page that moved focus elsewhere on mousedown gets it back
            // as a script would give it.
            if self.helper(&format!("__netweir.focused({sel})")).await? != true {
                self.focus_field("type", selector, left()).await?;
            } else if self.helper(&format!("__netweir.hasText({sel})")).await? == true {
                tokio::time::sleep(crate::events::pause(REACTION)).await;
                if !self.page.touches() {
                    self.page.select_all().await?;
                }
                // A phone has no Control-A, and a page may swallow it (a
                // masked field): what's left unselected is selected from
                // script, so the first key still replaces it.
                if self
                    .helper(&format!("__netweir.allSelected({sel})"))
                    .await?
                    != true
                {
                    self.helper(&format!("__netweir.focus({sel})")).await?;
                }
            }
        }
        tokio::time::sleep(crate::events::pause(REACTION)).await;
        if text.is_empty() {
            return self.page.press("Delete").await;
        }
        for (i, c) in text.chars().enumerate() {
            if i > 0 {
                tokio::time::sleep(crate::events::key_gap(delay)).await;
            }
            let key = match c {
                '\n' | '\r' => "Enter".to_string(),
                '\t' => "Tab".to_string(),
                c => c.to_string(),
            };
            self.page.press(&key).await?;
        }
        Ok(())
    }

    /// Waits for a visible, enabled, editable field, and focuses it with
    /// its contents selected.
    async fn focus_field(
        &self,
        action: &str,
        selector: &str,
        timeout: Option<Duration>,
    ) -> Result<()> {
        self.page.to_front().await?;
        let checks = ["attached", "visible", "enabled", "editable"];
        let call = format!(
            "__netweir.check({}, {}, true)",
            Value::from(selector),
            json!(checks)
        );
        self.attempt(action, selector, &call, None, timeout).await?;
        let focused = self
            .helper(&format!("__netweir.focus({})", Value::from(selector)))
            .await?;
        if focused != Value::Bool(true) {
            return Err(Error::Script(format!(
                "{action} {selector}: it couldn't be focused"
            )));
        }
        Ok(())
    }

    /// Waits until `selector` is "attached", "visible", "hidden" (or gone)
    /// or "detached".
    pub async fn wait_for(
        &self,
        selector: &str,
        state: &str,
        timeout: Option<Duration>,
    ) -> Result<()> {
        if !["attached", "visible", "hidden", "detached"].contains(&state) {
            return Err(Error::Invalid(format!(
                "state must be \"attached\", \"visible\", \"hidden\" or \"detached\", not {state:?}"
            )));
        }
        let deadline = Instant::now() + self.page.timeout(timeout);
        // Whatever draws it may wait on the page's next frame, which a
        // tab behind never gets.
        self.page.to_front().await?;
        let call = format!("__netweir.state({})", Value::from(selector));
        let mut pause = Duration::from_millis(20);
        loop {
            let now = self.helper(&call).await?;
            let now = now.as_str().unwrap_or("detached");
            let done = match state {
                "attached" => now != "detached",
                "visible" => now == "visible",
                "hidden" => now != "visible",
                _ => now == "detached",
            };
            if done {
                return Ok(());
            }
            if Instant::now() + pause >= deadline {
                return Err(Error::Timeout(format!(
                    "waiting for {selector} to be {state}"
                )));
            }
            tokio::time::sleep(pause).await;
            pause = (pause * 2).min(Duration::from_millis(200));
        }
    }

    /// Chooses the options `pick` names in the `<select>` `selector` finds,
    /// waiting for it to be visible and enabled and for the options to be
    /// there. Returns the values selected.
    pub async fn select_option(
        &self,
        selector: &str,
        pick: &Pick,
        timeout: Option<Duration>,
    ) -> Result<Vec<String>> {
        let (by, wanted) = match pick {
            Pick::Values(v) => ("value", json!(v)),
            Pick::Labels(v) => ("label", json!(v)),
            Pick::Indexes(v) => ("index", json!(v)),
        };
        self.page.to_front().await?;
        let call = format!(
            "__netweir.select({}, {}, {wanted})",
            Value::from(selector),
            Value::from(by)
        );
        let r = self
            .attempt("select_option", selector, &call, None, timeout)
            .await?;
        Ok(r["values"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect())
    }

    /// Checks (`on`) or unchecks a checkbox or radio button by clicking it,
    /// if it isn't that way already, and makes sure the click did it. A
    /// box that has to change waits to be enabled first.
    pub async fn set_checked(
        &self,
        selector: &str,
        on: bool,
        timeout: Option<Duration>,
    ) -> Result<()> {
        let action = if on { "check" } else { "uncheck" };
        let deadline = Instant::now() + self.page.timeout(timeout);
        let call = format!("__netweir.checked({}, {on})", Value::from(selector));
        let r = self.attempt(action, selector, &call, None, timeout).await?;
        if r["checked"] == on {
            return Ok(());
        }
        if !on && r["radio"] == true {
            return Err(Error::Script(format!(
                "uncheck {selector}: a radio button is unchecked by checking another in its group"
            )));
        }
        match self.click_as(action, selector, left(deadline)).await {
            // The usual reason: a styled box in front of a hidden real one.
            Err(Error::Timeout(m)) if m.ends_with(never("visible")) => {
                return Err(Error::Timeout(format!(
                    "{m}; if the page hides the real box behind a styled one, {action} its <label> \
                     instead"
                )));
            }
            other => other?,
        }
        let r = self
            .helper(&format!(
                "__netweir.checked({}, null)",
                Value::from(selector)
            ))
            .await?;
        if r["checked"] != on {
            return Err(Error::Script(format!(
                "{action} {selector}: clicking it didn't {action} it"
            )));
        }
        Ok(())
    }

    /// Gives the `<input type=file>` `selector` finds these files, as
    /// choosing them would; the page hears input and change. No files
    /// clears it.
    pub async fn set_input_files(
        &self,
        selector: &str,
        files: &[PathBuf],
        timeout: Option<Duration>,
    ) -> Result<()> {
        // Chrome wants whole paths, and says nothing of a missing file
        // until the page reads it.
        let mut paths = Vec::new();
        for file in files {
            let path = std::fs::canonicalize(file)
                .map_err(|_| Error::Invalid(format!("no file at {}", file.display())))?;
            if !path.is_file() {
                return Err(Error::Invalid(format!(
                    "{} is a directory, not a file",
                    file.display()
                )));
            }
            paths.push(path);
        }
        self.set_files_at(selector, &paths, timeout).await
    }

    /// `set_input_files` with `paths` taken as they are: whole paths,
    /// already checked, which are what Chrome is handed.
    pub(crate) async fn set_files_at(
        &self,
        selector: &str,
        files: &[PathBuf],
        timeout: Option<Duration>,
    ) -> Result<()> {
        let paths: Vec<String> = files.iter().map(|p| path_text(p)).collect();
        self.page.to_front().await?;
        let call = format!(
            "__netweir.fileInput({}, {})",
            Value::from(selector),
            paths.len()
        );
        self.attempt("set_input_files", selector, &call, None, timeout)
            .await?;
        if paths.is_empty() {
            // Chrome won't set no files; emptying the value is how a page
            // clears one itself.
            self.helper(&format!("__netweir.clearFiles({})", Value::from(selector)))
                .await?;
            return Ok(());
        }
        if self.page.is_firefox() {
            return crate::page::bidi::set_files(&self.page, &self.id, selector, &paths)
                .await
                .map_err(|e| self.gone_if_lost(e));
        }
        let id = self.world(false).await?;
        let r = self
            .call(
                "Runtime.evaluate",
                json!({"expression": format!("__netweir.find({})", Value::from(selector)), "contextId": id}),
            )
            .await?;
        let input = str_of(&r["result"], "objectId");
        if input.is_empty() {
            return Err(Error::Script(format!(
                "set_input_files {selector}: it went away"
            )));
        }
        let set = self
            .call(
                "DOM.setFileInputFiles",
                json!({"files": paths, "objectId": input}),
            )
            .await;
        self.release(&input).await;
        set.map(|_| ())
    }

    /// Scrolls with the mouse wheel, as a person would. With a selector
    /// and no distance, wheels the element into view; with a distance,
    /// wheels it over the element (or over the page, without one) by `x`
    /// and `y` pixels. With neither, scrolls to the bottom, waits up to
    /// `idle` for more to load, and goes on while more does, at most
    /// `times` times. Returns how many times more loaded. `timeout` is for
    /// the whole of it.
    pub async fn scroll(
        &self,
        selector: Option<&str>,
        x: f64,
        y: Option<f64>,
        times: Option<u32>,
        idle: Duration,
        timeout: Option<Duration>,
    ) -> Result<u32> {
        let deadline = Instant::now() + self.page.timeout(timeout);
        self.page.to_front().await?;
        let (dx, dy) = (x, y.unwrap_or(0.0));
        match selector {
            None if y.is_none() && x == 0.0 => {
                // `times` is a limit of its own; only a timeout the caller
                // gave cuts it short.
                let deadline = (times.is_none() || timeout.is_some()).then_some(deadline);
                self.to_bottom(times, idle, deadline).await
            }
            Some(selector) => {
                let (px, py) = self.bring_into_view(selector, deadline).await?;
                if dx != 0.0 || dy != 0.0 {
                    self.wheel(Some(selector), px, py, dx, dy).await?;
                }
                Ok(0)
            }
            None => {
                let (px, py) = self.page_point(0, Some(deadline)).await?;
                self.wheel(None, px, py, dx, dy).await?;
                Ok(0)
            }
        }
    }

    /// Where a wheel scrolls the frame's document, in the page's viewport:
    /// the middle, unless a box that scrolls by itself is there, which
    /// would take the wheel instead. `skip` passes over that many places,
    /// for when one moved nothing.
    async fn page_point(&self, skip: u32, deadline: Option<Instant>) -> Result<(f64, f64)> {
        let r = self
            .attempt(
                "scroll",
                "the page",
                &format!("__netweir.pagePoint({skip})"),
                Some(false),
                deadline.and_then(left),
            )
            .await?;
        Ok((
            r["x"].as_f64().unwrap_or(0.0),
            r["y"].as_f64().unwrap_or(0.0),
        ))
    }

    /// Turns the wheel at (x, y) by (dx, dy) and waits for the scroll to
    /// finish. Returns whether anything moved: the page, whatever scrolls
    /// under the wheel, or the element `selector` finds.
    async fn wheel(
        &self,
        selector: Option<&str>,
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
    ) -> Result<bool> {
        let selector = selector.map_or(Value::Null, Value::from);
        let before = self.helper(&format!("__netweir.where({selector})")).await?;
        self.page.mouse_wheel(x, y, dx, dy).await?;
        if self.page.touches() {
            // A swipe ends once what it dragged has stopped (see
            // [`Page::swipe`]). Waiting again for that, and where nothing
            // moved for a start that can't come, took up to a sixth of every
            // turn on a busy page.
            let now = self.helper(&format!("__netweir.where({selector})")).await?;
            return Ok(now != before || before.as_str().is_some_and(|b| b.ends_with('?')));
        }
        let moved = self
            .helper(&format!("__netweir.settle({selector}, {before})"))
            .await?;
        Ok(moved == true)
    }

    /// Wheels the element into view: over the box that scrolls it, by as
    /// much as brings it to the middle, until it's there. Where the wheel
    /// can't move it (the box that holds it is out of view, say), it's
    /// scrolled into view as click does. Returns its middle in the page's
    /// viewport.
    async fn bring_into_view(&self, selector: &str, deadline: Instant) -> Result<(f64, f64)> {
        let call = format!("__netweir.aim({})", Value::from(selector));
        let reveal = format!("__netweir.reveal({})", Value::from(selector));
        let mut revealed = false;
        // A few turns of the wheel bring most things into view; past that,
        // something keeps it from moving the way the wheel goes.
        for turn in 0..=20 {
            if turn == 20 && !revealed {
                self.helper(&reveal).await?;
                revealed = true;
            }
            let r = self
                .attempt("scroll", selector, &call, Some(false), left(deadline))
                .await?;
            let (x, y) = (
                r["x"].as_f64().unwrap_or(0.0),
                r["y"].as_f64().unwrap_or(0.0),
            );
            if r["done"] == true || revealed {
                return Ok((x, y));
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout(format!(
                    "scroll {selector}: it never came into view"
                )));
            }
            let (dx, dy) = (
                r["dx"].as_f64().unwrap_or(0.0),
                r["dy"].as_f64().unwrap_or(0.0),
            );
            if r["stuck"] == true || !self.wheel(Some(selector), x, y, dx, dy).await? {
                self.helper(&reveal).await?;
                revealed = true;
            }
        }
        unreachable!("the last turn returns")
    }

    /// Wheels down to the bottom, then waits up to `idle` for the page to
    /// grow; while it does, goes round again, up to `times` times. Returns
    /// how many rounds the page grew in. Without a deadline, each step is
    /// still bounded: every call to Chrome by the browser's timeout, each
    /// wait for more by `idle`, and the trip down by the stall limit.
    async fn to_bottom(
        &self,
        times: Option<u32>,
        idle: Duration,
        deadline: Option<Instant>,
    ) -> Result<u32> {
        if times == Some(0) {
            return Ok(0);
        }
        let height = |m: &Value| m["height"].as_f64().unwrap_or(0.0);
        let past = || deadline.is_some_and(|d| Instant::now() >= d);
        // Out of time on the way down, or waiting at the bottom for more.
        let late = |rounds: u32, waiting: bool| Error::ScrollTimeout {
            message: match (waiting, rounds) {
                (false, 0) => "scrolling to the bottom: still scrolling down, with no rounds \
                               finished"
                    .to_string(),
                (false, n) => format!(
                    "scrolling to the bottom: still scrolling down after {n} rounds of more loaded"
                ),
                (true, n) => format!(
                    "scrolling to the bottom: still waiting for more after {n} rounds of more \
                     loaded"
                ),
            },
            rounds,
        };
        let lines = &self.page.inner.scroll_trace;
        let trace = |line: String| lines.lock().unwrap_or_else(|e| e.into_inner()).push(line);
        lines.lock().unwrap_or_else(|e| e.into_inner()).clear();
        let num = |m: &Value, k: &str| m[k].as_f64().unwrap_or(0.0);
        let mut grew = 0;
        loop {
            let mut m = self.helper("__netweir.metrics()").await?;
            let start = height(&m);
            // To where the bottom was when the round began: what the page
            // adds on the way is the next round's, so each round is one
            // trip to the bottom.
            let max = m["max"].as_f64().unwrap_or(0.0);
            // Turns in a row that moved something, but not the page: a box
            // or a frame under the wheel that won't pass the scroll on.
            let mut stalled = 0;
            trace(format!("round {grew}: from {} to {max}", num(&m, "y")));
            loop {
                let at = m["y"].as_f64().unwrap_or(0.0);
                if at >= max - 1.0 {
                    trace(format!("reached {at} of {max}"));
                    break;
                }
                if past() {
                    return Err(late(grew, false));
                }
                // Whole notches, as a mouse wheel turns: about a window's
                // worth, and at the end as many as cover what's left, never
                // a fraction of one. How far a notch scrolls is Chrome's to
                // say (on Windows it differs from the delta), so a turn cut
                // to land exactly can come up short, and the sliver of a
                // turn after it can move nothing.
                let view = (m["view"].as_f64().unwrap_or(600.0) / NOTCH).floor();
                let left = ((max - at) / NOTCH).ceil();
                let mut step = view.min(left).max(1.0) * NOTCH;
                if self.page.touches() {
                    // A finger has no notches; it drags what one swipe
                    // across the screen can, about four fifths of it.
                    let reach = (m["view"].as_f64().unwrap_or(600.0) * 0.8).floor();
                    step = reach.min(max - at).max(1.0);
                }
                // The place is picked again each turn, since the page moves
                // what's under it; where a turn moves nothing, a few others
                // are tried before the page is taken to have stopped.
                let mut moved = false;
                for skip in 0..SPOTS {
                    let (x, y) = self.page_point(skip, deadline).await?;
                    moved = self.wheel(None, x, y, 0.0, step).await?;
                    trace(format!(
                        "turn {step} at ({x:.0}, {y:.0}) from {at}: moved {moved}"
                    ));
                    if moved {
                        break;
                    }
                }
                if !moved {
                    // As far as the wheel will take the page.
                    // Known limit: a page that scrolls in a box of its own,
                    // not as a whole, isn't scrolled by this; wheel the box
                    // with a selector instead.
                    break;
                }
                m = self.helper("__netweir.metrics()").await?;
                if m["y"].as_f64().unwrap_or(0.0) > at {
                    stalled = 0;
                } else {
                    stalled += 1;
                    if stalled >= STALLED {
                        trace(format!("stalled at {at}"));
                        break;
                    }
                }
            }
            // The wheel may not take the page its last few pixels: on
            // Windows CI the last notch, with 8 px to go, moved nothing,
            // though a script took it the rest of the way. Something
            // waiting at the very bottom has to see the page get there, so
            // however the trip ended, a page left less than a notch short
            // of as far as it really goes is scrolled there by script, as
            // click scrolls, which a page can tell apart: once a trip,
            // after any scroll still running has finished, so it can't
            // carry the page back.
            m = self.helper("__netweir.metrics()").await?;
            let short = num(&m, "max") - num(&m, "y");
            if short > 0.5 && short < NOTCH {
                self.helper("__netweir.settle(null, __netweir.where(null))")
                    .await?;
                self.helper("__netweir.toEnd()").await?;
                self.helper("__netweir.settle(null, __netweir.where(null))")
                    .await?;
                let was = num(&m, "y");
                m = self.helper("__netweir.metrics()").await?;
                trace(format!(
                    "{short} short at {was}: by script to {} of {}",
                    num(&m, "y"),
                    num(&m, "max")
                ));
            }
            // Waiting for what the page fetches as it comes into view: the
            // only sign there'll be no more is that none comes.
            let quiet = Instant::now() + idle;
            while height(&m) <= start && Instant::now() < quiet {
                if past() {
                    return Err(late(grew, true));
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
                m = self.helper("__netweir.metrics()").await?;
            }
            if height(&m) <= start {
                return Ok(grew);
            }
            grew += 1;
            if times.is_some_and(|t| grew >= t) {
                return Ok(grew);
            }
        }
    }
}

/// How many places a turn of the wheel is tried at before the page is
/// taken to have stopped scrolling.
const SPOTS: u32 = 4;

/// How many turns in a row may move something under the wheel but not the
/// page before scrolling to the bottom stops: each turn is up to a viewport,
/// so enough to run a box or a frame dozens of screens tall to its end,
/// where its scroll passes on to the page.
const STALLED: u32 = 40;

/// What's left until `deadline`, as a timeout for one step of a longer
/// action; never quite nothing, so a step past it fails on its own terms.
fn left(deadline: Instant) -> Option<Duration> {
    Some(
        deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1)),
    )
}

/// The value a `Runtime.evaluate` or `Runtime.callFunctionOn` returned, or
/// the error the script threw.
fn value_of(r: &Value) -> Result<Value> {
    if let Some(details) = r.get("exceptionDetails") {
        let message = details["exception"]["description"]
            .as_str()
            .or_else(|| details["text"].as_str())
            .unwrap_or("script error");
        return Err(Error::Script(message.to_string()));
    }
    let result = &r["result"];
    // NaN, Infinity, -0 and BigInts aren't JSON; they come as text.
    if let Some(text) = result["unserializableValue"].as_str() {
        return Ok(Value::from(text.trim_end_matches('n')));
    }
    Ok(result.get("value").cloned().unwrap_or(Value::Null))
}

/// A path as Chrome takes it. Windows' canonical paths start `\\?\`,
/// which Chrome doesn't read as a file.
fn path_text(path: &std::path::Path) -> String {
    let text = path.display().to_string();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => rest.to_string(),
        _ => text,
    }
}

/// The document as text, doctype first, as helpers.js's `content()` reads
/// it; run in a sandbox of netweir's on Firefox, which has no helpers yet.
const CONTENT: &str = "const doctype = document.doctype ? new XMLSerializer().serializeToString(document.doctype) : \"\"; \
                       return doctype + (document.documentElement ? document.documentElement.outerHTML : \"\");";

/// How a failed check reads in a timeout.
fn never(check: &str) -> &'static str {
    match check {
        "attached" => "nothing matched it",
        "visible" => "it never became visible",
        "stable" => "it never stopped moving",
        "enabled" => "it never became enabled",
        "editable" => "it never became editable",
        "receives events" => "something else was always at its centre, covering it",
        _ => "it never became actionable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// After a real Control-A (Command-A), the field counts as all
    /// selected, whatever kind it is, so `type` selects nothing from
    /// script on top.
    #[tokio::test(flavor = "multi_thread")]
    async fn control_a_counts_as_all_selected_in_every_kind_of_field() {
        let Ok(chrome) = crate::find_chrome() else {
            assert!(
                std::env::var_os("NETWEIR_REQUIRE_CHROME").is_none(),
                "no Chrome"
            );
            return;
        };
        let browser = crate::Browser::launch(crate::LaunchOptions {
            executable: Some(chrome),
            ..Default::default()
        })
        .await
        .unwrap();
        let page = browser.new_page().await.unwrap();
        let html = "<input id=t value=abc><input id=e type=email value=a@b.co>\
            <input id=n type=number value=123>\
            <div id=ce contenteditable><p>one</p><p>two</p></div>";
        page.goto(
            &format!("data:text/html,{html}"),
            crate::WaitUntil::Load,
            None,
        )
        .await
        .unwrap();
        let frame = page.main_frame();
        for id in ["#t", "#e", "#n", "#ce"] {
            page.click(id, None).await.unwrap();
            let sel = Value::from(id);
            assert_eq!(
                frame
                    .helper(&format!("__netweir.allSelected({sel})"))
                    .await
                    .unwrap(),
                false,
                "{id}: nothing selected yet"
            );
            page.select_all().await.unwrap();
            assert_eq!(
                frame
                    .helper(&format!("__netweir.allSelected({sel})"))
                    .await
                    .unwrap(),
                true,
                "{id}"
            );
        }
        browser.close().await.unwrap();
    }
}
