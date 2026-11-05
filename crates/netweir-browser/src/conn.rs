//! The protocol client: one connection to Chrome, carrying every session.
//!
//! Over the pipe a launched Chrome reads, each message is a JSON object
//! followed by a NUL byte; over a WebSocket, to a browser already running,
//! each is a text frame. Either way, a reader routes replies to the command
//! waiting for their `id` and events to the handler of the session they
//! name, and a writer does the writing, so no async task waits on the
//! connection.

use std::collections::{BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use crate::{Error, Result};

/// The protocol a connection speaks: Chrome's DevTools protocol, or
/// WebDriver BiDi, which Firefox speaks. They differ in how a command is
/// written, how a reply and an event are told apart, and what an event
/// is routed by: a CDP session, or a BiDi browsing context.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dialect {
    Cdp,
    Bidi,
}

/// One message from the browser, as the reader acts on it.
#[derive(Debug)]
pub(crate) enum Incoming<'a> {
    Reply {
        id: u64,
        result: std::result::Result<Value, String>,
    },
    Event {
        route: &'a str,
        method: &'a str,
        params: &'a Value,
    },
    /// The route is gone: what was sent to it won't be answered.
    Gone { route: &'a str },
}

impl Dialect {
    /// The browser on the far end, for messages.
    pub(crate) fn browser(self) -> &'static str {
        match self {
            Dialect::Cdp => "Chrome",
            Dialect::Bidi => "Firefox",
        }
    }

    /// Whether `method` is one of this dialect's. CDP names its domains in
    /// capitals and BiDi its modules in lower case, so a CDP method, from
    /// a part of Netweir that hasn't learnt Firefox yet, never reaches
    /// Firefox, where it would fail in a less helpful way.
    fn speaks(self, method: &str) -> Result<()> {
        if self == Dialect::Bidi && method.starts_with(|c: char| c.is_ascii_uppercase()) {
            return Err(Error::Unsupported {
                engine: self.browser(),
                feature: method.to_string(),
            });
        }
        Ok(())
    }

    /// Methods whose side effects page scripts can see. Never sent.
    fn forbidden(self) -> &'static [&'static str] {
        match self {
            Dialect::Cdp => &["Runtime.enable", "Console.enable", "Log.enable"],
            Dialect::Bidi => &[],
        }
    }

    /// The route a command waits on, so it fails if that goes away: its
    /// CDP session, or the browsing context a BiDi command names.
    fn route_of<'a>(self, route: &'a str, params: &'a Value) -> &'a str {
        match self {
            Dialect::Cdp => route,
            Dialect::Bidi => params
                .get("context")
                .or_else(|| params.get("target").and_then(|t| t.get("context")))
                .and_then(Value::as_str)
                .unwrap_or(""),
        }
    }

    /// The command as written, NUL-terminated. A BiDi command names its
    /// context inside `params`, so `route` only goes on a CDP one.
    fn encode(self, id: u64, route: &str, method: &str, params: Value) -> Vec<u8> {
        let mut message = json!({"id": id, "method": method, "params": params});
        if self == Dialect::Cdp && !route.is_empty() {
            message["sessionId"] = Value::from(route);
        }
        let mut bytes = serde_json::to_vec(&message).expect("a JSON value serialises");
        bytes.push(0);
        bytes
    }

    /// What `message` means, in the order the reader acts on it. Anything
    /// it doesn't recognise means nothing: one odd message shouldn't end
    /// the connection.
    fn read(self, message: &Value) -> Vec<Incoming<'_>> {
        match self {
            Dialect::Cdp => {
                if let Some(id) = message.get("id").and_then(Value::as_u64) {
                    let result = match message.get("error") {
                        Some(e) => Err(e
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("failed")
                            .to_string()),
                        None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    return vec![Incoming::Reply { id, result }];
                }
                let Some(method) = message.get("method").and_then(Value::as_str) else {
                    return Vec::new();
                };
                let params = message.get("params").unwrap_or(&Value::Null);
                let route = message
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let mut read = Vec::new();
                // A detached target's session hears it from the browser's.
                if method == "Target.detachedFromTarget"
                    && let Some(gone) = params.get("sessionId").and_then(Value::as_str)
                {
                    read.push(Incoming::Gone { route: gone });
                }
                read.push(Incoming::Event {
                    route,
                    method,
                    params,
                });
                read
            }
            Dialect::Bidi => match message.get("type").and_then(Value::as_str) {
                Some("success") => match message.get("id").and_then(Value::as_u64) {
                    Some(id) => vec![Incoming::Reply {
                        id,
                        result: Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                    }],
                    None => Vec::new(),
                },
                // An error with no id answers a command Firefox couldn't
                // read, which nothing is waiting for by id.
                Some("error") => match message.get("id").and_then(Value::as_u64) {
                    Some(id) => vec![Incoming::Reply {
                        id,
                        result: Err(message
                            .get("message")
                            .or_else(|| message.get("error"))
                            .and_then(Value::as_str)
                            .unwrap_or("failed")
                            .to_string()),
                    }],
                    None => Vec::new(),
                },
                Some("event") => {
                    let Some(method) = message.get("method").and_then(Value::as_str) else {
                        return Vec::new();
                    };
                    let params = message.get("params").unwrap_or(&Value::Null);
                    let route = params
                        .get("context")
                        .or_else(|| params.get("source").and_then(|s| s.get("context")))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let mut read = Vec::new();
                    // A context goes with every frame in it, and their
                    // commands won't be answered either.
                    if method == "browsingContext.contextDestroyed" {
                        let mut gone = vec![params];
                        while let Some(info) = gone.pop() {
                            if let Some(route) = info.get("context").and_then(Value::as_str) {
                                read.push(Incoming::Gone { route });
                            }
                            if let Some(children) = info.get("children").and_then(Value::as_array) {
                                gone.extend(children.iter().rev());
                            }
                        }
                    }
                    read.push(Incoming::Event {
                        route,
                        method,
                        params,
                    });
                    read
                }
                _ => Vec::new(),
            },
        }
    }
}

