//! What a page tells its listeners: what it fetches, and the dialogs,
//! popups and downloads it starts. The design of the last three is in
//! docs/design/dialogs.md.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::Instant;

use crate::browser::str_of;
use crate::conn::Connection;
use crate::network::went_away;
use crate::{Error, Page, Request, Response, Result};

/// What the page told its listeners about.
#[derive(Clone)]
pub enum PageEvent {
    Request(Request),
    Response(Response),
    /// A page it opened: `window.open()`, or a link with a target. It's
    /// running, with the opener's rules, on whatever document Chrome
    /// shows first; [`Page::wait_for_load`] waits for the one it was
    /// opened for.
    Popup(Page),
    Download(Download),
}

impl std::fmt::Debug for PageEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PageEvent::Request(r) => f.debug_tuple("Request").field(r).finish(),
            PageEvent::Response(r) => f.debug_tuple("Response").field(r).finish(),
            PageEvent::Popup(p) => f.debug_tuple("Popup").field(&p.url()).finish(),
            PageEvent::Download(d) => f.debug_tuple("Download").field(&d.url()).finish(),
        }
    }
}

/// An `alert()`, `confirm()`, `prompt()` or `beforeunload` the page is
/// waiting on, from [`Page::dialogs`]. Answered once; one nobody answers,
/// dropped or held past the browser's timeout, is dismissed
/// (`beforeunload` is accepted). No answer reaches the page sooner than a
/// person's would.
#[derive(Clone)]
pub struct Dialog {
    inner: Arc<DialogInner>,
}

struct DialogInner {
    kind: String,
    message: String,
    default_value: String,
    url: String,
    conn: Connection,
    answer: Answer,
    timeout: Duration,
    /// A moment after it opened: no answer goes sooner.
    not_before: Instant,
    answered: AtomicBool,
    runtime: tokio::runtime::Handle,
}

/// What a dialog says when it opens.
struct Opened {
    kind: String,
    message: String,
    default_value: String,
    url: String,
    answer: Answer,
}

/// Where a dialog's answer goes: Chrome's page session, or Firefox's
/// browsing context.
enum Answer {
    Cdp { session: String },
    Bidi { context: String },
}

impl Answer {
    /// The command that answers, with `accept`, and `text` for a prompt.
    fn command(&self, accept: bool, text: Option<&str>) -> (&str, &'static str, Value) {
        match self {
            Answer::Cdp { session } => {
                let mut params = json!({"accept": accept});
                if let Some(text) = text {
                    params["promptText"] = Value::from(text);
                }
                (session, "Page.handleJavaScriptDialog", params)
            }
            Answer::Bidi { context } => {
                let mut params = json!({"context": context, "accept": accept});
                if let Some(text) = text {
                    params["userText"] = Value::from(text);
                }
                ("", "browsingContext.handleUserPrompt", params)
            }
        }
    }
}

/// How long a person takes to react: to read a dialog and answer it, or
/// to switch to a tab before using it. A time picked at random between
/// the two, for each.
pub(crate) fn pause((least, most): (Duration, Duration)) -> Duration {
    let random = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    let spread = most.saturating_sub(least).as_micros() as u64;
    least + Duration::from_micros(random.checked_rem(spread).unwrap_or(0))
}

/// A number picked at random, more than 0 and less than 1.
fn unit() -> f64 {
    let random = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    ((random >> 11) as f64 + 0.5) / (1u64 << 53) as f64
}

/// A time picked from a log-normal spread, as people's timings are: most
/// near `median`, a few well above it, none below `least` or above
/// `most`. `sigma` is the spread of its logarithm.
pub(crate) fn skewed(median: Duration, sigma: f64, least: Duration, most: Duration) -> Duration {
    // Box-Muller: a standard normal from two uniforms.
    let normal = (-2.0 * unit().ln()).sqrt() * (std::f64::consts::TAU * unit()).cos();
    let secs = median.as_secs_f64() * (sigma * normal).exp();
    Duration::try_from_secs_f64(secs)
        .unwrap_or(most)
        .clamp(least, most)
}

