//! The crawl engine from Python: submit requests, await batches of events.
//!
//! Pages reached through declarative rules never reach Python: they are
//! parsed here, on blocking threads, their links are queued and their items
//! extracted, and Python receives only the finished items.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use netweir_core::checkpoint::Pending;
use netweir_core::queue::{ConnectError, Queue};
use netweir_core::{
    BrowserMode, CrawlRequest, CrawlSettings, Crawler as CoreCrawler, DropReason, Event,
    FetchError, ProxyRotation, Submitted,
};
use netweir_dom::auto;
use netweir_dom::date;
use netweir_dom::extract::{self, TrackContext, Value};
use netweir_dom::{Document, Query};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList, PyTuple};
use url::Url;

use crate::browser::BrowserPage;
use crate::fetch::{AuthPy, Response, fetch_error_value, fetch_options, policy, policy_py};
use crate::node::Node;
use crate::rules::{ItemSpec, Links, Notes, Part, Rule, StoreRef, TrackStore};

fn seconds(name: &str, value: f64) -> PyResult<Duration> {
    Duration::try_from_secs_f64(value).map_err(|_| {
        PyValueError::new_err(format!("{name} must be a non-negative number of seconds"))
    })
}

fn positive(name: &str, value: f64) -> PyResult<Duration> {
    if value.is_nan() || value <= 0.0 {
        return Err(PyValueError::new_err(format!("{name} must be positive")));
    }
    seconds(name, value)
}

/// Connects to a crawl's shared queue, without the GIL while it waits. A
/// URL, key or lease that can't work is a ValueError; a server that can't
/// be reached, a `netweir.QueueError`.
fn connect(py: Python<'_>, url: &str, key: &str, lease: Duration) -> PyResult<Queue> {
    py.detach(|| {
        pyo3_async_runtimes::tokio::get_runtime().block_on(Queue::connect(url, key, lease))
    })
    .map_err(|e| match e {
        ConnectError::Invalid(m) => PyValueError::new_err(m),
        ConnectError::Unreachable(m) => queue_error(py, m),
    })
}

/// `netweir.QueueError(message)`.
pub(crate) fn queue_error(py: Python<'_>, message: String) -> PyErr {
    match py
        .import("netweir._errors")
        .and_then(|m| m.getattr("QueueError"))
        .and_then(|class| class.call1((message,)))
    {
        Ok(error) => PyErr::from_value(error),
        Err(e) => e,
    }
}

/// Ids from here up belong to requests the rules made; Python's count from 0.
const RULE_IDS: u64 = 1 << 62;

/// Why the rules should leave a page alone: an error status (a 404 page
/// is no product page), or a body that isn't HTML (a PDF a loose rule
/// linked to).
fn not_a_page(r: &netweir_core::Response) -> Option<String> {
    if !(200..300).contains(&r.status) {
        return Some(format!("HTTP {}", r.status));
    }
    let ctype = r.header("content-type")?.to_ascii_lowercase();
    let html = [
        "text/html",
        "application/xhtml+xml",
        "text/xml",
        "application/xml",
    ];
    if html.iter().any(|h| ctype.trim_start().starts_with(h)) {
        None
    } else {
        Some(format!("not HTML ({})", ctype.trim()))
    }
}

/// What to do with a request's page besides returning it.
#[derive(Clone)]
struct Tag {
    /// The rule that led here; None for a request Python submitted.
    rule: Option<usize>,
    /// Take links from the page with every rule.
    apply_rules: bool,
    /// Give the page to Python.
    to_python: bool,
    url: String,
    /// The request's depth; links found on its page are one deeper.
    depth: u32,
}

/// What became of a page that needed Rust work.
struct Processed {
    id: u64,
    tag: Tag,
    /// Kept only for pages Python will see.
    response: Option<netweir_core::Response>,
    root: Option<Arc<Document>>,
    links: Vec<(String, usize)>,
    item: Option<Vec<Value>>,
    /// Items "here" rules took from the page, by rule.
    here: Vec<(usize, Vec<Value>)>,
    /// What tracking did to the item's tracked fields.
    notes: Notes,
    /// The page, when tracking failed on it, for repairs.
    html: Option<String>,
    error: Option<String>,
    /// Why the rules left the page alone: an error status, or not HTML.
    ignored: Option<String>,
    /// What recovering from broken XML repaired, for the log.
    repaired: Option<String>,
}

/// One event for Python, built under the GIL at the end of a batch.
enum Out {
    Fetched {
        id: u64,
        response: netweir_core::Response,
        root: Option<Arc<Document>>,
        page: Option<netweir_core::LivePage>,
    },
    Warning {
        message: String,
    },
    Failed {
        id: u64,
        error: FetchError,
    },
    Dropped {
        id: u64,
        why: &'static str,
    },
    /// Python's request was dealt with entirely in Rust.
    Handled {
        id: u64,
    },
    Item {
        rule: usize,
        url: String,
        values: Vec<Value>,
        notes: Notes,
        html: Option<String>,
    },
    Ruled {
        rule: usize,
        url: String,
        response: netweir_core::Response,
        root: Option<Arc<Document>>,
        depth: u32,
    },
    RuleFailed {
        rule: usize,
        url: String,
        error: FetchError,
    },
    RuleDropped {
        rule: usize,
        url: String,
        why: &'static str,
    },
    PageError {
        url: String,
        message: String,
    },
    /// A page the rules left alone, and why.
    Ignored {
        url: String,
        reason: String,
    },
    /// Still blocked after every retry. `rule` is set when a rule made
    /// the request; otherwise `id` is Python's.
    Blocked {
        id: u64,
        rule: Option<usize>,
        url: String,
        vendor: String,
        kind: &'static str,
        response: netweir_core::Response,
    },
    Paused {
        host: String,
        seconds: f64,
    },
    /// A request of Python's leased from the shared queue, with its row.
    Leased {
        request: Box<Pending>,
    },
    Fatal {
        message: String,
    },
}

/// The rule a saved or leased request was made by, if it still exists.
/// A request's cookies, checked against its URL now, so a bad one is an
/// error where the request was made rather than lost at sending.
fn request_cookies(
    url: &str,
    jar: Option<String>,
    cookies: Vec<Bound<'_, PyAny>>,
    dont_merge: bool,
) -> PyResult<netweir_core::RequestCookies> {
    let add = cookies
        .iter()
        .map(|c| crate::cookies::cookie_from(c, true))
        .collect::<PyResult<Vec<_>>>()?;
    if let (false, Ok(parsed)) = (add.is_empty(), Url::parse(url)) {
        for c in &add {
            netweir_core::cookies::Jar::check(c, Some(&parsed))
                .map_err(|e| PyValueError::new_err(format!("{url}: {e}")))?;
        }
    }
    Ok(netweir_core::RequestCookies {
        jar,
        add,
        dont_merge,
    })
}

fn rule_of(rules: &[Arc<Rule>], p: &Pending) -> Option<usize> {
    serde_json::from_str::<serde_json::Value>(&p.payload)
        .ok()
        .and_then(|v| v.get("rule").and_then(serde_json::Value::as_u64))
        .map(|r| r as usize)
        .filter(|r| *r < rules.len())
}

/// A request of Python's as `resume()` and the "leased" event give it:
/// (row, url, priority, headers, dont_filter, depth, payload),
/// (method, kind, body, content_type, referer, retry_post, proxy) and its
/// policy, (timeout, max_redirects, dont_redirect, auth).
type PendingPy = (
    (i64, String, i32, Vec<(String, String)>, bool, u32, String),
    (
        String,
        String,
        Option<Py<PyBytes>>,
        Option<String>,
        Option<String>,
        bool,
        Option<String>,
    ),
    (Option<f64>, Option<u32>, bool, Option<AuthPy>),
);

fn pending_py(py: Python<'_>, p: Pending) -> PendingPy {
    let body = p.body.map(|b| PyBytes::new(py, &b).unbind());
    (
        (
            p.row,
            p.url,
            p.priority,
            p.headers,
            p.dont_filter,
            p.depth,
            p.payload,
        ),
        (
            p.method,
            p.kind,
            body,
            p.content_type,
            p.referer,
            p.retry_post,
            p.proxy,
        ),
        policy_py(p.policy),
    )
}

struct Engine {
    core: CoreCrawler,
    /// Where tracked Item fields keep fingerprints, and the threshold.
    tracks: Mutex<Option<(StoreRef, f64)>>,
    /// Requests whose events Python has but hasn't finished with.
    unacked: Mutex<Vec<u64>>,
    /// One permit per core: how many rule pages are parsed at once.
    parsers: Arc<tokio::sync::Semaphore>,
    rules: Mutex<Arc<Vec<Arc<Rule>>>>,
    tags: Mutex<HashMap<u64, Tag>>,
    next_id: AtomicU64,
    base: Query,
    /// Whether the pages it parses as XML are read in recover mode.
    xml_recover: bool,
    /// How long parsing one page may take (Settings.parse_timeout).
    parse_budget: Duration,
    /// The call steps of pages in Chrome, for Python to run.
    calls: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<netweir_core::StepCall>>,
}

