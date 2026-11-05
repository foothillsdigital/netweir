//! Steps: a list of things to do on a page, written as JSON, and run one
//! after another with the page's own actions. A crawl runs a request's
//! steps once its page has loaded, before the callback sees it.
//!
//! Every step goes through the same methods a caller would use (`click`,
//! `type_text`, `scroll`...), so it moves the mouse, taps on a phone and
//! presses keys as they do, and never fires events from a script.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::time::Instant;

use crate::{Error, Frame, Page, PageEvent, Pick, Result, WaitUntil, glob_match, typing_time};

/// How long a key waits after the one before it, by default: within
/// these two, most often near the middle.
pub const TYPING: (Duration, Duration) = (Duration::from_millis(80), Duration::from_millis(250));

/// The longest any one timeout, wait or idle may be: a day. Past that
/// it's a mistake, and a big enough one overflows the clock.
pub const MAX_SECONDS: f64 = 86_400.0;

/// The longest pause between two keys `type` takes.
pub const MAX_DELAY: f64 = 10.0;

/// `t` plus `d`, or a year on from `t` where that would overflow.
fn after(t: Instant, d: Duration) -> Instant {
    t.checked_add(d)
        .unwrap_or_else(|| t + Duration::from_secs(365 * 24 * 3600))
}

/// What a step that fails does to the steps after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnError {
    /// Stop, and fail the whole: the request fails.
    Fail,
    /// Note it and go on with the next step.
    Continue,
    /// Stop here, and keep the page as it is.
    Stop,
}

/// Which frame a step acts in: the first whose name is `name`, and whose
/// URL matches the glob `url`.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameSel {
    pub name: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Click {
        selector: String,
    },
    DoubleClick {
        selector: String,
    },
    Fill {
        selector: String,
        text: String,
    },
    Type {
        selector: String,
        text: String,
        delay: (Duration, Duration),
    },
    Press {
        key: String,
    },
    Hover {
        selector: String,
    },
    WaitFor {
        selector: String,
        state: String,
    },
    Wait {
        seconds: Duration,
    },
    WaitForNavigation {
        wait: WaitUntil,
    },
    WaitForResponse {
        url: String,
    },
    Scroll {
        selector: Option<String>,
        x: f64,
        y: Option<f64>,
        times: Option<u32>,
        idle: Duration,
    },
    SelectOption {
        selector: String,
        pick: Pick,
    },
    Check {
        selector: String,
        on: bool,
    },
    SetInputFiles {
        selector: String,
        files: Vec<PathBuf>,
        /// The folder the files must be in, checked on their resolved
        /// paths just before they're uploaded: for steps read from data.
        within: Option<PathBuf>,
    },
    Evaluate {
        js: String,
    },
    Screenshot {
        full_page: bool,
        /// A JPEG of this quality (1 to 100) in place of a PNG.
        quality: Option<u8>,
    },
    Pdf {
        landscape: bool,
        print_background: bool,
    },
    AnswerDialogs {
        accept: bool,
        text: Option<String>,
    },
    /// A function of the caller's own, by name if it has one.
    Call {
        method: Option<String>,
    },
}

impl Action {
    /// The step's name, as written in its JSON.
    pub fn name(&self) -> &'static str {
        match self {
            Action::Click { .. } => "click",
            Action::DoubleClick { .. } => "double_click",
            Action::Fill { .. } => "fill",
            Action::Type { .. } => "type",
            Action::Press { .. } => "press",
            Action::Hover { .. } => "hover",
            Action::WaitFor { .. } => "wait_for",
            Action::Wait { .. } => "wait",
            Action::WaitForNavigation { .. } => "wait_for_navigation",
            Action::WaitForResponse { .. } => "wait_for_response",
            Action::Scroll { .. } => "scroll",
            Action::SelectOption { .. } => "select_option",
            Action::Check { on: true, .. } => "check",
            Action::Check { on: false, .. } => "uncheck",
            Action::SetInputFiles { .. } => "set_input_files",
            Action::Evaluate { .. } => "evaluate",
            Action::Screenshot { .. } => "screenshot",
            Action::Pdf { .. } => "pdf",
            Action::AnswerDialogs { .. } => "answer_dialogs",
            Action::Call { .. } => "call",
        }
    }

    fn selector(&self) -> Option<&str> {
        match self {
            Action::Click { selector }
            | Action::DoubleClick { selector }
            | Action::Fill { selector, .. }
            | Action::Type { selector, .. }
            | Action::Hover { selector }
            | Action::WaitFor { selector, .. }
            | Action::SelectOption { selector, .. }
            | Action::Check { selector, .. }
            | Action::SetInputFiles { selector, .. } => Some(selector),
            Action::Scroll { selector, .. } => selector.as_deref(),
            _ => None,
        }
    }
}

/// One step: what to do, how long it may take, and what its failing does.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub action: Action,
    /// None for the browser's timeout.
    pub timeout: Option<Duration>,
    pub on_error: OnError,
    /// The frame to act in; None for the page's own.
    pub frame: Option<FrameSel>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Failed,
    /// Never run: a step before it stopped them, or the page they were
    /// for never came.
    Skipped,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Failed => "failed",
            Status::Skipped => "skipped",
        }
    }

    fn parse(s: &str) -> Option<Status> {
        Some(match s {
            "ok" => Status::Ok,
            "failed" => Status::Failed,
            "skipped" => Status::Skipped,
            _ => return None,
        })
    }
}

/// What became of one step.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub action: String,
    pub status: Status,
    pub error: Option<String>,
    pub elapsed: Duration,
    /// What it gave: an evaluate's or a call's value, the values a
    /// select_option chose, how many times a scroll loaded more, the URL
    /// and status of the response waited for.
    pub value: Option<Value>,
    /// A screenshot's PNG, or JPEG with a `quality`, or a PDF's bytes.
    pub image: Option<Arc<Vec<u8>>>,
}

impl Outcome {
    fn new(action: &str, status: Status) -> Outcome {
        Outcome {
            action: action.to_string(),
            status,
            error: None,
            elapsed: Duration::ZERO,
            value: None,
            image: None,
        }
    }

    /// Never run, for `why`.
    pub fn skipped(step: &Step, why: Option<&str>) -> Outcome {
        Outcome {
            error: why.map(str::to_string),
            ..Outcome::new(step.action.name(), Status::Skipped)
        }
    }