/// Called on the reader thread for each event of a session.
pub(crate) type Handler = Arc<dyn Fn(&str, &Value) + Send + Sync>;

/// Sent to a session's handler, after which it gets nothing more.
pub(crate) const GONE: &str = "netweir.gone";

#[derive(Clone)]
pub(crate) struct Connection {
    shared: Arc<Shared>,
}

struct Shared {
    /// Each message ends in a NUL byte, which the pipe needs and a
    /// WebSocket frame drops.
    out: Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>,
    next_id: AtomicU64,
    state: Mutex<State>,
    dialect: Dialect,
}

#[derive(Default)]
struct State {
    closed: bool,
    /// Commands waiting for their answers, by id: the method, and the
    /// session it went to.
    pending: HashMap<u64, (String, String, oneshot::Sender<Result<Value>>)>,
    /// By session id; "" is the browser's own.
    handlers: HashMap<String, Handler>,
    /// Every method sent, for the test that nothing forbidden was.
    sent: BTreeSet<String>,
    /// Each BiDi frame's parent context, as Firefox told of it, so an
    /// event in a frame reaches the handler of the page it's in.
    parents: HashMap<String, String>,
}

impl Connection {
    /// Over a pipe, to a launched `chrome`, if given: see [`closes_with`].
    pub(crate) fn new(
        input: impl Read + Send + 'static,
        output: impl Write + Send + 'static,
        chrome: Option<&std::process::Child>,
    ) -> Connection {
        let (shared, mut rx) = Shared::new(Dialect::Cdp);
        std::thread::Builder::new()
            .name("netweir-cdp-write".into())
            .spawn(move || {
                let mut output = output;
                while let Some(message) = rx.blocking_recv() {
                    if output.write_all(&message).is_err() {
                        break;
                    }
                }
            })
            .expect("can't start the CDP writer thread");
        let reader = shared.clone();
        let reader = std::thread::Builder::new()
            .name("netweir-cdp-read".into())
            .spawn(move || read_loop(BufReader::new(input), &reader))
            .expect("can't start the CDP reader thread");
        if let Some(chrome) = chrome {
            closes_with(chrome, &reader, &shared);
        }
        Connection { shared }
    }