/// The URLs in one part of a navigation item.
fn navigation_links(found: &auto::Extracted, part: Part) -> Vec<String> {
    use serde_json::Value;
    let field = match part {
        Part::Items => "items",
        Part::NextPage => "next_page",
        Part::SubCategories => "sub_categories",
    };
    let url = |v: &Value| v.get("url").and_then(Value::as_str).map(str::to_string);
    match found
        .fields
        .iter()
        .find(|(f, _)| *f == field)
        .map(|(_, v)| v)
    {
        Some(Value::Array(list)) => list.iter().filter_map(url).collect(),
        Some(v @ Value::Object(_)) => url(v).into_iter().collect(),
        _ => Vec::new(),
    }
}

impl Engine {
    fn tags(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Tag>> {
        self.tags.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Queues the links a rule page gave, tagged with the rule that found
    /// each.
    fn submit_links(&self, rules: &[Arc<Rule>], links: Vec<(String, usize)>, depth: u32) {
        for (url, at) in links {
            let rule = &rules[at];
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            // Tagged first: the page could come back before submit returns.
            self.tags().insert(
                id,
                Tag {
                    rule: Some(at),
                    apply_rules: rule.follow,
                    to_python: rule.to_python,
                    url: url.clone(),
                    depth,
                },
            );
            let request = CrawlRequest {
                id,
                url,
                priority: rule.priority,
                headers: Vec::new(),
                dont_filter: false,
                depth,
                browser: rule.browser,
                outgoing: netweir_core::Outgoing::navigate(),
                retry_post: false,
                proxy: None,
                delta_key: Default::default(),
                cookies: Default::default(),
                steps: rule.steps.clone().map(Into::into),
            };
            let payload = format!(r#"{{"rule":{at}}}"#);
            if self.core.submit_with(request, &payload) != Submitted::Queued {
                self.tags().remove(&id);
            }
        }
    }

    /// Queues again a request rule `at` made, saved in the checkpoint or
    /// leased from the shared queue under `row`.
    fn resubmit_ruled(&self, rules: &[Arc<Rule>], at: usize, p: Pending, row: i64) {
        let rule = &rules[at];
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.tags().insert(
            id,
            Tag {
                rule: Some(at),
                apply_rules: rule.follow,
                to_python: rule.to_python,
                url: p.url.clone(),
                depth: p.depth,
            },
        );
        let request = CrawlRequest {
            id,
            url: p.url,
            priority: p.priority,
            headers: p.headers,
            dont_filter: p.dont_filter,
            depth: p.depth,
            browser: rule.browser,
            outgoing: netweir_core::Outgoing::navigate(),
            retry_post: false,
            proxy: p.proxy,
            delta_key: Default::default(),
            cookies: Default::default(),
            steps: rule.steps.clone().map(Into::into),
        };
        if self.core.resubmit(request, row) != Submitted::Queued {
            self.tags().remove(&id);
        }
    }

    /// Parses a page and runs the rules on it. Runs on a blocking thread.
    fn process(
        &self,
        rules: &[Arc<Rule>],
        id: u64,
        tag: Tag,
        response: netweir_core::Response,
    ) -> Processed {
        let mut done = Processed {
            id,
            tag,
            response: None,
            root: None,
            links: Vec::new(),
            item: None,
            here: Vec::new(),
            notes: Vec::new(),
            html: None,
            error: None,
            ignored: not_a_page(&response),
            repaired: None,
        };
        // XML is parsed here only when the rules need it. Otherwise the tree
        // is built when the callback first reads it (page.root, a query),
        // and a feed read node by node is never built whole.
        let ct = response.header("content-type");
        let sniffed = ct.is_none().then(|| response.text());
        let markup = crate::Markup::of(ct, sniffed.as_deref().unwrap_or(""));
        let rules_need =
            done.tag.apply_rules || done.tag.rule.is_some_and(|r| rules[r].item.is_some());
        let parse_here = markup == crate::Markup::Html || rules_need;
        if done.ignored.is_none() && parse_here {
            let text = sniffed.unwrap_or_else(|| response.text());
            match crate::parse_markup(&text, markup, self.parse_budget, self.xml_recover) {
                Ok((doc, repairs)) => {
                    if !repairs.is_empty() {
                        done.repaired = Some(format!("{}: {}", response.url, repairs.summary()));
                    }
                    let root = doc.root();
                    if done.tag.apply_rules {
                        match self.links(rules, &response.url, root) {
                            Ok(links) => done.links = links,
                            Err(e) => done.error = Some(e),
                        }
                    }
                    if let Some(item) = done.tag.rule.and_then(|r| rules[r].item.as_ref()) {
                        let page = Url::parse(&response.url).ok();
                        let base = self.page_base(&response.url, root);
                        let tracks = self
                            .tracks
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clone();
                        match tracks {
                            Some((tracks, threshold)) => {
                                let site = Url::parse(&response.url)
                                    .ok()
                                    .and_then(|u| u.host_str().map(str::to_string))
                                    .unwrap_or_default();
                                let ctx = TrackContext {
                                    tracks: &tracks,
                                    site: &site,
                                    threshold,
                                };
                                let (values, notes) =
                                    item.extract_tracked(root, page.as_ref(), base.as_ref(), &ctx);
                                done.item = Some(values);
                                if !notes.is_empty() {
                                    done.html = Some(text.clone());
                                }
                                done.notes = notes;
                            }
                            None => {
                                done.item = Some(item.extract(root, page.as_ref(), base.as_ref()))
                            }
                        }
                    }
                    // A page its own rule gave an item gives no other.
                    if done.tag.apply_rules && done.item.is_none() {
                        done.here = self.here(rules, &response.url, root);
                    }
                    if done.tag.to_python {
                        done.root = Some(Arc::new(doc));
                    }
                }
                Err(e @ crate::ParseFailure::Timeout(_)) => {
                    done.error = Some(e.message());
                    // Python would parse it again, without the budget.
                    done.tag.to_python = false;
                }
                Err(e @ crate::ParseFailure::Xml(_)) => {
                    // A callback still gets the page, and reading its
                    // document there raises the error, with what's wrong
                    // and where; it can read the text instead. Rules
                    // can't run on it, which is worth saying here.
                    if done.tag.apply_rules || done.tag.rule.is_some() {
                        done.error = Some(e.message());
                    }
                }
            }
        }
        if done.tag.to_python {
            done.response = Some(response);
        }
        done
    }

    /// The items "here" rules take from the page the rules run on: each
    /// rule's, when the page is of the rule's type (or it names none).
    fn here(
        &self,
        rules: &[Arc<Rule>],
        page_url: &str,
        root: netweir_dom::Node<'_>,
    ) -> Vec<(usize, Vec<Value>)> {
        let page = Url::parse(page_url).ok();
        let base = self.page_base(page_url, root);
        let mut kind: Option<Option<auto::Schema>> = None;
        let mut out = Vec::new();
        for (at, rule) in rules.iter().enumerate() {
            let (Links::Here, Some(item)) = (&rule.links, &rule.item) else {
                continue;
            };
            if let Some(when) = rule.when {
                let k = *kind.get_or_insert_with(|| {
                    auto::page_type(root, page.as_ref(), base.as_ref(), date::now_utc()).0
                });
                if k != Some(when) {
                    continue;
                }
            }
            out.push((at, item.extract(root, page.as_ref(), base.as_ref())));
        }
        out
    }

    /// What a page's relative links are relative to: its `<base href>`
    /// against its URL, or its URL, as a browser resolves them.
    fn page_base(&self, page_url: &str, root: netweir_dom::Node<'_>) -> Option<Url> {
        let page = Url::parse(page_url).ok()?;
        Some(
            self.base
                .strings(root)
                .into_iter()
                .next()
                .and_then(|b| page.join(b.trim()).ok())
                .unwrap_or(page),
        )
    }

    /// Every rule's links on the page, made absolute against its
    /// `<base href>` and URL, as a browser resolves them.
    fn links(
        &self,
        rules: &[Arc<Rule>],
        page_url: &str,
        root: netweir_dom::Node<'_>,
    ) -> Result<Vec<(String, usize)>, String> {
        let Some(base) = self.page_base(page_url, root) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        // One request per page per link, however many times it appears.
        let mut seen = std::collections::HashSet::new();
        // Each navigation type is read once per page, however many rules
        // take its parts.
        let mut navigation: HashMap<auto::Schema, auto::Extracted> = HashMap::new();
        let page = Url::parse(page_url).ok();
        for (at, rule) in rules.iter().enumerate() {
            let hrefs = match &rule.links {
                Links::Query(selector) => extract::links(selector, root)?,
                Links::Here => continue,
                Links::Navigation(schema, part) => {
                    let found = navigation.entry(*schema).or_insert_with(|| {
                        auto::extract(*schema, root, page.as_ref(), Some(&base), date::now_utc())
                    });
                    navigation_links(found, *part)
                }
            };
            for href in hrefs {
                if let Ok(mut url) = base.join(href.trim()) {
                    url.set_fragment(None);
                    if !rule.takes(&url) {
                        continue;
                    }
                    let url: String = url.into();
                    if seen.insert(url.clone()) {
                        out.push((url, at));
                    }
                }
            }
        }
        Ok(out)
    }

    /// The next batch of events Python needs to see. Pages that only rules
    /// care about are handled on the way, so an empty batch still means the
    /// crawl is over.
    async fn next(self: Arc<Self>, max: usize) -> Vec<Out> {
        loop {
            let events = self.core.next_owned(max).await;
            if events.is_empty() {
                return Vec::new();
            }
            let rules = self.rules.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let ids: Vec<u64> = events
                .iter()
                .filter_map(|e| match e {
                    Event::Fetched { id, .. }
                    | Event::Failed { id, .. }
                    | Event::Dropped { id, .. }
                    | Event::Blocked { id, .. } => Some(*id),
                    Event::Paused { .. }
                    | Event::Warning { .. }
                    | Event::Leased { .. }
                    | Event::Fatal { .. } => None,
                })
                .collect();
            let mut out = Vec::new();
            let mut work = Vec::new();
            for event in events {
                match event {
                    // A rules page's Chrome page closes here: rules read the
                    // HTML, not the live page.
                    Event::Fetched { id, response, page } => match self.tags().remove(&id) {
                        None => out.push(Out::Fetched {
                            id,
                            response,
                            root: None,
                            page,
                        }),
                        Some(tag) => {
                            let (engine, rules) = (self.clone(), rules.clone());
                            let job_tag = tag.clone();
                            let parsers = self.parsers.clone();
                            // At most one page per core is being parsed.
                            work.push((
                                id,
                                job_tag,
                                tokio::spawn(async move {
                                    let _turn = parsers.acquire_owned().await;
                                    tokio::task::spawn_blocking(move || {
                                        engine.process(&rules, id, tag, response)
                                    })
                                    .await
                                }),
                            ));
                        }
                    },
                    Event::Failed { id, error } => match self.tags().remove(&id) {
                        Some(Tag {
                            rule: Some(rule),
                            url,
                            ..
                        }) => out.push(Out::RuleFailed { rule, url, error }),
                        _ => out.push(Out::Failed { id, error }),
                    },
                    Event::Dropped { id, reason } => {
                        let why = match reason {
                            DropReason::Robots => "robots",
                            DropReason::TdmReserved => "tdm",
                            DropReason::Duplicate => "duplicate",
                        };
                        match self.tags().remove(&id) {
                            Some(Tag {
                                rule: Some(rule),
                                url,
                                ..
                            }) => out.push(Out::RuleDropped { rule, url, why }),
                            _ => out.push(Out::Dropped { id, why }),
                        }
                    }
                    Event::Blocked {
                        id,
                        vendor,
                        kind,
                        response,
                    } => {
                        let tag = self.tags().remove(&id);
                        out.push(Out::Blocked {
                            id,
                            rule: tag.as_ref().and_then(|t| t.rule),
                            url: tag.map_or_else(|| response.url.clone(), |t| t.url),
                            vendor,
                            kind: kind.as_str(),
                            response,
                        });
                    }
                    Event::Paused { host, pause } => out.push(Out::Paused {
                        host,
                        seconds: pause.as_secs_f64(),
                    }),
                    Event::Warning { message } => out.push(Out::Warning { message }),
                    Event::Fatal { message } => out.push(Out::Fatal { message }),
                    // A rule's request is the rules' again here; Python's
                    // goes back to Python to be made a Request.
                    Event::Leased { row, mut request } => match rule_of(&rules, &request) {
                        Some(at) => self.resubmit_ruled(&rules, at, *request, row),
                        None => {
                            request.row = row;
                            out.push(Out::Leased { request });
                        }
                    },
                }
            }
            for (id, tag, job) in work {
                let done = match job.await {
                    Ok(Ok(done)) => done,
                    Ok(Err(e)) | Err(e) => {
                        out.push(Out::PageError {
                            url: tag.url,
                            message: format!("processing the page failed: {e}"),
                        });
                        // Python still hears the last of its own requests.
                        if tag.rule.is_none() {
                            out.push(Out::Handled { id });
                        }
                        continue;
                    }
                };
                self.submit_links(&rules, done.links, done.tag.depth + 1);
                if let Some(message) = done.error {
                    out.push(Out::PageError {
                        url: done.tag.url.clone(),
                        message,
                    });
                }
                if let Some(message) = done.repaired {
                    out.push(Out::Warning { message });
                }
                if let Some(reason) = done.ignored {
                    out.push(Out::Ignored {
                        url: done.tag.url.clone(),
                        reason,
                    });
                }
                if let (Some(values), Some(rule)) = (done.item, done.tag.rule) {
                    out.push(Out::Item {
                        rule,
                        url: done.tag.url.clone(),
                        values,
                        notes: done.notes,
                        html: done.html,
                    });
                }
                for (rule, values) in done.here {
                    out.push(Out::Item {
                        rule,
                        url: done.tag.url.clone(),
                        values,
                        notes: Vec::new(),
                        html: None,
                    });
                }
                match (done.tag.rule, done.response) {
                    (Some(rule), Some(response)) => out.push(Out::Ruled {
                        rule,
                        url: done.tag.url,
                        response,
                        root: done.root,
                        depth: done.tag.depth,
                    }),
                    (None, Some(response)) => out.push(Out::Fetched {
                        id: done.id,
                        response,
                        root: done.root,
                        page: None,
                    }),
                    (None, None) => out.push(Out::Handled { id: done.id }),
                    (Some(_), None) => {}
                }
            }
            if out.is_empty() {
                // Only rule pages, whose links are saved by now: done.
                self.core.done(&ids);
            } else {
                // Done once Python says it has dealt with the batch.
                self.unacked
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend(ids);
                return out;
            }
        }
    }
}

/// A call step for Python to run: `id` is its request's, `index` the
/// step's place among its steps (from 0), `method` the spider method it
/// names, or None, `page` the live page and `timeout` the seconds it may
/// take. Answered once, with `reply(json)` or `fail(message)`; one let go
/// of unanswered fails the step.
#[pyclass(frozen, module = "netweir")]
pub struct StepCall {
    #[pyo3(get)]
    id: u64,
    #[pyo3(get)]
    index: usize,
    #[pyo3(get)]
    method: Option<String>,
    #[pyo3(get)]
    timeout: f64,
    /// The request's URL and depth, and for a rule's request the rule.
    #[pyo3(get)]
    url: String,
    #[pyo3(get)]
    depth: u32,
    #[pyo3(get)]
    rule: Option<usize>,
    page: netweir_core::LivePage,
    reply: Mutex<Option<tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>>>,
}

impl StepCall {
    fn of(call: netweir_core::StepCall, rule: Option<usize>) -> StepCall {
        StepCall {
            id: call.id,
            index: call.index,
            method: call.method,
            timeout: call.timeout.as_secs_f64(),
            url: call.url,
            depth: call.depth,
            rule,
            page: call.page,
            reply: Mutex::new(Some(call.reply)),
        }
    }
}

impl StepCall {
    fn answer(&self, answer: Result<serde_json::Value, String>) -> PyResult<()> {
        let reply = self.reply.lock().unwrap_or_else(|e| e.into_inner()).take();
        match reply {
            // The step may have given up waiting: nothing to tell.
            Some(reply) => {
                let _ = reply.send(answer);
                Ok(())
            }
            None => Err(PyValueError::new_err("the call step was already answered")),
        }
    }
}

#[pymethods]
impl StepCall {
    /// The page, as the callback's `page.browser` would be.
    #[getter]
    fn page(&self) -> BrowserPage {
        BrowserPage::from_crawl(self.page.clone())
    }

    /// The step went well, and gave `json` (JSON text).
    fn reply(&self, json: &str) -> PyResult<()> {
        let value = serde_json::from_str(json)
            .map_err(|e| PyValueError::new_err(format!("not JSON: {e}")))?;
        self.answer(Ok(value))
    }

    /// The step failed, for `message`.
    fn fail(&self, message: String) -> PyResult<()> {
        self.answer(Err(message))
    }
}

/// One crawl: per-host queues, robots.txt and TDMRep checks, throttling and
/// deduplication, all in Rust. Python submits requests by id and awaits
/// `next()` for what happened to them.
#[pyclass(frozen, module = "netweir")]
pub struct Crawler {
    engine: Arc<Engine>,
}

fn tuple<'py>(py: Python<'py>, items: Vec<Bound<'py, PyAny>>) -> PyResult<Bound<'py, PyAny>> {
    Ok(PyTuple::new(py, items)?.into_any())
}

fn root_py<'py>(py: Python<'py>, root: Option<Arc<Document>>) -> PyResult<Bound<'py, PyAny>> {
    Ok(match root {
        Some(doc) => Py::new(py, Node::root_of(doc))?.into_bound(py).into_any(),
        None => py.None().into_bound(py),
    })
}

fn out_py<'py>(py: Python<'py>, engine: &Engine, event: Out) -> PyResult<Bound<'py, PyAny>> {
    let s = |v: &str| -> PyResult<Bound<'py, PyAny>> { Ok(v.into_pyobject(py)?.into_any()) };
    let n = |v: u64| -> PyResult<Bound<'py, PyAny>> { Ok(v.into_pyobject(py)?.into_any()) };
    let response = |r| -> PyResult<Bound<'py, PyAny>> {
        Ok(Py::new(py, Response::from(r))?.into_bound(py).into_any())
    };
    match event {
        Out::Fetched {
            id,
            response: r,
            root,
            page,
        } => {
            let page = match page {
                Some(live) => Py::new(py, BrowserPage::from_crawl(live))?
                    .into_bound(py)
                    .into_any(),
                None => py.None().into_bound(py),
            };
            tuple(
                py,
                vec![
                    s("fetched")?,
                    n(id)?,
                    response(r)?,
                    root_py(py, root)?,
                    page,
                ],
            )
        }
        Out::Warning { message } => tuple(py, vec![s("warning")?, s(&message)?]),
        Out::Failed { id, error } => tuple(
            py,
            vec![
                s("failed")?,
                n(id)?,
                fetch_error_value(py, &error)?.into_bound(py),
            ],
        ),
        Out::Dropped { id, why } => tuple(py, vec![s("dropped")?, n(id)?, s(why)?]),
        Out::Handled { id } => tuple(py, vec![s("handled")?, n(id)?]),
        Out::Item {
            rule,
            url,
            values,
            notes,
            html,
        } => {
            let rules = engine
                .rules
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let item = rules[rule]
                .item
                .as_ref()
                .expect("an item event has an item spec");
            let (dict, invalid) = item.to_python(py, values)?;
            tuple(
                py,
                vec![
                    s("item")?,
                    n(rule as u64)?,
                    dict.into_any(),
                    invalid.into_pyobject(py)?.into_any(),
                    s(&url)?,
                    notes.into_pyobject(py)?.into_any(),
                    html.into_pyobject(py)?.into_any(),
                ],
            )
        }
        Out::Ruled {
            rule,
            url,
            response: r,
            root,
            depth,
        } => tuple(
            py,
            vec![
                s("ruled")?,
                n(rule as u64)?,
                s(&url)?,
                response(r)?,
                root_py(py, root)?,
                n(depth.into())?,
            ],
        ),
        Out::RuleFailed { rule, url, error } => tuple(
            py,
            vec![
                s("rule_failed")?,
                n(rule as u64)?,
                s(&url)?,
                fetch_error_value(py, &error)?.into_bound(py),
            ],
        ),
        Out::RuleDropped { rule, url, why } => tuple(
            py,
            vec![s("rule_dropped")?, n(rule as u64)?, s(&url)?, s(why)?],
        ),
        Out::Ignored { url, reason } => tuple(py, vec![s("ignored")?, s(&url)?, s(&reason)?]),
        Out::Blocked {
            id,
            rule,
            url,
            vendor,
            kind,
            response: r,
        } => {
            let rule = match rule {
                Some(r) => n(r as u64)?,
                None => py.None().into_bound(py),
            };
            tuple(
                py,
                vec![
                    s("blocked")?,
                    n(id)?,
                    rule,
                    s(&url)?,
                    s(&vendor)?,
                    s(kind)?,
                    response(r)?,
                ],
            )
        }
        Out::Paused { host, seconds } => tuple(
            py,
            vec![
                s("paused")?,
                s(&host)?,
                seconds.into_pyobject(py)?.into_any(),
            ],
        ),
        Out::PageError { url, message } => {
            tuple(py, vec![s("page_error")?, s(&url)?, s(&message)?])
        }
        Out::Leased { request } => {
            let (head, sent, policy) = pending_py(py, *request);
            tuple(
                py,
                vec![
                    s("leased")?,
                    head.into_pyobject(py)?.into_any(),
                    sent.into_pyobject(py)?.into_any(),
                    policy.into_pyobject(py)?.into_any(),
                ],
            )
        }
        Out::Fatal { message } => tuple(py, vec![s("fatal")?, s(&message)?]),
    }
}