    /// As JSON, the image as base64: to keep or send elsewhere.
    pub fn to_json(&self) -> Value {
        use base64::Engine;
        json!({
            "action": self.action,
            "status": self.status.as_str(),
            "error": self.error,
            "elapsed": self.elapsed.as_secs_f64(),
            "value": self.value,
            "image": self
                .image
                .as_ref()
                .map(|p| base64::engine::general_purpose::STANDARD.encode(p.as_slice())),
        })
    }

    /// Back from [`Outcome::to_json`].
    pub fn from_json(v: &Value) -> Option<Outcome> {
        use base64::Engine;
        let image = match &v["image"] {
            Value::String(s) => Some(Arc::new(
                base64::engine::general_purpose::STANDARD.decode(s).ok()?,
            )),
            _ => None,
        };
        Some(Outcome {
            action: v["action"].as_str()?.to_string(),
            status: Status::parse(v["status"].as_str()?)?,
            error: v["error"].as_str().map(str::to_string),
            elapsed: Duration::try_from_secs_f64(v["elapsed"].as_f64().unwrap_or(0.0))
                .unwrap_or_default(),
            value: Some(v["value"].clone()).filter(|v| !v.is_null()),
            image,
        })
    }
}

/// How the steps went: an outcome for each step, in order, and the step
/// that failed with `on_error` "fail", if one did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Report {
    pub outcomes: Vec<Outcome>,
    pub failed: Option<usize>,
}

impl Report {
    /// Every step skipped, for `why`: the page they were for never came.
    pub fn skipped(steps: &[Step], why: &str) -> Report {
        Report {
            outcomes: steps
                .iter()
                .map(|s| Outcome::skipped(s, Some(why)))
                .collect(),
            failed: None,
        }
    }

    /// What the step that failed the steps says, naming it.
    pub fn failure(&self) -> Option<String> {
        let i = self.failed?;
        let o = &self.outcomes[i];
        Some(format!(
            "step {} ({}) failed: {}",
            i + 1,
            o.action,
            o.error.as_deref().unwrap_or("for no reason given")
        ))
    }
}