/// How long a person holds a key down: about 85 ms, from 45 to 180, more
/// often longer than shorter (keystroke studies put most between 60 and
/// 120).
pub(crate) fn key_hold() -> Duration {
    skewed(
        Duration::from_millis(85),
        0.25,
        Duration::from_millis(45),
        Duration::from_millis(180),
    )
}

/// The longest [`key_hold`] gives.
pub(crate) const KEY_HOLD_MAX: Duration = Duration::from_millis(180);

/// A pause between two keys within (`least`, `most`): log-normal around
/// their geometric mean, as typists' intervals are; `least` itself when
/// the two are equal.
pub(crate) fn key_gap((least, most): (Duration, Duration)) -> Duration {
    if most <= least {
        return least;
    }
    let low = least.max(Duration::from_millis(1)).as_secs_f64();
    let high = most.as_secs_f64();
    let median = Duration::from_secs_f64((low * high).sqrt());
    // Most of them (two standard deviations either way) within the range.
    let sigma = (high / low).ln() / 4.0;
    skewed(median, sigma, least, most)
}

impl Dialog {
    /// For `Page.javascriptDialogOpening` on `session`.
    pub(crate) fn opened(
        conn: &Connection,
        session: &str,
        params: &Value,
        timeout: Duration,
        pause_range: (Duration, Duration),
        runtime: &tokio::runtime::Handle,
    ) -> Dialog {
        let opened = Opened {
            kind: str_of(params, "type"),
            message: str_of(params, "message"),
            default_value: str_of(params, "defaultPrompt"),
            url: str_of(params, "url"),
            answer: Answer::Cdp {
                session: session.to_string(),
            },
        };
        Dialog::make(conn, opened, timeout, pause_range, runtime)
    }

    /// For `browsingContext.userPromptOpened`, on Firefox, in the frame
    /// `context`, whose document is at `url`.
    pub(crate) fn opened_bidi(
        conn: &Connection,
        params: &Value,
        url: String,
        timeout: Duration,
        pause_range: (Duration, Duration),
        runtime: &tokio::runtime::Handle,
    ) -> Dialog {
        let opened = Opened {
            kind: str_of(params, "type"),
            message: str_of(params, "message"),
            default_value: str_of(params, "defaultValue"),
            url,
            answer: Answer::Bidi {
                context: str_of(params, "context"),
            },
        };
        Dialog::make(conn, opened, timeout, pause_range, runtime)
    }

    fn make(
        conn: &Connection,
        opened: Opened,
        timeout: Duration,
        pause_range: (Duration, Duration),
        runtime: &tokio::runtime::Handle,
    ) -> Dialog {
        let Opened {
            kind,
            message,
            default_value,
            url,
            answer,
        } = opened;
        let dialog = Dialog {
            inner: Arc::new(DialogInner {
                kind,
                message,
                default_value,
                url,
                conn: conn.clone(),
                answer,
                timeout,
                not_before: Instant::now() + pause(pause_range),
                answered: AtomicBool::new(false),
                runtime: runtime.clone(),
            }),
        };
        // A listener that never answers gets the browser's timeout.
        let waiting = Arc::downgrade(&dialog.inner);
        runtime.spawn(async move {
            tokio::time::sleep(timeout).await;
            if let Some(dialog) = waiting.upgrade() {
                dialog.release();
            }
        });
        dialog
    }

    /// "alert", "confirm", "prompt" or "beforeunload".
    pub fn kind(&self) -> &str {
        &self.inner.kind
    }

    pub fn message(&self) -> &str {
        &self.inner.message
    }

    /// The text a prompt offers; empty for the others.
    pub fn default_value(&self) -> &str {
        &self.inner.default_value
    }

    /// The URL of the frame that opened it.
    pub fn url(&self) -> &str {
        &self.inner.url
    }

    pub fn is_answered(&self) -> bool {
        self.inner.answered.load(Ordering::Relaxed)
    }