#[pymethods]
impl Crawler {
    #[new]
    #[pyo3(signature = (
        profile="chrome", proxy=None, timeout=30.0, verify=true,
        concurrency=64, per_domain=8, obey_robots=true, robots_agent="netweir", obey_tdmrep=true,
        throttle=true, start_delay=1.0, min_delay=0.0, max_delay=60.0, target_concurrency=1.0,
        max_depth=None, max_pages_per_domain=None, max_pages=None,
        retries=3, backoff_base=1.0, backoff_max=60.0, proxies=None,
        proxy_rotation="on_block", proxy_failures=3, proxy_cooldown=60.0,
        breaker_window=50, breaker_ratio=0.3, breaker_pause=300.0, checkpoint=None,
        browser="off", browser_pages=4, browser_block=Vec::new(), max_response_size=None, allowed_domains=None,
        cache=None, cache_expiry=None, deltafetch=None, deltafetch_reset=false,
        warc=None, har=None, queue=None, queue_key=None, queue_lease=60.0, queue_idle=30.0,
        ca_certificates=None, max_redirects=netweir_core::MAX_REDIRECTS, retry_http_codes=None,
        http_auth=None, saved_logins=Vec::new(),
        xml_recover=false, parse_timeout=Some(10.0),
        browser_environment=None, browser_worker_header_order=false, browser_steps=None,
        browser_steps_max=Some(120.0), browser_engine="chrome",
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        profile: &str,
        proxy: Option<String>,
        timeout: f64,
        verify: bool,
        concurrency: usize,
        per_domain: usize,
        obey_robots: bool,
        robots_agent: &str,
        obey_tdmrep: bool,
        throttle: bool,
        start_delay: f64,
        min_delay: f64,
        max_delay: f64,
        target_concurrency: f64,
        max_depth: Option<u32>,
        max_pages_per_domain: Option<u64>,
        max_pages: Option<u64>,
        retries: u32,
        backoff_base: f64,
        backoff_max: f64,
        proxies: Option<Vec<String>>,
        proxy_rotation: &str,
        proxy_failures: u32,
        proxy_cooldown: f64,
        breaker_window: usize,
        breaker_ratio: f64,
        breaker_pause: f64,
        checkpoint: Option<std::path::PathBuf>,
        browser: &str,
        browser_pages: usize,
        browser_block: Vec<String>,
        max_response_size: Option<u64>,
        allowed_domains: Option<Vec<String>>,
        cache: Option<std::path::PathBuf>,
        cache_expiry: Option<f64>,
        deltafetch: Option<std::path::PathBuf>,
        deltafetch_reset: bool,
        warc: Option<std::path::PathBuf>,
        har: Option<std::path::PathBuf>,
        queue: Option<String>,
        queue_key: Option<String>,
        queue_lease: f64,
        queue_idle: Option<f64>,
        ca_certificates: Option<Vec<u8>>,
        max_redirects: u32,
        retry_http_codes: Option<Vec<u16>>,
        http_auth: Option<AuthPy>,
        saved_logins: Vec<AuthPy>,
        xml_recover: bool,
        parse_timeout: Option<f64>,
        browser_environment: Option<Bound<'_, pyo3::types::PyDict>>,
        browser_worker_header_order: bool,
        browser_steps: Option<&str>,
        browser_steps_max: Option<f64>,
        browser_engine: &str,
        py: Python<'_>,
    ) -> PyResult<Crawler> {
        let environment = crate::environment::environment_from(browser_environment.as_ref())?;
        if http_auth.as_ref().is_some_and(|(_, _, d)| d.is_none()) {
            return Err(PyValueError::new_err(
                "http_auth needs a domain to send it to: a login sent everywhere could \
                 reach any site a link or redirect leads to",
            ));
        }
        let allowed_domains = netweir_core::allowed_domains(&allowed_domains.unwrap_or_default())
            .map_err(PyValueError::new_err)?;
        let browser = match browser {
            "off" => BrowserMode::Off,
            "on_block" => BrowserMode::OnBlock,
            "always" => BrowserMode::Always,
            _ => {
                return Err(PyValueError::new_err(format!(
                    "browser must be \"off\", \"on_block\" or \"always\", not {browser:?}"
                )));
            }
        };
        let proxy_rotation = match proxy_rotation {
            "on_block" => ProxyRotation::OnBlock,
            "per_request" => ProxyRotation::PerRequest,
            "per_host" => ProxyRotation::PerHost,
            _ => {
                return Err(PyValueError::new_err(format!(
                    "proxy_rotation must be \"on_block\", \"per_request\" or \"per_host\", \
                     not {proxy_rotation:?}"
                )));
            }
        };
        if browser_pages == 0 {
            return Err(PyValueError::new_err("browser_pages must be at least 1"));
        }
        if !(0.0..=1.0).contains(&breaker_ratio) {
            return Err(PyValueError::new_err(
                "breaker_ratio must be between 0 and 1",
            ));
        }
        if concurrency == 0 || per_domain == 0 {
            return Err(PyValueError::new_err(
                "concurrency and per_domain must be at least 1",
            ));
        }
        if !(target_concurrency > 0.0 && target_concurrency.is_finite()) {
            return Err(PyValueError::new_err(
                "target_concurrency must be a positive number",
            ));
        }
        let (min_delay, max_delay) = (
            seconds("min_delay", min_delay)?,
            seconds("max_delay", max_delay)?,
        );
        if min_delay > max_delay {
            return Err(PyValueError::new_err("min_delay must not exceed max_delay"));
        }
        let cache_expiry = match cache_expiry {
            Some(s) if s <= 0.0 => {
                return Err(PyValueError::new_err("cache_expiry must be positive"));
            }
            Some(s) => Some(seconds("cache_expiry", s)?),
            None => None,
        };
        let mut settings = CrawlSettings {
            concurrency,
            per_domain,
            obey_robots,
            robots_agent: robots_agent.to_string(),
            obey_tdmrep,
            throttle,
            start_delay: seconds("start_delay", start_delay)?,
            min_delay,
            max_delay,
            target_concurrency,
            max_depth,
            max_pages_per_domain,
            max_pages,
            retries,
            retry_statuses: retry_http_codes
                .unwrap_or_else(|| netweir_core::RETRY_STATUSES.to_vec()),
            http_auth: policy(None, None, false, http_auth)?.auth,
            saved_logins: saved_logins
                .into_iter()
                .map(|login| Ok(policy(None, None, false, Some(login))?.auth))
                .collect::<PyResult<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect(),
            backoff_base: seconds("backoff_base", backoff_base)?,
            backoff_max: seconds("backoff_max", backoff_max)?,
            proxies: proxies.unwrap_or_default(),
            proxy_rotation,
            proxy_failures,
            proxy_cooldown: seconds("proxy_cooldown", proxy_cooldown)?,
            breaker_window,
            breaker_ratio,
            breaker_pause: seconds("breaker_pause", breaker_pause)?,
            checkpoint,
            browser,
            browser_pages,
            browser_launch: netweir_browser::LaunchOptions {
                environment,
                worker_header_order: browser_worker_header_order,
                ..Default::default()
            },
            browser_block,
            browser_steps_max: browser_steps_max
                .map(|m| positive("browser_steps_max", m))
                .transpose()?,
            browser_steps: browser_steps
                .map(crate::browser::steps_from)
                .transpose()?
                .unwrap_or_default(),
            allowed_domains,
            cache,
            cache_expiry,
            deltafetch,
            deltafetch_reset,
            warc,
            har,
        };
        let mut options = fetch_options(
            profile,
            proxy,
            timeout,
            verify,
            ca_certificates,
            max_redirects,
        )?;
        options.max_body = max_response_size;
        // "match" goes by the HTTP profile, so the browser a site sees is
        // the one the crawl's requests look like.
        let firefox = match browser_engine {
            "chrome" => false,
            "firefox" => true,
            "match" => options.profile.name.starts_with("firefox-"),
            _ => {
                return Err(PyValueError::new_err(format!(
                    "browser_engine must be \"chrome\", \"firefox\" or \"match\", not \
                     {browser_engine:?}"
                )));
            }
        };
        if firefox {
            // Asked for by any request with browser=True, whatever
            // `browser` says.
            if options.proxy.is_some() || !settings.proxies.is_empty() {
                return Err(PyValueError::new_err(
                    "Firefox can't go through a proxy in a crawl yet, so a crawl with a proxy or \
                     proxies uses browser_engine=\"chrome\"",
                ));
            }
            settings.browser_launch.engine = netweir_browser::Engine::Firefox;
        }
        // The scheduler runs on the shared runtime.
        let core = match queue {
            Some(url) => {
                let key = queue_key.unwrap_or_default();
                let lease = positive("queue_lease", queue_lease)?;
                let idle = queue_idle.map(|s| positive("queue_idle", s)).transpose()?;
                let queue = connect(py, &url, &key, lease)?;
                let _runtime = pyo3_async_runtimes::tokio::get_runtime().enter();
                CoreCrawler::distributed(options, settings, queue, idle)
            }
            None => {
                let _runtime = pyo3_async_runtimes::tokio::get_runtime().enter();
                CoreCrawler::new(options, settings)
            }
        }
        .map_err(|e| PyValueError::new_err(e.message))?;
        Ok(Crawler {
            engine: Arc::new(Engine {
                calls: tokio::sync::Mutex::new(
                    core.step_calls()
                        .expect("a new crawl's calls are there to take"),
                ),
                core,
                tracks: Mutex::new(None),
                unacked: Mutex::new(Vec::new()),
                parsers: Arc::new(tokio::sync::Semaphore::new(
                    std::thread::available_parallelism().map_or(4, |n| n.get()),
                )),
                rules: Mutex::new(Arc::new(Vec::new())),
                tags: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(RULE_IDS),
                base: Query::new("base[href]::attr(href)").expect("a valid selector"),
                xml_recover,
                parse_budget: crate::budget(parse_timeout)?,
            }),
        })
    }

    /// Adds a link rule and returns its number. `kind` is "css" or "xpath";
    /// or "navigation", with "<type>:<part>" as `query`, for the links in
    /// a part (items, next_page, sub_categories) of the page's
    /// auto-extracted navigation item; or "here", for `item` taken from
    /// each page the rules run on, of page type `when` if given.
    /// `item` is extracted from the pages it leads to; `follow` applies the
    /// rules to those pages too; `to_python` hands them to Python as
    /// "ruled" events.
    /// `allow` and `deny` are regexes for the links to keep and skip;
    /// `allow_domains` and `deny_domains` limit them by site.
    /// `browser` fetches the pages it leads to in Chrome, and `steps`
    /// (JSON) is what to do on them first, in place of the crawl's.
    #[pyo3(signature = (kind, query, item=None, follow=true, to_python=false, priority=0, allow=None, deny=None, allow_domains=None, deny_domains=None, browser=false, steps=None, when=None))]
    #[allow(clippy::too_many_arguments)]
    fn add_rule(
        &self,
        kind: &str,
        query: &str,
        item: Option<&ItemSpec>,
        follow: bool,
        to_python: bool,
        priority: i32,
        allow: Option<Vec<String>>,
        deny: Option<Vec<String>>,
        allow_domains: Option<Vec<String>>,
        deny_domains: Option<Vec<String>>,
        browser: bool,
        steps: Option<&str>,
        when: Option<&str>,
    ) -> PyResult<usize> {
        let patterns = |what: &str, list: Option<Vec<String>>| {
            list.unwrap_or_default()
                .iter()
                .map(|p| {
                    regex::Regex::new(p)
                        .map_err(|e| PyValueError::new_err(format!("{what} pattern {p:?}: {e}")))
                })
                .collect::<PyResult<Vec<_>>>()
        };
        let domains = |list: Option<Vec<String>>| {
            netweir_core::allowed_domains(&list.unwrap_or_default()).map_err(PyValueError::new_err)
        };
        let when = when
            .map(|w| {
                auto::Schema::from_name(w)
                    .ok_or_else(|| PyValueError::new_err(format!("no page type {w:?}")))
            })
            .transpose()?;
        let rule = Rule {
            links: Links::new(kind, query)?,
            when,
            item: item.map(|i| i.inner.clone()),
            follow,
            to_python,
            priority,
            allow: patterns("allow", allow)?,
            deny: patterns("deny", deny)?,
            allow_domains: domains(allow_domains)?,
            deny_domains: domains(deny_domains)?,
            browser,
            steps: steps.map(crate::browser::steps_from).transpose()?,
        };
        let mut rules = self.engine.rules.lock().unwrap_or_else(|e| e.into_inner());
        let mut next = rules.as_ref().clone();
        next.push(Arc::new(rule));
        *rules = Arc::new(next);
        Ok(rules.len() - 1)
    }

    /// Queues a request. Returns "queued", "duplicate", "invalid",
    /// "too_deep", "trap", "domain_full", "offsite", "bad_proxy" or
    /// "delta" (its page gave items before). `delta_key` is its key in
    /// that record in place of its fingerprint; with `delta` false, the
    /// record isn't asked.
    /// With `apply_rules`, the rules take links from the page; then the page
    /// reaches Python only if `to_python`, and a "handled" event says so
    /// otherwise. `method`, `kind` ("navigate", "form", "fetch" or "image"), `body`,
    /// `content_type` and `referer` say what to send, as Fetcher.send
    /// takes them. `proxy` sends it through that proxy, whatever the
    /// rotation. With `answered`, the request is checked and seen but
    /// not queued, as the caller has its response: "answered" if it
    /// passed. `timeout`, `max_redirects`, `dont_redirect` and `auth`
    /// ((user, password, domain or None)) are its policy, as
    /// Fetcher.send takes them. `jar` names its cookie jar (None for the
    /// crawl's own), `cookies` are its own, and `dont_merge_cookies`
    /// leaves the jar out.
    #[pyo3(signature = (id, url, priority=0, headers=None, dont_filter=false, apply_rules=false, to_python=true, depth=0, payload="{}", row=None, browser=false, method="GET", kind="navigate", body=None, content_type=None, referer=None, retry_post=false, proxy=None, delta_key=None, delta=true, answered=false, timeout=None, max_redirects=None, dont_redirect=false, auth=None, jar=None, cookies=None, dont_merge_cookies=false, steps=None, steps_in_fingerprint=true))]
    #[allow(clippy::too_many_arguments)]
    fn submit(
        &self,
        id: u64,
        url: String,
        priority: i32,
        headers: Option<Vec<(String, String)>>,
        dont_filter: bool,
        apply_rules: bool,
        to_python: bool,
        depth: u32,
        payload: &str,
        row: Option<i64>,
        browser: bool,
        method: &str,
        kind: &str,
        body: Option<Vec<u8>>,
        content_type: Option<String>,
        referer: Option<String>,
        retry_post: bool,
        proxy: Option<String>,
        delta_key: Option<Vec<u8>>,
        delta: bool,
        answered: bool,
        timeout: Option<f64>,
        max_redirects: Option<u32>,
        dont_redirect: bool,
        auth: Option<AuthPy>,
        jar: Option<String>,
        cookies: Option<Vec<Bound<'_, PyAny>>>,
        dont_merge_cookies: bool,
        steps: Option<&str>,
        steps_in_fingerprint: bool,
    ) -> PyResult<&'static str> {
        let steps = steps
            .map(crate::browser::steps_from)
            .transpose()?
            .map(|steps| netweir_core::RequestSteps {
                steps,
                in_fingerprint: steps_in_fingerprint,
            });
        let kind = netweir_core::Kind::parse(kind)
            .ok_or_else(|| PyValueError::new_err(format!("unknown kind of request {kind:?}")))?;
        let cookies = request_cookies(&url, jar, cookies.unwrap_or_default(), dont_merge_cookies)?;
        let mut outgoing =
            netweir_core::Outgoing::from_parts(method, kind, body, content_type, referer);
        outgoing.policy = policy(timeout, max_redirects, dont_redirect, auth)?;
        if id >= RULE_IDS {
            return Err(PyValueError::new_err("request ids must be below 2**62"));
        }
        if apply_rules && !answered {
            self.engine.tags().insert(
                id,
                Tag {
                    rule: None,
                    apply_rules,
                    to_python,
                    url: url.clone(),
                    depth,
                },
            );
        }
        let request = CrawlRequest {
            id,
            url,
            priority,
            headers: headers.unwrap_or_default(),
            dont_filter,
            depth,
            browser,
            outgoing,
            retry_post,
            proxy,
            delta_key: match (delta, delta_key) {
                (false, _) => netweir_core::DeltaKey::Off,
                (true, Some(key)) => netweir_core::DeltaKey::Custom(key),
                (true, None) => netweir_core::DeltaKey::Fingerprint,
            },
            cookies,
            steps,
        };
        let outcome = match row {
            _ if answered => self.engine.core.claim(request),
            Some(row) => self.engine.core.resubmit(request, row),
            None => self.engine.core.submit_with(request, payload),
        };
        if outcome != Submitted::Queued {
            self.engine.tags().remove(&id);
        }
        Ok(match outcome {
            Submitted::Queued => "queued",
            Submitted::Duplicate => "duplicate",
            Submitted::Invalid => "invalid",
            Submitted::TooDeep => "too_deep",
            Submitted::Trap => "trap",
            Submitted::DomainFull => "domain_full",
            Submitted::Offsite => "offsite",
            Submitted::BadProxy => "bad_proxy",
            Submitted::Delta => "delta",
            Submitted::Pushed => "pushed",
            Submitted::Answered => "answered",
            Submitted::Capped => "capped",
        })
    }

    /// Fetches a file a page links to through the crawl, outside its pages
    /// (see netweir_core's `Crawler::download`): for pipeline stages, which
    /// await it while the crawl loop waits for them. `kind` is "fetch" or
    /// "image"; `id` comes from the same count as `submit`'s. Awaits one of:
    ///
    /// - ("fetched", Response, cached)
    /// - ("failed", FetchError)
    /// - ("dropped", "robots" | "tdm")
    /// - ("blocked", vendor, kind, Response)
    /// - None, if the crawl stopped first
    #[pyo3(signature = (id, url, kind="fetch", referer=None, headers=None, proxy=None, jar=None))]
    #[allow(clippy::too_many_arguments)]
    fn download<'py>(
        &self,
        py: Python<'py>,
        id: u64,
        url: String,
        kind: &str,
        referer: Option<String>,
        headers: Option<Vec<(String, String)>>,
        proxy: Option<String>,
        jar: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let outgoing = match kind {
            "fetch" => netweir_core::Outgoing::fetch("GET", None, None, referer),
            "image" => netweir_core::Outgoing::image(referer),
            _ => {
                return Err(PyValueError::new_err(format!(
                    "a download is a \"fetch\" or an \"image\", not {kind:?}"
                )));
            }
        };
        if id >= RULE_IDS {
            return Err(PyValueError::new_err("request ids must be below 2**62"));
        }
        let request = CrawlRequest {
            id,
            url,
            priority: 0,
            headers: headers.unwrap_or_default(),
            dont_filter: true,
            depth: 0,
            browser: false,
            outgoing,
            retry_post: false,
            proxy,
            delta_key: netweir_core::DeltaKey::Off,
            cookies: netweir_core::RequestCookies {
                jar,
                ..Default::default()
            },
            steps: None,
        };
        let done = self.engine.core.download(request);
        crate::aio::future_into_py(py, async move {
            let done = done.await;
            Python::attach(|py| -> PyResult<Py<PyAny>> {
                let s =
                    |v: &str| -> PyResult<Bound<'_, PyAny>> { Ok(v.into_pyobject(py)?.into_any()) };
                let response = |r| -> PyResult<Bound<'_, PyAny>> {
                    Ok(Py::new(py, Response::from(r))?.into_bound(py).into_any())
                };
                let Some(done) = done else {
                    return Ok(py.None());
                };
                let out = match done.event {
                    Event::Fetched { response: r, .. } => tuple(
                        py,
                        vec![
                            s("fetched")?,
                            response(r)?,
                            pyo3::types::PyBool::new(py, done.cached)
                                .to_owned()
                                .into_any(),
                        ],
                    )?,
                    Event::Failed { error, .. } => tuple(
                        py,
                        vec![s("failed")?, fetch_error_value(py, &error)?.into_bound(py)],
                    )?,
                    Event::Dropped { reason, .. } => {
                        let why = match reason {
                            DropReason::Robots => "robots",
                            DropReason::TdmReserved => "tdm",
                            DropReason::Duplicate => "duplicate",
                        };
                        tuple(py, vec![s("dropped")?, s(why)?])?
                    }
                    Event::Blocked {
                        vendor,
                        kind,
                        response: r,
                        ..
                    } => tuple(
                        py,
                        vec![s("blocked")?, s(&vendor)?, s(kind.as_str())?, response(r)?],
                    )?,
                    _ => py.None().into_bound(py),
                };
                Ok(out.unbind())
            })
        })
    }

    /// Gives up a request of a "leased" event without submitting it: it's
    /// finished, as far as the shared queue is concerned.
    fn release(&self, row: i64) {
        self.engine.core.release(row);
    }

    /// Whether max_pages requests have been sent.
    fn capped(&self) -> bool {
        self.engine.core.capped()
    }

    /// Awaits up to `max` events, as tuples:
    ///
    /// - ("fetched", id, Response, root Node or None, BrowserPage or None)
    /// - ("failed", id, FetchError)
    /// - ("dropped", id, "robots" | "tdm")
    /// - ("handled", id): dealt with by the rules in Rust
    /// - ("blocked", id, rule or None, url, vendor, kind, Response)
    /// - ("item", rule, dict, [(field, text, kind)] problems, url,
    ///   [(field, "relocated" | "lost", score)] tracking notes, the page's
    ///   HTML when there are notes, else None)
    /// - ("ruled", rule, url, Response, root Node, depth)
    /// - ("rule_failed", rule, url, FetchError)
    /// - ("rule_dropped", rule, url, reason)
    /// - ("page_error", url, message)
    /// - ("paused", host, seconds)
    /// - ("warning", message)
    /// - ("leased", (row, url, ...), (method, ...)): a request of Python's
    ///   from the shared queue, as `resume()` gives them, to submit with
    ///   `row` or give up with `release(row)`
    /// - ("fatal", message): the shared queue is lost; the crawl can't go on
    ///
    /// An empty list means the crawl is finished.
    #[pyo3(signature = (max=256))]
    fn next<'py>(&self, py: Python<'py>, max: usize) -> PyResult<Bound<'py, PyAny>> {
        let engine = self.engine.clone();
        crate::aio::future_into_py(py, async move {
            let events = engine.clone().next(max).await;
            crate::aio::attach(|py| -> PyResult<Py<PyAny>> {
                let list = PyList::empty(py);
                for event in events {
                    list.append(out_py(py, &engine, event)?)?;
                }
                Ok(list.into_any().unbind())
            })
        })
    }

    /// What the checkpoint held, after requests the rules made are queued
    /// again: a dict with "pending" (Python's requests, as (row, url,
    /// priority, headers, dont_filter, depth, payload, method, kind, body,
    /// content_type, referer, retry_post, proxy)), "items" (ids of
    /// items already delivered) and "counters" (JSON, or None). None
    /// without a checkpoint. Call it once, after adding the rules.
    fn resume<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyDict>>> {
        let Some(saved) = self.engine.core.saved() else {
            return Ok(None);
        };
        let rules = self
            .engine
            .rules
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut python = Vec::new();
        for p in saved.pending {
            match rule_of(&rules, &p) {
                Some(at) => {
                    let row = p.row;
                    self.engine.resubmit_ruled(&rules, at, p, row);
                }
                None => python.push(pending_py(py, p)),
            }
        }
        let out = PyDict::new(py);
        out.set_item("pending", python)?;
        out.set_item("items", saved.items.into_iter().collect::<Vec<_>>())?;
        out.set_item("counters", saved.counters)?;
        Ok(Some(out))
    }

    /// Reads a page Python answered a request with, as the rules read a
    /// fetched page of a request with no callback: the links they find are
    /// queued, one deeper than `depth`. Returns (error, ignored, items):
    /// why the page couldn't be read, or why the rules left it alone, if
    /// either, and the "item" events of the items Extract rules took from
    /// it.
    #[allow(clippy::type_complexity)]
    fn apply_rules(
        &self,
        py: Python<'_>,
        response: &Response,
        url: String,
        depth: u32,
    ) -> PyResult<(Option<String>, Option<String>, Vec<Py<PyAny>>)> {
        let rules = self
            .engine
            .rules
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let page = url.clone();
        let tag = Tag {
            rule: None,
            apply_rules: true,
            to_python: false,
            url,
            depth,
        };
        let response = response.core().clone();
        let engine = self.engine.clone();
        let (error, ignored, here) = py.detach({
            let rules = rules.clone();
            move || {
                let done = engine.process(&rules, 0, tag, response);
                engine.submit_links(&rules, done.links, depth + 1);
                (done.error, done.ignored, done.here)
            }
        });
        let mut items = Vec::new();
        for (rule, values) in here {
            let Some(item) = rules[rule].item.as_ref() else {
                continue;
            };
            let (dict, invalid) = item.to_python(py, values)?;
            let event = (
                "item",
                rule,
                dict,
                invalid,
                page.as_str(),
                Vec::<(String, String, f64)>::new(),
                None::<String>,
            );
            items.push(event.into_pyobject(py)?.into_any().unbind());
        }
        Ok((error, ignored, items))
    }

    /// Tracked Item fields keep their fingerprints in `store`, and are
    /// relocated when they score at least `threshold`.
    fn set_tracks(&self, store: &TrackStore, threshold: f64) {
        *self.engine.tracks.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((store.tracks(), threshold));
    }

    /// Python has dealt with every event of the batches it has: their
    /// requests are done, and a resumed crawl won't repeat them.
    fn ack(&self) {
        let ids = std::mem::take(
            &mut *self
                .engine
                .unacked
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        );
        self.engine.core.done(&ids);
    }

    /// Records an item as delivered, by its stable id.
    fn item_done(&self, id: String) {
        self.engine.core.item_done(id);
    }

    /// Records items as delivered and saves Python's state (JSON) in one
    /// commit.
    fn settle(&self, items: Vec<String>, state: String) {
        self.engine.core.settle(items, state);
    }

    /// Checks a request's cookie dicts against its URL as `submit` will:
    /// TypeError or ValueError, saying which cookie and why, for one the
    /// jar won't take.
    #[staticmethod]
    fn check_cookies(url: &str, cookies: Vec<Bound<'_, PyAny>>) -> PyResult<()> {
        let url = Url::parse(url).ok();
        for c in &cookies {
            let c = crate::cookies::cookie_from(c, true)?;
            netweir_core::cookies::Jar::check(&c, url.as_ref()).map_err(PyValueError::new_err)?;
        }
        Ok(())
    }

    /// A request's 16-byte key in the deltafetch record: `delta_key`
    /// when given, else its fingerprint in the cookie jar `jar` names (as
    /// `submit` takes it). None for a URL that can't be crawled.
    #[staticmethod]
    /// `steps` (JSON) are its own steps, when they're part of its
    /// fingerprint.
    #[pyo3(signature = (method, url, body=None, delta_key=None, jar=None, steps=None))]
    fn delta_key<'py>(
        py: Python<'py>,
        method: &str,
        url: &str,
        body: Option<Vec<u8>>,
        delta_key: Option<Vec<u8>>,
        jar: Option<&str>,
        steps: Option<&str>,
    ) -> PyResult<Option<Bound<'py, PyBytes>>> {
        let steps = steps.map(crate::browser::steps_from).transpose()?;
        let key = netweir_core::deltafetch::key(method, url, body.as_deref(), delta_key.as_deref());
        Ok(key
            .map(|k| match delta_key {
                Some(_) => k,
                None => {
                    let k = netweir_core::canonical::in_jar(k, jar);
                    match &steps {
                        Some(steps) => netweir_core::canonical::with_steps(k, steps),
                        None => k,
                    }
                }
            })
            .map(|k| PyBytes::new(py, &k)))
    }

    /// Records that the pages with these keys (from `delta_key`) gave
    /// items, so this and later crawls skip them, and how long each output
    /// file (by path) is with their items in it, with a digest of its end
    /// and whether its writer has closed it. Call it once the items are on
    /// disk. Raises OSError if the write failed.
    fn produced(
        &self,
        py: Python<'_>,
        keys: Vec<Vec<u8>>,
        files: Vec<netweir_core::deltafetch::Output>,
    ) -> PyResult<()> {
        let keys: Vec<_> = keys
            .iter()
            .filter_map(|k| <[u8; 16]>::try_from(k.as_slice()).ok())
            .collect();
        let engine = self.engine.clone();
        py.detach(move || engine.core.produced(&keys, &files))
            .map_err(pyo3::exceptions::PyOSError::new_err)
    }

    /// Each output file, by path, as the last crawl recorded it in the
    /// deltafetch record: (length, digest of its end, closed cleanly).
    /// None without one.
    fn delta_files(&self) -> Option<HashMap<String, netweir_core::deltafetch::Recorded>> {
        self.engine.core.delta_files()
    }

    /// Why the checkpoint's last commit failed and whether it has stopped
    /// for good, or None if it worked; without waiting on a commit.
    fn checkpoint_failing(&self) -> Option<(String, bool)> {
        self.engine.core.checkpoint_failing()
    }

    /// Whether the deltafetch record had no pages when the crawl started;
    /// None without one.
    fn delta_was_empty(&self) -> Option<bool> {
        self.engine.core.delta_was_empty()
    }

    /// Waits until every checkpoint write so far is committed. Raises
    /// OSError if that commit failed: its writes, and those of any failed
    /// commit before it, go again with the next.
    fn flush(&self, py: Python<'_>) -> PyResult<()> {
        let engine = self.engine.clone();
        py.detach(move || engine.core.commit())
            .map_err(pyo3::exceptions::PyOSError::new_err)
    }

    /// Saves Python's counters (JSON) in the checkpoint.
    fn save_counters(&self, json: String) {
        self.engine.core.save_counters(json);
    }

    /// The next call step of a page in Chrome, once one comes: a StepCall
    /// for Python to run and answer.
    fn next_call<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let engine = self.engine.clone();
        crate::aio::future_into_py(py, async move {
            let call = engine.calls.lock().await.recv().await;
            Ok(call.map(|call| {
                let rule = engine.tags().get(&call.id).and_then(|t| t.rule);
                StepCall::of(call, rule)
            }))
        })
    }

    /// Says nothing will run call steps from now on: each fails at once.
    fn close_calls(&self) {
        self.engine.core.close_calls();
    }

    /// Closes Chrome, if the crawl started it.
    fn close_browser<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let engine = self.engine.clone();
        crate::aio::future_into_py(py, async move {
            engine.core.close_browser().await;
            Ok(())
        })
    }

    /// Opens the WARC and HAR files and starts writing what was fetched.
    /// Raises OSError if a file can't be opened, or a resumed one can't be
    /// read.
    fn start_archive(&self, py: Python<'_>) -> PyResult<()> {
        let engine = self.engine.clone();
        py.detach(move || engine.core.start_archive())
            .map_err(pyo3::exceptions::PyOSError::new_err)
    }

    /// Commits the checkpoint and closes its file, and finishes the WARC
    /// and HAR files. Raises OSError if a write failed.
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        let engine = self.engine.clone();
        py.detach(move || engine.core.close())
            .map_err(pyo3::exceptions::PyOSError::new_err)
    }

    /// Counters so far.
    fn stats<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let s = self.engine.core.stats();
        let d = PyDict::new(py);
        d.set_item("queued", s.queued)?;
        d.set_item("in_flight", s.in_flight)?;
        d.set_item("fetched", s.fetched)?;
        d.set_item("failed", s.failed)?;
        d.set_item("duplicates", s.duplicates)?;
        d.set_item("dropped_robots", s.dropped_robots)?;
        d.set_item("dropped_tdm", s.dropped_tdm)?;
        d.set_item("bytes", s.bytes)?;
        d.set_item("skipped_depth", s.skipped_depth)?;
        d.set_item("skipped_traps", s.skipped_traps)?;
        d.set_item("skipped_domain_full", s.skipped_domain_full)?;
        d.set_item("skipped_delta", s.skipped_delta)?;
        d.set_item("retries", s.retries)?;
        d.set_item("blocked", s.blocked)?;
        d.set_item("throttled", s.throttled)?;
        d.set_item("sessions_replaced", s.sessions_replaced)?;
        d.set_item("breaker_trips", s.breaker_trips)?;
        d.set_item("browser_fetches", s.browser_fetches)?;
        d.set_item("browser_unblocked", s.browser_unblocked)?;
        d.set_item("browser_steps", s.browser_steps)?;
        d.set_item("browser_steps_failed", s.browser_steps_failed)?;
        d.set_item("offsite", s.offsite)?;
        d.set_item("cache_hits", s.cache_hits)?;
        d.set_item("cache_stores", s.cache_stores)?;
        d.set_item("pages_served", s.served)?;
        d.set_item("proxies_benched", s.proxies_benched)?;
        d.set_item("archive_errors", s.archive_errors)?;
        if self.engine.core.has_checkpoint() {
            d.set_item("checkpoint_failures", s.checkpoint_failures)?;
        }
        if self.engine.core.has_queue() {
            d.set_item("pushed", s.pushed)?;
            d.set_item("leased", s.leased)?;
            d.set_item("leases_reclaimed", s.leases_reclaimed)?;
            d.set_item("queue_bad", s.queue_bad)?;
        }
        Ok(d)
    }

    /// What each proxy did: {url: {"requests", "failures", "benched"}},
    /// the crawl's proxies first, then any a request named.
    fn proxy_stats<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let out = PyDict::new(py);
        for (url, s) in self.engine.core.proxy_stats() {
            let d = PyDict::new(py);
            d.set_item("requests", s.requests)?;
            d.set_item("failures", s.failures)?;
            d.set_item("benched", s.benched)?;
            out.set_item(url, d)?;
        }
        Ok(out)
    }
}