/// Runs a call step for the caller: the step's position, the method it
/// names (if any), and how long it may take. Resolves to the value it
/// gave, or why it failed.
pub type Call = dyn Fn(
        usize,
        Option<String>,
        Duration,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<Value, String>> + Send>>
    + Send
    + Sync;

/// Reads steps from JSON: a list of objects, each with its `action` and
/// that action's fields, and optionally `timeout` (seconds), `on_error`
/// ("fail", "continue" or "stop") and `frame` ({"name", "url"}). A field
/// it doesn't know is an error, so a misspelt one isn't quietly ignored.
pub fn parse(steps: &Value) -> std::result::Result<Vec<Step>, String> {
    let Some(list) = steps.as_array() else {
        return Err("steps must be a list".into());
    };
    list.iter()
        .enumerate()
        .map(|(i, s)| parse_step(s).map_err(|e| format!("step {}: {e}", i + 1)))
        .collect()
}

fn parse_step(v: &Value) -> std::result::Result<Step, String> {
    let Some(o) = v.as_object() else {
        return Err(format!("a step is an object with an \"action\", not {v}"));
    };
    let Some(name) = o.get("action").and_then(Value::as_str) else {
        return Err("it has no \"action\"".into());
    };
    let mut known: Vec<&str> = vec!["action", "timeout", "on_error"];
    let mut f = Fields {
        o,
        known: &mut known,
    };
    let action = (|| -> std::result::Result<Action, String> {
    Ok(match name {
        "click" => Action::Click {
            selector: f.selector()?,
        },
        "double_click" => Action::DoubleClick {
            selector: f.selector()?,
        },
        "fill" => Action::Fill {
            selector: f.selector()?,
            text: f.string("text")?,
        },
        "type" => Action::Type {
            selector: f.selector()?,
            text: f.string("text")?,
            delay: f.delay()?,
        },
        "press" => Action::Press {
            key: f.string("key")?,
        },
        "hover" => Action::Hover {
            selector: f.selector()?,
        },
        "wait_for" => {
            let selector = f.selector()?;
            let state = f.opt_string("state")?.unwrap_or_else(|| "visible".into());
            if !["attached", "visible", "hidden", "detached"].contains(&state.as_str()) {
                return Err(format!(
                    "state must be \"attached\", \"visible\", \"hidden\" or \"detached\", not {state:?}"
                ));
            }
            Action::WaitFor { selector, state }
        }
        "wait" => Action::Wait {
            seconds: f.seconds("seconds")?.ok_or("wait needs \"seconds\"")?,
        },
        "wait_for_navigation" => Action::WaitForNavigation {
            wait: match f.opt_string("wait")? {
                None => WaitUntil::Load,
                Some(w) => w.parse().map_err(|e: Error| e.to_string())?,
            },
        },
        "wait_for_response" => Action::WaitForResponse {
            url: f.string("url")?,
        },
        "scroll" => {
            let selector = f.opt_string("selector")?;
            let x = f.number("x")?.unwrap_or(0.0);
            let y = f.number("y")?;
            let times = match f.take("times") {
                None | Some(Value::Null) => None,
                Some(t) => Some(
                    t.as_u64()
                        .and_then(|t| u32::try_from(t).ok())
                        .ok_or(format!("times must be a whole number, not {t}"))?,
                ),
            };
            let idle = f.seconds("idle")?.unwrap_or(Duration::from_secs(2));
            f.allow("frame");
            Action::Scroll {
                selector,
                x,
                y,
                times,
                idle,
            }
        }
        "select_option" => {
            let selector = f.selector()?;
            let pick = match (f.take("value"), f.take("label"), f.take("index")) {
                (Some(v), None, None) => Pick::Values(many(v, "value", text)?),
                (None, Some(l), None) => Pick::Labels(many(l, "label", text)?),
                (None, None, Some(i)) => Pick::Indexes(many(i, "index", |v| {
                    v.as_u64().and_then(|n| u32::try_from(n).ok())
                })?),
                _ => return Err("select_option takes one of value, label or index".into()),
            };
            Action::SelectOption { selector, pick }
        }
        "check" | "uncheck" => Action::Check {
            selector: f.selector()?,
            on: name == "check",
        },
        "set_input_files" => {
            let selector = f.selector()?;
            let files = match f.take("files") {
                Some(Value::String(one)) => vec![PathBuf::from(one)],
                Some(Value::Array(list)) => list
                    .iter()
                    .map(|p| p.as_str().map(PathBuf::from))
                    .collect::<Option<_>>()
                    .ok_or("files must be a path, or a list of paths")?,
                _ => return Err("set_input_files needs \"files\": a path, or a list".into()),
            };
            let within = f.opt_string("within")?.map(PathBuf::from);
            Action::SetInputFiles {
                selector,
                files,
                within,
            }
        }
        "evaluate" => {
            f.allow("frame");
            Action::Evaluate {
                js: f.string("js")?,
            }
        }
        "screenshot" => Action::Screenshot {
            full_page: f.boolean("full_page")?.unwrap_or(false),
            quality: match f.take("quality") {
                None | Some(Value::Null) => None,
                Some(q) => Some(
                    q.as_u64()
                        .filter(|q| (1..=100).contains(q))
                        .map(|q| q as u8)
                        .ok_or(format!("quality must be a whole number from 1 to 100, not {q}"))?,
                ),
            },
        },
        "pdf" => Action::Pdf {
            landscape: f.boolean("landscape")?.unwrap_or(false),
            print_background: f.boolean("print_background")?.unwrap_or(false),
        },
        "answer_dialogs" => Action::AnswerDialogs {
            accept: f.boolean("accept")?.unwrap_or(true),
            text: f.opt_string("text")?,
        },
        "call" => Action::Call {
            method: f.opt_string("method")?,
        },
        other => {
            return Err(format!(
                "unknown action {other:?}; the actions are click, double_click, fill, type, press, hover, \
                 wait_for, wait, wait_for_navigation, wait_for_response, scroll, \
                 select_option, check, uncheck, set_input_files, evaluate, screenshot, \
                 pdf, answer_dialogs and call"
            ));
        }
    })
    })()
    .map_err(|e| misspelt(o, name).unwrap_or(e))?;
    let timeout = f.seconds("timeout")?;
    if timeout == Some(Duration::ZERO) {
        return Err("timeout must be more than 0 seconds".into());
    }
    let on_error = match f.opt_string("on_error")?.as_deref() {
        None | Some("fail") => OnError::Fail,
        Some("continue") => OnError::Continue,
        Some("stop") => OnError::Stop,
        Some(other) => {
            return Err(format!(
                "on_error must be \"fail\", \"continue\" or \"stop\", not {other:?}"
            ));
        }
    };
    let frame = if f.known.contains(&"frame") {
        match f.take("frame") {
            None | Some(Value::Null) => None,
            Some(Value::Object(sel)) => {
                let part = |k: &str| -> std::result::Result<Option<String>, String> {
                    match sel.get(k) {
                        None | Some(Value::Null) => Ok(None),
                        Some(Value::String(s)) => Ok(Some(s.clone())),
                        Some(v) => Err(format!("frame's {k} must be a string, not {v}")),
                    }
                };
                if let Some(k) = sel.keys().find(|k| !["name", "url"].contains(&k.as_str())) {
                    return Err(format!("frame takes \"name\" and \"url\", not {k:?}"));
                }
                let found = FrameSel {
                    name: part("name")?,
                    url: part("url")?,
                };
                if found.name.is_none() && found.url.is_none() {
                    return Err("frame needs a \"name\" or a \"url\"".into());
                }
                Some(found)
            }
            Some(v) => return Err(format!("frame is {{\"name\", \"url\"}}, not {v}")),
        }
    } else {
        None
    };
    if let Some(k) = o.keys().find(|k| !f.known.contains(&k.as_str())) {
        return Err(format!("{name} has no field {k:?}"));
    }
    Ok(Step {
        action,
        timeout,
        on_error,
        frame,
    })
}

/// Every field any step has: one that isn't is a misspelling, which says
/// more than the field it was meant to be going missing.
const FIELDS: &[&str] = &[
    "action",
    "timeout",
    "on_error",
    "frame",
    "selector",
    "text",
    "delay",
    "key",
    "state",
    "seconds",
    "wait",
    "url",
    "x",
    "y",
    "times",
    "idle",
    "value",
    "label",
    "index",
    "files",
    "within",
    "js",
    "full_page",
    "quality",
    "accept",
    "method",
];

fn misspelt(o: &Map<String, Value>, name: &str) -> Option<String> {
    o.keys()
        .find(|k| !FIELDS.contains(&k.as_str()))
        .map(|k| format!("{name} has no field {k:?}"))
}

/// A step's fields, read one at a time; those read are the ones it knows.
struct Fields<'a, 'k> {
    o: &'a Map<String, Value>,
    known: &'k mut Vec<&'static str>,
}

impl<'a> Fields<'a, '_> {
    fn allow(&mut self, key: &'static str) {
        if !self.known.contains(&key) {
            self.known.push(key);
        }
    }

    fn take(&mut self, key: &'static str) -> Option<&'a Value> {
        self.allow(key);
        self.o.get(key)
    }

    fn opt_string(&mut self, key: &'static str) -> std::result::Result<Option<String>, String> {
        match self.take(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(v) => Err(format!("{key} must be a string, not {v}")),
        }
    }

    fn string(&mut self, key: &'static str) -> std::result::Result<String, String> {
        self.opt_string(key)?
            .ok_or_else(|| format!("it needs \"{key}\""))
    }

    /// A selector, which also lets the step name a frame.
    fn selector(&mut self) -> std::result::Result<String, String> {
        self.allow("frame");
        let s = self.string("selector")?;
        if s.trim().is_empty() {
            return Err("selector is empty".into());
        }
        Ok(s)
    }

    fn number(&mut self, key: &'static str) -> std::result::Result<Option<f64>, String> {
        match self.take(key) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => match v.as_f64().filter(|n| n.is_finite()) {
                Some(n) => Ok(Some(n)),
                None => Err(format!("{key} must be a number, not {v}")),
            },
        }
    }

    fn seconds(&mut self, key: &'static str) -> std::result::Result<Option<Duration>, String> {
        match self.number(key)? {
            None => Ok(None),
            Some(n) if n > MAX_SECONDS => Err(format!(
                "{key} must be at most {MAX_SECONDS} seconds (a day), not {n}"
            )),
            Some(n) => Duration::try_from_secs_f64(n)
                .map(Some)
                .map_err(|_| format!("{key} must be a number of seconds, 0 or more")),
        }
    }

    fn boolean(&mut self, key: &'static str) -> std::result::Result<Option<bool>, String> {
        match self.take(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(v) => Err(format!("{key} must be true or false, not {v}")),
        }
    }

    /// A pause between keys: seconds, or [least, most].
    fn delay(&mut self) -> std::result::Result<(Duration, Duration), String> {
        let secs = |v: &Value| {
            v.as_f64()
                .filter(|n| *n <= MAX_DELAY)
                .and_then(|n| Duration::try_from_secs_f64(n).ok())
                .ok_or(format!(
                    "delay must be seconds (at most {MAX_DELAY}), or [least, most], not {v}"
                ))
        };
        match self.take("delay") {
            None | Some(Value::Null) => Ok(TYPING),
            Some(Value::Array(pair)) if pair.len() == 2 => {
                let (least, most) = (secs(&pair[0])?, secs(&pair[1])?);
                if least > most {
                    return Err("delay's least is more than its most".into());
                }
                Ok((least, most))
            }
            Some(v) => secs(v).map(|d| (d, d)),
        }
    }
}