    /// Over a WebSocket, to a browser already running. Needs a tokio
    /// runtime.
    pub(crate) fn over_websocket<S>(
        socket: tokio_tungstenite::WebSocketStream<S>,
        dialect: Dialect,
    ) -> Connection
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (shared, mut rx) = Shared::new(dialect);
        let (mut sink, mut stream) = socket.split();
        tokio::spawn(async move {
            while let Some(mut message) = rx.recv().await {
                message.pop();
                let text = String::from_utf8(message).expect("serde_json writes UTF-8");
                if sink.send(Message::text(text)).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });
        let reader = shared.clone();
        tokio::spawn(async move {
            while let Some(Ok(message)) = stream.next().await {
                let value = match &message {
                    Message::Text(text) => serde_json::from_str::<Value>(text).ok(),
                    Message::Binary(bytes) => serde_json::from_slice::<Value>(bytes).ok(),
                    Message::Close(_) => break,
                    _ => None,
                };
                if let Some(value) = value {
                    reader.deliver(value);
                }
            }
            reader.close();
        });
        Connection { shared }
    }

    /// Sends `method` to `session` ("" for the browser) and waits for the
    /// result.
    pub(crate) async fn call(&self, session: &str, method: &str, params: Value) -> Result<Value> {
        let dialect = self.shared.dialect;
        dialect.speaks(method)?;
        if dialect.forbidden().contains(&method) {
            return Err(Error::Invalid(format!(
                "{method} is never sent: pages can detect it"
            )));
        }
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let route = dialect.route_of(session, &params).to_string();
        let bytes = dialect.encode(id, session, method, params);
        let (tx, rx) = oneshot::channel();
        {
            let mut state = self.shared.lock();
            if state.closed {
                return Err(Error::Closed);
            }
            state.sent.insert(method.to_string());
            state.pending.insert(id, (method.to_string(), route, tx));
        }
        let sent = match &*self.shared.out.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(out) => out.send(bytes).is_ok(),
            None => false,
        };
        if !sent {
            self.shared.lock().pending.remove(&id);
            return Err(Error::Closed);
        }
        rx.await.unwrap_or(Err(Error::Closed))
    }

    /// `call`, but an error naming `method` if Chrome hasn't answered within
    /// `limit`: a renderer kept busy by a page's script, or a Chrome that
    /// has stopped responding, may never answer at all.
    pub(crate) async fn call_within(
        &self,
        limit: std::time::Duration,
        session: &str,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        tokio::time::timeout(limit, self.call(session, method, params))
            .await
            .unwrap_or_else(|_| {
                Err(Error::Timeout(format!(
                    "{} didn't answer {method} within {}s",
                    self.shared.dialect.browser(),
                    limit.as_secs_f64()
                )))
            })
    }

    /// Sends without waiting for the reply, for the reader thread, which
    /// can't wait: answering a request Chrome is holding, say.
    pub(crate) fn send(&self, session: &str, method: &str, params: Value) {
        if self.shared.dialect.speaks(method).is_err() {
            return;
        }
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let bytes = self.shared.dialect.encode(id, session, method, params);
        self.shared.lock().sent.insert(method.to_string());
        if let Some(out) = &*self.shared.out.lock().unwrap_or_else(|e| e.into_inner()) {
            let _ = out.send(bytes);
        }
    }

    pub(crate) fn on(&self, session: &str, handler: Handler) {
        let mut state = self.shared.lock();
        if state.closed {
            drop(state);
            handler(GONE, &Value::Null);
            return;
        }
        state.handlers.insert(session.to_string(), handler);
    }

    /// Stops events reaching the session's handler, which hears GONE.
    pub(crate) fn off(&self, session: &str) {
        let handler = self.shared.lock().handlers.remove(session);
        if let Some(handler) = handler {
            handler(GONE, &Value::Null);
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.shared.lock().closed
    }

    pub(crate) fn dialect(&self) -> Dialect {
        self.shared.dialect
    }

    pub(crate) fn methods_sent(&self) -> Vec<String> {
        self.shared.lock().sent.iter().cloned().collect()
    }

    /// Stops writing; the reader ends when Chrome closes its end.
    pub(crate) fn shut(&self) {
        self.shared
            .out
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }
}