/// A crawl's shared queue in Redis, from outside the crawl: to feed it
/// URLs, see how it stands, or clear it.
#[pyclass(frozen, module = "netweir", name = "Queue")]
pub struct QueueHandle {
    queue: Queue,
    runtime: &'static tokio::runtime::Runtime,
}

#[pymethods]
impl QueueHandle {
    /// Connects to the Redis at `url` (``redis://host:6379/0``) for the
    /// crawl `key`. Raises ValueError for a URL or key that can't work, and
    /// netweir.QueueError if the server can't be reached.
    #[new]
    fn new(py: Python<'_>, url: &str, key: &str) -> PyResult<Self> {
        // A lease is only taken by a crawl, never from here.
        let queue = connect(py, url, key, Duration::from_secs(60))?;
        Ok(QueueHandle {
            queue,
            runtime: pyo3_async_runtimes::tokio::get_runtime(),
        })
    }

    /// Queues each URL as a start request: depth 0, the spider's default
    /// callback, and through the spider's middlewares in the process that
    /// takes it. One seen before is left out unless `dont_filter`. Returns
    /// how many were queued.
    #[pyo3(signature = (urls, priority=0, dont_filter=false))]
    fn push(
        &self,
        py: Python<'_>,
        urls: Vec<String>,
        priority: i32,
        dont_filter: bool,
    ) -> PyResult<usize> {
        let mut batch = Vec::new();
        for url in urls {
            let Some(fp) = netweir_core::canonical::fingerprint("GET", &url) else {
                return Err(PyValueError::new_err(format!(
                    "not an http(s) URL: {url:?}"
                )));
            };
            batch.push((
                fp,
                Pending {
                    row: 0,
                    url,
                    priority,
                    headers: Vec::new(),
                    dont_filter,
                    depth: 0,
                    payload: r#"{"pushed": true}"#.into(),
                    method: "GET".into(),
                    kind: "navigate".into(),
                    body: None,
                    content_type: None,
                    referer: None,
                    retry_post: false,
                    proxy: None,
                    proxy_sha256: None,
                    policy: Default::default(),
                },
            ));
        }
        let queued = py
            .detach(|| self.runtime.block_on(self.queue.push(&batch)))
            .map_err(|e| queue_error(py, e))?;
        Ok(queued.into_iter().filter(|q| *q).count())
    }