/// A JSON string as an owned one.
fn text(v: &Value) -> Option<String> {
    v.as_str().map(str::to_string)
}

fn many<T>(
    v: &Value,
    what: &str,
    one: impl Fn(&Value) -> Option<T>,
) -> std::result::Result<Vec<T>, String> {
    let bad = || format!("{what} must be one, or a list, not {v}");
    match v {
        Value::Array(list) => list.iter().map(&one).collect::<Option<_>>().ok_or_else(bad),
        v => one(v).map(|x| vec![x]).ok_or_else(bad),
    }
}

/// The most one step may take: its timeout (the browser's `default` for
/// one that gives none), a wait's seconds, or for `type` the time for the
/// field and then for every key.
fn step_time(step: &Step, default: Duration) -> Duration {
    let timeout = step.timeout.unwrap_or(default);
    match &step.action {
        Action::Wait { seconds } => *seconds,
        Action::Type { text, delay, .. } => timeout.saturating_add(typing_time(text, *delay)),
        _ => timeout,
    }
}

/// At most how long `steps` take, the browser's `default` timeout standing
/// in for each step that gives none, and never more than `cap`, the most
/// a page's steps may take: for a caller's own limit on the page.
pub fn budget(steps: &[Step], default: Duration, cap: Option<Duration>) -> Duration {
    if steps.is_empty() {
        return Duration::ZERO;
    }
    let all = steps
        .iter()
        .map(|s| step_time(s, default))
        .fold(Duration::ZERO, Duration::saturating_add)
        .saturating_add(GRACE.saturating_mul(steps.len() as u32));
    match cap {
        Some(cap) => all.min(cap.saturating_add(GRACE)),
        None => all,
    }
}

/// The time `steps` ask for outright: each wait's seconds, each timeout
/// given, and the keys of each `type`. A step leaning on the browser's
/// timeout asks for nothing here, as the run holds it to the cap anyway.
pub fn asked(steps: &[Step]) -> Duration {
    steps
        .iter()
        .map(|s| match (&s.action, s.timeout) {
            (Action::Wait { .. }, _) | (Action::Type { .. }, _) => {
                step_time(s, s.timeout.unwrap_or_default())
            }
            (_, Some(t)) => t,
            (_, None) => Duration::ZERO,
        })
        .fold(Duration::ZERO, Duration::saturating_add)
}

/// Refuses `steps` that ask outright for more than `cap` seconds, saying
/// how much they ask for.
pub fn within(steps: &[Step], cap: Option<Duration>) -> std::result::Result<(), String> {
    let Some(cap) = cap else {
        return Ok(());
    };
    let asked = asked(steps);
    if asked > cap {
        return Err(format!(
            "the steps ask for {:.1}s (their waits, timeouts and typing), more than the {:.1}s \
             browser_steps_max allows a page's steps",
            asked.as_secs_f64(),
            cap.as_secs_f64()
        ));
    }
    Ok(())
}

/// What the steps do, as canonical JSON, for a request's fingerprint:
/// each action and what it acts on, in a fixed order, without its
/// timeout, `on_error` or typing pace, which change how long a step may
/// take but not the page it leaves.
pub fn key(steps: &[Step]) -> String {
    let list: Vec<Value> = steps
        .iter()
        .map(|step| {
            let mut v = match &step.action {
                Action::Click { selector }
                | Action::DoubleClick { selector }
                | Action::Hover { selector } => json!([step.action.name(), selector]),
                Action::Fill { selector, text } | Action::Type { selector, text, .. } => {
                    json!([step.action.name(), selector, text])
                }
                Action::Press { key } => json!(["press", key]),
                Action::WaitFor { selector, state } => json!(["wait_for", selector, state]),
                Action::Wait { seconds } => json!(["wait", seconds.as_secs_f64()]),
                Action::WaitForNavigation { wait } => {
                    // Written out, so a fingerprint never hangs on how
                    // Rust happens to print the enum.
                    let wait = match wait {
                        WaitUntil::Load => "load",
                        WaitUntil::DomContentLoaded => "domcontentloaded",
                        WaitUntil::NetworkIdle => "networkidle",
                    };
                    json!(["wait_for_navigation", wait])
                }
                Action::WaitForResponse { url } => json!(["wait_for_response", url]),
                Action::Scroll {
                    selector,
                    x,
                    y,
                    times,
                    idle,
                } => json!(["scroll", selector, x, y, times, idle.as_secs_f64()]),
                Action::SelectOption { selector, pick } => {
                    let pick = match pick {
                        Pick::Values(v) => json!(["value", v]),
                        Pick::Labels(l) => json!(["label", l]),
                        Pick::Indexes(i) => json!(["index", i]),
                    };
                    json!(["select_option", selector, pick])
                }
                Action::Check { selector, .. } => json!([step.action.name(), selector]),
                Action::SetInputFiles {
                    selector, files, ..
                } => {
                    let files: Vec<String> =
                        files.iter().map(|f| f.display().to_string()).collect();
                    json!(["set_input_files", selector, files])
                }
                Action::Evaluate { js } => json!(["evaluate", js]),
                Action::Screenshot { full_page, quality } => {
                    json!(["screenshot", full_page, quality])
                }
                Action::Pdf {
                    landscape,
                    print_background,
                } => json!(["pdf", landscape, print_background]),
                Action::AnswerDialogs { accept, text } => json!(["answer_dialogs", accept, text]),
                Action::Call { method } => json!(["call", method]),
            };
            if let (Some(frame), Some(list)) = (&step.frame, v.as_array_mut()) {
                list.push(json!({"frame": [frame.name, frame.url]}));
            }
            v
        })
        .collect();
    Value::Array(list).to_string()
}