    /// Presses OK, with `text` in a prompt's box (its default if None);
    /// for `beforeunload`, leaves. It counts as answered from the moment
    /// this is called; the answer reaches the page once a person could
    /// have given it.
    pub fn accept(&self, text: Option<&str>) -> impl Future<Output = Result<()>> + Send + 'static {
        self.answer(true, text)
    }

    /// Presses Cancel; for `beforeunload`, stays.
    pub fn dismiss(&self) -> impl Future<Output = Result<()>> + Send + 'static {
        self.answer(false, None)
    }

    /// Answers it the default way now, unless it's been answered.
    pub fn release(&self) {
        self.inner.release();
    }

    fn answer(
        &self,
        accept: bool,
        text: Option<&str>,
    ) -> impl Future<Output = Result<()>> + Send + 'static {
        let inner = self.inner.clone();
        let answered = inner.answered.swap(true, Ordering::Relaxed);
        let text = text.map(str::to_string);
        let already = format!(
            "the {} \"{}\" was already answered, or answered the default way when nobody had \
             in time",
            inner.kind, inner.message
        );
        // The answer goes from a task of its own, so a caller that drops
        // the future (or times it out) still has it reach the page: the
        // dialog counts as answered, and nothing else would answer it.
        let sending = (!answered).then(|| {
            let runtime = inner.runtime.clone();
            runtime.spawn(async move { inner.send(accept, text.as_deref()).await })
        });
        async move {
            match sending {
                Some(sending) => sending.await.unwrap_or(Err(Error::Closed)),
                None => Err(Error::Gone(already)),
            }
        }
    }
}