    /// {"waiting", "held", "processes", "seen", "queued"}: requests
    /// waiting, leased and not finished, processes on record, fingerprints
    /// seen, and requests ever queued.
    fn status<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let s = py
            .detach(|| self.runtime.block_on(self.queue.status()))
            .map_err(|e| queue_error(py, e))?;
        let d = PyDict::new(py);
        d.set_item("waiting", s.waiting)?;
        d.set_item("held", s.held)?;
        d.set_item("processes", s.processes)?;
        d.set_item("seen", s.seen)?;
        d.set_item("queued", s.queued)?;
        Ok(d)
    }

    /// Deletes the crawl's keys: its queue, what processes hold, and its
    /// seen set, so the next crawl under the key starts afresh.
    fn clear(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.runtime.block_on(self.queue.clear()))
            .map_err(|e| queue_error(py, e))
    }
}

/// Raises ValueError if `url` isn't a queue URL the client can use; it
/// doesn't connect.
#[pyfunction]
fn _check_queue_url(url: &str) -> PyResult<()> {
    netweir_core::queue::check_url(url).map_err(PyValueError::new_err)
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Crawler>()?;
    m.add_class::<QueueHandle>()?;
    m.add_class::<StepCall>()?;
    m.add_function(wrap_pyfunction!(_check_queue_url, m)?)
}