/// What a step may take past its timeout before it's cut short, so the
/// action's own timeout, which says better what never happened, comes
/// first.
const GRACE: Duration = Duration::from_millis(500);

/// Runs `steps` on `page`, in order, and says how each went. A step that
/// fails is dealt with as its `on_error` says; those after a step that
/// stops them are skipped. `call` runs the call steps. The steps together
/// take at most `cap`: the step running when it's reached fails them, as
/// does any after it, whatever its `on_error`.
pub async fn run(page: &Page, steps: &[Step], call: &Call, cap: Option<Duration>) -> Report {
    let mut report = Report::default();
    let ends = cap.map(|c| after(Instant::now(), c));
    let out_of_time = || {
        format!(
            "the page's steps ran out of time: browser_steps_max is {}s",
            cap.unwrap_or_default().as_secs_f64()
        )
    };
    // Only heard if a step waits for a response: the page's network
    // events cost nothing until someone listens.
    let mut heard = steps
        .iter()
        .any(|s| matches!(s.action, Action::WaitForResponse { .. }))
        .then(|| Heard {
            rx: page.listen(),
            seen: Vec::new(),
        });
    // The document showing before each step, for a wait_for_navigation
    // after the step that navigates.
    let mut before = page.response();
    let mut dialogs: Option<tokio::task::JoinHandle<()>> = None;
    let mut stopped = false;
    for (i, step) in steps.iter().enumerate() {
        if stopped {
            report.outcomes.push(Outcome::skipped(step, None));
            continue;
        }
        if let Some(h) = &mut heard {
            h.drain(i.wrapping_sub(1));
        }
        let showing = page.response();
        let started = Instant::now();
        let mut outcome = Outcome::new(step.action.name(), Status::Ok);
        let left = ends.map(|e| e.saturating_duration_since(started));
        if left.is_some_and(|l| l.is_zero()) {
            outcome.status = Status::Failed;
            outcome.error = Some(out_of_time());
            report.failed = Some(i);
            report.outcomes.push(outcome);
            stopped = true;
            continue;
        }
        let mut timeout = step.timeout.unwrap_or(page.default_timeout());
        // Whether the page's time, not the step's own, is what limits it.
        let capped = left.is_some_and(|l| l < step_time(step, page.default_timeout()));
        if let Some(left) = left {
            timeout = timeout.min(left);
        }
        let done = match &step.action {
            Action::AnswerDialogs { accept, text } => {
                if let Some(old) = dialogs.take() {
                    old.abort();
                }
                dialogs = Some(answer_dialogs(page, *accept, text.clone()));
                Ok(None)
            }
            // A Firefox page doesn't tell of its responses yet.
            Action::WaitForResponse { .. } if page.is_firefox() => Err(Error::Unsupported {
                engine: "Firefox",
                feature: "a page's responses, which wait_for_response waits on".into(),
            }),
            Action::WaitForResponse { url } => {
                let h = heard.as_mut().expect("listening for responses");
                h.wait(i, url, timeout).await.map(Some)
            }
            Action::WaitForNavigation { wait } => page
                .wait_for_navigation(&before, *wait, Some(timeout))
                .await
                .map(|r| Some(json!({"url": r.url, "status": r.status}))),
            Action::Call { method } => {
                let limit = timeout.saturating_add(GRACE);
                match tokio::time::timeout(limit, call(i, method.clone(), timeout)).await {
                    Ok(Ok(v)) => Ok(Some(v).filter(|v| !v.is_null())),
                    Ok(Err(why)) => Err(Error::Script(why)),
                    Err(_) => Err(Error::Timeout(format!(
                        "the call step took more than {}s",
                        timeout.as_secs_f64()
                    ))),
                }
            }
            action => {
                let mut limit = match action {
                    Action::Wait { seconds } => *seconds,
                    // The typing has time of its own, after the field.
                    Action::Type { text, delay, .. } => {
                        timeout.saturating_add(typing_time(text, *delay))
                    }
                    _ => timeout,
                }
                .saturating_add(GRACE);
                if let Some(left) = left {
                    limit = limit.min(left);
                }
                let work = act(page, step, timeout, &mut outcome);
                match tokio::time::timeout(limit, work).await {
                    Ok(r) => r,
                    Err(_) => Err(Error::Timeout(match action.selector() {
                        Some(s) => format!(
                            "{} {s} took more than {}s",
                            action.name(),
                            timeout.as_secs_f64()
                        ),
                        None => format!(
                            "{} took more than {}s",
                            action.name(),
                            timeout.as_secs_f64()
                        ),
                    })),
                }
            }
        };
        match done {
            Ok(value) => {
                if value.is_some() {
                    outcome.value = value;
                }
                // A step that moved the page on (a link clicked, a form
                // sent) leaves the next one the new document, loaded: the
                // old one's elements are gone.
                if page.response().loader != showing.loader {
                    let left = after(started, timeout).saturating_duration_since(Instant::now());
                    let _ = page.wait_for_load(WaitUntil::Load, Some(left)).await;
                }
                outcome.elapsed = started.elapsed();
            }
            Err(e) => {
                outcome.elapsed = started.elapsed();
                outcome.status = Status::Failed;
                let spent = ends.is_some_and(|e| Instant::now() >= e)
                    || (capped && matches!(e, Error::Timeout(_)));
                outcome.error = Some(if spent {
                    format!("{e}; {}", out_of_time())
                } else {
                    e.to_string()
                });
                // Out of the page's time, nothing after it can run.
                let on_error = if spent { OnError::Fail } else { step.on_error };
                match on_error {
                    OnError::Continue => {}
                    OnError::Stop => stopped = true,
                    OnError::Fail => {
                        stopped = true;
                        report.failed = Some(i);
                    }
                }
            }
        }
        report.outcomes.push(outcome);
        if let Some(h) = &mut heard {
            h.drain(i);
        }
        before = showing;
    }
    if let Some(task) = dialogs {
        task.abort();
    }
    report
}