impl DialogInner {
    async fn send(&self, accept: bool, text: Option<&str>) -> Result<()> {
        tokio::time::sleep_until(self.not_before).await;
        let (session, method, params) = self.answer.command(accept, text);
        let answer = self.conn.call_within(self.timeout, session, method, params);
        match answer.await {
            Ok(_) => Ok(()),
            Err(e) if went_away(&e) => Ok(()),
            // Answered in Chrome's window, or the page moved on: nothing
            // waits for it any more.
            Err(Error::Protocol { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn release(&self) {
        if self.answered.swap(true, Ordering::Relaxed) {
            return;
        }
        // Leaving is what netweir's own goto() asked for; anything else
        // is Escape.
        let accept = self.kind == "beforeunload";
        let (session, method, params) = self.answer.command(accept, None);
        let (conn, session, at) = (self.conn.clone(), session.to_string(), self.not_before);
        self.runtime.spawn(async move {
            tokio::time::sleep_until(at).await;
            conn.send(&session, method, params);
        });
    }
}

impl Drop for DialogInner {
    fn drop(&mut self) {
        self.release();
    }
}

/// A file the page downloaded, or is downloading, into its context's own
/// folder, where Chrome names it after its GUID. It's removed when the
/// context closes. One let go before it finishes, every copy dropped (or
/// never heard, with nobody listening), is cancelled: nothing could
/// read it.
#[derive(Clone)]
pub struct Download {
    inner: Arc<DownloadInner>,
}

struct DownloadInner {
    guid: String,
    url: String,
    suggested_filename: String,
    /// Where Chrome writes it; None for a page whose context has no
    /// folder of netweir's.
    file: Option<PathBuf>,
    context: Option<String>,
    conn: Connection,
    timeout: Duration,
    /// None while it's under way; then whether it finished, or why not.
    done: tokio::sync::watch::Sender<Option<std::result::Result<(), String>>>,
}

impl Download {
    pub fn url(&self) -> &str {
        &self.inner.url
    }

    /// The name the server or the page gave it.
    pub fn suggested_filename(&self) -> &str {
        &self.inner.suggested_filename
    }

    /// Waits for it to end, for no longer than `timeout` (the browser's
    /// if None): Ok once the whole file is there, or why not ("canceled",
    /// which is also what Chrome says of one whose connection broke).
    async fn finished(&self, timeout: Option<Duration>) -> Result<std::result::Result<(), String>> {
        let inner = &self.inner;
        let mut done = inner.done.subscribe();
        let ended = tokio::time::timeout(
            timeout.unwrap_or(inner.timeout),
            done.wait_for(Option::is_some),
        );
        match ended.await {
            Ok(Ok(end)) => Ok(end.clone().expect("waited for")),
            Ok(Err(_)) => Ok(Err("the browser closed".into())),
            Err(_) => Err(Error::Timeout(format!(
                "waiting for the download of {} to finish",
                inner.url
            ))),
        }
    }

    /// Waits for it to end; None if it finished, or why it didn't. Gives
    /// up after `timeout`, the browser's if None, and leaves it going.
    pub async fn failure(&self, timeout: Option<Duration>) -> Result<Option<String>> {
        Ok(self.finished(timeout).await?.err())
    }

    /// Waits for it to finish, and returns the file Chrome wrote. Gives up
    /// after `timeout`, the browser's if None, and leaves it going.
    pub async fn path(&self, timeout: Option<Duration>) -> Result<PathBuf> {
        let inner = &self.inner;
        if let Err(why) = self.finished(timeout).await? {
            return Err(Error::Gone(format!(
                "the download of {} didn't finish: {why}",
                inner.url
            )));
        }
        inner.file.clone().ok_or_else(|| {
            Error::Gone(format!(
                "the download of {} went where Chrome keeps them, not to a folder of netweir's",
                inner.url
            ))
        })
    }

    /// Stops it, if it hasn't finished.
    pub async fn cancel(&self) -> Result<()> {
        let inner = &self.inner;
        let cancel = inner.conn.call_within(
            inner.timeout,
            "",
            "Browser.cancelDownload",
            inner.cancel_params(),
        );
        match cancel.await {
            // Already over.
            Ok(_) | Err(Error::Protocol { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

impl DownloadInner {
    fn cancel_params(&self) -> Value {
        let mut params = json!({"guid": self.guid});
        if let Some(id) = &self.context {
            params["browserContextId"] = Value::from(id.as_str());
        }
        params
    }
}

impl Drop for DownloadInner {
    fn drop(&mut self) {
        if self.done.borrow().is_none() {
            self.conn
                .send("", "Browser.cancelDownload", self.cancel_params());
        }
    }
}

/// Every download under way, and the folders they go to: one for each
/// context, under one the browser makes the first time it's needed,
/// which a launch's sweep takes if the program dies.
#[derive(Default)]
pub(crate) struct Downloads {
    root: Mutex<Option<crate::launch::TempProfile>>,
    under_way: Mutex<UnderWay>,
}

type End = std::result::Result<(), String>;

/// Kept until Chrome says each has ended, a dropped one's too, so that
/// a download is only ever made once.
#[derive(Default)]
struct UnderWay {
    downloads: HashMap<String, Weak<DownloadInner>>,
    /// Those whose page is being looked for, and how each ended, if it
    /// has meanwhile.
    looking: HashMap<String, Option<End>>,
}

impl Downloads {
    /// The folder for `context`'s downloads (None for the default
    /// context), made if need be.
    pub(crate) fn folder(&self, context: Option<&str>) -> std::io::Result<PathBuf> {
        let mut root = self.root.lock().unwrap_or_else(|e| e.into_inner());
        if root.is_none() {
            *root = Some(crate::launch::TempProfile::make()?);
        }
        let folder = root
            .as_ref()
            .expect("made above")
            .dir()
            .join(context.unwrap_or("default"));
        std::fs::create_dir_all(&folder)?;
        Ok(folder)
    }

    /// The folder for `context`'s downloads, if it has one.
    pub(crate) fn folder_of(&self, context: Option<&str>) -> Option<PathBuf> {
        let root = self.root.lock().unwrap_or_else(|e| e.into_inner());
        let folder = root.as_ref()?.dir().join(context.unwrap_or("default"));
        folder.is_dir().then_some(folder)
    }

    /// Removes `context`'s folder, and what's in it.
    pub(crate) fn remove(&self, context: Option<&str>) {
        if let Some(folder) = self.folder_of(context) {
            let _ = std::fs::remove_dir_all(folder);
        }
    }

    /// Removes every folder; for the browser closing.
    pub(crate) fn remove_all(&self) {
        self.root.lock().unwrap_or_else(|e| e.into_inner()).take();
    }

    /// For `Page.downloadWillBegin`: the download, or None if it's been
    /// heard of already (from another of the page's sessions).
    pub(crate) fn begin(
        &self,
        conn: &Connection,
        timeout: Duration,
        context: Option<&str>,
        params: &Value,
    ) -> Option<Download> {
        let guid = str_of(params, "guid");
        let mut under_way = self.under_way.lock().unwrap_or_else(|e| e.into_inner());
        if under_way.downloads.contains_key(&guid) {
            return None;
        }
        // Taken in the same lock that progress() ends it in, so an end
        // that came while its page was looked for isn't missed.
        let ended = under_way.looking.remove(&guid).flatten();
        let download = Download {
            inner: Arc::new(DownloadInner {
                file: self.folder_of(context).map(|f| f.join(&guid)),
                guid: guid.clone(),
                url: str_of(params, "url"),
                suggested_filename: str_of(params, "suggestedFilename"),
                context: context.map(str::to_string),
                conn: conn.clone(),
                timeout,
                done: tokio::sync::watch::Sender::new(ended.clone()),
            }),
        };
        if ended.is_none() {
            under_way
                .downloads
                .insert(guid, Arc::downgrade(&download.inner));
        }
        Some(download)
    }

    /// Whether the download `guid` has been made, or its page is being
    /// looked for.
    pub(crate) fn knows(&self, guid: &str) -> bool {
        let under_way = self.under_way.lock().unwrap_or_else(|e| e.into_inner());
        under_way.downloads.contains_key(guid) || under_way.looking.contains_key(guid)
    }

    /// Notes that `guid`'s page is being looked for, until begin() or
    /// forget().
    pub(crate) fn look_for(&self, guid: &str) {
        let mut under_way = self.under_way.lock().unwrap_or_else(|e| e.into_inner());
        under_way.looking.insert(guid.to_string(), None);
    }

    /// `guid` is no page's of netweir's.
    pub(crate) fn forget(&self, guid: &str) {
        let mut under_way = self.under_way.lock().unwrap_or_else(|e| e.into_inner());
        under_way.looking.remove(guid);
    }

    /// For `Browser.downloadProgress`.
    pub(crate) fn progress(&self, params: &Value) {
        let end = match params["state"].as_str() {
            Some("completed") => Ok(()),
            Some("canceled") => Err("canceled".to_string()),
            _ => return,
        };
        let guid = str_of(params, "guid");
        let download = {
            let mut under_way = self.under_way.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(looking) = under_way.looking.get_mut(&guid) {
                *looking = Some(end);
                return;
            }
            under_way.downloads.remove(&guid).and_then(|d| d.upgrade())
        };
        if let Some(download) = download {
            download.done.send_replace(Some(end));
        }
    }

    /// The browser has gone: nothing under way will finish.
    pub(crate) fn gone(&self) {
        let under_way =
            std::mem::take(&mut *self.under_way.lock().unwrap_or_else(|e| e.into_inner()));
        for download in under_way
            .downloads
            .into_values()
            .filter_map(|d| d.upgrade())
        {
            download
                .done
                .send_replace(Some(Err("the browser closed".into())));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pause_is_as_long_as_a_person_takes() {
        let range = (Duration::from_millis(500), Duration::from_millis(1500));
        for _ in 0..100 {
            let p = pause(range);
            assert!(p >= range.0 && p < range.1);
        }
        let all: std::collections::HashSet<_> = (0..100).map(|_| pause(range)).collect();
        assert!(all.len() > 10, "picked at random");
        let none = (Duration::ZERO, Duration::ZERO);
        assert_eq!(pause(none), Duration::ZERO, "no pause at all");
    }

    #[test]
    fn each_context_has_a_folder_of_its_own_that_goes_with_it() {
        let downloads = Downloads::default();
        assert_eq!(downloads.folder_of(Some("a")), None);
        let a = downloads.folder(Some("a")).unwrap();
        let b = downloads.folder(None).unwrap();
        assert_ne!(a, b);
        assert_eq!(downloads.folder_of(Some("a")), Some(a.clone()));
        let root = a.parent().unwrap().to_path_buf();
        assert!(b.starts_with(&root));
        downloads.remove(Some("a"));
        assert!(!a.exists() && b.exists());
        downloads.remove_all();
        assert!(!root.exists());
    }
}