impl Shared {
    fn new(dialect: Dialect) -> (Arc<Shared>, mpsc::UnboundedReceiver<Vec<u8>>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            out: Mutex::new(Some(tx)),
            next_id: AtomicU64::new(1),
            state: Mutex::new(State::default()),
            dialect,
        });
        (shared, rx)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn deliver(&self, message: Value) {
        for incoming in self.dialect.read(&message) {
            match incoming {
                Incoming::Reply { id, result } => {
                    let Some((method, _, tx)) = self.lock().pending.remove(&id) else {
                        continue;
                    };
                    let _ = tx.send(result.map_err(|message| Error::Protocol { method, message }));
                }
                // The browser never answers what was sent to a route that
                // has gone, so that fails now.
                Incoming::Gone { route } => {
                    let stranded: Vec<_> = {
                        let mut state = self.lock();
                        let ids: Vec<u64> = state
                            .pending
                            .iter()
                            .filter(|(_, (_, r, _))| r == route)
                            .map(|(id, _)| *id)
                            .collect();
                        ids.iter()
                            .filter_map(|id| state.pending.remove(id))
                            .collect()
                    };
                    for (method, _, tx) in stranded {
                        let _ = tx.send(Err(Error::Protocol {
                            method,
                            message: format!(
                                "the session went away before {} answered",
                                self.dialect.browser()
                            ),
                        }));
                    }
                    let handler = {
                        let mut state = self.lock();
                        state.parents.remove(route);
                        state.handlers.remove(route)
                    };
                    if let Some(handler) = handler {
                        handler(GONE, &Value::Null);
                    }
                }
                Incoming::Event {
                    route,
                    method,
                    params,
                } => {
                    let handler = {
                        let mut state = self.lock();
                        if method == "browsingContext.contextCreated"
                            && let Some(parent) = params.get("parent").and_then(Value::as_str)
                        {
                            state.parents.insert(route.to_string(), parent.to_string());
                        }
                        // A frame without a handler of its own is its
                        // page's, however deep it is.
                        let (mut at, mut hops) = (route, 0);
                        let mut handler = state.handlers.get(at).cloned();
                        // A frame tree is never this deep; a loop in it,
                        // which Firefox would never send, would never end.
                        while handler.is_none()
                            && hops < 64
                            && let Some(parent) = state.parents.get(at)
                        {
                            at = parent;
                            hops += 1;
                            handler = state.handlers.get(at).cloned();
                        }
                        handler
                    };
                    if let Some(handler) = handler {
                        handler(method, params);
                    }
                }
            }
        }
    }

    fn close(&self) {
        let (pending, handlers) = {
            let mut state = self.lock();
            state.closed = true;
            (
                std::mem::take(&mut state.pending),
                std::mem::take(&mut state.handlers),
            )
        };
        for (_, (_, _, tx)) in pending {
            let _ = tx.send(Err(Error::Closed));
        }
        for handler in handlers.into_values() {
            handler(GONE, &Value::Null);
        }
        self.out.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

/// On Windows, any process the program starts while a Chrome launches
/// inherits Chrome's end of the pipe (see launch::start), and while one
/// holds it the reader never reads Chrome's exit. So once Chrome's process
/// has ended, the reader's blocked read is cancelled, which ends it as the
/// pipe's end would. A read only blocks once the pipe is empty, and a dead
/// Chrome writes nothing more, so nothing Chrome sent is lost. Elsewhere
/// Chrome's end is never inherited, and the pipe's end is enough.
///
/// Ended means its exit code is set. Its handle is signalled only once
/// Windows has torn the whole process down, which on a busy machine came
/// up to five and a half seconds after its exit code was set and later
/// than an unheld pipe's end: so the code is looked at every 20 ms, and
/// the handle is the backstop for a Chrome whose code is STILL_ACTIVE's.
#[cfg(windows)]
fn closes_with(
    chrome: &std::process::Child,
    reader: &std::thread::JoinHandle<()>,
    shared: &Arc<Shared>,
) {
    use std::os::windows::io::{AsHandle, AsRawHandle};
    use windows_sys::Win32::Foundation::{STILL_ACTIVE, WAIT_TIMEOUT};
    use windows_sys::Win32::System::IO::CancelSynchronousIo;
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};

    let (Ok(chrome), Ok(reader)) = (
        chrome.as_handle().try_clone_to_owned(),
        reader.as_handle().try_clone_to_owned(),
    ) else {
        return;
    };
    let shared = shared.clone();
    let _ = std::thread::Builder::new()
        .name("netweir-chrome-exit".into())
        .spawn(move || {
            // SAFETY: both handles are open, owned by this thread, and the
            // code is written to a u32 of its own.
            let running = || unsafe {
                let mut code = 0;
                WaitForSingleObject(chrome.as_raw_handle() as _, 20) == WAIT_TIMEOUT
                    && (GetExitCodeProcess(chrome.as_raw_handle() as _, &mut code) == 0
                        || code == STILL_ACTIVE as u32)
            };
            // A reader that ended by itself needs nothing more.
            while running() {
                if shared.lock().closed {
                    return;
                }
            }
            // Between reads the cancel misses, so it's tried until the
            // reader has ended.
            while !shared.lock().closed {
                unsafe { CancelSynchronousIo(reader.as_raw_handle() as _) };
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        });
}

#[cfg(not(windows))]
fn closes_with(_: &std::process::Child, _: &std::thread::JoinHandle<()>, _: &Arc<Shared>) {}

fn read_loop(mut input: impl BufRead, shared: &Shared) {
    let mut message = Vec::new();
    loop {
        message.clear();
        match input.read_until(0, &mut message) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if message.last() == Some(&0) {
            message.pop();
        }
        // Known limit: a message that isn't JSON is dropped; Chrome only sends
        // JSON, and one bad message shouldn't end the session.
        if let Ok(value) = serde_json::from_slice::<Value>(&message) {
            shared.deliver(value);
        }
    }
    shared.close();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unframed(bytes: &[u8]) -> Value {
        serde_json::from_slice(bytes.strip_suffix(&[0]).expect("NUL-terminated")).unwrap()
    }

    #[test]
    fn cdp_encoding_is_unchanged() {
        let bytes = Dialect::Cdp.encode(7, "S1", "Page.navigate", json!({"url": "about:blank"}));
        assert_eq!(
            unframed(&bytes),
            json!({"id": 7, "method": "Page.navigate", "params": {"url": "about:blank"}, "sessionId": "S1"})
        );
        let browser = unframed(&Dialect::Cdp.encode(8, "", "Browser.getVersion", json!({})));
        assert!(browser.get("sessionId").is_none(), "{browser}");
    }

    #[test]
    fn bidi_encoding_has_no_session() {
        let params = json!({"context": "ctx", "url": "about:blank"});
        let bytes = Dialect::Bidi.encode(3, "ctx", "browsingContext.navigate", params.clone());
        assert_eq!(
            unframed(&bytes),
            json!({"id": 3, "method": "browsingContext.navigate", "params": params})
        );
    }

    #[test]
    fn bidi_replies_and_errors() {
        let ok = json!({"type": "success", "id": 4, "result": {"navigation": "n1"}});
        assert!(matches!(
            &Dialect::Bidi.read(&ok)[..],
            [Incoming::Reply { id: 4, result: Ok(r) }] if r["navigation"] == "n1"
        ));
        let err = json!({"type": "error", "id": 5, "error": "no such frame",
            "message": "Browsing context with id x not found"});
        assert!(matches!(
            &Dialect::Bidi.read(&err)[..],
            [Incoming::Reply { id: 5, result: Err(m) }] if m.contains("not found")
        ));
    }

    #[test]
    fn bidi_events_route_by_context() {
        let load = json!({"type": "event", "method": "browsingContext.load",
            "params": {"context": "C1", "url": "x"}});
        assert!(matches!(
            &Dialect::Bidi.read(&load)[..],
            [Incoming::Event {
                route: "C1",
                method: "browsingContext.load",
                ..
            }]
        ));
        let log = json!({"type": "event", "method": "log.entryAdded",
            "params": {"source": {"realm": "r", "context": "C2"}}});
        assert!(matches!(
            &Dialect::Bidi.read(&log)[..],
            [Incoming::Event { route: "C2", .. }]
        ));
        let browser = json!({"type": "event", "method": "browser.somethingNew", "params": {}});
        assert!(matches!(
            &Dialect::Bidi.read(&browser)[..],
            [Incoming::Event { route: "", .. }]
        ));
        let gone = json!({"type": "event", "method": "browsingContext.contextDestroyed",
            "params": {"context": "C3", "children": []}});
        assert!(matches!(
            &Dialect::Bidi.read(&gone)[..],
            [
                Incoming::Gone { route: "C3" },
                Incoming::Event { route: "C3", .. }
            ]
        ));
    }

    #[test]
    fn bidi_destroyed_context_ends_its_children() {
        let gone = json!({"type": "event", "method": "browsingContext.contextDestroyed",
        "params": {"context": "top", "children": [
            {"context": "frame1", "children": [{"context": "inner", "children": []}]},
            {"context": "frame2", "children": null},
        ]}});
        let routes: Vec<&str> = Dialect::Bidi
            .read(&gone)
            .iter()
            .filter_map(|i| match i {
                Incoming::Gone { route } => Some(*route),
                _ => None,
            })
            .collect();
        assert_eq!(routes, ["top", "frame1", "inner", "frame2"]);
        // One with no context ends nothing, least of all the browser's own
        // handler.
        let nameless = json!({"type": "event", "method": "browsingContext.contextDestroyed",
            "params": {}});
        assert!(matches!(
            &Dialect::Bidi.read(&nameless)[..],
            [Incoming::Event { route: "", .. }]
        ));
    }

    #[tokio::test]
    async fn no_cdp_method_reaches_firefox() {
        let (conn, _firefox) = firefox().await;
        let err = conn
            .call("ctx", "Page.captureScreenshot", json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Unsupported { engine: "Firefox", feature } if feature == "Page.captureScreenshot"),
            "{err:?}"
        );
        conn.send("ctx", "Fetch.continueRequest", json!({}));
        assert!(conn.methods_sent().is_empty(), "{:?}", conn.methods_sent());
    }

    #[test]
    fn bidi_unknown_messages_are_dropped() {
        for m in [
            json!({"id": 1}),
            json!({"type": "error", "id": null, "error": "invalid argument", "message": "unparsable"}),
            json!({"type": "weird"}),
            json!("text"),
        ] {
            assert!(Dialect::Bidi.read(&m).is_empty(), "{m}");
        }
    }

    #[test]
    fn dialects_name_their_browser() {
        assert_eq!(Dialect::Cdp.browser(), "Chrome");
        assert_eq!(Dialect::Bidi.browser(), "Firefox");
        assert!(Dialect::Bidi.forbidden().is_empty());
        assert_eq!(
            Dialect::Cdp.forbidden(),
            &["Runtime.enable", "Console.enable", "Log.enable"]
        );
    }

    /// A BiDi connection and the far end, played by the test.
    async fn firefox() -> (
        Connection,
        tokio_tungstenite::WebSocketStream<tokio::io::DuplexStream>,
    ) {
        let (ours, theirs) = tokio::io::duplex(1 << 16);
        let (ours, theirs) = tokio::join!(
            tokio_tungstenite::client_async("ws://test/session", ours),
            tokio_tungstenite::accept_async(theirs)
        );
        let conn = Connection::over_websocket(ours.unwrap().0, Dialect::Bidi);
        (conn, theirs.unwrap())
    }

    async fn command(
        far: &mut tokio_tungstenite::WebSocketStream<tokio::io::DuplexStream>,
    ) -> Value {
        loop {
            if let Some(Ok(Message::Text(text))) = far.next().await {
                return serde_json::from_str(&text).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn bidi_call_gets_its_reply() {
        let (conn, mut far) = firefox().await;
        let call = tokio::spawn({
            let conn = conn.clone();
            async move { conn.call("", "session.status", json!({})).await }
        });
        let sent = command(&mut far).await;
        assert!(sent.get("sessionId").is_none(), "{sent}");
        let reply = json!({"type": "success", "id": sent["id"], "result": {"ready": true}});
        far.send(Message::text(reply.to_string())).await.unwrap();
        assert_eq!(call.await.unwrap().unwrap(), json!({"ready": true}));
    }

    #[tokio::test]
    async fn bidi_error_names_the_method() {
        let (conn, mut far) = firefox().await;
        let call = tokio::spawn({
            let conn = conn.clone();
            async move {
                conn.call(
                    "",
                    "browsingContext.navigate",
                    json!({"context": "C1", "url": "x"}),
                )
                .await
            }
        });
        let sent = command(&mut far).await;
        let reply =
            json!({"type": "error", "id": sent["id"], "error": "no such frame", "message": "gone"});
        far.send(Message::text(reply.to_string())).await.unwrap();
        let err = call.await.unwrap().unwrap_err();
        assert!(
            matches!(&err, Error::Protocol { method, message }
                if method == "browsingContext.navigate" && message == "gone"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn bidi_command_to_destroyed_context_fails() {
        let (conn, mut far) = firefox().await;
        let heard = Arc::new(Mutex::new(Vec::<String>::new()));
        conn.on("C1", {
            let heard = heard.clone();
            Arc::new(move |method: &str, _: &Value| heard.lock().unwrap().push(method.into()))
        });
        let call = tokio::spawn({
            let conn = conn.clone();
            async move {
                conn.call(
                    "",
                    "browsingContext.navigate",
                    json!({"context": "C1", "url": "x"}),
                )
                .await
            }
        });
        command(&mut far).await;
        let destroyed = json!({"type": "event", "method": "browsingContext.contextDestroyed",
            "params": {"context": "C1", "children": []}});
        far.send(Message::text(destroyed.to_string()))
            .await
            .unwrap();
        let err = tokio::time::timeout(std::time::Duration::from_secs(5), call)
            .await
            .expect("the waiting command should fail, not hang")
            .unwrap()
            .unwrap_err();
        assert!(
            matches!(&err, Error::Protocol { message, .. }
                if message.contains("went away before Firefox answered")),
            "{err}"
        );
        assert_eq!(heard.lock().unwrap().as_slice(), [GONE]);
    }

    #[tokio::test]
    async fn bidi_events_in_a_frame_reach_its_page() {
        let (conn, mut far) = firefox().await;
        let heard = Arc::new(Mutex::new(Vec::<String>::new()));
        conn.on("top", {
            let heard = heard.clone();
            Arc::new(move |_: &str, p: &Value| {
                let context = p["context"].as_str().unwrap_or_default().to_string();
                heard.lock().unwrap().push(context)
            })
        });
        for event in [
            json!({"type": "event", "method": "browsingContext.contextCreated",
                "params": {"context": "frame", "parent": "top"}}),
            json!({"type": "event", "method": "browsingContext.contextCreated",
                "params": {"context": "inner", "parent": "frame"}}),
            json!({"type": "event", "method": "browsingContext.userPromptOpened",
                "params": {"context": "inner"}}),
            json!({"type": "event", "method": "browsingContext.contextDestroyed",
                "params": {"context": "inner", "children": []}}),
            // Gone, it's no one's any more, its going included.
            json!({"type": "event", "method": "browsingContext.load",
                "params": {"context": "inner"}}),
            json!({"type": "event", "method": "browsingContext.load",
                "params": {"context": "frame"}}),
        ] {
            far.send(Message::text(event.to_string())).await.unwrap();
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while heard.lock().unwrap().len() < 4 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            heard.lock().unwrap().as_slice(),
            ["frame", "inner", "inner", "frame"]
        );
    }

    #[tokio::test]
    async fn bidi_close_fails_waiting_commands() {
        let (conn, mut far) = firefox().await;
        let call = tokio::spawn({
            let conn = conn.clone();
            async move { conn.call("", "session.status", json!({})).await }
        });
        command(&mut far).await;
        far.close(None).await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), call)
            .await
            .expect("the waiting command should fail, not hang")
            .unwrap();
        assert!(matches!(result, Err(Error::Closed)), "{result:?}");
    }

    #[tokio::test]
    async fn forbidden_methods_are_refused_without_being_sent() {
        let (input, _keep) = std::io::pipe().unwrap();
        let conn = Connection::new(input, std::io::sink(), None);
        let err = conn
            .call("", "Runtime.enable", json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "{err}");
        assert!(conn.methods_sent().is_empty());
    }

    #[tokio::test]
    async fn a_session_that_detaches_fails_what_was_waiting_on_it() {
        let (input, mut writer) = std::io::pipe().unwrap();
        let conn = Connection::new(input, std::io::sink(), None);
        let waiting = tokio::spawn({
            let conn = conn.clone();
            async move { conn.call("gone", "Page.getFrameTree", json!({})).await }
        });
        let other = tokio::spawn({
            let conn = conn.clone();
            async move { conn.call("kept", "Page.getFrameTree", json!({})).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        writer
            .write_all(
                b"{\"method\":\"Target.detachedFromTarget\",\"params\":{\"sessionId\":\"gone\"}}\0",
            )
            .unwrap();
        let r = tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .expect("still waiting on a session that's gone")
            .unwrap();
        assert!(matches!(r, Err(Error::Protocol { .. })), "{r:?}");
        assert!(!other.is_finished(), "another session's command waits on");
        drop(writer);
    }

    #[tokio::test]
    async fn a_closed_pipe_fails_what_was_waiting() {
        let (input, writer) = std::io::pipe().unwrap();
        let conn = Connection::new(input, std::io::sink(), None);
        let waiting = tokio::spawn({
            let conn = conn.clone();
            async move { conn.call("", "Browser.getVersion", json!({})).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(writer);
        assert_eq!(waiting.await.unwrap(), Err(Error::Closed));
        assert!(conn.is_closed());
        assert_eq!(
            conn.call("", "Browser.getVersion", json!({})).await,
            Err(Error::Closed)
        );
    }

    /// A Chrome that exits is read as gone though another process still
    /// holds its end of the pipe, and what it sent before is still read.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_chrome_that_exits_closes_the_pipe_someone_else_holds() {
        let (input, mut held) = std::io::pipe().unwrap();
        let mut chrome = std::process::Command::new("cmd")
            .args(["/c", "ping -n 3 127.0.0.1 >nul"])
            .spawn()
            .unwrap();
        let conn = Connection::new(input, std::io::sink(), Some(&chrome));
        let answered = tokio::spawn({
            let conn = conn.clone();
            async move { conn.call("", "Browser.getVersion", json!({})).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        held.write_all(b"{\"id\":1,\"result\":{}}\0").unwrap();
        assert_eq!(answered.await.unwrap(), Ok(json!({})));
        let waiting = tokio::spawn({
            let conn = conn.clone();
            async move { conn.call("", "Browser.getVersion", json!({})).await }
        });
        chrome.wait().unwrap();
        let r = tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .expect("Chrome's exit went unread");
        assert_eq!(r.unwrap(), Err(Error::Closed));
        assert!(conn.is_closed());
        drop(held);
    }
}