/// The responses a page got while steps ran, each with the step it came
/// in during (or after: the step it's drained at the start of the next
/// is the one before).
struct Heard {
    rx: tokio::sync::mpsc::UnboundedReceiver<PageEvent>,
    /// Each response's step, URL and status, and whether a wait already
    /// took it: two waits for the same URL want two responses.
    seen: Vec<(usize, String, u16, bool)>,
}

impl Heard {
    fn drain(&mut self, step: usize) {
        while let Ok(event) = self.rx.try_recv() {
            if let PageEvent::Response(r) = event {
                self.seen.push((step, r.url, r.status, false));
            }
        }
    }

    /// The first response matching `url` since the step before step `i`
    /// started, waiting up to `timeout` for one.
    async fn wait(&mut self, i: usize, url: &str, timeout: Duration) -> Result<Value> {
        let since = i.checked_sub(1);
        self.drain(i);
        let found = self.seen.iter_mut().find(|(step, u, _, taken)| {
            !*taken && since.is_none_or(|s| *step != usize::MAX && *step >= s) && glob_match(url, u)
        });
        if let Some((_, u, status, taken)) = found {
            *taken = true;
            return Ok(json!({"url": u, "status": status}));
        }
        let deadline = after(Instant::now(), timeout);
        loop {
            match tokio::time::timeout_at(deadline, self.rx.recv()).await {
                Ok(Some(PageEvent::Response(r))) => {
                    let matched = glob_match(url, &r.url);
                    let v = json!({"url": r.url, "status": r.status});
                    self.seen.push((i, r.url, r.status, matched));
                    if matched {
                        return Ok(v);
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) => return Err(Error::Closed),
                Err(_) => {
                    return Err(Error::Timeout(format!("waiting for a response from {url}")));
                }
            }
        }
    }
}

/// Answers every dialog the page opens, from now until the task ends.
fn answer_dialogs(page: &Page, accept: bool, text: Option<String>) -> tokio::task::JoinHandle<()> {
    let mut rx = page.dialogs();
    tokio::spawn(async move {
        while let Some(dialog) = rx.recv().await {
            let answered = if accept {
                dialog.accept(text.as_deref()).await
            } else {
                dialog.dismiss().await
            };
            if let Err(e) = answered {
                crate::warn(&format!("a step couldn't answer a dialog: {e}"));
            }
        }
    })
}

/// The frame `sel` names, waited for until `deadline`: a frame often
/// loads after the page (a consent banner's does).
async fn frame_of(page: &Page, sel: &FrameSel, deadline: Instant) -> Result<Frame> {
    loop {
        for f in page.frames().await? {
            let named = sel.name.as_deref().is_none_or(|n| n == f.name());
            let at = sel.url.as_deref().is_none_or(|u| glob_match(u, f.url()));
            if named && at {
                return Ok(f);
            }
        }
        if Instant::now() >= deadline {
            let what = match (&sel.name, &sel.url) {
                (Some(n), Some(u)) => format!("named {n:?} at {u}"),
                (Some(n), None) => format!("named {n:?}"),
                (None, u) => format!("at {}", u.as_deref().unwrap_or_default()),
            };
            return Err(Error::Timeout(format!("waiting for a frame {what}")));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Does one of the page's own actions, for a step.
async fn act(
    page: &Page,
    step: &Step,
    timeout: Duration,
    outcome: &mut Outcome,
) -> Result<Option<Value>> {
    let deadline = after(Instant::now(), timeout);
    let frame = match &step.frame {
        Some(sel) => frame_of(page, sel, deadline).await?,
        None => page.main_frame(),
    };
    // What's left of the step's time, once the frame was found.
    let left = Some(deadline.saturating_duration_since(Instant::now()));
    Ok(match &step.action {
        Action::Click { selector } => {
            frame.click(selector, left).await?;
            None
        }
        Action::DoubleClick { selector } => {
            frame.dblclick(selector, left).await?;
            None
        }
        Action::Fill { selector, text } => {
            frame.fill(selector, text, left).await?;
            None
        }
        Action::Type {
            selector,
            text,
            delay,
        } => {
            frame.type_text(selector, text, *delay, left).await?;
            None
        }
        Action::Press { key } => {
            page.press(key).await?;
            None
        }
        Action::Hover { selector } => {
            frame.hover(selector, left).await?;
            None
        }
        Action::WaitFor { selector, state } => {
            frame.wait_for(selector, state, left).await?;
            None
        }
        Action::Wait { seconds } => {
            tokio::time::sleep(*seconds).await;
            None
        }
        Action::Scroll {
            selector,
            x,
            y,
            times,
            idle,
        } => {
            // Held to the step's timeout even with `times`, so a step list
            // has a limit the crawl can count on.
            let loaded = frame
                .scroll(selector.as_deref(), *x, *y, *times, *idle, left)
                .await?;
            Some(json!(loaded))
        }
        Action::SelectOption { selector, pick } => {
            Some(json!(frame.select_option(selector, pick, left).await?))
        }
        Action::Check { selector, on } => {
            frame.set_checked(selector, *on, left).await?;
            None
        }
        Action::SetInputFiles {
            selector,
            files,
            within,
        } => {
            if let Some(missing) = files.iter().find(|f| !f.is_file()) {
                return Err(Error::Invalid(format!("no file at {}", missing.display())));
            }
            match within {
                // Copies Chrome reads, made from the files as they were
                // checked: nothing swapped in later reaches it.
                Some(root) => {
                    let (dir, copies) = crate::uploads::held_copies(root, files)?;
                    page.inner.uploads.keep(dir);
                    frame.set_files_at(selector, &copies, left).await?;
                }
                None => frame.set_input_files(selector, files, left).await?,
            }
            None
        }
        Action::Evaluate { js } => Some(frame.evaluate(js).await?).filter(|v| !v.is_null()),
        Action::Screenshot { full_page, quality } => {
            outcome.image = Some(Arc::new(page.capture(*full_page, *quality).await?));
            None
        }
        Action::Pdf {
            landscape,
            print_background,
        } => {
            let options = crate::PdfOptions {
                landscape: *landscape,
                print_background: *print_background,
                ..Default::default()
            };
            outcome.image = Some(Arc::new(page.pdf(&options).await?));
            None
        }
        Action::AnswerDialogs { .. }
        | Action::WaitForResponse { .. }
        | Action::WaitForNavigation { .. }
        | Action::Call { .. } => unreachable!("run does these itself"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(v: Value) -> std::result::Result<Step, String> {
        parse(&json!([v])).map(|mut s| s.remove(0))
    }

    #[test]
    fn every_action_reads_from_json() {
        let steps = parse(&json!([
            {"action": "click", "selector": "#more", "timeout": 5, "on_error": "continue"},
            {"action": "fill", "selector": "#q", "text": "tea"},
            {"action": "type", "selector": "#q", "text": "tea", "delay": [0.01, 0.02]},
            {"action": "type", "selector": "#q", "text": "tea", "delay": 0.05},
            {"action": "press", "key": "Enter"},
            {"action": "hover", "selector": "nav"},
            {"action": "wait_for", "selector": "li", "state": "attached"},
            {"action": "wait", "seconds": 0.5},
            {"action": "wait_for_navigation", "wait": "domcontentloaded"},
            {"action": "wait_for_response", "url": "**/api/*"},
            {"action": "scroll", "times": 3, "idle": 1},
            {"action": "scroll", "selector": "#feed", "y": 600},
            {"action": "select_option", "selector": "#size", "label": ["S", "M"]},
            {"action": "select_option", "selector": "#size", "index": 2},
            {"action": "check", "selector": "#gift"},
            {"action": "uncheck", "selector": "#gift"},
            {"action": "set_input_files", "selector": "#cv", "files": "cv.pdf"},
            {"action": "evaluate", "js": "document.title", "frame": {"name": "f"}},
            {"action": "screenshot", "full_page": true, "on_error": "stop"},
            {"action": "answer_dialogs", "accept": true, "text": "Ada"},
            {"action": "call", "method": "log_in"},
            {"action": "click", "selector": "button", "frame": {"url": "**/consent/*"}},
        ]))
        .unwrap();
        assert_eq!(steps.len(), 22);
        assert_eq!(steps[0].timeout, Some(Duration::from_secs(5)));
        assert_eq!(steps[0].on_error, OnError::Continue);
        assert_eq!(steps[1].on_error, OnError::Fail);
        assert_eq!(
            steps[2].action,
            Action::Type {
                selector: "#q".into(),
                text: "tea".into(),
                delay: (Duration::from_millis(10), Duration::from_millis(20)),
            }
        );
        assert!(
            matches!(steps[3].action, Action::Type { delay: (a, b), .. } if a == b && a == Duration::from_millis(50))
        );
        assert!(matches!(
            steps[8].action,
            Action::WaitForNavigation {
                wait: WaitUntil::DomContentLoaded
            }
        ));
        assert!(matches!(
            &steps[10].action,
            Action::Scroll {
                selector: None,
                times: Some(3),
                y: None,
                ..
            }
        ));
        assert_eq!(
            steps[12].action,
            Action::SelectOption {
                selector: "#size".into(),
                pick: Pick::Labels(vec!["S".into(), "M".into()]),
            }
        );
        assert!(
            matches!(steps[13].action, Action::SelectOption { pick: Pick::Indexes(ref i), .. } if i == &[2])
        );
        assert_eq!(steps[15].action.name(), "uncheck");
        assert_eq!(
            steps[17].frame,
            Some(FrameSel {
                name: Some("f".into()),
                url: None
            })
        );
        assert_eq!(steps[18].on_error, OnError::Stop);
        assert_eq!(
            steps[20].action,
            Action::Call {
                method: Some("log_in".into())
            }
        );
        assert_eq!(
            steps[21].frame.as_ref().and_then(|f| f.url.as_deref()),
            Some("**/consent/*")
        );
    }

    #[test]
    fn a_wrong_step_says_which_and_why() {
        let err = |v: Value| one(v).unwrap_err();
        assert_eq!(
            err(json!({"action": "click"})),
            "step 1: it needs \"selector\""
        );
        assert_eq!(
            err(json!({"action": "click", "selecter": "#a", "selector": "#a"})),
            "step 1: click has no field \"selecter\""
        );
        assert!(err(json!({"action": "tap", "selector": "#a"})).contains("unknown action \"tap\""));
        assert!(
            err(json!({"action": "click", "selector": "#a", "on_error": "ignore"}))
                .contains("on_error")
        );
        assert!(
            err(json!({"action": "click", "selector": "#a", "timeout": 0})).contains("more than 0")
        );
        assert!(
            err(json!({"action": "click", "selector": "#a", "timeout": -1})).contains("seconds")
        );
        assert!(err(json!({"action": "wait"})).contains("seconds"));
        assert!(
            err(json!({"action": "wait_for", "selector": "a", "state": "gone"})).contains("state")
        );
        assert!(err(json!({"action": "select_option", "selector": "s"})).contains("one of value"));
        assert!(
            err(json!({"action": "select_option", "selector": "s", "value": "a", "index": 1}))
                .contains("one of value")
        );
        assert!(
            err(json!({"action": "type", "selector": "a", "text": "b", "delay": [2, 1]}))
                .contains("least")
        );
        assert!(
            err(json!({"action": "press", "key": "Enter", "frame": {"name": "x"}}))
                .contains("no field \"frame\"")
        );
        assert!(err(json!({"action": "click", "selector": "a", "frame": {}})).contains("name"));
        assert!(
            err(json!({"action": "click", "selector": "a", "frame": {"id": "x"}}))
                .contains("\"id\"")
        );
        assert!(err(json!({"action": "click", "selector": " "})).contains("empty"));
        assert!(err(json!("click")).contains("object"));
        assert_eq!(
            parse(&json!({"action": "click"})).unwrap_err(),
            "steps must be a list"
        );
        assert_eq!(
            parse(&json!([{"action": "press", "key": "a"}, {"action": "nope"}]))
                .unwrap_err()
                .split(':')
                .next(),
            Some("step 2")
        );
    }

    #[test]
    fn the_budget_covers_every_step() {
        let steps = parse(&json!([
            {"action": "click", "selector": "a"},
            {"action": "click", "selector": "a", "timeout": 2},
            {"action": "wait", "seconds": 3},
            {"action": "type", "selector": "a", "text": "abcd", "delay": 0.5},
        ]))
        .unwrap();
        let default = Duration::from_secs(10);
        let typing = typing_time(
            "abcd",
            (Duration::from_millis(500), Duration::from_millis(500)),
        );
        // 10 + 2 + 3 + (10 + the keys), and the grace each.
        let all = Duration::from_secs(25) + typing + GRACE * 4;
        assert_eq!(budget(&steps, default, None), all);
        assert_eq!(budget(&[], default, None), Duration::ZERO);
        let cap = Duration::from_secs(4);
        assert_eq!(budget(&steps, default, Some(cap)), cap + GRACE);
        // What they ask for outright: the 2 s timeout, the wait, the keys.
        assert_eq!(asked(&steps), Duration::from_secs(5) + typing);
        assert!(within(&steps, None).is_ok());
        assert!(within(&steps, Some(Duration::from_secs(60))).is_ok());
        let err = within(&steps, Some(cap)).unwrap_err();
        assert!(err.contains("browser_steps_max"), "{err}");
    }

    #[test]
    fn typing_has_time_for_every_key() {
        let fast = (Duration::ZERO, Duration::ZERO);
        // Each key is held, Shift and all, even with no pause between.
        let t = typing_time(&"x".repeat(100), fast);
        assert!(t >= crate::events::KEY_HOLD_MAX * 100, "{t:?}");
        assert!(typing_time("", fast) > Duration::ZERO);
        assert!(typing_time(&"x".repeat(1_000_000), (Duration::MAX, Duration::MAX)) > t);
    }

    #[test]
    fn huge_values_are_refused_rather_than_overflowing() {
        for v in [
            json!({"action": "click", "selector": "a", "timeout": 1e19}),
            json!({"action": "click", "selector": "a", "timeout": 86_401}),
            json!({"action": "wait", "seconds": 1e300}),
            json!({"action": "scroll", "idle": 1e12}),
            json!({"action": "type", "selector": "a", "text": "b", "delay": 1e9}),
            json!({"action": "type", "selector": "a", "text": "b", "delay": [0, 11]}),
        ] {
            let err = one(v.clone()).unwrap_err();
            assert!(err.contains("at most"), "{v}: {err}");
        }
        assert!(one(json!({"action": "wait", "seconds": 86_400})).is_ok());
        // The clock never overflows, however far out a deadline is.
        let now = Instant::now();
        assert!(after(now, Duration::MAX) > now);
        let wait = json!({"action": "wait", "seconds": 86_400});
        let steps = parse(&json!([wait, wait, wait])).unwrap();
        assert_eq!(
            budget(&steps, Duration::MAX, None),
            Duration::from_secs(3 * 86_400) + GRACE * 3
        );
    }

    #[test]
    fn the_key_is_what_the_steps_do_not_how_long_they_may_take() {
        let k = |v: Value| key(&parse(&v).unwrap());
        let base = k(json!([{"action": "click", "selector": "#more"}]));
        assert_eq!(
            base,
            k(
                json!([{"action": "click", "selector": "#more", "timeout": 3, "on_error": "continue"}])
            )
        );
        assert_ne!(base, k(json!([{"action": "click", "selector": "#less"}])));
        assert_ne!(
            base,
            k(json!([{"action": "double_click", "selector": "#more"}]))
        );
        assert_ne!(
            base,
            k(json!([{"action": "click", "selector": "#more", "frame": {"name": "f"}}]))
        );
        assert_eq!(
            k(json!([{"action": "type", "selector": "q", "text": "tea", "delay": 0.01}])),
            k(json!([{"action": "type", "selector": "q", "text": "tea"}]))
        );
        assert_eq!(k(json!([])), "[]");
        // Fixed: a fingerprint kept in a checkpoint must mean the same
        // thing in the next version.
        assert_eq!(base, r##"[["click","#more"]]"##);
    }

    #[test]
    fn an_upload_step_names_its_folder_but_isnt_keyed_by_it() {
        let s = one(
            json!({"action": "set_input_files", "selector": "f", "files": "a", "within": "/up"}),
        )
        .unwrap();
        assert!(
            matches!(s.action, Action::SetInputFiles { within: Some(ref w), .. } if w == std::path::Path::new("/up"))
        );
        // The folder is the crawl's to say, not part of what the step does.
        assert_eq!(
            key(&[s]),
            key(
                &parse(&json!([{"action": "set_input_files", "selector": "f", "files": "a"}]))
                    .unwrap()
            )
        );
        assert_eq!(
            key(
                &parse(&json!([{"action": "wait_for_navigation", "wait": "domcontentloaded"}]))
                    .unwrap()
            ),
            r#"[["wait_for_navigation","domcontentloaded"]]"#
        );
    }

    #[test]
    fn double_click_and_jpeg_screenshots_read_from_json() {
        let s = one(json!({"action": "double_click", "selector": "td"})).unwrap();
        assert_eq!(
            s.action,
            Action::DoubleClick {
                selector: "td".into()
            }
        );
        let s = one(json!({"action": "screenshot", "quality": 60})).unwrap();
        assert_eq!(
            s.action,
            Action::Screenshot {
                full_page: false,
                quality: Some(60)
            }
        );
        assert!(
            one(json!({"action": "screenshot", "quality": 0}))
                .unwrap_err()
                .contains("1 to 100")
        );
    }

    #[test]
    fn an_outcome_goes_to_json_and_back() {
        let outcome = Outcome {
            action: "screenshot".into(),
            status: Status::Failed,
            error: Some("gone".into()),
            elapsed: Duration::from_millis(1500),
            value: Some(json!({"n": 1})),
            image: Some(Arc::new(vec![0x89, b'P', b'N', b'G'])),
        };
        let back = Outcome::from_json(&outcome.to_json()).unwrap();
        assert_eq!(back, outcome);
        let plain = Outcome::new("click", Status::Ok);
        assert_eq!(Outcome::from_json(&plain.to_json()).unwrap(), plain);
    }

    #[test]
    fn the_failure_names_the_step() {
        let steps = parse(&json!([
            {"action": "click", "selector": "a"},
            {"action": "click", "selector": "b"},
        ]))
        .unwrap();
        let mut report = Report::skipped(&steps, "blocked");
        assert_eq!(report.outcomes[1].status, Status::Skipped);
        assert_eq!(report.outcomes[1].error.as_deref(), Some("blocked"));
        assert_eq!(report.failure(), None);
        report.outcomes[1].status = Status::Failed;
        report.outcomes[1].error = Some("no b".into());
        report.failed = Some(1);
        assert_eq!(report.failure().unwrap(), "step 2 (click) failed: no b");
    }
}
