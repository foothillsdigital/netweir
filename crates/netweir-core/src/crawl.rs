//! The crawl engine: a queue per host, robots.txt and TDMRep checks before
//! a site's first request, per-host concurrency and adaptive delays, and
//! deduplication. The caller submits requests and takes events in batches.

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use netweir_browser::steps::{self, Report, Step};
use netweir_browser::{Browser, Cookie, LaunchOptions, WaitUntil};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use url::Url;

use crate::archive::Archive;
use crate::cache::{Cache, Policy, Redirect};
use crate::canonical::{Fingerprint, fingerprint, fingerprint_with, in_jar, with_steps};
use crate::checkpoint::{Checkpoint, Pending, Saved, proxy_sha256, saved_policy, saved_proxy};
use crate::classify::{BlockKind, Outcome, classify};
use crate::cookies::Jar;
use crate::deltafetch::{self, DeltaKey, Record};
use crate::fetch::{
    FetchError, FetchErrorKind, FetchOptions, Fetcher, Hop, Outgoing, Response, check_proxy, masked,
};
use crate::profile::Profile;
use crate::queue::{BATCH, Prepared, Queue};
use crate::robots::Robots;
use crate::tdmrep::{self, Reservation, TdmFile};
use crate::traps;

#[derive(Debug, Clone)]
pub struct CrawlSettings {
    /// Requests in flight across all sites.
    pub concurrency: usize,
    /// Requests in flight to one host.
    pub per_domain: usize,
    pub obey_robots: bool,
    /// The product token matched against robots.txt user-agent lines.
    pub robots_agent: String,
    pub obey_tdmrep: bool,
    /// Adjust each host's delay to its response times (AutoThrottle).
    pub throttle: bool,
    /// Each host's delay before its first response is timed.
    pub start_delay: Duration,
    pub min_delay: Duration,
    pub max_delay: Duration,
    /// Requests the throttle aims to have in flight to one host.
    pub target_concurrency: f64,
    /// Links followed from a start page beyond which requests are refused.
    pub max_depth: Option<u32>,
    /// Requests accepted for one host beyond which more are refused.
    pub max_pages_per_domain: Option<u64>,
    /// Requests sent, each retry among them, beyond which none is started
    /// and the crawl ends once those under way finish. What was still
    /// queued stays in the checkpoint for a later run.
    pub max_pages: Option<u64>,
    /// Further tries after a server error, a network error, throttling or
    /// a block.
    pub retries: u32,
    /// The statuses retried (Scrapy's RETRY_HTTP_CODES). A throttling
    /// status left out still slows its host down, but isn't sent again.
    pub retry_statuses: Vec<u16>,
    /// A login for every request to its domain (and subdomains), on every
    /// redirect hop there, robots.txt included; a request's own wins.
    pub http_auth: Option<crate::Auth>,
    /// Where a resumed crawl finds the passwords of saved requests' own
    /// logins, by user and domain. Sent to nothing by being here.
    pub saved_logins: Vec<crate::Auth>,
    /// Retry n waits a random time up to backoff_base * 2^n, capped at
    /// backoff_max ("full jitter").
    pub backoff_base: Duration,
    pub backoff_max: Duration,
    /// The crawl's proxies, used as `proxy_rotation` says. Empty: the
    /// fetch options' proxy, with a fresh cookie jar for each new session.
    pub proxies: Vec<String>,
    pub proxy_rotation: ProxyRotation,
    /// Failures in a row (`FetchError::proxy_at_fault`, a 407 or a block)
    /// after which a proxy is benched.
    pub proxy_failures: u32,
    /// How long a proxy is benched the first time; twice that each time
    /// after, up to `MAX_COOLDOWN`, until it gets an answer through.
    pub proxy_cooldown: Duration,
    /// The circuit breaker looks at a host's last `breaker_window`
    /// responses; if more than `breaker_ratio` of them were blocks, the
    /// host pauses for `breaker_pause`.
    pub breaker_window: usize,
    pub breaker_ratio: f64,
    pub breaker_pause: Duration,
    /// A file to keep the crawl's state in, so it resumes after a crash.
    pub checkpoint: Option<std::path::PathBuf>,
    /// Which requests go through Chrome rather than the HTTP client.
    pub browser: BrowserMode,
    /// Chrome pages open at once.
    pub browser_pages: usize,
    /// How Chrome is started, the first time a request needs it.
    pub browser_launch: LaunchOptions,
    /// Resource types ("image", "font", ...) the crawl's Chrome pages
    /// fail before they're fetched.
    pub browser_block: Vec<String>,
    /// What every page fetched in Chrome does once it has loaded, before
    /// the caller sees it, unless its request has steps of its own.
    pub browser_steps: Arc<[Step]>,
    /// The most a page's steps may take, all told; None for no limit.
    pub browser_steps_max: Option<Duration>,
    /// Domains requests may go to, with their subdomains, normalised by
    /// `allowed_domains`; empty for any. A request elsewhere is dropped
    /// unless it has `dont_filter`.
    pub allowed_domains: Vec<String>,
    /// A file to keep responses in, so a rerun is served from it rather
    /// than the site.
    pub cache: Option<std::path::PathBuf>,
    /// How long a kept response stays fresh; for good if None.
    pub cache_expiry: Option<Duration>,
    /// A file recording requests whose pages gave items, which this and
    /// later crawls skip (`Crawler::produced`).
    pub deltafetch: Option<std::path::PathBuf>,
    /// Empty that record when the crawl starts.
    pub deltafetch_reset: bool,
    /// A WARC file (gzipped if the name ends in `.gz`) to keep every
    /// request and response the crawl's HTTP client made in. `Crawler::new`
    /// only checks it; it's opened by `Crawler::start_archive`, or else
    /// when events are first read, and until then what's fetched waits.
    pub warc: Option<std::path::PathBuf>,
    /// A HAR file to keep them in, opened as `warc` is. Not the same file.
    pub har: Option<std::path::PathBuf>,
}

/// How a crawl uses its `proxies`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProxyRotation {
    /// The fetch options' proxy until a host blocks a session; then the
    /// host's new session takes the next proxy.
    #[default]
    OnBlock,
    /// Every request takes the next proxy, with the crawl's cookies.
    PerRequest,
    /// Each host keeps a proxy, and its cookies with it, until a block or
    /// the proxy being benched moves it to the next.
    PerHost,
}

/// The longest a proxy is benched for, however often it has failed.
pub const MAX_COOLDOWN: Duration = Duration::from_secs(30 * 60);

/// Proxies outside `proxies` that requests named, each counted on its own
/// in `proxy_stats`; any more are counted together under `OTHER_PROXIES`.
pub const MAX_OTHER_PROXIES: usize = 1000;

/// The `proxy_stats` row for proxies beyond `MAX_OTHER_PROXIES`.
pub const OTHER_PROXIES: &str = "other";

/// Clients through proxies requests named, kept beyond the pool's own:
/// each holds a connection pool, so a gateway named with a new login per
/// request mustn't keep one per request for the whole crawl.
const VIA_EXTRA: usize = 256;

/// How often a running crawl saves its cookie jars to the checkpoint, at
/// most. A crash loses what they took in since, which the pages fetched
/// again set again.
const JAR_SAVES: Duration = Duration::from_secs(5);

/// Clients a host keeps through proxies its requests named.
const HOST_VIA: usize = 16;

/// What `Pool::pick` found.
#[derive(Debug, PartialEq, Eq)]
enum Picked {
    Healthy(String),
    /// Back from the bench soonest, and whether to warn the pool is
    /// exhausted.
    Soonest(String, bool),
    None,
}

/// A map that keeps its `cap` most recently used entries.
struct Lru<T> {
    map: HashMap<String, (T, u64)>,
    clock: u64,
    cap: usize,
}

impl<T: Clone> Lru<T> {
    fn new(cap: usize) -> Lru<T> {
        Lru {
            map: HashMap::new(),
            clock: 0,
            cap: cap.max(1),
        }
    }

    fn get(&mut self, key: &str) -> Option<T> {
        self.clock += 1;
        let clock = self.clock;
        self.map.get_mut(key).map(|(value, used)| {
            *used = clock;
            value.clone()
        })
    }

    fn insert(&mut self, key: &str, value: T) {
        if self.map.len() >= self.cap && !self.map.contains_key(key) {
            // Known limit: a scan to find the oldest, only when full; a linked
            // list would make it O(1) if caps ever grow large.
            if let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| k.clone())
            {
                self.map.remove(&oldest);
            }
        }
        self.clock += 1;
        self.map.insert(key.to_string(), (value, self.clock));
    }

    fn clear(&mut self) {
        self.map.clear();
    }
}

impl Default for Lru<Fetcher> {
    fn default() -> Self {
        Lru::new(VIA_EXTRA)
    }
}

/// Which requests a crawl fetches in Chrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BrowserMode {
    /// Only those that ask for it.
    #[default]
    Off,
    /// Those that ask, and any still blocked after its retries, once.
    OnBlock,
    /// Every request.
    Always,
}

impl Default for CrawlSettings {
    fn default() -> Self {
        CrawlSettings {
            concurrency: 64,
            per_domain: 8,
            obey_robots: true,
            robots_agent: "netweir".to_string(),
            obey_tdmrep: true,
            throttle: true,
            start_delay: Duration::from_secs(1),
            min_delay: Duration::ZERO,
            max_delay: Duration::from_secs(60),
            target_concurrency: 1.0,
            max_depth: None,
            max_pages_per_domain: None,
            max_pages: None,
            retries: 3,
            retry_statuses: RETRY_STATUSES.to_vec(),
            http_auth: None,
            saved_logins: Vec::new(),
            backoff_base: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            proxies: Vec::new(),
            proxy_rotation: ProxyRotation::OnBlock,
            proxy_failures: 3,
            proxy_cooldown: Duration::from_secs(60),
            breaker_window: 50,
            breaker_ratio: 0.3,
            breaker_pause: Duration::from_secs(300),
            checkpoint: None,
            browser: BrowserMode::Off,
            browser_pages: 4,
            browser_launch: LaunchOptions::default(),
            browser_block: Vec::new(),
            browser_steps: Arc::default(),
            browser_steps_max: Some(Duration::from_secs(120)),
            allowed_domains: Vec::new(),
            cache: None,
            cache_expiry: None,
            deltafetch: None,
            deltafetch_reset: false,
            warc: None,
            har: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CrawlRequest {
    /// The caller's handle for this request; events carry it back.
    pub id: u64,
    pub url: String,
    /// Higher goes first among a host's waiting requests.
    pub priority: i32,
    pub headers: Vec<(String, String)>,
    /// Fetch even if an equal request was seen before.
    pub dont_filter: bool,
    /// Links followed to get here from a start page (which is 0).
    pub depth: u32,
    /// Fetch it in Chrome.
    pub browser: bool,
    /// The method, body and kind of request, and the page it comes from;
    /// typing the address by default.
    pub outgoing: Outgoing,
    /// Retry a POST or PATCH after a server error too. Off by default:
    /// the server may have acted on it, and a retry would act twice.
    pub retry_post: bool,
    /// Send it through this proxy, whatever the rotation. Over HTTP only:
    /// Chrome has one proxy for every page.
    pub proxy: Option<String>,
    /// How the deltafetch record knows the request: by its fingerprint
    /// (the default), by a key of the caller's, or not at all.
    pub delta_key: DeltaKey,
    /// Its own cookies and the jar it shares.
    pub cookies: RequestCookies,
    /// What to do on its page once Chrome has loaded it, before the
    /// caller sees it, in place of the crawl's `browser_steps`; None for
    /// those.
    pub steps: Option<RequestSteps>,
}

/// A request's own steps, and whether they're part of its fingerprint.
/// They are by default, as different steps leave a different page: the
/// duplicate filter and deltafetch then tell the same URL with other steps
/// apart.
#[derive(Debug, Clone)]
pub struct RequestSteps {
    pub steps: Arc<[Step]>,
    pub in_fingerprint: bool,
}

impl From<Arc<[Step]>> for RequestSteps {
    fn from(steps: Arc<[Step]>) -> RequestSteps {
        RequestSteps {
            steps,
            in_fingerprint: true,
        }
    }
}

/// `request`'s fingerprint: its method, URL and body, in its cookie jar,
/// and with what its own steps do when they count.
/// The fingerprint a redirect back to `request` would have: without its
/// steps, which a redirect doesn't carry.
fn first_hop(request: &CrawlRequest) -> Option<Fingerprint> {
    let o = &request.outgoing;
    fingerprint_with(&o.method, &request.url, o.body.as_deref())
        .map(|fp| in_jar(fp, request.cookies.jar.as_deref()))
}

/// Whether a redirect to `fp` ends `passed`'s request as a duplicate: the
/// crawl has seen it, and not on this request's own way there.
fn seen_elsewhere(
    seen: &HashSet<Fingerprint>,
    passed: &Option<Vec<Fingerprint>>,
    fp: Fingerprint,
) -> bool {
    passed
        .as_ref()
        .is_some_and(|passed| !passed.contains(&fp) && seen.contains(&fp))
}

fn fingerprint_of(request: &CrawlRequest) -> Option<Fingerprint> {
    let o = &request.outgoing;
    let fp = fingerprint_with(&o.method, &request.url, o.body.as_deref())
        .map(|fp| in_jar(fp, request.cookies.jar.as_deref()))?;
    Some(match &request.steps {
        Some(own) if own.in_fingerprint => with_steps(fp, &own.steps),
        _ => fp,
    })
}

/// The cookies a request goes with: Scrapy's `Request.cookies`,
/// `meta["cookiejar"]` and `meta["dont_merge_cookies"]`.
#[derive(Debug, Clone, Default)]
pub struct RequestCookies {
    /// The jar it shares with every request naming the same; None (or
    /// empty) for the crawl's own.
    pub jar: Option<String>,
    /// Put in its jar each time it's sent, as if the URL it was submitted
    /// with had set them, so a redirect to another host never carries
    /// them there. An empty domain is that URL's host's own, an empty
    /// path its directory.
    pub add: Vec<Cookie>,
    /// Leave the jar out: send `add` alone, and keep nothing the response
    /// sets.
    pub dont_merge: bool,
}

impl RequestCookies {
    /// The jar's name, if it names one.
    fn named(&self) -> Option<&str> {
        self.jar.as_deref().filter(|n| !n.is_empty())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submitted {
    Queued,
    /// An equal request was already submitted.
    Duplicate,
    /// Not an http(s) URL.
    Invalid,
    /// Deeper than `max_depth`.
    TooDeep,
    /// The path repeats itself, as crawler traps do.
    Trap,
    /// The host already has `max_pages_per_domain` requests.
    DomainFull,
    /// Outside `allowed_domains`.
    Offsite,
    /// Its proxy isn't one the client can use.
    BadProxy,
    /// Its page gave items in an earlier crawl (`deltafetch`).
    Delta,
    /// Sent to the shared queue, from which this process or another
    /// leases it (`Event::Leased`).
    Pushed,
    /// Passed every check, and seen from now on, but not queued: the
    /// caller already has its response (`claim`).
    Answered,
    /// `claim` only: `max_pages` pages have been sent or served already.
    Capped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DropReason {
    /// robots.txt disallows it.
    Robots,
    /// The site reserves text and data mining rights for it (TDMRep).
    TdmReserved,
    /// It redirected to a page the crawl had already seen.
    Duplicate,
}

#[derive(Debug, Clone)]
pub enum Event {
    Fetched {
        id: u64,
        response: Response,
        /// For a request fetched in Chrome, the page, still open.
        page: Option<LivePage>,
    },
    Failed {
        id: u64,
        error: FetchError,
    },
    Dropped {
        id: u64,
        reason: DropReason,
    },
    /// Still blocked after every retry and a new session for each.
    Blocked {
        id: u64,
        vendor: String,
        kind: BlockKind,
        response: Response,
    },
    /// The circuit breaker paused a host that kept blocking.
    Paused {
        host: String,
        pause: Duration,
    },
    /// Something the caller should hear about, once.
    Warning {
        message: String,
    },
    /// A request this process took from the shared queue. The caller
    /// queues it with `resubmit(request, row)`, or gives it up with
    /// `release(row)`; `done` releases it in the queue.
    Leased {
        row: i64,
        request: Box<Pending>,
    },
    /// The shared queue couldn't be reached for longer than a lease: the
    /// crawl can't go on.
    Fatal {
        message: String,
    },
}

/// A download's priority: ahead of every page of its host, whose
/// priorities are kept below it.
const DOWNLOAD_PRIORITY: i32 = i32::MAX;

/// How a download (`Crawler::download`) ended: its `Fetched`, `Failed`,
/// `Dropped` or `Blocked` event, and whether the response came from the
/// cache.
#[derive(Debug)]
pub struct Downloaded {
    pub event: Event,
    pub cached: bool,
}

/// A call step for the caller to run with code of its own: which
/// request's page (`id`), which of its steps (`index`, from 0), the method
/// the step names if it names one, the page itself, and how long the step
/// may take. The step waits for `reply`; one dropped unanswered fails it.
pub struct StepCall {
    pub id: u64,
    /// The request's URL and depth, for one the caller never made (a
    /// rule's).
    pub url: String,
    pub depth: u32,
    pub index: usize,
    pub method: Option<String>,
    pub page: LivePage,
    pub timeout: Duration,
    pub reply: tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>,
}

impl std::fmt::Debug for StepCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StepCall")
            .field("id", &self.id)
            .field("index", &self.index)
            .field("method", &self.method)
            .finish_non_exhaustive()
    }
}

/// A page still open in Chrome after a browser fetch. Closing it, or
/// dropping the last clone, closes the page and frees its place among
/// `browser_pages`.
#[derive(Clone)]
pub struct LivePage {
    inner: Arc<Live>,
}

struct Live {
    page: netweir_browser::Page,
    permit: Mutex<Option<OwnedSemaphorePermit>>,
    runtime: tokio::runtime::Handle,
    /// Told when the page's slot frees, so a browser request waiting for
    /// one can start.
    shared: std::sync::Weak<Shared>,
    /// The jar the page's cookies go back to when it closes, and the
    /// cookies the page started with: its request's site's.
    jar: Option<(Arc<Jar>, Seed)>,
}

/// The cookies a page started with, and the site of the request it was
/// opened for, which with the page's last URL's is all it gives back.
#[derive(Clone)]
struct Seed {
    cookies: Vec<Cookie>,
    site: Option<String>,
}

/// A cookie as Chrome and the jar both know it: its name, domain, path
/// and partition.
type CookieKey<'a> = (&'a str, &'a str, &'a str, Option<&'a str>);

fn key(c: &Cookie) -> CookieKey<'_> {
    (&c.name, &c.domain, &c.path, c.partition_key.as_deref())
}

/// Puts what changed in the page's context since it opened with `seed`
/// into `jar`, as the page closes: the cookies Chrome set or changed, and
/// the removal of those of `seed` it deleted (a logout). Only those of
/// the site the request was for, and of the page's last URL's: a cookie
/// another site set in Chrome (an ad's, say) stays there. A cookie Chrome
/// left as it was stays out, so one the jar took in over HTTP meanwhile
/// isn't put back to the value the page started with, nor is one the jar
/// renewed taken out.
async fn give_back(page: &netweir_browser::Page, jar: &Jar, seed: &Seed) {
    let Ok(now) = page.cookies().await else {
        return;
    };
    let mut sites: Vec<String> = seed.site.iter().cloned().collect();
    if let Some(last) = Url::parse(&page.url())
        .ok()
        .and_then(|u| crate::cookies::site_key(u.host_str()?))
        && !sites.contains(&last)
    {
        sites.push(last);
    }
    let ours = |c: &Cookie| crate::cookies::site_key(&c.domain).is_some_and(|k| sites.contains(&k));
    let started: HashMap<CookieKey<'_>, &Cookie> =
        seed.cookies.iter().map(|c| (key(c), c)).collect();
    let in_chrome: HashSet<CookieKey<'_>> = now.iter().map(key).collect();
    for c in now.iter().filter(|c| ours(c)) {
        let unchanged = started.get(&key(c)).is_some_and(|s| {
            s.value == c.value
                && s.secure == c.secure
                && s.http_only == c.http_only
                && s.same_site == c.same_site
                // Chrome keeps expiry times to the microsecond.
                && (s.expires - c.expires).abs() < 1.0
        });
        if !unchanged {
            let _ = jar.add(c, None);
        }
    }
    let closing = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64());
    // The jar's own copies now, of the sites the page started with.
    let held: Vec<Cookie> = seed
        .site
        .iter()
        .flat_map(|site| jar.of_site(site))
        .collect();
    let held: HashMap<CookieKey<'_>, &Cookie> = held.iter().map(|c| (key(c), c)).collect();
    // Gone from Chrome, as Chrome deleted it, and only while the jar still
    // has it as the page started: not one that ran out in Chrome, nor one
    // the jar has since had a newer copy of.
    let deleted = seed.cookies.iter().filter(|s| {
        !in_chrome.contains(&key(s))
            && !(s.expires >= 0.0 && s.expires <= closing)
            && held
                .get(&key(s))
                .is_some_and(|c| c.value == s.value && (c.expires - s.expires).abs() < 1.0)
    });
    for s in deleted {
        // An expired cookie removes the one it matches.
        let _ = jar.add(
            &Cookie {
                expires: 0.0,
                ..s.clone()
            },
            None,
        );
    }
}

impl Live {
    fn free(&self) {
        let freed = self
            .permit
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .is_some();
        if freed && let Some(shared) = self.shared.upgrade() {
            shared.schedule.notify_one();
        }
    }
}

impl LivePage {
    pub fn page(&self) -> &netweir_browser::Page {
        &self.inner.page
    }

    pub async fn close(&self) {
        if let Some((jar, seed)) = &self.inner.jar
            && !self.inner.page.is_closed()
        {
            give_back(&self.inner.page, jar, seed).await;
        }
        let _ = self.inner.page.close().await;
        self.inner.free();
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.free();
        if !self.page.is_closed() {
            let (page, jar) = (self.page.clone(), self.jar.take());
            self.runtime.spawn(async move {
                if let Some((jar, seed)) = jar {
                    give_back(&page, &jar, &seed).await;
                }
                let _ = page.close().await;
            });
        }
    }
}

/// The cookies a browser request's page starts with, and the jar they go
/// back to when it closes: its named jar's, or its host's session's (the
/// crawl's own until a block gives the host one), with the request's own
/// added; or, for a request that leaves the jar out, only the request's
/// own, which go nowhere after. Only the cookies of the request's site:
/// a page has no use for the rest of a jar that may hold thousands.
fn browser_cookies(shared: &Shared, request: &CrawlRequest) -> (Seed, Option<Arc<Jar>>) {
    let site = Url::parse(&request.url)
        .ok()
        .and_then(|u| crate::cookies::site_key(u.host_str()?));
    let host = Url::parse(&request.url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default();
    let jar = if request.cookies.dont_merge {
        None
    } else {
        Some(match request.cookies.named() {
            Some(name) => shared.jar(Some(name)),
            // Taken under the crawl's lock; the cookies read after it.
            None => session_for(shared, &shared.lock(), &request.url)
                .jar()
                .clone(),
        })
    };
    let own = Jar::new();
    let into = jar.as_deref().unwrap_or(&own);
    add_request_cookies(into, request);
    let seed = Seed {
        cookies: into.of_site(&host),
        site,
    };
    (seed, jar)
}

impl std::fmt::Debug for LivePage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LivePage({})", self.inner.page.url())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    /// Waiting in a host's queue.
    pub queued: usize,
    pub in_flight: usize,
    pub fetched: u64,
    pub failed: u64,
    pub duplicates: u64,
    pub dropped_robots: u64,
    pub dropped_tdm: u64,
    pub bytes: u64,
    pub skipped_depth: u64,
    pub skipped_traps: u64,
    pub skipped_domain_full: u64,
    /// Requests skipped because their page gave items before (`deltafetch`).
    pub skipped_delta: u64,
    pub retries: u64,
    /// Responses that were blocks, retried or not.
    pub blocked: u64,
    /// 429s, and 503s with Retry-After.
    pub throttled: u64,
    pub sessions_replaced: u64,
    pub breaker_trips: u64,
    /// Pages fetched in Chrome.
    pub browser_fetches: u64,
    /// Requests blocked over HTTP that Chrome got through.
    pub browser_unblocked: u64,
    /// Steps run on pages in Chrome, and those of them that failed,
    /// whatever their `on_error`.
    pub browser_steps: u64,
    pub browser_steps_failed: u64,
    /// Requests dropped for going outside `allowed_domains`.
    pub offsite: u64,
    /// Pages served from the cache rather than fetched.
    pub cache_hits: u64,
    /// Pages the caller answered requests with, never fetched (`claim`).
    pub served: u64,
    /// Responses written to the cache.
    pub cache_stores: u64,
    /// Times a proxy was benched.
    pub proxies_benched: u64,
    /// Responses that didn't reach the WARC or HAR file they should have,
    /// once for each file.
    pub archive_errors: u64,
    /// Requests sent to the shared queue (not counting those it already
    /// had), taken from it, and taken back from processes that died.
    pub pushed: u64,
    pub leased: u64,
    pub leases_reclaimed: u64,
    /// Members of the shared queue that weren't requests netweir could
    /// read, dropped.
    pub queue_bad: u64,
    /// Checkpoint commits that failed. Their writes go again with the
    /// next commit.
    pub checkpoint_failures: u64,
}

/// What one proxy did in a crawl.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProxyStats {
    /// Requests sent through it.
    pub requests: u64,
    /// Those that failed it (`FetchError::proxy_at_fault`, a 407 or a
    /// block).
    pub failures: u64,
    /// Times it was benched.
    pub benched: u64,
}

/// A crawl in progress. Dropping it stops the scheduler; fetches already
/// under way finish on their own.
pub struct Crawler {
    shared: Arc<Shared>,
}

struct Shared {
    fetcher: Fetcher,
    checkpoint: Option<Checkpoint>,
    /// The queue shared with the crawl's other processes, if any.
    queue: Option<Linked>,
    cache: Option<Arc<Cache>>,
    delta: Option<Record>,
    /// What the checkpoint held at the start, until the caller takes it.
    saved: Mutex<Option<Saved>>,
    /// What new sessions are made from.
    options: FetchOptions,
    settings: CrawlSettings,
    state: Mutex<State>,
    /// Woken when there may be something to schedule.
    schedule: Notify,
    /// Woken when an event arrives or the crawl may have finished.
    events: Notify,
    /// Chrome, started the first time a request needs it.
    /// A Chrome that dies is started again for the next request that
    /// needs one. One that can't start is tried again a minute on, not for
    /// every request meanwhile.
    browser: tokio::sync::Mutex<Option<Result<Browser, (String, Instant)>>>,
    /// Places for open Chrome pages.
    pages: Arc<Semaphore>,
    runtime: tokio::runtime::Handle,
    /// Where call steps go to be run, and, until the caller takes it with
    /// `step_calls`, where they arrive.
    calls: tokio::sync::mpsc::UnboundedSender<StepCall>,
    called: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<StepCall>>>,
    /// Set once whoever took the calls has stopped running them: call
    /// steps then fail at once rather than wait out their timeouts.
    calls_closed: std::sync::atomic::AtomicBool,
    /// The jars requests name (`RequestCookies::jar`), made as they're
    /// first named.
    jars: Mutex<HashMap<String, Arc<Jar>>>,
    /// Each jar's `changes` when the checkpoint last saved it, by name
    /// ("" for the crawl's own).
    /// And when it last saved them.
    jars_saved: Mutex<(HashMap<String, u64>, Option<Instant>)>,
}

impl Shared {
    /// The jar `name`d, made if it's new; the crawl's own for None or "".
    fn jar(&self, name: Option<&str>) -> Arc<Jar> {
        match name.filter(|n| !n.is_empty()) {
            None => self.fetcher.jar().clone(),
            Some(name) => self
                .jars
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(name.to_string())
                .or_default()
                .clone(),
        }
    }

    /// Hands the checkpoint every jar that changed since it last did: at
    /// most every `JAR_SAVES` while the crawl runs, as a busy crawl's jar
    /// changes with nearly every response, and always with `now`.
    fn save_jars(&self, now: bool) {
        let Some(cp) = &self.checkpoint else {
            return;
        };
        let mut guard = self.jars_saved.lock().unwrap_or_else(|e| e.into_inner());
        let (saved, at) = &mut *guard;
        if !now && at.is_some_and(|t| t.elapsed() < JAR_SAVES) {
            return;
        }
        *at = Some(Instant::now());
        let mut all = vec![(String::new(), self.fetcher.jar().clone())];
        all.extend(
            self.jars
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .map(|(k, v)| (k.clone(), v.clone())),
        );
        for (name, jar) in all {
            let changes = jar.changes();
            if saved.get(&name) != Some(&changes) {
                cp.jar(name.clone(), jar.snapshot());
                saved.insert(name, changes);
            }
        }
        // And each host's own session's, kept apart from the named ones.
        // With the identity each goes out with, its cookies' other half:
        // only the sessions are taken under the crawl's lock, and written
        // after it's let go.
        let hosts: Vec<(String, Arc<Jar>, FetchOptions)> = self
            .lock()
            .hosts
            .iter()
            .filter_map(|(host, h)| {
                let options = h.options.clone().unwrap_or_else(|| self.options.clone());
                Some((host.clone(), h.session.as_ref()?.jar().clone(), options))
            })
            .collect();
        for (host, jar, options) in hosts {
            let key = host_key(&host);
            // A session with no cookies, never saved with any, has nothing
            // to keep: per_host gives every site one.
            if jar.is_empty() && !saved.contains_key(&key) {
                continue;
            }
            let Ok(identity) = serde_json::to_string(&Identity::of(&options)) else {
                continue;
            };
            let now = host_saved(&jar, &identity);
            if saved.get(&key) != Some(&now) {
                cp.host_jar(host, jar.snapshot(), identity);
                saved.insert(key, now);
            }
        }
    }

    /// The fetcher `request` goes out with, from its route's `session`:
    /// with its named jar, or with a jar of its own cookies alone when it
    /// leaves the jar out. Its own cookies go into that jar first: on its
    /// first hop, or on every hop for a jar of its own, which keeps
    /// nothing between them. A later hop's jar has them from the first,
    /// and whatever the site set since.
    fn with_request_jar(&self, session: Fetcher, request: &CrawlRequest, hops: usize) -> Fetcher {
        let cookies = &request.cookies;
        let session = if cookies.dont_merge {
            session.with_jar(Arc::new(Jar::new()))
        } else {
            match cookies.named() {
                Some(name) => session.with_jar(self.jar(Some(name))),
                None => session,
            }
        };
        if hops == 0 || cookies.dont_merge {
            add_request_cookies(session.jar(), request);
        }
        session
    }
}

/// The key a host's own session's jar has among the saved jars' changes:
/// one no jar's name can be, as names are never empty.
fn host_key(host: &str) -> String {
    format!("\0host {host}")
}

/// The identity a host's session goes out with, as the checkpoint keeps
/// it beside the session's jar: its profile's name and its proxy. A
/// site's clearance cookies are bound to both.
#[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
struct Identity {
    profile: String,
    /// The proxy, never with its login: as `masked` writes it, for
    /// messages, and the SHA-256 of its URL, which a resume finds it
    /// among the crawl's proxies by.
    proxy: Option<SavedProxy>,
}

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
struct SavedProxy {
    masked: String,
    sha256: String,
}

impl SavedProxy {
    fn of(url: &str) -> SavedProxy {
        SavedProxy {
            masked: masked(url),
            sha256: proxy_sha256(url),
        }
    }
}

impl Identity {
    fn of(options: &FetchOptions) -> Identity {
        Identity {
            profile: options.profile.name.clone(),
            proxy: options.proxy.as_deref().map(SavedProxy::of),
        }
    }
}

/// Gives `host` back the session it had before the crawl stopped, with
/// its cookies, as the same visitor: the same profile and proxy, or none
/// at all. Why not, if it can't: the profile isn't one this build knows,
/// or the proxy is no longer one the crawl goes through. With
/// `per_request` too: a session Chrome handed back is the host's own
/// there, as in the run that saved it.
fn restore_session(
    shared: &Shared,
    state: &mut State,
    host: &str,
    jar: Arc<Jar>,
    identity: &Identity,
) -> Result<(), String> {
    let mut options = shared.options.clone();
    if identity.profile != options.profile.name {
        options.profile = Profile::named(&identity.profile)
            .map_err(|_| format!("its profile {} isn't one this build has", identity.profile))?;
    }
    // The crawl's proxies now: its pool, its own and Chrome's.
    let proxy = match &identity.proxy {
        None if shared.options.proxy.is_none() => None,
        None => return Err("it went out without a proxy, and the crawl now has one".into()),
        Some(saved) => Some(
            shared
                .settings
                .proxies
                .iter()
                .chain(&shared.options.proxy)
                .chain(&shared.settings.browser_launch.proxy)
                .find(|p| proxy_sha256(p) == saved.sha256)
                .cloned()
                .ok_or_else(|| {
                    format!("its proxy {} is no longer one the crawl uses", saved.masked)
                })?,
        ),
    };
    options.proxy = proxy.clone();
    let fetcher = Fetcher::new(options.clone()).map_err(|e| e.to_string())?;
    let pooled = proxy.filter(|p| state.proxies.pooled(p));
    let h = state
        .hosts
        .entry(host.to_string())
        .or_insert_with(|| Host::new(&shared.settings));
    h.session = Some(fetcher.with_jar(jar));
    h.proxy = pooled;
    h.options = Some(options);
    Ok(())
}

/// What the saved-changes map holds for a host's session: a hash of its
/// jar (which session), the jar's changes and its identity, so a new
/// session or identity is saved even with the count unchanged.
fn host_saved(jar: &Arc<Jar>, identity: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (Arc::as_ptr(jar) as usize, jar.changes(), identity).hash(&mut h);
    h.finish()
}

/// Puts `request`'s own cookies in `jar`, as if the URL it was submitted
/// with had set them (`enqueue` gave them its host and directory).
fn add_request_cookies(jar: &Jar, request: &CrawlRequest) {
    for c in &request.cookies.add {
        let _ = jar.add(c, None);
    }
}

#[derive(Default)]
struct State {
    seq: u64,
    /// Each saved request's row in the checkpoint, by id.
    rows: HashMap<u64, i64>,
    next_row: i64,
    seen: HashSet<Fingerprint>,
    /// Hosts outside allowed_domains that requests were dropped for, so
    /// each is warned about once.
    offsite_hosts: HashSet<String>,
    hosts: HashMap<String, Host>,
    origins: HashMap<String, Origin>,
    events: VecDeque<Event>,
    /// Hosts that may be able to start a request. The scheduler visits only
    /// these, so its work follows what changed, not how many hosts exist.
    ready: VecDeque<String>,
    /// The same, for hosts with a download first in their queue: those the
    /// scheduler visits while pages are held.
    ready_downloads: VecDeque<String>,
    /// Hosts waiting out their delay, soonest first.
    timers: BinaryHeap<Reverse<(Instant, String)>>,
    /// robots.txt and tdmrep.json fetches under way.
    gate_fetches: usize,
    /// Requests sent so far, retries among them, for `max_pages`.
    sent: u64,
    /// Requests the cache has a response for, served ahead of any host.
    hits: BinaryHeap<Queued>,
    /// Whether a hit is being read from the cache.
    serving: bool,
    stats: Stats,
    closed: bool,
    /// Whether the crawl was told no HTTP profile matches its Chrome.
    warned_profile: bool,
    /// Whether the crawl was told a blocked request couldn't go to Chrome.
    warned_browser: bool,
    /// Whether the crawl was told its cache failed.
    warned_cache: bool,
    /// Hosts whose next request waits for a free Chrome page.
    page_waiters: Vec<String>,
    proxies: Pool,
    /// The crawl's cookies through each proxy, made the first time a
    /// request goes through it without a session of its host's own. Room
    /// for every proxy of the pool and `VIA_EXTRA` that requests named.
    via: Lru<Fetcher>,
    /// The proxy Chrome was started with, which the cookies it earns are
    /// tied to.
    chrome_proxy: Option<String>,
    /// Whether the crawl was told no proxy of the pool suits Chrome.
    warned_chrome_proxy: bool,
    /// Proxies saved requests asked for whose login the crawl was told
    /// it couldn't find (see `unmask_proxy`).
    lost_proxies: HashSet<String>,
    /// Saved requests' logins already warned about, the same way.
    lost_logins: HashSet<String>,
    /// Requests leased from the shared queue and not yet released, by
    /// row, with the member that releases them.
    held: HashMap<i64, Vec<u8>>,
    /// Leased requests the caller hasn't queued or given up yet.
    transit: usize,
    /// The shared queue has been empty for its grace period: the crawl
    /// is over once this process is idle.
    queue_over: bool,
    /// Downloads under way (`Crawler::download`), by id, with where their
    /// outcome goes instead of the events.
    downloads: HashMap<u64, tokio::sync::oneshot::Sender<Downloaded>>,
}

/// The shared queue, and the way to its task.
struct Linked {
    queue: Queue,
    /// How long the queue must stay empty before the crawl ends; None to
    /// wait for more for good.
    idle: Option<Duration>,
    ops: tokio::sync::mpsc::UnboundedSender<QueueOp>,
}

/// What the queue's task sends to Redis, in the order it's given.
enum QueueOp {
    Push(Fingerprint, Box<Pending>),
    Release(i64),
    /// Give back what's still held, leave the crawl, and say so.
    Close(Option<std::sync::mpsc::Sender<()>>),
}

/// The crawl's proxies and how each is doing, in `proxies` order; then
/// any other proxy a request named, which is counted but never benched.
#[derive(Default)]
struct Pool {
    entries: Vec<PoolEntry>,
    index: HashMap<String, usize>,
    /// The first `pooled` entries are `settings.proxies`.
    pooled: usize,
    /// The entry the round robin looks at next.
    turn: usize,
    /// Whether the crawl was told every proxy is benched, since one was
    /// last healthy.
    warned_exhausted: bool,
}

struct PoolEntry {
    url: String,
    stats: ProxyStats,
    /// Failures since its last success.
    run: u32,
    /// Times benched since its last success, which doubles the cooldown.
    level: u32,
    /// Benched until then.
    until: Option<Instant>,
}

impl Pool {
    fn new(proxies: &[String]) -> Pool {
        let mut pool = Pool::default();
        for url in proxies {
            pool.push(url);
        }
        pool.pooled = pool.entries.len();
        pool
    }

    /// The entry for `url`, made if it's new. Proxies outside the pool
    /// beyond `MAX_OTHER_PROXIES` share one entry, `OTHER_PROXIES`: a
    /// rotating gateway with a session in each login would otherwise add
    /// one for every request.
    fn entry(&mut self, url: &str) -> usize {
        if let Some(&k) = self.index.get(url) {
            return k;
        }
        if self.entries.len() - self.pooled >= MAX_OTHER_PROXIES {
            return self.push(OTHER_PROXIES);
        }
        self.push(url)
    }

    fn push(&mut self, url: &str) -> usize {
        if let Some(&k) = self.index.get(url) {
            return k;
        }
        self.entries.push(PoolEntry {
            url: url.to_string(),
            stats: ProxyStats::default(),
            run: 0,
            level: 0,
            until: None,
        });
        self.index.insert(url.to_string(), self.entries.len() - 1);
        self.entries.len() - 1
    }

    fn pooled(&self, url: &str) -> bool {
        self.index.get(url).is_some_and(|&k| k < self.pooled)
    }

    /// Whether `url` is in the pool and benched now.
    fn benched(&self, url: &str, now: Instant) -> bool {
        self.index
            .get(url)
            .is_some_and(|&k| k < self.pooled && self.entries[k].benched(now))
    }

    fn any_healthy(&self, now: Instant) -> bool {
        self.entries[..self.pooled].iter().any(|e| !e.benched(now))
    }

    /// The first of the pool in turn that `usable` takes and isn't
    /// benched, and true; or, with every one it takes benched, the one
    /// back soonest, and false. None if it takes none.
    fn choose(&self, now: Instant, usable: impl Fn(&str) -> bool) -> Option<(usize, bool)> {
        let n = self.pooled;
        let usable_at = |k: usize| usable(&self.entries[k].url);
        let healthy = (0..n)
            .map(|i| (self.turn + i) % n)
            .find(|&k| usable_at(k) && !self.entries[k].benched(now));
        if let Some(k) = healthy {
            return Some((k, true));
        }
        (0..n)
            .filter(|&k| usable_at(k))
            .min_by_key(|&k| self.entries[k].until)
            .map(|k| (k, false))
    }

    /// The next healthy proxy in turn that `usable` takes, another than
    /// `avoid` if there is one; or, with every one it takes benched, the
    /// one back soonest. The second says to warn that the pool is
    /// exhausted: only when every proxy is benched, not only every one
    /// `usable` takes, and once until one is healthy again.
    fn pick(&mut self, now: Instant, avoid: Option<&str>, usable: impl Fn(&str) -> bool) -> Picked {
        let other = self
            .choose(now, |p| usable(p) && Some(p) != avoid)
            .filter(|(_, healthy)| *healthy);
        let Some((k, healthy)) = other.or_else(|| self.choose(now, &usable)) else {
            return Picked::None;
        };
        let url = self.entries[k].url.clone();
        if healthy {
            self.turn = k + 1;
            self.warned_exhausted = false;
            return Picked::Healthy(url);
        }
        let warn = !self.any_healthy(now) && !self.warned_exhausted;
        self.warned_exhausted |= warn;
        Picked::Soonest(url, warn)
    }

    /// The pool's proxy back from the bench soonest.
    fn soonest(&self) -> Option<&str> {
        self.entries[..self.pooled]
            .iter()
            .min_by_key(|e| e.until)
            .map(|e| e.url.as_str())
    }
}

impl PoolEntry {
    fn benched(&self, now: Instant) -> bool {
        self.until.is_some_and(|until| until > now)
    }
}

struct Host {
    queue: BinaryHeap<Queued>,
    in_flight: usize,
    next_at: Instant,
    delay: Duration,
    /// The robots.txt Crawl-delay, below which the delay never goes.
    floor: Duration,
    /// In `State::ready`.
    listed: bool,
    /// In `State::ready_downloads`.
    download_listed: bool,
    /// When this host's timer is set for, so it is set once.
    timer: Option<Instant>,
    /// Requests submitted for this host, for `max_pages_per_domain`.
    accepted: u64,
    /// This host's own session after a block, or from its first request
    /// with per_host rotation; until then, the crawl's.
    session: Option<Fetcher>,
    /// The proxy of `proxies` the session goes through, if it's one.
    proxy: Option<String>,
    /// What the session was made with (Chrome's profile, after a hand
    /// back), for the clients through proxies its requests name.
    options: Option<FetchOptions>,
    /// The session through proxies requests named, made as they're
    /// needed; the last `HOST_VIA` of them.
    via: Lru<Fetcher>,
    /// Whether each of the last responses was a block, newest last.
    recent: VecDeque<bool>,
    /// New connections being made for this host's requests. While one is,
    /// no other request is handed out: the one waiting for it hasn't left
    /// yet, and the delay runs from when it does.
    connecting: usize,
}

impl Host {
    fn new(settings: &CrawlSettings) -> Self {
        Host {
            queue: BinaryHeap::new(),
            in_flight: 0,
            next_at: Instant::now(),
            delay: if settings.throttle {
                settings.start_delay
            } else {
                settings.min_delay
            },
            floor: Duration::ZERO,
            listed: false,
            download_listed: false,
            timer: None,
            accepted: 0,
            session: None,
            proxy: None,
            options: None,
            via: Lru::new(HOST_VIA),
            recent: VecDeque::new(),
            connecting: 0,
        }
    }
}

#[derive(Default)]
struct Origin {
    robots: Gate<Robots>,
    tdm: Gate<Option<TdmFile>>,
    /// Hosts waiting for this origin's robots.txt or TDMRep file.
    waiting: Vec<String>,
    /// Proxies of the pool that failed to fetch this origin's robots.txt
    /// or tdmrep.json, so the next try goes through another.
    failed_proxies: Vec<String>,
    /// The proxy that next try goes through, when the host has no session
    /// of its own to move.
    retry_through: Option<String>,
}

#[derive(Default)]
enum Gate<T> {
    #[default]
    Unknown,
    Fetching,
    Ready(T),
}

struct Queued {
    priority: i32,
    seq: u64,
    request: CrawlRequest,
    host: String,
    origin: String,
    path: String,
    /// Redirects followed so far to get to `request.url`.
    hops: usize,
    /// Where the request has been: its own fingerprint, then each
    /// redirect's. A redirect back to one of them isn't a duplicate, since
    /// a site that sets a cookie often sends you back to the same page.
    /// None when no redirect may be dropped as a duplicate: a download, a
    /// `dont_filter` request, or one read back from the checkpoint, whose
    /// redirects this same request marked seen before the crawl stopped.
    passed: Option<Vec<Fingerprint>>,
    /// Tries already made.
    attempts: u32,
    /// Fetch it in Chrome.
    in_browser: bool,
    /// What the cache needs to keep its response. None if it isn't cached.
    keep: Option<Keep>,
    /// Blocked over HTTP and sent to Chrome: what the block was, for
    /// on_block if Chrome doesn't get through either.
    blocked: Option<Box<(String, BlockKind, Response)>>,
}

/// A cached request's way through to its response.
struct Keep {
    /// The fingerprint of the request as submitted, before any redirect,
    /// which the cache keeps the response under.
    key: Fingerprint,
    /// The redirects followed so far.
    redirects: Vec<Redirect>,
}

impl PartialEq for Queued {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Queued {}
impl PartialOrd for Queued {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Queued {
    /// Higher priority first, then first submitted first.
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority
            .cmp(&other.priority)
            .then(other.seq.cmp(&self.seq))
    }
}

impl Crawler {
    /// Starts the scheduler on the current tokio runtime. The WARC and
    /// HAR files are checked here, and opened by `start_archive` or the
    /// first `next`.
    pub fn new(fetch: FetchOptions, settings: CrawlSettings) -> Result<Crawler, FetchError> {
        Crawler::start(fetch, settings, None)
    }

    /// A crawl that shares `queue` with its other processes: what's
    /// submitted goes there, and what's fetched is leased from there. It
    /// ends once the queue and every process's leases have been empty for
    /// `idle`; with None, it waits for more until it's stopped.
    pub fn distributed(
        fetch: FetchOptions,
        settings: CrawlSettings,
        queue: Queue,
        idle: Option<Duration>,
    ) -> Result<Crawler, FetchError> {
        if settings.checkpoint.is_some() {
            return Err(FetchError::invalid(
                "a crawl with a queue can't have a checkpoint too: the queue is its saved state",
            ));
        }
        if settings.deltafetch.is_some() {
            return Err(FetchError::invalid(
                "a crawl with a queue can't have deltafetch too: its record is one \
                 machine's, and the queue's seen set already outlasts the crawl",
            ));
        }
        Crawler::start(fetch, settings, Some((queue, idle)))
    }

    fn start(
        mut fetch: FetchOptions,
        settings: CrawlSettings,
        queue: Option<(Queue, Option<Duration>)>,
    ) -> Result<Crawler, FetchError> {
        check_proxies(&fetch, &settings)?;
        let mut state = State {
            proxies: Pool::new(&settings.proxies),
            via: Lru::new(settings.proxies.len() + VIA_EXTRA),
            ..State::default()
        };
        let (checkpoint, saved) = match &settings.checkpoint {
            Some(path) => {
                let (cp, mut saved) =
                    Checkpoint::open(path).map_err(|e| FetchError::invalid(e.to_string()))?;
                state.seen = std::mem::take(&mut saved.seen);
                state.next_row = saved.next_row;
                (Some(cp), Some(saved))
            }
            None => (None, None),
        };
        if settings.warc.is_some() || settings.har.is_some() {
            // A crawl the checkpoint has anything of resumes, and adds to
            // its files; any other starts them afresh.
            let resuming = !state.seen.is_empty()
                || saved.as_ref().is_some_and(|s| {
                    !s.pending.is_empty() || !s.items.is_empty() || s.counters.is_some()
                });
            fetch.archive = Some(
                Archive::new(settings.warc.as_deref(), settings.har.as_deref(), resuming)
                    .map_err(FetchError::invalid)?,
            );
        }
        let cache = match &settings.cache {
            Some(path) => Some(Arc::new(
                Cache::open(
                    path,
                    settings.cache_expiry,
                    Policy {
                        robots: settings.obey_robots.then(|| settings.robots_agent.clone()),
                        tdm: settings.obey_tdmrep,
                    },
                )
                .map_err(FetchError::invalid)?,
            )),
            None => None,
        };
        let delta = match &settings.deltafetch {
            Some(path) => {
                Some(Record::open(path, settings.deltafetch_reset).map_err(FetchError::invalid)?)
            }
            None => None,
        };
        let settings_pages = settings.browser_pages.max(1);
        let (queue, inbox) = match queue {
            Some((queue, idle)) => {
                let (ops, inbox) = tokio::sync::mpsc::unbounded_channel();
                (Some(Linked { queue, idle, ops }), Some(inbox))
            }
            None => (None, None),
        };
        // The jars the checkpoint kept: the crawl's own ("") and named ones.
        let fetcher = Fetcher::new(fetch.clone())?;
        let mut jars: HashMap<String, Arc<Jar>> = HashMap::new();
        let mut jars_saved = HashMap::new();
        if let Some(saved) = &saved {
            for (name, snapshot) in &saved.jars {
                let jar = match name.as_str() {
                    "" => fetcher.jar().clone(),
                    _ => jars.entry(name.clone()).or_default().clone(),
                };
                // A jar that won't read starts empty: the crawl goes on, as
                // a new visitor.
                if jar.restore(snapshot).is_ok() {
                    jars_saved.insert(name.clone(), jar.changes());
                }
            }
        }
        let (calls, called) = tokio::sync::mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            fetcher,
            jars: Mutex::new(jars),
            jars_saved: Mutex::new((jars_saved, None)),
            checkpoint,
            queue,
            cache,
            delta,
            saved: Mutex::new(saved),
            options: fetch,
            settings,
            state: Mutex::new(state),
            schedule: Notify::new(),
            events: Notify::new(),
            browser: tokio::sync::Mutex::new(None),
            pages: Arc::new(Semaphore::new(settings_pages)),
            runtime: tokio::runtime::Handle::current(),
            calls,
            called: Mutex::new(Some(called)),
            calls_closed: std::sync::atomic::AtomicBool::new(false),
        });
        // Each host's own session the checkpoint kept, with its cookies.
        let host_jars = shared
            .saved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|s| s.host_jars.clone())
            .unwrap_or_default();
        // Old cookies never go with a new identity: a host whose profile
        // or proxy can't come back starts afresh, and one warning says so.
        let mut fresh = Vec::new();
        for (host, snapshot, identity) in host_jars {
            let jar = Arc::new(Jar::new());
            if jar.restore(&snapshot).is_err() {
                fresh.push(format!("{host} (its cookies couldn't be read)"));
                continue;
            }
            let identity: Option<Identity> = identity
                .as_deref()
                .and_then(|j| serde_json::from_str(j).ok());
            let Some(identity) = identity else {
                fresh.push(format!("{host} (its identity wasn't saved)"));
                continue;
            };
            let mut state = shared.lock();
            match restore_session(&shared, &mut state, &host, jar.clone(), &identity) {
                Ok(()) => {
                    drop(state);
                    let identity = serde_json::to_string(&identity).unwrap_or_default();
                    shared
                        .jars_saved
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .0
                        .insert(host_key(&host), host_saved(&jar, &identity));
                }
                Err(why) => fresh.push(format!("{host} ({why})")),
            }
        }
        if !fresh.is_empty() {
            let message = format!(
                "{} site(s) start as a new visitor, without the cookies their session had: \
                 they only work as the same visitor, and that can't come back: {}",
                fresh.len(),
                fresh.join(", ")
            );
            let mut state = shared.lock();
            shared.push_event(&mut state, Event::Warning { message });
        }
        // Saved requests get their proxies' logins back.
        let lost: Vec<String> = shared
            .saved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter_mut()
            .flat_map(|s| s.pending.iter_mut())
            .filter_map(|p| unmask_proxy(&shared, p))
            .collect();
        let mut state = shared.lock();
        if let Some(warning) = lost_proxies(&mut state, lost) {
            shared.push_event(&mut state, warning);
        }
        drop(state);
        // And their own logins' passwords.
        let lost: Vec<String> = shared
            .saved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter_mut()
            .flat_map(|s| s.pending.iter_mut())
            .filter_map(|p| unmask_login(&shared.settings, p))
            .collect();
        let mut state = shared.lock();
        if let Some(warning) = lost_logins(&mut state, lost) {
            shared.push_event(&mut state, warning);
        }
        drop(state);
        tokio::spawn(schedule_loop(shared.clone()));
        if let Some(inbox) = inbox {
            spawn_queue(shared.clone(), inbox);
        }
        Ok(Crawler { shared })
    }

    pub fn submit(&self, request: CrawlRequest) -> Submitted {
        self.submit_with(request, "{}")
    }

    /// `submit`, saving `payload` (the caller's own part of the request, as
    /// JSON) with it in the checkpoint, if there is one.
    pub fn submit_with(&self, request: CrawlRequest, payload: &str) -> Submitted {
        self.admit(request, Persist::New(payload))
    }

    /// Checks a request as `submit` does, and counts it as seen and as a
    /// page toward `max_pages` and `max_pages_per_domain`, but doesn't
    /// queue it: the caller answers it without a fetch (a Python
    /// middleware with a response in hand). `Answered` if it passed,
    /// `Capped` if `max_pages` pages have already been sent or served.
    pub fn claim(&self, request: CrawlRequest) -> Submitted {
        self.admit(request, Persist::Answered)
    }

    /// Queues a request read back from the checkpoint (`saved().pending`),
    /// or leased from the shared queue (`Event::Leased`). It was accepted
    /// before, so it isn't checked for duplicates again. A leased request
    /// refused here is released at once.
    pub fn resubmit(&self, request: CrawlRequest, row: i64) -> Submitted {
        let outcome = self.admit(request, Persist::Restored(row));
        if self.shared.queue.is_some() {
            let mut state = self.shared.lock();
            state.transit = state.transit.saturating_sub(1);
            drop(state);
            if outcome != Submitted::Queued {
                self.shared.release(row);
            }
        }
        outcome
    }

    /// Gives up a leased request without queueing it: it's finished.
    pub fn release(&self, row: i64) {
        let mut state = self.shared.lock();
        state.transit = state.transit.saturating_sub(1);
        drop(state);
        self.shared.release(row);
    }

    /// Whether the crawl keeps a checkpoint.
    pub fn has_checkpoint(&self) -> bool {
        self.shared.checkpoint.is_some()
    }

    /// Why the checkpoint's last commit failed, if it did, and whether
    /// it has stopped for good; without waiting on a commit.
    pub fn checkpoint_failing(&self) -> Option<(String, bool)> {
        let cp = self.shared.checkpoint.as_ref()?;
        cp.failing().map(|why| (why, cp.stopped()))
    }

    /// Whether the crawl shares a queue with other processes.
    pub fn has_queue(&self) -> bool {
        self.shared.queue.is_some()
    }

    /// Whether `max_pages` requests have been sent.
    pub fn capped(&self) -> bool {
        self.shared.lock().capped(&self.shared.settings)
    }

    /// What the checkpoint held when the crawl started; once.
    pub fn saved(&self) -> Option<Saved> {
        self.shared
            .saved
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// The caller has finished with these requests' events: they are done
    /// for good, and a resumed crawl won't fetch them again.
    pub fn done(&self, ids: &[u64]) {
        let mut state = self.shared.lock();
        for id in ids {
            if let Some(row) = state.rows.remove(id) {
                if let Some(cp) = &self.shared.checkpoint {
                    cp.done(row);
                }
                self.shared.release(row);
            }
        }
    }

    /// Records an item as delivered, by its stable id.
    pub fn item_done(&self, id: String) {
        if let Some(cp) = &self.shared.checkpoint {
            cp.item(id);
        }
    }

    /// Records items as delivered and saves the caller's counters (JSON)
    /// in one commit.
    pub fn settle(&self, items: Vec<String>, counters: String) {
        self.shared.save_jars(false);
        if let Some(cp) = &self.shared.checkpoint {
            cp.settle(items, counters);
        }
    }

    /// Records that the pages of these requests (their `deltafetch::key`)
    /// gave items, so this and later crawls skip them. Call it once the
    /// items are on disk, with each output file's length then: the next
    /// crawl cuts a file back to it, as what came after was never
    /// recorded and is fetched again. A failed write is an error, and the
    /// crawl can go on: those pages are skipped for the rest of this
    /// crawl, and fetched again by the next.
    pub fn produced(
        &self,
        keys: &[crate::canonical::Fingerprint],
        files: &[deltafetch::Output],
    ) -> Result<(), String> {
        let Some(record) = &self.shared.delta else {
            return Ok(());
        };
        record.add(keys, files).map_err(|e| {
            format!(
                "the deltafetch record at {} couldn't be written: {e}",
                record.path().display()
            )
        })
    }

    /// Each output file as the last crawl recorded it (its length, the
    /// caller's digest of its end, and whether it was closed cleanly), if
    /// there is a deltafetch record.
    pub fn delta_files(&self) -> Option<std::collections::HashMap<String, deltafetch::Recorded>> {
        self.shared.delta.as_ref().map(|r| r.files().clone())
    }

    /// Whether the deltafetch record had no pages when the crawl started;
    /// None without one.
    pub fn delta_was_empty(&self) -> Option<bool> {
        self.shared.delta.as_ref().map(Record::was_empty)
    }

    /// Saves the caller's counters (JSON) alongside the crawl.
    pub fn save_counters(&self, json: String) {
        self.shared.save_jars(false);
        if let Some(cp) = &self.shared.checkpoint {
            cp.counters(json);
        }
    }

    /// Waits until every checkpoint write so far is on disk; an error
    /// while one is failing, as it goes again with each commit until one
    /// works.
    pub fn flush(&self) -> Result<(), String> {
        match &self.shared.checkpoint {
            Some(cp) => cp.flush(),
            None => Ok(()),
        }
    }

    /// Waits until every checkpoint write so far is committed; an error if
    /// that commit failed, or the checkpoint has closed.
    pub fn commit(&self) -> Result<(), String> {
        match &self.shared.checkpoint {
            Some(cp) => cp.commit(),
            None => Ok(()),
        }
    }

    /// Opens the WARC and HAR files and starts writing to them. Until
    /// then they're only checked, so a crawl that fails to start leaves
    /// them as they were; what's fetched meanwhile waits to be written.
    pub fn start_archive(&self) -> Result<(), String> {
        match &self.shared.options.archive {
            Some(archive) => archive.start(),
            None => Ok(()),
        }
    }

    /// Commits the checkpoint and closes its file, and finishes the WARC
    /// and HAR files, for when the crawl is over. Checkpoint writes after
    /// this are dropped.
    pub fn close(&self) -> Result<(), String> {
        self.shared.save_jars(true);
        if let Some(link) = &self.shared.queue {
            // What's queued for Redis goes first, then what's still held
            // goes back for the other processes. Not waited for past a
            // while: a Redis that's gone can't hold the process up.
            let (tx, rx) = std::sync::mpsc::channel();
            if link.ops.send(QueueOp::Close(Some(tx))).is_ok() {
                let _ = rx.recv_timeout(Duration::from_secs(15));
            }
        }
        let mut flushed = self.flush();
        if let Some(archive) = &self.shared.options.archive {
            flushed = flushed.and(archive.close());
        }
        if let Some(cp) = &self.shared.checkpoint {
            cp.close();
        }
        if let Some(cache) = &self.shared.cache {
            cache.close();
        }
        if let Some(delta) = &self.shared.delta {
            delta.close();
        }
        flushed
    }

    /// The jar `name`d (made if it's new), or with None the crawl's own:
    /// the one requests use until a block gives a host a session of its
    /// own.
    pub fn jar(&self, name: Option<&str>) -> Arc<Jar> {
        self.shared.jar(name)
    }

    /// The Chrome the crawl is using, if it has started one.
    pub async fn browser(&self) -> Option<Browser> {
        match &*self.shared.browser.lock().await {
            Some(Ok(browser)) => Some(browser.clone()),
            _ => None,
        }
    }

    /// The call steps of pages in Chrome, as they come, for the caller to
    /// run: the first call has them, and later ones None. Until a caller
    /// takes them, a call step fails at once.
    pub fn step_calls(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<StepCall>> {
        self.shared
            .called
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// Says the call steps that come from now on have nobody to run them,
    /// so each fails at once.
    pub fn close_calls(&self) {
        self.shared
            .calls_closed
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Closes Chrome, if the crawl started it.
    pub async fn close_browser(&self) {
        if let Some(browser) = self.browser().await {
            let _ = browser.close().await;
        }
    }

    /// Fetches a file a page links to (an image, a PDF) through the crawl:
    /// its host's queue, delay and concurrency, robots.txt and TDMRep, the
    /// sessions, proxies, retries, cache and archive, as a page. But it isn't
    /// a page: it goes ahead of the host's pages, isn't checked against
    /// `allowed_domains`, `max_depth`, `max_pages_per_domain`, the
    /// duplicate filter or deltafetch, isn't saved in the checkpoint or
    /// sent to the shared queue, never goes to Chrome, doesn't count toward
    /// `max_pages`, and starts even while the caller has events to read,
    /// which is what lets a caller wait for it in the middle of handling
    /// them. Its outcome comes back here rather than as an event; None if
    /// the crawl stopped first. `id` must not be one a request in flight
    /// has.
    pub fn download(
        &self,
        mut request: CrawlRequest,
    ) -> impl Future<Output = Option<Downloaded>> + Send + 'static {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let id = request.id;
        let o = &request.outgoing;
        let fp = fingerprint_with(&o.method, &request.url, o.body.as_deref())
            .map(|fp| in_jar(fp, request.cookies.jar.as_deref()));
        let bad_proxy = request
            .proxy
            .as_deref()
            .is_some_and(|p| !p.is_empty() && check_proxy(p).is_err());
        let invalid = |message: String| Downloaded {
            event: Event::Failed {
                id,
                error: FetchError::invalid(message),
            },
            cached: false,
        };
        let refused = match fp {
            None => Some(format!("not an http(s) URL: {}", request.url)),
            Some(_) if bad_proxy => Some(format!("{}: its proxy can't be used", request.url)),
            Some(_) => None,
        };
        if let Some(message) = refused {
            let _ = tx.send(invalid(message));
        } else if let Some(fp) = fp {
            request.priority = DOWNLOAD_PRIORITY;
            request.browser = false;
            let cache = self.shared.cache.as_ref();
            let hit = cache.is_some_and(|c| c.has(&fp));
            let keep = cache.map(|_| Keep {
                key: fp,
                redirects: Vec::new(),
            });
            let url = request.url.clone();
            let mut state = self.shared.lock();
            state.downloads.insert(id, tx);
            let queued = enqueue(
                &self.shared.settings,
                &mut state,
                request,
                &url,
                0,
                None,
                keep,
                hit,
            );
            if !queued && let Some(tx) = state.downloads.remove(&id) {
                let _ = tx.send(invalid(format!("not an http(s) URL: {url}")));
            }
            drop(state);
            self.shared.schedule.notify_one();
        }
        async move { rx.await.ok() }
    }

    fn admit(&self, mut request: CrawlRequest, persist: Persist<'_>) -> Submitted {
        // Below every download's, which an item is waiting for.
        request.priority = request.priority.min(DOWNLOAD_PRIORITY - 1);
        let Some(fp) = fingerprint_of(&request) else {
            return Submitted::Invalid;
        };
        let Ok(parsed) = Url::parse(&request.url) else {
            return Submitted::Invalid;
        };
        if request
            .proxy
            .as_deref()
            // An empty proxy sends the request direct.
            .is_some_and(|p| !p.is_empty() && check_proxy(p).is_err())
        {
            return Submitted::BadProxy;
        }
        let settings = &self.shared.settings;
        let mut state = self.shared.lock();
        if !request.dont_filter && offsite(&parsed, &settings.allowed_domains) {
            state.stats.offsite += 1;
            let host = parsed.host_str().unwrap_or_default().to_string();
            if state.offsite_hosts.insert(host.clone()) {
                // Once per site, as there may be thousands of links to it.
                self.shared.push_event(
                    &mut state,
                    Event::Warning {
                        message: format!(
                            "requests to {host} are dropped: it isn't in allowed_domains"
                        ),
                    },
                );
            }
            return Submitted::Offsite;
        }
        if settings.max_depth.is_some_and(|max| request.depth > max) {
            state.stats.skipped_depth += 1;
            return Submitted::TooDeep;
        }
        if traps::repeats(parsed.path()) {
            state.stats.skipped_traps += 1;
            return Submitted::Trap;
        }
        let restored = matches!(persist, Persist::Restored(_));
        if !state.seen.insert(fp) && !request.dont_filter && !restored {
            state.stats.duplicates += 1;
            return Submitted::Duplicate;
        }
        // After the duplicate filter, so a page linked from many places is
        // skipped once and a duplicate after. Not for a request read back
        // from the checkpoint: its items may not be in the output yet.
        if let Some(record) = &self.shared.delta
            && !request.dont_filter
            && !restored
        {
            let key = match &request.delta_key {
                DeltaKey::Custom(custom) => {
                    deltafetch::key(&request.outgoing.method, &request.url, None, Some(custom))
                }
                DeltaKey::Fingerprint => Some(fp),
                DeltaKey::Off => None,
            };
            if key.is_some_and(|k| record.has(&k)) {
                // Seen in memory only: a resumed crawl asks the record again.
                state.stats.skipped_delta += 1;
                return Submitted::Delta;
            }
        }
        let answered = matches!(persist, Persist::Answered);
        // An answered request has nothing to fetch, so nothing to push.
        if let (Some(link), Persist::New(payload), false) = (&self.shared.queue, &persist, answered)
        {
            // The queue's own duplicate filter has the last word, and
            // max_pages_per_domain counts what each process leases.
            let pending = Box::new(pending_of(&request, payload, 0));
            let _ = link.ops.send(QueueOp::Push(fp, pending));
            return Submitted::Pushed;
        }
        let host = parsed.host_str().unwrap_or_default();
        let accepted = state.hosts.get(host).map_or(0, |h| h.accepted);
        if settings
            .max_pages_per_domain
            .is_some_and(|max| accepted >= max)
        {
            // Not seen after all: the same URL is refused the same way.
            state.seen.remove(&fp);
            state.stats.skipped_domain_full += 1;
            return Submitted::DomainFull;
        }
        if answered {
            // A page all the same, for every limit that counts pages.
            if state.capped(settings) {
                state.seen.remove(&fp);
                return Submitted::Capped;
            }
            state.sent += 1;
            state.stats.served += 1;
            let host = host.to_string();
            state
                .hosts
                .entry(host)
                .or_insert_with(|| Host::new(settings))
                .accepted += 1;
            return Submitted::Answered;
        }
        // Chrome hands the callback a live page, which no kept response
        // can stand in for; and a kept response may have followed
        // redirects a request with a limit of its own wouldn't.
        let policy = &request.outgoing.policy;
        let cache = self.shared.cache.as_ref().filter(|_| {
            !wants_browser(settings, &request)
                && !policy.dont_redirect
                && policy.max_redirects.is_none()
        });
        let hit = cache.is_some_and(|c| c.has(&fp));
        let keep = cache.map(|_| Keep {
            key: fp,
            redirects: Vec::new(),
        });
        let url = request.url.clone();
        let saving = self.shared.checkpoint.as_ref().map(|cp| {
            let row = match persist {
                Persist::Restored(row) => row,
                Persist::Answered => unreachable!("an answered request returns before this"),
                Persist::New(payload) => {
                    let row = state.next_row;
                    state.next_row += 1;
                    cp.request(fp, pending_of(&request, payload, row));
                    row
                }
            };
            (request.id, row)
        });
        // A leased request is released by its row when it's done.
        let saving = match (saving, &self.shared.queue, persist) {
            (None, Some(_), Persist::Restored(row)) => Some((request.id, row)),
            (saving, ..) => saving,
        };
        let passed = (!restored && !request.dont_filter)
            .then(|| first_hop(&request))
            .flatten()
            .map(|fp| vec![fp]);
        if !enqueue(settings, &mut state, request, &url, 0, passed, keep, hit) {
            return Submitted::Invalid;
        }
        if let Some((id, row)) = saving {
            state.rows.insert(id, row);
        }
        if let Some(h) = parsed.host_str().and_then(|h| state.hosts.get_mut(h)) {
            h.accepted += 1;
        }
        drop(state);
        self.shared.schedule.notify_one();
        Submitted::Queued
    }

    /// Waits for at least one event and returns up to `max`. Returns an
    /// empty batch once nothing is queued, in flight or being checked: the
    /// crawl is finished unless more is submitted.
    pub async fn next(&self, max: usize) -> Vec<Event> {
        self.archive_by_now();
        next_events(self.shared.clone(), max).await
    }

    /// `next`, as a future that owns what it needs, for callers that can't
    /// hold a borrow across the await (such as Python bindings).
    pub fn next_owned(
        &self,
        max: usize,
    ) -> impl std::future::Future<Output = Vec<Event>> + Send + 'static {
        self.archive_by_now();
        next_events(self.shared.clone(), max)
    }

    /// Starts the archive, if `start_archive` hasn't: by the time the
    /// caller reads events, the crawl has started. A file that can't be
    /// opened is a warning; the exchanges meant for it count in
    /// `archive_errors`.
    fn archive_by_now(&self) {
        if let Some(archive) = &self.shared.options.archive
            && let Err(message) = archive.start()
        {
            let mut state = self.shared.lock();
            self.shared
                .push_event(&mut state, Event::Warning { message });
        }
    }

    pub fn stats(&self) -> Stats {
        let mut stats = self.shared.lock().stats.clone();
        if let Some(archive) = &self.shared.options.archive {
            stats.archive_errors = archive.errors();
        }
        if let Some(cp) = &self.shared.checkpoint {
            stats.checkpoint_failures = cp.failures();
        }
        stats
    }

    /// Each of `proxies`, then each other proxy a request named, with what
    /// it did.
    pub fn proxy_stats(&self) -> Vec<(String, ProxyStats)> {
        self.shared
            .lock()
            .proxies
            .entries
            .iter()
            .map(|e| (e.url.clone(), e.stats.clone()))
            .collect()
    }
}

/// Refuses proxy settings that can't work, or that leave it unclear which
/// proxy a request goes through.
fn check_proxies(fetch: &FetchOptions, settings: &CrawlSettings) -> Result<(), FetchError> {
    let mode = match settings.proxy_rotation {
        ProxyRotation::OnBlock => None,
        ProxyRotation::PerRequest => Some("per_request"),
        ProxyRotation::PerHost => Some("per_host"),
    };
    if let Some(mode) = mode {
        if settings.proxies.is_empty() {
            return Err(FetchError::invalid(format!(
                "proxy_rotation \"{mode}\" rotates through proxies, and none were given"
            )));
        }
        if fetch.proxy.is_some() {
            return Err(FetchError::invalid(format!(
                "proxy_rotation \"{mode}\" takes proxies, not proxy as well: it isn't clear \
                 which would win; put that proxy in proxies"
            )));
        }
    }
    if settings.proxy_failures == 0 {
        return Err(FetchError::invalid("proxy_failures must be at least 1"));
    }
    if settings.proxy_cooldown.is_zero() {
        return Err(FetchError::invalid("proxy_cooldown must be positive"));
    }
    for proxy in &settings.proxies {
        check_proxy(proxy)?;
    }
    Ok(())
}

async fn next_events(shared: Arc<Shared>, max: usize) -> Vec<Event> {
    loop {
        let wake = shared.events.notified();
        tokio::pin!(wake);
        wake.as_mut().enable();
        {
            let mut state = shared.lock();
            if !state.events.is_empty() {
                let n = max.max(1).min(state.events.len());
                let batch = state.events.drain(..n).collect();
                drop(state);
                // Room in the backlog may let the scheduler start more.
                shared.schedule.notify_one();
                return batch;
            }
            let idle = state.stats.in_flight == 0 && state.gate_fetches == 0 && !state.serving;
            // With a shared queue, this process running out isn't the end.
            let over = shared.queue.is_none() || (state.queue_over && state.transit == 0);
            if idle && ((state.stats.queued == 0 && over) || state.capped(&shared.settings)) {
                return Vec::new();
            }
        }
        wake.await;
    }
}

impl Drop for Crawler {
    fn drop(&mut self) {
        self.shared.lock().closed = true;
        self.shared.schedule.notify_one();
        // Ends the HAR file and lets go of both; a crawl that wasn't
        // closed has no one to tell if that fails.
        if let Some(archive) = &self.shared.options.archive {
            let _ = archive.close();
        }
        if let Some(link) = &self.shared.queue {
            let _ = link.ops.send(QueueOp::Close(None));
        }
        // The scheduler keeps the shared state alive a while longer; the
        // checkpoint file shouldn't stay open with it.
        if let Some(cp) = &self.shared.checkpoint {
            cp.close();
        }
        if let Some(cache) = &self.shared.cache {
            cache.close();
        }
        if let Some(delta) = &self.shared.delta {
            delta.close();
        }
        let shared = self.shared.clone();
        self.shared.runtime.spawn(async move {
            if let Some(Ok(browser)) = &*shared.browser.lock().await {
                let _ = browser.close().await;
            }
        });
    }
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn push_event(&self, state: &mut State, event: Event) {
        self.push_event_from(state, event, false);
    }

    /// `push_event`, saying whether the response came from the cache. A
    /// download's outcome goes to its waiter, not to the events.
    fn push_event_from(&self, state: &mut State, event: Event, cached: bool) {
        match &event {
            Event::Fetched { response, .. } => {
                state.stats.fetched += 1;
                state.stats.bytes += response.body.len() as u64;
            }
            Event::Failed { .. } => state.stats.failed += 1,
            Event::Dropped {
                reason: DropReason::Robots,
                ..
            } => state.stats.dropped_robots += 1,
            Event::Dropped {
                reason: DropReason::TdmReserved,
                ..
            } => state.stats.dropped_tdm += 1,
            // Counted with the other duplicates where it's dropped.
            Event::Dropped {
                reason: DropReason::Duplicate,
                ..
            }
            | Event::Blocked { .. }
            | Event::Paused { .. }
            | Event::Warning { .. }
            | Event::Leased { .. }
            | Event::Fatal { .. } => {}
        }
        let id = match &event {
            Event::Fetched { id, .. }
            | Event::Failed { id, .. }
            | Event::Dropped { id, .. }
            | Event::Blocked { id, .. } => Some(*id),
            _ => None,
        };
        if let Some(tx) = id.and_then(|id| state.downloads.remove(&id)) {
            let _ = tx.send(Downloaded { event, cached });
            return;
        }
        state.events.push_back(event);
        self.events.notify_waiters();
    }

    /// Releases a leased request in the shared queue, once what it led to
    /// is there; nothing without a queue.
    fn release(&self, row: i64) {
        if let Some(link) = &self.queue {
            let _ = link.ops.send(QueueOp::Release(row));
        }
    }
}

/// `domains`, made ready for `offsite`: lowercase, international names in
/// their ASCII form, an optional port kept. A URL rather than a domain is an
/// error, since it would match nothing.
pub fn allowed_domains(domains: &[String]) -> Result<Vec<String>, String> {
    domains
        .iter()
        .map(|d| {
            let d = d.trim();
            let parsed = (!d.is_empty() && !d.contains(['/', '?', '#', '@']))
                .then(|| Url::parse(&format!("http://{d}/")).ok())
                .flatten()
                .filter(|u| u.host_str().is_some());
            let Some(u) = parsed else {
                return Err(format!(
                    "allowed_domains takes domains such as example.com, not {d:?}"
                ));
            };
            let host = u
                .host_str()
                .unwrap_or_default()
                .trim_end_matches('.')
                .to_string();
            Ok(match u.port() {
                Some(port) => format!("{host}:{port}"),
                None => host,
            })
        })
        .collect()
}

/// Whether `url` is outside `allowed` (from `allowed_domains`): not one of
/// them or a subdomain of one, or not the port a domain names. Nothing is
/// outside an empty list.
pub fn offsite(url: &Url, allowed: &[String]) -> bool {
    if allowed.is_empty() {
        return false;
    }
    let host = url.host_str().unwrap_or_default().trim_end_matches('.');
    let port = url.port_or_known_default();
    !allowed.iter().any(|d| {
        let (domain, want) = match d.rsplit_once(':').filter(|(_, p)| p.parse::<u16>().is_ok()) {
            Some((h, p)) => (h, p.parse::<u16>().ok()),
            None => (d.as_str(), None),
        };
        let within = host == domain
            || (host.len() > domain.len()
                && host.ends_with(domain)
                && host.as_bytes()[host.len() - domain.len() - 1] == b'.');
        within && want.is_none_or(|w| port == Some(w))
    })
}

/// Whether a submitted request is new, or read back from the checkpoint or
/// leased from the shared queue.
enum Persist<'a> {
    New(&'a str),
    Restored(i64),
    /// Checked and seen, never queued: see `claim`.
    Answered,
}

/// `request` as the checkpoint and the shared queue keep it.
fn pending_of(request: &CrawlRequest, payload: &str, row: i64) -> Pending {
    let (proxy, proxy_sha256) = saved_proxy(request.proxy.as_deref());
    Pending {
        row,
        url: request.url.clone(),
        priority: request.priority,
        headers: request.headers.clone(),
        dont_filter: request.dont_filter,
        depth: request.depth,
        payload: payload.to_string(),
        method: request.outgoing.method.clone(),
        kind: request.outgoing.kind.as_str().to_string(),
        body: request.outgoing.body.clone(),
        content_type: request.outgoing.content_type.clone(),
        referer: request.outgoing.referer.clone(),
        retry_post: request.retry_post,
        proxy,
        proxy_sha256,
        policy: saved_policy(&request.outgoing.policy),
    }
}

/// Gives a saved request's login its password back: from `saved_logins`
/// or `http_auth`, whichever has the same user and domain. With neither,
/// the request goes as the crawl's others do, and the login is returned
/// to warn about. A login saved before passwords were hidden is used as
/// it was saved.
fn unmask_login(settings: &CrawlSettings, p: &mut Pending) -> Option<String> {
    let auth = p.policy.auth.as_mut().filter(|a| a.hidden)?;
    let found = settings
        .saved_logins
        .iter()
        .chain(&settings.http_auth)
        .find(|c| c.user == auth.user && c.domain == auth.domain);
    if let Some(found) = found {
        *auth = found.clone();
        return None;
    }
    let lost = match &auth.domain {
        Some(domain) => format!("{} at {domain}", auth.user),
        None => format!("{} at {}", auth.user, p.url),
    };
    p.policy.auth = None;
    Some(lost)
}

/// The warning for saved requests whose login `unmask_login` couldn't
/// find, each login named once in the crawl.
fn lost_logins(state: &mut State, lost: Vec<String>) -> Option<Event> {
    let new: Vec<String> = lost
        .into_iter()
        .filter(|l| state.lost_logins.insert(l.clone()))
        .collect();
    (!new.is_empty()).then(|| Event::Warning {
        message: format!(
            "saved requests logging in as {} go without that login: its password isn't in \
             saved_logins or http_auth",
            new.join(", ")
        ),
    })
}

/// Gives a saved request its proxy's login back, as the crawl has it
/// now: the proxy among its pool, its own and Chrome's whose URL hashes
/// the same, or failing that the only one at the same scheme, host and
/// port, whose login changed. With neither, the request goes as the
/// crawl's others do, and the proxy is returned to warn about. A proxy
/// saved with no login, or with its login before logins were hidden,
/// is used as it was saved.
fn unmask_proxy(shared: &Shared, p: &mut Pending) -> Option<String> {
    let sha = p.proxy_sha256.take()?;
    let saved = p.proxy.take()?;
    let mut ours: Vec<&String> = shared
        .settings
        .proxies
        .iter()
        .chain(&shared.options.proxy)
        .chain(&shared.settings.browser_launch.proxy)
        .collect();
    ours.sort();
    ours.dedup();
    p.proxy = match ours.iter().find(|c| proxy_sha256(c) == sha) {
        Some(c) => Some(c.to_string()),
        None => match ours
            .iter()
            .filter(|c| masked(c) == saved)
            .collect::<Vec<_>>()[..]
        {
            [only] => Some(only.to_string()),
            _ => None,
        },
    };
    p.proxy.is_none().then_some(saved)
}

/// The warning for saved requests whose proxy `unmask_proxy` couldn't
/// find, each proxy named once in the crawl.
fn lost_proxies(state: &mut State, lost: Vec<String>) -> Option<Event> {
    let new: Vec<String> = lost
        .into_iter()
        .filter(|p| state.lost_proxies.insert(p.clone()))
        .collect();
    (!new.is_empty()).then(|| Event::Warning {
        message: format!(
            "saved requests through {} go as the crawl's others do: their proxy's login \
             isn't among the crawl's proxies now",
            new.join(", ")
        ),
    })
}

/// How often the queue's task looks for work when it found none.
const QUEUE_POLL: Duration = Duration::from_millis(250);

/// Starts the queue's task, and turns a panic in it into the end of the
/// crawl rather than a silent stop.
fn spawn_queue(s: Arc<Shared>, inbox: tokio::sync::mpsc::UnboundedReceiver<QueueOp>) {
    spawn_watched(s.clone(), queue_loop(s, inbox));
}

/// Runs `work` as a task, and if it panics, says so with a fatal event.
fn spawn_watched(s: Arc<Shared>, work: impl Future<Output = ()> + Send + 'static) {
    let task = tokio::spawn(work);
    tokio::spawn(async move {
        if let Err(e) = task.await
            && e.is_panic()
        {
            let panic = e.into_panic();
            let why = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap_or("a panic");
            let message = format!("the shared queue's task failed ({why}), so this process stops");
            let mut state = s.lock();
            s.push_event(&mut state, Event::Fatal { message });
        }
    });
}

/// Runs until the crawl closes: sends what the crawl queues and finishes
/// to Redis, in order; leases requests while there's room; beats; and
/// says when the shared queue has stayed empty long enough to end.
async fn queue_loop(s: Arc<Shared>, mut inbox: tokio::sync::mpsc::UnboundedReceiver<QueueOp>) {
    let Some(link) = &s.queue else {
        return;
    };
    let q = &link.queue;
    let mut ops = VecDeque::new();
    // The run of pushes being sent, numbered once, so sending it again
    // after a lost reply can't queue a second copy.
    let mut pushing: Option<Vec<Prepared>> = None;
    let mut beat = Beat {
        next: Instant::now(),
        last: Instant::now(),
    };
    // A lease failed: its reply may have been lost after Redis applied it.
    let mut unsure = false;
    // Since when the queue has looked empty, and how many requests had
    // been queued then: one more since, and it wasn't.
    let mut quiet: Option<(Instant, i64)> = None;
    // Since when every step has failed.
    let mut failing: Option<Instant> = None;
    loop {
        while let Ok(op) = inbox.try_recv() {
            ops.push_back(op);
        }
        let backlog = ops.len();
        let turn = async {
            if let Some(reply) = send_ops(&s, q, &mut ops, &mut pushing, &mut beat).await? {
                // What's held and not finished is for others now. If Redis
                // can't be told, the leases run out instead.
                let _ = q.give_back().await;
                return Ok::<_, String>(Turn::Closed(reply));
            }
            if unsure {
                q.reconcile().await?;
                unsure = false;
            }
            beat.if_due(&s, q).await?;
            let room = {
                let state = s.lock();
                if state.closed || state.capped(&s.settings) {
                    0
                } else {
                    s.settings
                        .concurrency
                        .saturating_sub(state.stats.queued + state.transit)
                }
            };
            let (leased, unreadable) = q.lease(room).await.inspect_err(|_| unsure = true)?;
            let full = room > 0 && leased.len() as u64 + unreadable == room as u64;
            if unreadable > 0 {
                let mut state = s.lock();
                state.stats.queue_bad += unreadable;
                let message = format!(
                    "{unreadable} member(s) of the shared queue weren't requests netweir can \
                     read, and were dropped"
                );
                s.push_event(&mut state, Event::Warning { message });
            }
            if !leased.is_empty() {
                quiet = None;
                let mut state = s.lock();
                // Not over after all: the end waits for a whole grace of
                // quiet after this work too.
                state.queue_over = false;
                let mut lost = Vec::new();
                let mut logins = Vec::new();
                for (member, mut request) in leased {
                    lost.extend(unmask_proxy(&s, &mut request));
                    logins.extend(unmask_login(&s.settings, &mut request));
                    let row = state.next_row;
                    state.next_row += 1;
                    state.held.insert(row, member);
                    state.transit += 1;
                    state.stats.leased += 1;
                    s.push_event(
                        &mut state,
                        Event::Leased {
                            row,
                            request: Box::new(request),
                        },
                    );
                }
                if let Some(warning) = lost_proxies(&mut state, lost) {
                    s.push_event(&mut state, warning);
                }
                if let Some(warning) = lost_logins(&mut state, logins) {
                    s.push_event(&mut state, warning);
                }
            }
            let idle = {
                let state = s.lock();
                state.stats.queued == 0
                    && state.stats.in_flight == 0
                    && state.gate_fetches == 0
                    && !state.serving
                    && state.events.is_empty()
                    && state.transit == 0
                    && state.held.is_empty()
            } && ops.is_empty()
                && inbox.is_empty();
            match (link.idle, idle) {
                (Some(grace), true) => {
                    let status = q.status().await?;
                    let empty = status.waiting == 0 && status.held == 0;
                    quiet = match quiet {
                        Some((since, queued)) if empty && queued == status.queued => {
                            Some((since, queued))
                        }
                        _ if empty => Some((Instant::now(), status.queued)),
                        _ => None,
                    };
                    if quiet.is_some_and(|(since, _)| since.elapsed() >= grace) {
                        s.lock().queue_over = true;
                        s.events.notify_waiters();
                    }
                }
                _ => quiet = None,
            }
            Ok(if full { Turn::Busy } else { Turn::Waiting })
        };
        let pause = match turn.await {
            Ok(Turn::Closed(reply)) => {
                if let Some(reply) = reply {
                    let _ = reply.send(());
                }
                return;
            }
            Ok(turn) => {
                failing = None;
                if matches!(turn, Turn::Busy) {
                    Duration::ZERO
                } else {
                    QUEUE_POLL
                }
            }
            Err(e) => {
                // Some of the backlog got through first: whatever failed
                // since is a new failure, not the old one going on, or a
                // backlog whose batches each fail once would stop the
                // process however well it was getting on.
                if ops.len() < backlog {
                    failing = None;
                }
                let mut state = s.lock();
                let first = failing.is_none();
                let since = *failing.get_or_insert_with(Instant::now);
                // No heartbeat for a lease: the other processes have taken
                // this one for dead, and what it holds is being fetched by
                // them by now. Or a step has failed for a whole lease while
                // heartbeats got through: it isn't going to work.
                if beat.last.elapsed() >= q.lease_time() || since.elapsed() >= q.lease_time() {
                    let message = format!(
                        "the queue failed for longer than queue_lease ({} s), so this process \
                         stops: {e}",
                        q.lease_time().as_secs_f64()
                    );
                    s.push_event(&mut state, Event::Fatal { message });
                    return;
                }
                if first {
                    let message = format!("{e}; trying again");
                    s.push_event(&mut state, Event::Warning { message });
                }
                (q.lease_time() / 6).min(Duration::from_secs(1))
            }
        };
        tokio::select! {
            op = inbox.recv() => ops.extend(op),
            _ = tokio::time::sleep(pause) => {}
        }
    }
}

/// How a turn of the queue's task went.
enum Turn {
    /// It leased all it had room for: there may be more at once.
    Busy,
    /// Nothing more to do until something happens.
    Waiting,
    /// The crawl closed, and here is who to tell when it's done.
    Closed(Option<std::sync::mpsc::Sender<()>>),
}

/// When this process last told Redis it's alive, and when it next must.
struct Beat {
    next: Instant,
    /// When the last heartbeat that got through was sent: the other
    /// processes count this one's lease from about then.
    last: Instant,
}

impl Beat {
    /// Sends a heartbeat if one is due.
    async fn if_due(&mut self, s: &Shared, q: &Queue) -> Result<(), String> {
        if Instant::now() < self.next {
            return Ok(());
        }
        // Taken before the call: the deadline it sets is counted from
        // about then, not from after any reaping it does.
        let sent = Instant::now();
        let reclaimed = q.heartbeat().await?;
        s.lock().stats.leases_reclaimed += reclaimed;
        self.last = sent;
        self.next = sent + q.lease_time() / 3;
        Ok(())
    }
}

/// Sends the queued operations to Redis in order, a run of pushes or
/// releases at a time; each run leaves `ops` only once Redis has it. A run
/// of pushes is numbered once, in `pushing`, and sent as it is until it
/// gets through. Heartbeats go between runs, so a long backlog doesn't get
/// a live process taken for dead. On a close, returns its reply channel,
/// with what came before it sent.
async fn send_ops(
    s: &Shared,
    q: &Queue,
    ops: &mut VecDeque<QueueOp>,
    pushing: &mut Option<Vec<Prepared>>,
    beat: &mut Beat,
) -> Result<Option<Option<std::sync::mpsc::Sender<()>>>, String> {
    while let Some(op) = ops.front() {
        beat.if_due(s, q).await?;
        match op {
            QueueOp::Push(..) => {
                let prepared = match pushing.take() {
                    Some(prepared) => prepared,
                    None => {
                        let batch: Vec<(Fingerprint, &Pending)> = ops
                            .iter()
                            .take(BATCH)
                            .map_while(|op| match op {
                                QueueOp::Push(fp, p) => Some((*fp, &**p)),
                                _ => None,
                            })
                            .collect();
                        q.prepare(&batch).await?
                    }
                };
                let queued = match q.enqueue(&prepared).await {
                    Ok(queued) => queued,
                    Err(e) => {
                        *pushing = Some(prepared);
                        return Err(e);
                    }
                };
                ops.drain(..prepared.len());
                let pushed = queued.iter().filter(|q| **q).count() as u64;
                let mut state = s.lock();
                state.stats.pushed += pushed;
                state.stats.duplicates += queued.len() as u64 - pushed;
            }
            QueueOp::Release(_) => {
                let rows: Vec<i64> = ops
                    .iter()
                    .take(BATCH)
                    .map_while(|op| match op {
                        QueueOp::Release(row) => Some(*row),
                        _ => None,
                    })
                    .collect();
                let members: Vec<Vec<u8>> = {
                    let state = s.lock();
                    rows.iter()
                        .filter_map(|row| state.held.get(row).cloned())
                        .collect()
                };
                q.release(&members).await?;
                ops.drain(..rows.len());
                let mut state = s.lock();
                for row in rows {
                    state.held.remove(&row);
                }
            }
            QueueOp::Close(_) => {
                let Some(QueueOp::Close(reply)) = ops.pop_front() else {
                    unreachable!("the front is a close");
                };
                return Ok(Some(reply));
            }
        }
    }
    Ok(None)
}

/// The session a request goes out with, and the proxy of the pool or
/// the request's own it goes through (None for the fetch options').
type Route = (Result<Fetcher, FetchError>, Option<String>);

/// What the scheduler decided to do with a host's next request.
enum Action {
    Fetch(Box<Queued>, Route),
    /// In Chrome, in the page slot the permit holds.
    Render(Box<Queued>, OwnedSemaphorePermit),
    CheckRobots(String, Route),
    CheckTdm(String, Route),
    /// Read a hit from the cache.
    Serve(Box<Queued>),
}

/// Runs until the Crawler is dropped, which sets `closed` and wakes it.
async fn schedule_loop(s: Arc<Shared>) {
    loop {
        let wake = s.schedule.notified();
        tokio::pin!(wake);
        wake.as_mut().enable();
        let (actions, sleep_until) = {
            let mut state = s.lock();
            if state.closed {
                return;
            }
            plan(&s.settings, &mut state, &s)
        };
        for action in actions {
            match action {
                Action::Render(q, permit) => {
                    tokio::spawn(browser_fetch(s.clone(), *q, permit));
                }
                Action::Fetch(q, route) => {
                    tokio::spawn(fetch(s.clone(), *q, route));
                }
                Action::CheckRobots(origin, route) => {
                    tokio::spawn(check_robots(s.clone(), origin, route));
                }
                Action::CheckTdm(origin, route) => {
                    tokio::spawn(check_tdm(s.clone(), origin, route));
                }
                Action::Serve(q) => {
                    tokio::spawn(serve(s.clone(), *q));
                }
            }
        }
        match sleep_until {
            Some(at) => {
                tokio::select! {
                    _ = wake => {}
                    _ = tokio::time::sleep_until(at.into()) => {}
                }
            }
            None => wake.await,
        }
    }
}

/// Hears of a new connection made for one of `host`'s requests. Made, the
/// request goes out now, so the host's next turn is a delay from now, not
/// from when the request was handed out: a request that waited for a TLS
/// handshake, a proxy's tunnel or a slow DNS answer would otherwise leave
/// close behind it, the next on a connection already open. Scrapy spaces
/// requests from when they're handed out; the site sees when they arrive.
fn connected(shared: &Shared, host: &str, connect: crate::fetch::Connect) {
    use crate::fetch::Connect;
    let mut state = shared.lock();
    let Some(h) = state.hosts.get_mut(host) else {
        return;
    };
    match connect {
        Connect::Started => h.connecting += 1,
        Connect::Ready(at) => {
            h.connecting = h.connecting.saturating_sub(1);
            h.next_at = h.next_at.max(at + h.delay.max(h.floor));
        }
        Connect::Failed => h.connecting = h.connecting.saturating_sub(1),
    }
    if connect != Connect::Started {
        state.make_ready(host);
        drop(state);
        shared.schedule.notify_one();
    }
}

/// Whether `request` goes to Chrome. Chrome navigates to a URL; a POST
/// stays with the HTTP client.
fn wants_browser(settings: &CrawlSettings, request: &CrawlRequest) -> bool {
    (request.browser || settings.browser == BrowserMode::Always) && request.outgoing.method == "GET"
}

/// Queues `request` for `url` (the request's own URL, or where a redirect
/// led) on its host, and marks the host ready; or, for a `hit`, on the
/// queue served from the cache. False if the URL can't be crawled.
#[allow(clippy::too_many_arguments)]
fn enqueue(
    settings: &CrawlSettings,
    state: &mut State,
    mut request: CrawlRequest,
    url: &str,
    hops: usize,
    passed: Option<Vec<Fingerprint>>,
    keep: Option<Keep>,
    hit: bool,
) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    let (Some(host), true) = (
        parsed.host_str(),
        matches!(parsed.scheme(), "http" | "https"),
    ) else {
        return false;
    };
    let host = host.to_string();
    let origin = parsed.origin().ascii_serialization();
    let path = match parsed.query() {
        Some(q) => format!("{}?{q}", parsed.path()),
        None => parsed.path().to_string(),
    };
    if hops == 0 {
        for c in &mut request.cookies.add {
            crate::cookies::scope(c, &parsed);
        }
    }
    request.url = url.to_string();
    state.seq += 1;
    let in_browser =
        wants_browser(settings, &request) && !state.downloads.contains_key(&request.id);
    let queued = Queued {
        priority: request.priority,
        seq: state.seq,
        request,
        host: host.clone(),
        origin,
        path,
        hops,
        passed,
        attempts: 0,
        in_browser,
        keep,
        blocked: None,
    };
    // The host is made either way: it counts the request toward
    // max_pages_per_domain, and a hit the cache no longer has goes there.
    let host_entry = state
        .hosts
        .entry(host.clone())
        .or_insert_with(|| Host::new(settings));
    if hit {
        state.hits.push(queued);
    } else {
        host_entry.queue.push(queued);
        state.make_ready(&host);
    }
    state.stats.queued += 1;
    true
}

impl State {
    /// Whether `max_pages` requests have been sent.
    fn capped(&self, settings: &CrawlSettings) -> bool {
        settings.max_pages.is_some_and(|max| self.sent >= max)
    }

    /// Whether pages may not start now, for room in the backlog or for
    /// `max_pages`; downloads still may.
    fn pages_held(&self, settings: &CrawlSettings) -> bool {
        self.events.len() >= backlog_limit(settings) || self.capped(settings)
    }

    fn is_download(&self, q: &Queued) -> bool {
        self.downloads.contains_key(&q.request.id)
    }

    fn make_ready(&mut self, name: &str) {
        let Some(host) = self.hosts.get_mut(name) else {
            return;
        };
        if !host.listed {
            host.listed = true;
            self.ready.push_back(name.to_string());
        }
        // Listed for downloads too, if one is first in its queue (they go
        // ahead of pages), so plan can find it while pages are held.
        let download_first = host
            .queue
            .peek()
            .is_some_and(|q| self.downloads.contains_key(&q.request.id));
        if download_first && !host.download_listed {
            host.download_listed = true;
            self.ready_downloads.push_back(name.to_string());
        }
    }
}

/// Events held for the reader before the scheduler stops starting fetches,
/// so a slow reader doesn't mean the whole frontier downloaded into memory.
fn backlog_limit(settings: &CrawlSettings) -> usize {
    settings.concurrency.saturating_mul(2)
}

/// Starts everything that may start now; returns what to do and when to
/// look again.
fn plan(
    settings: &CrawlSettings,
    state: &mut State,
    shared: &Shared,
) -> (Vec<Action>, Option<Instant>) {
    let now = Instant::now();
    while let Some(Reverse((at, _))) = state.timers.peek() {
        if *at > now {
            break;
        }
        let Some(Reverse((_, name))) = state.timers.pop() else {
            break;
        };
        if let Some(host) = state.hosts.get_mut(&name) {
            host.timer = None;
        }
        state.make_ready(&name);
    }
    // A Chrome page has freed: the hosts that waited for one go first.
    if !state.page_waiters.is_empty() && shared.pages.available_permits() > 0 {
        for name in std::mem::take(&mut state.page_waiters) {
            state.make_ready(&name);
        }
    }
    let mut actions = Vec::new();
    // Hits first: they wait for no host, delay or slot, only for room in
    // the backlog, and count toward max_pages as a fetch would. One at a
    // time, as the cache has one connection anyway: they come back in the
    // order they were queued.
    let download_hit = state.hits.peek().is_some_and(|q| state.is_download(q));
    if !state.serving
        && (download_hit || !state.pages_held(settings))
        && let Some(q) = state.hits.pop()
    {
        state.stats.queued -= 1;
        state.serving = true;
        if !state.is_download(&q) {
            state.sent += 1;
        }
        actions.push(Action::Serve(Box::new(q)));
    }
    'hosts: loop {
        // Gate fetches count against the limit like any other request.
        if state.stats.in_flight + state.gate_fetches >= settings.concurrency {
            break;
        }
        // While pages are held, only the hosts with a download first in
        // line are visited, so a crawl of many sites waiting on one file
        // doesn't walk them all on every wake.
        let name = if state.pages_held(settings) {
            let Some(name) = state.ready_downloads.pop_front() else {
                break;
            };
            if let Some(host) = state.hosts.get_mut(&name) {
                host.download_listed = false;
            }
            name
        } else {
            let Some(name) = state.ready.pop_front() else {
                break;
            };
            if let Some(host) = state.hosts.get_mut(&name) {
                host.listed = false;
            }
            name
        };
        loop {
            if state.stats.in_flight + state.gate_fetches >= settings.concurrency {
                // Still has work: back in line for when a slot frees.
                state.make_ready(&name);
                break 'hosts;
            }
            let Some(host) = state.hosts.get(&name) else {
                break;
            };
            let Some(next) = host.queue.peek() else { break };
            // Downloads go ahead of a host's pages, so only a page waits:
            // its host stays in line for when pages may start again.
            if state.pages_held(settings) && !state.downloads.contains_key(&next.request.id) {
                state.make_ready(&name);
                break;
            }
            let origin = next.origin.clone();
            // A robots.txt or tdmrep.json goes the way this request will.
            let wanted = next.request.proxy.clone();
            let path = &next.path;
            // Robots and TDMRep must be known for this origin first.
            let o = state.origins.entry(origin.clone()).or_default();
            // Some(true) to fetch robots.txt, Some(false) tdmrep.json.
            let mut gate = None;
            let mut wait = false;
            if settings.obey_robots {
                match o.robots {
                    Gate::Unknown => {
                        o.robots = Gate::Fetching;
                        gate = Some(true);
                    }
                    Gate::Fetching => wait = true,
                    Gate::Ready(_) => {}
                }
            }
            if gate.is_none() && !wait && settings.obey_tdmrep {
                match o.tdm {
                    Gate::Unknown => {
                        o.tdm = Gate::Fetching;
                        gate = Some(false);
                    }
                    Gate::Fetching => wait = true,
                    Gate::Ready(_) => {}
                }
            }
            if gate.is_some() || wait {
                if !o.waiting.contains(&name) {
                    o.waiting.push(name.clone());
                }
                if let Some(robots) = gate {
                    state.gate_fetches += 1;
                    // Through the proxy the host's pages would go through,
                    // so the site sees one visitor.
                    let route = gate_route(shared, state, &origin, &name, wanted.as_deref());
                    actions.push(if robots {
                        Action::CheckRobots(origin, route)
                    } else {
                        Action::CheckTdm(origin, route)
                    });
                }
                break;
            }
            let blocked = match (&o.robots, &o.tdm) {
                (Gate::Ready(r), _)
                    if settings.obey_robots && !r.allowed(&settings.robots_agent, path) =>
                {
                    Some(DropReason::Robots)
                }
                (_, Gate::Ready(Some(f)))
                    if settings.obey_tdmrep && f.reservation(path).reserved == Some(true) =>
                {
                    Some(DropReason::TdmReserved)
                }
                _ => None,
            };
            let floor = match &o.robots {
                Gate::Ready(r) => r
                    .crawl_delay(&settings.robots_agent)
                    .unwrap_or_default()
                    .min(settings.max_delay),
                _ => Duration::ZERO,
            };
            let host = state.hosts.get_mut(&name).expect("looked up above");
            if let Some(reason) = blocked {
                let q = host.queue.pop().expect("peeked above");
                state.stats.queued -= 1;
                shared.push_event(
                    state,
                    Event::Dropped {
                        id: q.request.id,
                        reason,
                    },
                );
                continue;
            }
            host.floor = floor;
            if host.in_flight >= settings.per_domain {
                // Ready again when one of its fetches finishes.
                break;
            }
            if host.connecting > 0 && !host.delay.max(host.floor).is_zero() {
                // Ready again when its connection is made (see `connected`).
                break;
            }
            if now < host.next_at {
                if host.timer != Some(host.next_at) {
                    host.timer = Some(host.next_at);
                    state.timers.push(Reverse((host.next_at, name.clone())));
                }
                break;
            }
            // A browser request starts only with a Chrome page to use, so
            // it never holds a slot other requests could have had while it
            // waits for one.
            let permit = if host.queue.peek().is_some_and(|q| q.in_browser) {
                match shared.pages.clone().try_acquire_owned() {
                    Ok(permit) => Some(permit),
                    Err(_) => {
                        if !state.page_waiters.contains(&name) {
                            state.page_waiters.push(name.clone());
                        }
                        break;
                    }
                }
            } else {
                None
            };
            let q = host.queue.pop().expect("peeked above");
            host.in_flight += 1;
            host.next_at = now + host.delay.max(host.floor);
            state.stats.queued -= 1;
            state.stats.in_flight += 1;
            if !state.is_download(&q) {
                state.sent += 1;
            }
            actions.push(match permit {
                Some(permit) => Action::Render(Box::new(q), permit),
                None => {
                    let route = route(shared, state, &name, q.request.proxy.as_deref());
                    Action::Fetch(Box::new(q), route)
                }
            });
        }
    }
    let sleep_until = state.timers.peek().map(|Reverse((at, _))| *at);
    (actions, sleep_until)
}

/// Runs `work` as its own task, so a panic inside it (a parser bug on
/// hostile input, say) becomes `fallback` instead of killing the caller
/// before it has done its bookkeeping.
async fn guarded<T: Send + 'static>(
    work: impl std::future::Future<Output = T> + Send + 'static,
    fallback: impl FnOnce(String) -> T,
) -> T {
    match tokio::spawn(work).await {
        Ok(value) => value,
        Err(e) => fallback(e.to_string()),
    }
}

/// `outgoing` with the crawl's login (`http_auth`) when it has none of its
/// own; the login decides at each hop whether it goes there.
fn with_crawl_login(settings: &CrawlSettings, outgoing: &Outgoing) -> Outgoing {
    let mut outgoing = outgoing.clone();
    if outgoing.policy.auth.is_none() {
        outgoing.policy.auth = settings.http_auth.clone();
    }
    outgoing
}

async fn fetch(shared: Arc<Shared>, mut q: Queued, (session, proxy): Route) {
    let started = Instant::now();
    let (url, headers, hops) = (q.request.url.clone(), q.request.headers.clone(), q.hops);
    let outgoing = with_crawl_login(&shared.settings, &q.request.outgoing);
    let tell: crate::fetch::ConnectTold = {
        let (shared, host) = (shared.clone(), q.host.clone());
        Arc::new(move |c| connected(&shared, &host, c))
    };
    let result = match session {
        Ok(session) => {
            let session = shared.with_request_jar(session, &q.request, hops);
            guarded(
                crate::fetch::telling_connects(tell, async move {
                    session.hop_as(&url, &outgoing, &headers, hops).await
                }),
                |why| {
                    Err(FetchError {
                        kind: FetchErrorKind::Other,
                        message: format!("the fetch failed inside netweir: {why}"),
                        steps: Default::default(),
                    })
                },
            )
            .await
        }
        Err(e) => Err(e),
    };
    let latency = started.elapsed();
    let settings = &shared.settings;
    let outcome = match &result {
        Ok(Hop::Done(r)) => Some(classify(r)),
        _ => None,
    };
    let reserved = match &result {
        Ok(Hop::Done(r)) if settings.obey_tdmrep => {
            page_reserved(&shared.lock(), &q.origin, &q.path, r)
        }
        _ => false,
    };
    // Kept before its event is out, so a page the caller has is on disk.
    let mut stored = Ok(false);
    if let (Some(keep), Some(cache), Ok(Hop::Done(r))) = (&q.keep, &shared.cache, &result)
        && !reserved
        && worth_keeping(r, outcome.as_ref(), &settings.retry_statuses)
    {
        let (cache, r) = (cache.clone(), r.clone());
        let (key, redirects) = (keep.key, keep.redirects.clone());
        stored = tokio::task::spawn_blocking(move || cache.put(&key, &r, &redirects))
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
    }
    let mut state = shared.lock();
    match stored {
        Ok(true) => state.stats.cache_stores += 1,
        Ok(false) => {}
        Err(e) => cache_failed(
            &shared,
            &mut state,
            "written",
            &e,
            "carrying on without it for this page",
        ),
    }
    state.stats.in_flight -= 1;
    if let Some(archive) = &shared.options.archive {
        for message in archive.take_warnings() {
            shared.push_event(&mut state, Event::Warning { message });
        }
    }
    if let Some(host) = state.hosts.get_mut(&q.host) {
        host.in_flight -= 1;
        if settings.throttle {
            let status = match &result {
                Ok(Hop::Done(r)) => Some(r.status),
                _ => None,
            };
            host.delay = adjust_delay(settings, host.delay, latency, status);
        } else if matches!(outcome, Some(Outcome::Ok)) {
            // No throttle to bring a delay a block added back down: each
            // good response halves it.
            host.delay = (host.delay / 2).max(settings.min_delay);
        }
    }
    if let Some(outcome) = &outcome {
        breaker(
            &shared,
            &mut state,
            &q.host,
            matches!(outcome, Outcome::Blocked { .. }),
        );
    }
    state.make_ready(&q.host);
    // Before a block picks a new session's proxy, so it knows whether this
    // one is still fit to pick.
    if let Some(p) = &proxy {
        let failed = match &result {
            Err(e) => proxy_trouble(e),
            Ok(Hop::Done(r)) => r.status == 407 || matches!(outcome, Some(Outcome::Blocked { .. })),
            Ok(Hop::Redirect(..)) => false,
        };
        record(&shared, &mut state, p, failed);
    }
    // The pool picked this proxy, so the next try can go through another
    // (with per_host, once this one is benched).
    let rotated =
        q.request.proxy.is_none() && proxy.as_deref().is_some_and(|p| state.proxies.pooled(p));
    let can_retry = q.attempts < settings.retries;
    // A POST the server may have acted on is only sent again when asked.
    let may_repeat = q.request.outgoing.idempotent() || q.request.retry_post;
    let event = match (result, outcome) {
        (Ok(Hop::Done(response)), Some(Outcome::Blocked { vendor, kind })) => {
            state.stats.blocked += 1;
            if can_retry {
                let _ = new_session(&shared, &mut state, &q.host);
            }
            slow_down(settings, &mut state, &q.host, None, true);
            if can_retry {
                retry(settings, &mut state, q);
                None
            } else if settings.browser == BrowserMode::OnBlock
                && q.request.outgoing.method == "GET"
                && !state.is_download(&q)
            {
                // One more go, in Chrome, once the host's delay is up. Not a
                // retry: the retries are spent. Chrome replaying a POST could
                // submit it twice, so only GETs go.
                q.in_browser = true;
                q.blocked = Some(Box::new((vendor, kind, response)));
                requeue(&mut state, q, Duration::ZERO);
                None
            } else {
                Some(Event::Blocked {
                    id: q.request.id,
                    vendor,
                    kind,
                    response,
                })
            }
        }
        (Ok(Hop::Done(response)), Some(Outcome::Throttled { retry_after })) => {
            state.stats.throttled += 1;
            // With the throttle on, it has already doubled the delay for
            // this 429.
            slow_down(
                settings,
                &mut state,
                &q.host,
                retry_after,
                !settings.throttle,
            );
            if can_retry && settings.retry_statuses.contains(&response.status) {
                retry(settings, &mut state, q);
                None
            } else {
                Some(Event::Fetched {
                    id: q.request.id,
                    response,
                    page: None,
                })
            }
        }
        (Ok(Hop::Done(response)), Some(Outcome::HttpError(status)))
            if can_retry && may_repeat && settings.retry_statuses.contains(&status) =>
        {
            drop(response);
            retry(settings, &mut state, q);
            None
        }
        // A proxy that wants a login never passed the request on.
        (Ok(Hop::Done(response)), _) if response.status == 407 && rotated && can_retry => {
            drop(response);
            retry(settings, &mut state, q);
            None
        }
        (Ok(Hop::Done(response)), _) => {
            if reserved {
                Some(Event::Dropped {
                    id: q.request.id,
                    reason: DropReason::TdmReserved,
                })
            } else {
                Some(Event::Fetched {
                    id: q.request.id,
                    response,
                    page: None,
                })
            }
        }
        // A redirect to a page the crawl already has, from another link or
        // another redirect, ends here, as Scrapy's does: fetching it again
        // would give the same items twice.
        (Ok(Hop::Redirect(next, then, _)), _)
            if fingerprint_with(&then.method, &next, then.body.as_deref())
                .map(|fp| in_jar(fp, q.request.cookies.jar.as_deref()))
                .is_some_and(|fp| seen_elsewhere(&state.seen, &q.passed, fp)) =>
        {
            state.stats.duplicates += 1;
            Some(Event::Dropped {
                id: q.request.id,
                reason: DropReason::Duplicate,
            })
        }
        // The next hop is queued like a new request, on its own host, so it
        // goes through that host's robots.txt, TDMRep and limits. Its URL
        // counts as seen.
        (Ok(Hop::Redirect(next, then, cookies)), _) => {
            // A download's redirect isn't a page the crawl has seen.
            let seen = fingerprint_with(&then.method, &next, then.body.as_deref())
                .map(|fp| in_jar(fp, q.request.cookies.jar.as_deref()))
                .filter(|_| !state.is_download(&q));
            if let (Some(fp), Some(passed)) = (seen, &mut q.passed) {
                passed.push(fp);
            }
            if let Some(fp) = seen {
                state.seen.insert(fp);
                if let Some(cp) = &shared.checkpoint {
                    cp.seen(fp);
                }
            }
            // For a hit to do the same, with no hops.
            if let Some(keep) = &mut q.keep {
                keep.redirects.push(Redirect {
                    url: q.request.url.clone(),
                    cookies,
                    seen,
                });
            }
            let id = q.request.id;
            q.attempts = 0;
            // The crawl's own login is added at each hop, never kept with
            // the request: a checkpoint holds only what the request asked.
            let mut then = then;
            then.policy.auth = q.request.outgoing.policy.auth.take();
            q.request.outgoing = then;
            if enqueue(
                settings,
                &mut state,
                q.request,
                &next,
                q.hops + 1,
                q.passed,
                q.keep,
                false,
            ) {
                None
            } else {
                Some(Event::Failed {
                    id,
                    error: FetchError::invalid(format!(
                        "redirect to a URL netweir can't fetch: {next}"
                    )),
                })
            }
        }
        // A connection that never opened never carried the request; any
        // other failure may have come after the server acted on it.
        (Err(error), _)
            if can_retry
                && transient(&error)
                && (may_repeat || error.kind == FetchErrorKind::Connect) =>
        {
            retry(settings, &mut state, q);
            None
        }
        (Err(error), _) => Some(Event::Failed {
            id: q.request.id,
            error,
        }),
    };
    if let Some(event) = event {
        shared.push_event(&mut state, event);
    }
    drop(state);
    shared.schedule.notify_one();
}

/// Whether the cache keeps `r`: an answer that ended the request and will
/// be the same next time. Never a block, a throttle, a 402, a server error
/// or a status that would have been retried; nor a 401, a 403 or a 451,
/// which may be a block no signature knows yet, a 407 from the crawl's own
/// proxy, or a 304, which only makes sense to the request it answered.
fn worth_keeping(r: &Response, outcome: Option<&Outcome>, retried: &[u16]) -> bool {
    matches!(outcome, Some(Outcome::Ok | Outcome::HttpError(_)))
        && r.status < 500
        && !RETRY_STATUSES.contains(&r.status)
        && !retried.contains(&r.status)
        && ![304, 401, 403, 407, 451].contains(&r.status)
}

/// Hands the caller a hit, read from the cache, as the `Fetched` event a
/// fetch would have made.
async fn serve(shared: Arc<Shared>, q: Queued) {
    let read = match (shared.cache.clone(), q.keep.as_ref().map(|k| k.key)) {
        (Some(cache), Some(key)) => tokio::task::spawn_blocking(move || cache.get(&key))
            .await
            .unwrap_or_else(|e| Err(e.to_string())),
        _ => Ok(None),
    };
    let mut state = shared.lock();
    state.serving = false;
    let read = read.unwrap_or_else(|e| {
        cache_failed(&shared, &mut state, "read", &e, "fetching the page instead");
        None
    });
    match read {
        Some((mut response, redirects)) => {
            state.stats.cache_hits += 1;
            // The request's own cookies go into its jar as a fetch would
            // have put them, before what the kept response set.
            if let Some(keeper) = cookie_keeper(&shared, &state, &q.request, &q.request.url) {
                add_request_cookies(keeper.jar(), &q.request);
            }
            // What the fetch did along the way, done again: each hop's
            // cookies into the jar its host's requests use, and where each
            // led marked seen, or the request dropped where a fetch would
            // have dropped it.
            let mut passed = q.passed.clone();
            for r in &redirects {
                if let Some(keeper) = cookie_keeper(&shared, &state, &q.request, &r.url) {
                    keeper.add_set_cookies(&r.url, &q.request.outgoing, &r.cookies);
                }
                if let Some(fp) = r.seen {
                    if seen_elsewhere(&state.seen, &passed, fp) {
                        state.stats.duplicates += 1;
                        let event = Event::Dropped {
                            id: q.request.id,
                            reason: DropReason::Duplicate,
                        };
                        shared.push_event_from(&mut state, event, true);
                        drop(state);
                        shared.schedule.notify_one();
                        return;
                    }
                    if let Some(passed) = &mut passed {
                        passed.push(fp);
                    }
                    state.seen.insert(fp);
                    if let Some(cp) = &shared.checkpoint {
                        cp.seen(fp);
                    }
                }
            }
            let cookies: Vec<String> = response
                .headers
                .iter()
                .filter(|(k, _)| k == "set-cookie")
                .map(|(_, v)| v.clone())
                .collect();
            if let Some(keeper) = cookie_keeper(&shared, &state, &q.request, &response.url) {
                keeper.add_set_cookies(&response.url, &q.request.outgoing, &cookies);
            }
            // Kept under a key that ignores fragments and tracking
            // parameters, so without a redirect the URL is the one asked for,
            // written as a fetch would write it.
            if redirects.is_empty() {
                response.url =
                    Url::parse(&q.request.url).map_or_else(|_| q.request.url.clone(), String::from);
            }
            // `bytes` counts what was downloaded.
            let size = response.body.len() as u64;
            let event = Event::Fetched {
                id: q.request.id,
                response,
                page: None,
            };
            shared.push_event_from(&mut state, event, true);
            state.stats.bytes -= size;
        }
        // Gone or gone stale since it was admitted: fetched after all.
        None => {
            if !state.is_download(&q) {
                state.sent -= 1;
            }
            requeue(&mut state, q, Duration::ZERO);
        }
    }
    drop(state);
    shared.schedule.notify_one();
}

/// The session a request to `url` goes out with: its host's own, after a
/// block, or the crawl's.
fn session_for(shared: &Shared, state: &State, url: &str) -> Fetcher {
    Url::parse(url)
        .ok()
        .and_then(|u| state.hosts.get(u.host_str()?)?.session.clone())
        .unwrap_or_else(|| shared.fetcher.clone())
}

/// What keeps the cookies a response to `request` at `url` sets: its
/// named jar, or its host's session; nothing when it leaves the jar out.
fn cookie_keeper(
    shared: &Shared,
    state: &State,
    request: &CrawlRequest,
    url: &str,
) -> Option<Fetcher> {
    if request.cookies.dont_merge {
        return None;
    }
    let session = session_for(shared, state, url);
    Some(match request.cookies.named() {
        Some(name) => session.with_jar(shared.jar(Some(name))),
        None => session,
    })
}

/// Tells the caller the cache failed, once per crawl: a disk that's full
/// or a file another hand broke would otherwise say so for every page.
fn cache_failed(shared: &Shared, state: &mut State, doing: &str, error: &str, instead: &str) {
    if state.warned_cache {
        return;
    }
    state.warned_cache = true;
    let path = shared
        .cache
        .as_ref()
        .map(|c| c.path().display().to_string());
    let message = format!(
        "the cache at {} couldn't be {doing}: {error}; {instead}",
        path.unwrap_or_default()
    );
    shared.push_event(state, Event::Warning { message });
}

/// How long a challenge page gets to pass and move on in Chrome.
const CHALLENGE_WAIT: Duration = Duration::from_secs(20);

/// What `render` brings back: the response, the open page, its cookies
/// when asked for, how long the page took to load, and Chrome's version.
type Rendered = (Response, LivePage, Vec<Cookie>, Duration, String);

/// Fetches `q` in Chrome, in the page slot `permit` holds, and deals with
/// the outcome as `fetch` does for the HTTP client. A page that's still
/// blocked isn't retried in Chrome.
async fn browser_fetch(shared: Arc<Shared>, mut q: Queued, permit: OwnedSemaphorePermit) {
    let timeout = shared.options.timeout;
    let steps = match &q.request.steps {
        Some(own) => own.steps.clone(),
        None if q.blocked.is_some() => lenient(&shared.settings.browser_steps),
        None => shared.settings.browser_steps.clone(),
    };
    let limit = fetch_limit(timeout, &steps, &shared.settings);
    // Set if robots.txt or TDMRep stopped a document Chrome was about to
    // fetch: a redirect hop, or a navigation the page's script started.
    let refused: Refusal = Arc::default();
    // What render is doing, for the error if it runs out of time.
    let doing: Doing = Arc::new(Mutex::new("starting"));
    let work = render(
        shared.clone(),
        (q.request.id, q.request.url.clone(), q.request.depth, steps),
        q.blocked.is_some(),
        browser_cookies(&shared, &q.request),
        permit,
        refused.clone(),
        doing.clone(),
    );
    let rendered = guarded(
        async move {
            tokio::time::timeout(limit, work).await.unwrap_or_else(|_| {
                Err(FetchError {
                    kind: FetchErrorKind::Timeout,
                    message: format!(
                        "Chrome took more than {}s, and was still {}",
                        limit.as_secs(),
                        doing.lock().unwrap_or_else(|e| e.into_inner())
                    ),
                    steps: Default::default(),
                })
            })
        },
        |why| {
            Err(FetchError {
                kind: FetchErrorKind::Other,
                message: format!("the browser fetch failed inside netweir: {why}"),
                steps: Default::default(),
            })
        },
    )
    .await;
    let settings = &shared.settings;
    let outcome = rendered.as_ref().ok().map(|(r, ..)| classify(r));
    let mut state = shared.lock();
    state.stats.in_flight -= 1;
    if let Some(host) = state.hosts.get_mut(&q.host) {
        host.in_flight -= 1;
        if settings.throttle
            && let Ok((r, _, _, latency, _)) = &rendered
        {
            host.delay = adjust_delay(settings, host.delay, *latency, Some(r.status));
        } else if !settings.throttle && matches!(outcome, Some(Outcome::Ok)) {
            // As over HTTP: a good response halves a delay a block or a 429
            // added, with no throttle to bring it back down.
            host.delay = (host.delay / 2).max(settings.min_delay);
        }
    }
    if let Some(outcome) = &outcome {
        state.stats.browser_fetches += 1;
        breaker(
            &shared,
            &mut state,
            &q.host,
            matches!(outcome, Outcome::Blocked { .. }),
        );
    }
    state.make_ready(&q.host);
    let refusal = *refused.lock().unwrap_or_else(|e| e.into_inner());
    // Pages to close once the lock is released.
    let mut discard = None;
    let event = match (rendered, outcome) {
        (Ok((response, page, ..)), Some(Outcome::Blocked { vendor, kind })) => {
            discard = Some(page);
            state.stats.blocked += 1;
            slow_down(settings, &mut state, &q.host, None, true);
            Some(Event::Blocked {
                id: q.request.id,
                vendor,
                kind,
                response,
            })
        }
        // A server error is tried again, as the HTTP client would, unless
        // this is a blocked request's one go in Chrome.
        (Ok((response, page, ..)), Some(Outcome::HttpError(status)))
            if q.blocked.is_none()
                && q.attempts < settings.retries
                && settings.retry_statuses.contains(&status) =>
        {
            discard = Some(page);
            drop(response);
            retry(settings, &mut state, q);
            None
        }
        // Throttled (a 429, or a 503 with Retry-After): slowed down and
        // tried again, as over HTTP.
        (Ok((response, page, ..)), Some(Outcome::Throttled { retry_after })) => {
            state.stats.throttled += 1;
            slow_down(
                settings,
                &mut state,
                &q.host,
                retry_after,
                !settings.throttle,
            );
            if q.blocked.is_none()
                && q.attempts < settings.retries
                && settings.retry_statuses.contains(&response.status)
            {
                discard = Some(page);
                drop(response);
                retry(settings, &mut state, q);
                None
            } else {
                Some(Event::Fetched {
                    id: q.request.id,
                    response,
                    page: Some(page),
                })
            }
        }
        (Ok((response, page, cookies, _, version)), outcome) => {
            // Chrome follows redirects itself: where it ended up counts as
            // seen, as a redirect the HTTP client followed would. With the
            // steps that got it there when they count toward the request's
            // fingerprint: the same URL without them is another page.
            if let Some(fp) = fingerprint("GET", &response.url)
                .map(|fp| in_jar(fp, q.request.cookies.jar.as_deref()))
                .map(|fp| match &q.request.steps {
                    Some(own) if own.in_fingerprint => with_steps(fp, &own.steps),
                    _ => fp,
                })
                && state.seen.insert(fp)
                && let Some(cp) = &shared.checkpoint
            {
                cp.seen(fp);
            }
            if q.blocked.is_some() && matches!(outcome, Some(Outcome::Ok)) {
                state.stats.browser_unblocked += 1;
                // A named jar's cookies (a login, say) go back to that jar
                // when the page closes, never to the host's session, which
                // is another visitor; nor do those of a request that leaves
                // the jar out.
                let own = &q.request.cookies;
                let cookies = if own.dont_merge || own.named().is_some() {
                    Vec::new()
                } else {
                    cookies
                };
                let said = page.page().request_headers();
                if let Some(message) =
                    hand_back(&shared, &mut state, &q.host, &cookies, &version, &said)
                {
                    shared.push_event(&mut state, Event::Warning { message });
                }
            }
            // Chrome may have landed elsewhere: its page is judged by the
            // site it came from.
            let landed =
                origin_and_path(&response.url).unwrap_or((q.origin.clone(), q.path.clone()));
            if settings.obey_tdmrep && page_reserved(&state, &landed.0, &landed.1, &response) {
                discard = Some(page);
                Some(Event::Dropped {
                    id: q.request.id,
                    reason: DropReason::TdmReserved,
                })
            } else {
                Some(Event::Fetched {
                    id: q.request.id,
                    response,
                    page: Some(page),
                })
            }
        }
        (Err(_), _) if refusal.is_some() => Some(Event::Dropped {
            id: q.request.id,
            reason: refusal.expect("checked"),
        }),
        (Err(error), _) if q.attempts < settings.retries && transient(&error) => {
            retry(settings, &mut state, q);
            None
        }
        (Err(error), _) => match q.blocked.take() {
            // Chrome didn't get anywhere: it's still the block it was. Say
            // why once, or escalation would fail without a word.
            Some(blocked) => {
                if !state.warned_browser {
                    state.warned_browser = true;
                    let message = format!(
                        "a blocked request couldn't be tried in Chrome, so on_block gets it: {}",
                        error.message
                    );
                    shared.push_event(&mut state, Event::Warning { message });
                }
                let (vendor, kind, response) = *blocked;
                Some(Event::Blocked {
                    id: q.request.id,
                    vendor,
                    kind,
                    response,
                })
            }
            None => Some(Event::Failed {
                id: q.request.id,
                error,
            }),
        },
    };
    if let Some(event) = event {
        shared.push_event(&mut state, event);
    }
    drop(state);
    drop(discard);
    shared.schedule.notify_one();
}

/// Loads `url` in a new Chrome page, runs `steps` on it, and reads it
/// back. A challenge page is given time to pass and move on, before the
/// steps and after them. With `cookies`, also returns the page's cookies,
/// as the steps left them. `id` is the request's, for call steps.
async fn render(
    shared: Arc<Shared>,
    (id, url, depth, steps): (u64, String, u32, Arc<[Step]>),
    cookies: bool,
    (seed, jar): (Seed, Option<Arc<Jar>>),
    permit: OwnedSemaphorePermit,
    refused: Refusal,
    doing: Doing,
) -> Result<Rendered, FetchError> {
    let now = |what: &'static str| *doing.lock().unwrap_or_else(|e| e.into_inner()) = what;
    now("waiting for Chrome to start");
    let browser = {
        let mut slot = shared.browser.lock().await;
        let start = match &*slot {
            None => true,
            Some(Ok(b)) => b.is_closed(),
            Some(Err((_, when))) => when.elapsed() > Duration::from_secs(60),
        };
        if start {
            let mut launched = Browser::launch(browser_launch(&shared))
                .await
                .map_err(|e| e.to_string());
            // The shell fetches pages ahead of time where the guard below
            // never sees them, so a crawl that has to ask first can't use
            // it.
            if let Ok(browser) = &launched
                && browser.is_headless_shell()
                && (shared.settings.obey_robots || shared.settings.obey_tdmrep)
            {
                let _ = browser.close().await;
                launched = Err(
                    "chrome-headless-shell fetches the pages a page's speculation rules name \
                     before robots.txt and TDMRep are checked, so a crawl that obeys them won't \
                     use it: use Chrome (`netweir install chrome`, without --headless-shell), \
                     or turn obey_robots and obey_tdmrep off if you mean to"
                        .to_string(),
                );
            }
            *slot = Some(launched.map_err(|e| (e, Instant::now())));
        }
        slot.clone().expect("set above").map_err(|(e, _)| e)
    }
    .map_err(|e| FetchError {
        kind: FetchErrorKind::Other,
        message: format!("can't start Chrome: {e}"),
        steps: Default::default(),
    })?;
    let guarded = shared.settings.obey_robots || shared.settings.obey_tdmrep;
    now("opening a page");
    let page = if guarded {
        // Chrome follows redirects, and the page's scripts navigate, by
        // itself; each document is checked against its site's robots.txt
        // and TDMRep before Chrome asks for it, as the HTTP client checks
        // each hop it follows.
        let guarding = shared.clone();
        let refusing = refused.clone();
        let guard: netweir_browser::Guard = Arc::new(move |url: String| {
            let (shared, refused) = (guarding.clone(), refusing.clone());
            Box::pin(async move {
                let refusal = refusal_of(&shared, &url).await;
                if refusal.is_some() {
                    *refused.lock().unwrap_or_else(|e| e.into_inner()) = refusal;
                }
                refusal.is_none()
            })
        });
        browser.new_guarded_page(guard).await
    } else if jar.is_some() || !seed.cookies.is_empty() {
        // A jar's cookies stay in a context of their own, never a kept
        // profile's, where other pages and the next run would see them.
        browser.new_page_apart().await
    } else {
        browser.new_page().await
    }
    .map_err(browser_error)?;
    let blocked = &shared.settings.browser_block;
    if !blocked.is_empty() {
        let types: Vec<&str> = blocked.iter().map(String::as_str).collect();
        if let Err(e) = page.block(&types, &[]).await {
            let _ = page.close().await;
            return Err(browser_error(e));
        }
    }
    // The page starts with the cookies of the request's jar, or its own.
    if !seed.cookies.is_empty()
        && let Err(e) = page.set_cookies(&seed.cookies).await
    {
        let _ = page.close().await;
        return Err(browser_error(e));
    }
    let live = LivePage {
        inner: Arc::new(Live {
            page: page.clone(),
            permit: Mutex::new(Some(permit)),
            runtime: shared.runtime.clone(),
            shared: Arc::downgrade(&shared),
            jar: jar.map(|jar| (jar, seed)),
        }),
    };
    let timeout = shared.options.timeout;
    let started = Instant::now();
    // A guard may have to fetch a site's robots.txt or tdmrep.json, each
    // with the request timeout, before the page itself can load.
    let load_limit = if guarded {
        timeout.saturating_mul(3)
    } else {
        timeout
    };
    now("loading the page");
    let document = page
        .goto(&url, WaitUntil::Load, Some(load_limit))
        .await
        .map_err(browser_error)?;
    let latency = started.elapsed();
    let mut response = settle(&page, document, CHALLENGE_WAIT.min(timeout), &now).await?;
    if !steps.is_empty() {
        let report = match classify(&response) {
            Outcome::Ok => {
                now("running its steps");
                let call = caller(&shared, (id, &url, depth), &live);
                let cap = shared.settings.browser_steps_max;
                let report = steps::run(&page, &steps, &*call, cap).await;
                {
                    let mut state = shared.lock();
                    let ran = report
                        .outcomes
                        .iter()
                        .filter(|o| o.status != steps::Status::Skipped);
                    for o in ran {
                        state.stats.browser_steps += 1;
                        if o.status == steps::Status::Failed {
                            state.stats.browser_steps_failed += 1;
                        }
                    }
                }
                // A step may have moved the page on, to a challenge as
                // likely as anywhere: given the same time to pass.
                response =
                    settle(&page, page.response(), CHALLENGE_WAIT.min(timeout), &now).await?;
                report
            }
            Outcome::Blocked { .. } => Report::skipped(&steps, "the page was blocked"),
            _ => Report::skipped(&steps, &format!("the page answered {}", response.status)),
        };
        let blocked = matches!(classify(&response), Outcome::Blocked { .. });
        let outcomes: Arc<[steps::Outcome]> = report.outcomes.clone().into();
        // A step that failed on a page that's now blocked failed for the
        // block, which is what the request hears about.
        if let (Some(why), false) = (report.failure(), blocked) {
            return Err(FetchError {
                kind: FetchErrorKind::Step,
                message: why,
                steps: outcomes,
            });
        }
        response.steps = outcomes;
    }
    // A navigation that was refused leaves Chrome's error page showing,
    // which isn't the page.
    if refused.lock().unwrap_or_else(|e| e.into_inner()).is_some()
        && (response.status == 0 || response.url.starts_with("chrome-error://"))
    {
        return Err(FetchError {
            kind: FetchErrorKind::Other,
            message: "Chrome was refused the page it was going to".into(),
            steps: Default::default(),
        });
    }
    now("reading its cookies");
    let cookies = if cookies {
        page.cookies().await.map_err(browser_error)?
    } else {
        Vec::new()
    };
    Ok((
        response,
        live,
        cookies,
        latency,
        browser.version().to_string(),
    ))
}

/// The page as it is once a script that moved it on has its document
/// loaded, and a challenge showing has had until `wait` to pass, starting
/// from `document`, the one it was showing.
async fn settle(
    page: &netweir_browser::Page,
    mut document: netweir_browser::Response,
    wait: Duration,
    now: &impl Fn(&'static str),
) -> Result<Response, FetchError> {
    let deadline = Instant::now() + wait;
    loop {
        // A script may already have moved the page on (a challenge that
        // passes at once does): read the newest document, once loaded.
        if page.response().loader != document.loader {
            now("loading the page a script moved on to");
            let left = deadline.saturating_duration_since(Instant::now());
            match page
                .wait_for_navigation(&document, WaitUntil::Load, Some(left))
                .await
            {
                Ok(next) => document = next,
                Err(netweir_browser::Error::Timeout(_)) => {}
                Err(e) => return Err(browser_error(e)),
            }
        }
        now("reading the page");
        let response = rendered(&document, content(page).await?);
        let challenged = matches!(
            classify(&response),
            Outcome::Blocked {
                kind: BlockKind::Challenge,
                ..
            }
        );
        let left = deadline.saturating_duration_since(Instant::now());
        if !challenged || left.is_zero() {
            return Ok(response);
        }
        now("waiting for a challenge to pass");
        match page
            .wait_for_navigation(&document, WaitUntil::Load, Some(left))
            .await
        {
            Ok(next) => document = next,
            // Out of time. One last look, for a challenge that swaps the
            // page's content without navigating.
            Err(netweir_browser::Error::Timeout(_)) => {
                now("reading the page");
                return Ok(rendered(&page.response(), content(page).await?));
            }
            Err(e) => return Err(browser_error(e)),
        }
    }
}

/// Runs the call steps of request `id`'s page by sending them to whoever
/// took `step_calls`; with nobody there, each fails at once.
fn caller(
    shared: &Arc<Shared>,
    (id, url, depth): (u64, &str, u32),
    live: &LivePage,
) -> Box<steps::Call> {
    let (shared, live, url) = (shared.clone(), live.clone(), url.to_string());
    Box::new(move |index, method, timeout| {
        let taken = shared
            .called
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
            && !shared
                .calls_closed
                .load(std::sync::atomic::Ordering::SeqCst);
        let (reply, answer) = tokio::sync::oneshot::channel();
        let call = StepCall {
            id,
            url: url.clone(),
            depth,
            index,
            method,
            page: live.clone(),
            timeout,
            reply,
        };
        let sent = taken && shared.calls.send(call).is_ok();
        Box::pin(async move {
            if !sent {
                return Err("nothing here runs call steps".to_string());
            }
            answer
                .await
                .unwrap_or_else(|_| Err("the call step was never answered".to_string()))
        })
    })
}

/// The crawl's steps for the one go Chrome gets at a request blocked over
/// HTTP: a step failing there (a banner this site doesn't show) goes on
/// to the next rather than fail a request Chrome may have got through.
fn lenient(steps: &Arc<[Step]>) -> Arc<[Step]> {
    steps
        .iter()
        .map(|s| Step {
            on_error: match s.on_error {
                steps::OnError::Fail => steps::OnError::Continue,
                other => other,
            },
            ..s.clone()
        })
        .collect()
}

/// How long a Chrome fetch may take in all, so a Chrome that stops
/// answering mid-page can't hold the request, and its place in the crawl,
/// for good: twice the request timeout and a challenge's wait, and for
/// steps, their own time (no more than `browser_steps_max`) and as long
/// again for a challenge they lead to.
fn fetch_limit(timeout: Duration, steps: &[Step], settings: &CrawlSettings) -> Duration {
    let mut limit = timeout.saturating_mul(2).saturating_add(CHALLENGE_WAIT);
    if !steps.is_empty() {
        let budget = steps::budget(
            steps,
            settings.browser_launch.timeout,
            settings.browser_steps_max,
        );
        limit = limit.saturating_add(budget).saturating_add(CHALLENGE_WAIT);
    }
    limit
}

/// Why a document Chrome is about to fetch may not be: robots.txt or
/// TDMRep, if the crawl obeys them.
type Refusal = Arc<Mutex<Option<DropReason>>>;

/// What a Chrome fetch is doing, in words that finish "still ...".
type Doing = Arc<Mutex<&'static str>>;

/// A URL's origin and path (with its query), as the scheduler keys them.
fn origin_and_path(url: &str) -> Option<(String, String)> {
    let parsed = Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let path = match parsed.query() {
        Some(q) => format!("{}?{q}", parsed.path()),
        None => parsed.path().to_string(),
    };
    Some((parsed.origin().ascii_serialization(), path))
}

/// Why the crawl may not fetch `url`, if it may not: its site's robots.txt
/// or TDMRep file, each fetched first, as the scheduler would, if it isn't
/// known yet.
async fn refusal_of(shared: &Arc<Shared>, url: &str) -> Option<DropReason> {
    let settings = &shared.settings;
    let (origin, path) = origin_and_path(url)?;
    let host = Url::parse(url).ok()?.host_str()?.to_string();
    if settings.obey_robots {
        loop {
            let fetch = {
                let mut state = shared.lock();
                let o = state.origins.entry(origin.clone()).or_default();
                match &o.robots {
                    Gate::Ready(r) if r.allowed(&settings.robots_agent, &path) => break,
                    Gate::Ready(_) => return Some(DropReason::Robots),
                    Gate::Fetching => None,
                    Gate::Unknown => {
                        o.robots = Gate::Fetching;
                        state.gate_fetches += 1;
                        Some(gate_route(shared, &mut state, &origin, &host, None))
                    }
                }
            };
            if let Some(route) = fetch {
                check_robots(shared.clone(), origin.clone(), route).await;
            } else {
                // Known limit: another fetch of it is under way; polled, as
                // this is one request's wait, not the scheduler's.
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    if settings.obey_tdmrep {
        loop {
            let fetch = {
                let mut state = shared.lock();
                let o = state.origins.entry(origin.clone()).or_default();
                match &o.tdm {
                    Gate::Ready(Some(f)) if f.reservation(&path).reserved == Some(true) => {
                        return Some(DropReason::TdmReserved);
                    }
                    Gate::Ready(_) => break,
                    Gate::Fetching => None,
                    Gate::Unknown => {
                        o.tdm = Gate::Fetching;
                        state.gate_fetches += 1;
                        Some(gate_route(shared, &mut state, &origin, &host, None))
                    }
                }
            };
            if let Some(route) = fetch {
                check_tdm(shared.clone(), origin.clone(), route).await;
            } else {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    None
}

/// The page's HTML, read again if a script moved the page on mid-read
/// (Chrome says the isolated world's context is gone).
async fn content(page: &netweir_browser::Page) -> Result<String, FetchError> {
    let mut tries = 0;
    loop {
        match page.content().await {
            Err(netweir_browser::Error::Protocol { message, .. })
                if message.contains("context") && tries < 3 =>
            {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            other => return other.map_err(browser_error),
        }
    }
}

/// How to start Chrome for this crawl: through the crawl's proxy, login
/// and all, so the cookies Chrome earns come from the address the HTTP
/// client uses. With per_request or per_host rotation there's no crawl
/// proxy, and Chrome takes the next healthy one of `proxies`: it has one
/// proxy for all its pages, so rotation stops at its door.
fn browser_launch(shared: &Shared) -> LaunchOptions {
    let mut launch = shared.settings.browser_launch.clone();
    if launch.engine == netweir_browser::Engine::Firefox {
        // Firefox takes no proxy here, and has no client hints to brand.
        return launch;
    }
    let mut state = shared.lock();
    if launch.proxy.is_none() {
        launch.proxy = match shared.settings.proxy_rotation {
            ProxyRotation::OnBlock => shared.options.proxy.clone(),
            ProxyRotation::PerRequest | ProxyRotation::PerHost => {
                let picked = pick_where(shared, &mut state, None, chrome_can_use);
                if picked.is_none() && !state.warned_chrome_proxy {
                    state.warned_chrome_proxy = true;
                    let message = "Chrome can't log in to a SOCKS proxy, and every one of \
                                   proxies is one with a login, so Chrome goes out without a proxy"
                        .to_string();
                    shared.push_event(&mut state, Event::Warning { message });
                }
                picked
            }
        };
    }
    state.chrome_proxy = launch.proxy.clone();
    drop(state);
    if launch.brands.is_none() {
        launch.brands = Some(Arc::new(Profile::chrome_brands));
    }
    launch
}

/// Whether Chrome can go through `proxy`: it can't log in to a SOCKS one.
fn chrome_can_use(proxy: &str) -> bool {
    let socks = proxy.to_ascii_lowercase().starts_with("socks");
    let login = proxy
        .split_once("://")
        .is_some_and(|(_, rest)| rest.contains('@'));
    !(socks && login)
}

/// A Response for a page Chrome rendered: the document's status and
/// headers, and the HTML as it is now. The body is UTF-8 whatever the
/// server sent, so the content type says so, and nothing about the body's
/// original encoding or length still applies.
fn rendered(document: &netweir_browser::Response, html: String) -> Response {
    let mut headers: Vec<(String, String)> = document
        .headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
        .filter(|(k, _)| {
            !matches!(
                k.as_str(),
                "content-encoding" | "content-length" | "transfer-encoding"
            )
        })
        .collect();
    let mime = headers
        .iter()
        .find(|(k, _)| k == "content-type")
        .and_then(|(_, v)| v.split(';').next())
        .map_or("text/html", str::trim)
        .to_string();
    headers.retain(|(k, _)| k != "content-type");
    headers.push(("content-type".into(), format!("{mime}; charset=utf-8")));
    Response {
        url: document.url.clone(),
        status: document.status,
        version: "browser",
        headers,
        body: html.into(),
        steps: Default::default(),
    }
}

fn browser_error(e: netweir_browser::Error) -> FetchError {
    use netweir_browser::{Error, NetFailure};
    let kind = match (&e, e.net_failure()) {
        (Error::Timeout(_), _) | (_, Some(NetFailure::Timeout)) => FetchErrorKind::Timeout,
        (_, Some(NetFailure::Tls)) => FetchErrorKind::Tls,
        (_, Some(NetFailure::Connect)) => FetchErrorKind::Connect,
        _ => FetchErrorKind::Other,
    };
    FetchError {
        kind,
        message: format!("in Chrome: {e}"),
        steps: Default::default(),
    }
}

/// Gives the host an HTTP session holding the cookies the browser earned,
/// so its next requests don't need it. A site may tie those cookies to
/// the browser they were issued to, so the session looks like the same
/// Chrome or Firefox when netweir has its profile. When it doesn't, the session keeps
/// the crawl's profile and a warning (returned, once) says so.
fn hand_back(
    shared: &Shared,
    state: &mut State,
    host: &str,
    cookies: &[Cookie],
    version: &str,
    said: &[(String, String)],
) -> Option<String> {
    // "Chrome/154.0.8037.98" or "Firefox/159.0.1".
    let (family, number) = version.split_once('/').unwrap_or(("Chrome", version));
    let family = family.to_ascii_lowercase();
    let major = number.split('.').next().unwrap_or_default();
    let mut options = shared.options.clone();
    let mut warning = None;
    if !options
        .profile
        .name
        .starts_with(&format!("{family}-{major}-"))
    {
        match Profile::for_browser(&family, major) {
            Some(profile) => options.profile = profile,
            None if !state.warned_profile => {
                state.warned_profile = true;
                warning = Some(format!(
                    "the installed {version} has no matching netweir profile, so cookies it earned \
                     go to requests that look like {}; a site may not accept them",
                    options.profile.name
                ));
            }
            None => {}
        }
    }
    // And to the browser Chrome said it was: its language, and a phone's
    // user agent and client hints.
    for (name, value) in said {
        options.profile.set_header(name, value);
    }
    // The cookies are tied to the address Chrome went out from.
    options.proxy = state.chrome_proxy.clone();
    let pooled = state
        .chrome_proxy
        .clone()
        .filter(|p| state.proxies.pooled(p));
    if let (Ok(fetcher), Some(h)) = (Fetcher::new(options.clone()), state.hosts.get_mut(host)) {
        fetcher.add_cookies(cookies);
        h.session = Some(fetcher);
        h.options = Some(options);
        h.proxy = pooled;
        h.via.clear();
        state.stats.sessions_replaced += 1;
    }
    warning
}

/// Statuses worth another try by default: timeouts, throttling, and
/// overloaded or unreachable upstreams (522 and 524 are Cloudflare's).
/// Scrapy's RETRY_HTTP_CODES are the same.
pub const RETRY_STATUSES: [u16; 8] = [408, 429, 500, 502, 503, 504, 522, 524];

/// Network failures that may pass on another try.
fn transient(error: &FetchError) -> bool {
    matches!(
        error.kind,
        FetchErrorKind::Timeout
            | FetchErrorKind::Connect
            | FetchErrorKind::Body
            | FetchErrorKind::Other
    )
}

/// Puts `q` back on its host's queue, after a backoff.
fn retry(settings: &CrawlSettings, state: &mut State, mut q: Queued) {
    q.attempts += 1;
    state.stats.retries += 1;
    let cap = settings
        .backoff_base
        .saturating_mul(1u32 << q.attempts.min(20))
        .min(settings.backoff_max);
    let wait = cap.mul_f64(random_fraction(q.seq));
    requeue(state, q, wait);
}

/// Puts `q` back on its host's queue, to start no sooner than `wait`.
fn requeue(state: &mut State, mut q: Queued, wait: Duration) {
    let host = q.host.clone();
    state.seq += 1;
    q.seq = state.seq;
    if let Some(h) = state.hosts.get_mut(&host) {
        h.next_at = h.next_at.max(Instant::now() + wait);
        h.queue.push(q);
        state.stats.queued += 1;
    }
    state.make_ready(&host);
}

/// A number in [0, 1) that differs per call; for jitter, not secrets.
fn random_fraction(salt: u64) -> f64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(salt);
    (h.finish() >> 11) as f64 / (1u64 << 53) as f64
}

/// Blocks and throttling count against a host's pace: its delay doubles
/// (to at least half a second), and a Retry-After pushes its next request
/// back.
fn slow_down(
    settings: &CrawlSettings,
    state: &mut State,
    host: &str,
    wait: Option<Duration>,
    double: bool,
) {
    let Some(h) = state.hosts.get_mut(host) else {
        return;
    };
    if double {
        h.delay = h
            .delay
            .saturating_mul(2)
            .max(Duration::from_millis(500))
            .min(settings.max_delay);
    }
    // From now, not from the host's next turn, which was set before.
    let wait = wait
        .unwrap_or_default()
        .min(settings.max_delay)
        .max(h.delay);
    h.next_at = h.next_at.max(Instant::now() + wait);
}

/// Replaces the host's session: a fresh cookie jar, through the next
/// healthy proxy (another than the host had, if there is one) when there
/// are proxies. With per_request rotation, the host's cookies leave the
/// crawl's jar instead, and its requests go back to the rotation.
fn new_session(shared: &Shared, state: &mut State, host: &str) -> Result<(), FetchError> {
    new_session_through(shared, state, host, None)
}

/// `new_session`, through `proxy` when it's given.
fn new_session_through(
    shared: &Shared,
    state: &mut State,
    host: &str,
    proxy: Option<String>,
) -> Result<(), FetchError> {
    let mode = shared.settings.proxy_rotation;
    let Some(h) = state.hosts.get_mut(host) else {
        return Ok(());
    };
    if mode == ProxyRotation::PerRequest {
        shared.fetcher.forget(host);
        h.session = None;
        h.proxy = None;
        h.options = None;
        h.via.clear();
        state.stats.sessions_replaced += 1;
        return Ok(());
    }
    // A host's first session, with per_host, replaces nothing.
    let replacing = h.session.is_some() || mode == ProxyRotation::OnBlock;
    let current = h.proxy.clone();
    let proxy = proxy.or_else(|| pick(shared, state, current.as_deref()));
    let mut options = shared.options.clone();
    if proxy.is_some() {
        options.proxy = proxy.clone();
    }
    let fetcher = Fetcher::new(options.clone())?;
    if let Some(h) = state.hosts.get_mut(host) {
        h.session = Some(fetcher);
        h.proxy = proxy;
        h.options = Some(options);
        h.via.clear();
    }
    if replacing {
        state.stats.sessions_replaced += 1;
    }
    Ok(())
}

/// The session and proxy a request to host `name` goes out with; `wanted`
/// is the request's own proxy. A host whose proxy is benched moves to a
/// healthy one first, if there is one, and with per_host a host's first
/// request gets it its proxy.
fn route(shared: &Shared, state: &mut State, name: &str, wanted: Option<&str>) -> Route {
    let mode = shared.settings.proxy_rotation;
    let Some(h) = state.hosts.get(name) else {
        // A site Chrome went to on its own: no session to keep to.
        return match mode {
            ProxyRotation::OnBlock => (Ok(shared.fetcher.clone()), None),
            _ => rotate(shared, state),
        };
    };
    let now = Instant::now();
    let benched = h
        .proxy
        .as_deref()
        .is_some_and(|p| state.proxies.benched(p, now));
    // Off a benched proxy: to a healthy one, or with every one benched, to
    // the one back soonest, if that's another.
    let moving = benched
        && (state.proxies.any_healthy(now) || state.proxies.soonest() != h.proxy.as_deref());
    // A per_host site gets its proxy when a request first takes the
    // rotation; one whose requests name their own never does.
    let unassigned = mode == ProxyRotation::PerHost && h.session.is_none() && wanted.is_none();
    if (moving || unassigned)
        && let Err(e) = new_session(shared, state, name)
    {
        return (Err(e), None);
    }
    let h = state.hosts.get_mut(name).expect("looked up above");
    if let Some(proxy) = wanted {
        let fetcher = match (h.session.clone(), h.via.get(proxy)) {
            (Some(_), Some(fetcher)) => Ok(fetcher),
            (Some(session), None) => {
                let options = h.options.clone().unwrap_or_else(|| shared.options.clone());
                let made = Fetcher::sharing_cookies(with_proxy(options, proxy), &session);
                if let Ok(fetcher) = &made {
                    h.via.insert(proxy, fetcher.clone());
                }
                made
            }
            (None, _) => through(shared, state, proxy),
        };
        // An empty proxy is no proxy at all, which has no stats.
        return (fetcher, Some(proxy.to_string()).filter(|p| !p.is_empty()));
    }
    if let Some(session) = &h.session {
        return (Ok(session.clone()), h.proxy.clone());
    }
    match mode {
        ProxyRotation::PerRequest => rotate(shared, state),
        // Never without a session: it would go out with no proxy at all.
        ProxyRotation::PerHost => (
            Err(FetchError::invalid(
                "the host has no session through a proxy",
            )),
            None,
        ),
        ProxyRotation::OnBlock => (Ok(shared.fetcher.clone()), None),
    }
}

/// The crawl's cookies, through the next healthy proxy.
fn rotate(shared: &Shared, state: &mut State) -> Route {
    match pick(shared, state, None) {
        Some(proxy) => (through(shared, state, &proxy), Some(proxy)),
        None => (
            Err(FetchError::invalid("no proxies to rotate through")),
            None,
        ),
    }
}

/// The crawl's cookies, through `proxy`.
fn through(shared: &Shared, state: &mut State, proxy: &str) -> Result<Fetcher, FetchError> {
    if let Some(fetcher) = state.via.get(proxy) {
        return Ok(fetcher);
    }
    let options = with_proxy(shared.options.clone(), proxy);
    let fetcher = Fetcher::sharing_cookies(options, &shared.fetcher)?;
    state.via.insert(proxy, fetcher.clone());
    Ok(fetcher)
}

/// `options` through `proxy`; an empty one is none at all, not even the
/// crawl's own.
fn with_proxy(mut options: FetchOptions, proxy: &str) -> FetchOptions {
    options.proxy = Some(proxy.to_string()).filter(|p| !p.is_empty());
    options
}

/// The next healthy proxy in turn, another than `avoid` if there is one.
/// When every proxy is benched, the one back soonest, and a warning (once,
/// until one is healthy again): the crawl slows, but doesn't stall. None
/// without proxies.
fn pick(shared: &Shared, state: &mut State, avoid: Option<&str>) -> Option<String> {
    pick_where(shared, state, avoid, |_| true)
}

/// `pick`, among the proxies `usable` takes.
fn pick_where(
    shared: &Shared,
    state: &mut State,
    avoid: Option<&str>,
    usable: impl Fn(&str) -> bool,
) -> Option<String> {
    let (url, warn) = match state.proxies.pick(Instant::now(), avoid, usable) {
        Picked::Healthy(url) => return Some(url),
        Picked::Soonest(url, warn) => (url, warn),
        Picked::None => return None,
    };
    if warn {
        let message = format!(
            "every proxy is benched; requests go through {}, the first back, until one is healthy",
            masked(&url)
        );
        shared.push_event(state, Event::Warning { message });
    }
    Some(url)
}

/// Counts a request through `proxy`. One of the pool that has failed
/// `proxy_failures` times in a row is benched for `proxy_cooldown`, twice
/// that each time after, until a success clears its record.
fn record(shared: &Shared, state: &mut State, proxy: &str, failed: bool) {
    let settings = &shared.settings;
    let now = Instant::now();
    let pool = &mut state.proxies;
    let k = pool.entry(proxy);
    let pooled = k < pool.pooled;
    let e = &mut pool.entries[k];
    e.stats.requests += 1;
    if !failed {
        e.run = 0;
        e.level = 0;
        e.until = None;
        return;
    }
    e.stats.failures += 1;
    e.run += 1;
    // One back from the bench goes straight back on it if it fails again.
    if !pooled || e.run < settings.proxy_failures || e.benched(now) {
        return;
    }
    let cooldown = settings
        .proxy_cooldown
        .saturating_mul(1 << e.level.min(20))
        .min(MAX_COOLDOWN.max(settings.proxy_cooldown));
    e.level += 1;
    e.until = Some(now + cooldown);
    e.stats.benched += 1;
    let failed = match e.run {
        1 => "once".to_string(),
        n => format!("{n} times in a row"),
    };
    let message = format!(
        "proxy {} failed {failed}; benched for {cooldown:?}",
        masked(proxy)
    );
    state.stats.proxies_benched += 1;
    shared.push_event(state, Event::Warning { message });
}

/// Whether a failed request counts against the proxy it went through;
/// see `FetchError::proxy_at_fault`.
fn proxy_trouble(error: &FetchError) -> bool {
    error.proxy_at_fault()
}

/// Responses the breaker needs to have seen from a host before it judges.
const BREAKER_MIN_SEEN: usize = 10;

/// Records whether the host's latest response was a block, and pauses the
/// host when too many of its recent ones were.
fn breaker(shared: &Shared, state: &mut State, host: &str, blocked: bool) {
    let settings = &shared.settings;
    let Some(h) = state.hosts.get_mut(host) else {
        return;
    };
    h.recent.push_back(blocked);
    while h.recent.len() > settings.breaker_window {
        h.recent.pop_front();
    }
    let blocks = h.recent.iter().filter(|b| **b).count();
    // It judges once it has seen a few responses, not only once the window
    // is full: a site that blocks from its first answer pauses quickly.
    let enough = settings.breaker_window.min(BREAKER_MIN_SEEN);
    if settings.breaker_window == 0
        || h.recent.len() < enough
        || blocks as f64 <= settings.breaker_ratio * h.recent.len() as f64
    {
        return;
    }
    h.recent.clear();
    h.next_at = h.next_at.max(Instant::now() + settings.breaker_pause);
    state.stats.breaker_trips += 1;
    shared.push_event(
        state,
        Event::Paused {
            host: host.to_string(),
            pause: settings.breaker_pause,
        },
    );
}

/// The file's word for this path, overridden by the response's headers and
/// then its `<meta>`, as TDMRep orders them.
fn page_reserved(state: &State, origin: &str, path: &str, response: &Response) -> bool {
    let from_file = match state.origins.get(origin).map(|o| &o.tdm) {
        Some(Gate::Ready(Some(f))) => f.reservation(path),
        _ => Reservation::default(),
    };
    let headers = tdmrep::from_headers(
        response
            .headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str())),
    );
    let html = tdmrep::from_html(&response.body);
    from_file
        .overridden_by(headers)
        .overridden_by(html)
        .reserved
        == Some(true)
}

/// AutoThrottle: aim for `latency / target_concurrency` between requests,
/// moving halfway there each response. Only a successful response may lower
/// the delay; a 429 doubles it.
fn adjust_delay(
    s: &CrawlSettings,
    current: Duration,
    latency: Duration,
    status: Option<u16>,
) -> Duration {
    let target = latency.div_f64(s.target_concurrency.max(0.01));
    let next = match status {
        Some(429) => current.saturating_mul(2).max(Duration::from_millis(500)),
        Some(200) => (current + target) / 2,
        _ => current.max((current + target) / 2),
    };
    next.clamp(s.min_delay, s.max_delay)
}

async fn check_robots(shared: Arc<Shared>, origin: String, route: Route) {
    let url = format!("{origin}/robots.txt");
    let Some(got) = gate_get(&shared, &origin, url, route, |o| &mut o.robots).await else {
        return;
    };
    let robots = guarded(
        async move {
            match got {
                // The parser reads at most robots::MAX_BYTES of it.
                Ok(r) if (200..300).contains(&r.status) => Robots::parse(&r.text()),
                // A proxy wanting a login isn't the site's answer: as good
                // as no answer, like a network error.
                Ok(r) if r.status == 407 => Robots::disallow_all(),
                // RFC 9309: an unavailable file (4xx) means no rules.
                Ok(r) if (400..500).contains(&r.status) => Robots::allow_all(),
                // An unreachable file (5xx, or no answer) means everything
                // is off limits for now.
                _ => Robots::disallow_all(),
            }
        },
        // A file netweir can't read is treated as unreachable.
        |_| Robots::disallow_all(),
    )
    .await;
    let mut state = shared.lock();
    state.origins.entry(origin.clone()).or_default().robots = Gate::Ready(robots);
    gate_done(&shared, state, &origin);
}

async fn check_tdm(shared: Arc<Shared>, origin: String, route: Route) {
    let url = format!("{origin}/.well-known/tdmrep.json");
    let Some(got) = gate_get(&shared, &origin, url, route, |o| &mut o.tdm).await else {
        return;
    };
    let file = guarded(
        async move {
            match got {
                Ok(r) if r.status == 200 => TdmFile::parse(&r.text()),
                // Anything else, a network error or a proxy's 407 among
                // them, is no file.
                _ => None,
            }
        },
        |_| None,
    )
    .await;
    let mut state = shared.lock();
    state.origins.entry(origin.clone()).or_default().tdm = Gate::Ready(file);
    gate_done(&shared, state, &origin);
}

/// GETs a robots.txt or tdmrep.json, and counts it toward its proxy's
/// health. None if a proxy of the pool failed and there's a healthy one
/// that hasn't failed this origin yet: the file is then fetched again
/// through it, whatever `retries` says, since a dead proxy mustn't decide
/// what a whole site allows. A host with a session of its own moves to
/// that proxy, as its pages would have gone through the dead one too.
async fn gate_get<T>(
    shared: &Arc<Shared>,
    origin: &str,
    url: String,
    (fetcher, proxy): Route,
    gate: impl FnOnce(&mut Origin) -> &mut Gate<T>,
) -> Option<Result<Response, FetchError>> {
    // Fetched with the crawl's login, as Scrapy's robots.txt requests go
    // through its auth middleware.
    let outgoing = with_crawl_login(&shared.settings, &Outgoing::navigate());
    let got = match fetcher {
        Ok(fetcher) => {
            guarded(
                async move { fetcher.send(&url, &outgoing, &[]).await },
                |why| {
                    Err(FetchError {
                        kind: FetchErrorKind::Other,
                        message: format!("the fetch failed inside netweir: {why}"),
                        steps: Default::default(),
                    })
                },
            )
            .await
        }
        Err(e) => Err(e),
    };
    let Some(proxy) = proxy else {
        return Some(got);
    };
    let failed = match &got {
        Err(e) => proxy_trouble(e),
        Ok(r) => r.status == 407,
    };
    let mut state = shared.lock();
    record(shared, &mut state, &proxy, failed);
    if !failed || !state.proxies.pooled(&proxy) {
        return Some(got);
    }
    let o = state.origins.entry(origin.to_string()).or_default();
    o.failed_proxies.push(proxy);
    let failed_here = o.failed_proxies.clone();
    let next = state
        .proxies
        .choose(Instant::now(), |p| !failed_here.iter().any(|f| f == p))
        .filter(|(_, healthy)| *healthy)
        .map(|(k, _)| state.proxies.entries[k].url.clone());
    let Some(next) = next else {
        return Some(got);
    };
    let host = Url::parse(origin)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
        .unwrap_or_default();
    let has_session = state.hosts.get(&host).is_some_and(|h| h.session.is_some());
    if has_session && shared.settings.proxy_rotation != ProxyRotation::PerRequest {
        // Moving it may fail only as building any client may; the next
        // try then goes as the host's requests would.
        let _ = new_session_through(shared, &mut state, &host, Some(next));
    } else {
        state
            .origins
            .entry(origin.to_string())
            .or_default()
            .retry_through = Some(next);
    }
    *gate(state.origins.entry(origin.to_string()).or_default()) = Gate::Unknown;
    gate_done(shared, state, origin);
    None
}

/// The way a robots.txt or tdmrep.json fetch for `origin`, on behalf of
/// host `name`, goes: through the proxy a failed try picked, or as the
/// request waiting for it would (`wanted` being its own proxy).
fn gate_route(
    shared: &Shared,
    state: &mut State,
    origin: &str,
    name: &str,
    wanted: Option<&str>,
) -> Route {
    let again = state
        .origins
        .get_mut(origin)
        .and_then(|o| o.retry_through.take());
    match again {
        Some(proxy) => (through(shared, state, &proxy), Some(proxy)),
        None => route(shared, state, name, wanted),
    }
}

/// A gate fetch finished: the hosts waiting on it may go on.
fn gate_done(shared: &Shared, mut state: std::sync::MutexGuard<'_, State>, origin: &str) {
    state.gate_fetches -= 1;
    let waiting = state
        .origins
        .get_mut(origin)
        .map(|o| std::mem::take(&mut o.waiting))
        .unwrap_or_default();
    for host in waiting {
        state.make_ready(&host);
    }
    drop(state);
    shared.schedule.notify_one();
    shared.events.notify_waiters();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chrome_fetch_has_time_for_its_steps_and_never_overflows() {
        let parse = |v: serde_json::Value| -> Vec<Step> { steps::parse(&v).unwrap() };
        let settings = CrawlSettings::default();
        let timeout = Duration::from_secs(10);
        let plain = fetch_limit(timeout, &[], &settings);
        assert_eq!(plain, Duration::from_secs(20) + CHALLENGE_WAIT);
        let some = parse(serde_json::json!([{"action": "wait", "seconds": 5}]));
        let with = fetch_limit(timeout, &some, &settings);
        assert!(
            with >= plain + Duration::from_secs(5) + CHALLENGE_WAIT,
            "{with:?}"
        );
        // Never past browser_steps_max, and no panic however big it gets.
        let long = parse(serde_json::json!([{"action": "wait", "seconds": 86_400}]));
        let capped = fetch_limit(timeout, &long, &settings);
        assert!(
            capped < plain + Duration::from_secs(200) + CHALLENGE_WAIT,
            "{capped:?}"
        );
        let unlimited = CrawlSettings {
            browser_steps_max: None,
            browser_launch: LaunchOptions {
                timeout: Duration::MAX,
                ..LaunchOptions::default()
            },
            ..CrawlSettings::default()
        };
        let click = serde_json::json!({"action": "click", "selector": "a"});
        let clicks = parse(serde_json::json!([click, click]));
        assert_eq!(
            fetch_limit(Duration::MAX, &clicks, &unlimited),
            Duration::MAX
        );
    }

    #[test]
    fn the_crawls_steps_on_a_blocked_requests_go_never_fail_it() {
        let list: Arc<[Step]> = steps::parse(&serde_json::json!([
            {"action": "click", "selector": "a"},
            {"action": "click", "selector": "b", "on_error": "stop"},
        ]))
        .unwrap()
        .into();
        let went = lenient(&list);
        assert_eq!(went[0].on_error, steps::OnError::Continue);
        assert_eq!(went[1].on_error, steps::OnError::Stop);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn work_that_arrives_after_the_queue_went_quiet_gets_a_fresh_grace() {
        use crate::queue::tests::{Fake, fake_queue};
        let fake = Arc::new(Fake::default());
        let lease = Duration::from_secs(5);
        let grace = Duration::from_millis(600);
        let settings = CrawlSettings {
            obey_robots: false,
            obey_tdmrep: false,
            retries: 0,
            ..CrawlSettings::default()
        };
        let crawler = Crawler::distributed(
            FetchOptions::new(Profile::named("chrome").unwrap()),
            settings,
            fake_queue(&fake, "books", lease),
            Some(grace),
        )
        .unwrap();
        // Nothing to do: the crawl is over once the grace has gone by.
        let ended = tokio::time::timeout(Duration::from_secs(10), crawler.next(10)).await;
        assert!(ended.unwrap().is_empty());
        // Then a request arrives, and this process leases it.
        let url = "http://127.0.0.1:1/late";
        let pending = pending_of(&request(url), "{}", 0);
        let fp = crate::canonical::fingerprint("GET", url).unwrap();
        fake_queue(&fake, "books", lease)
            .push(&[(fp, pending)])
            .await
            .unwrap();
        // The task looks four times a second: by now it has leased it.
        tokio::time::sleep(Duration::from_secs(1)).await;
        let events = tokio::time::timeout(Duration::from_secs(10), crawler.next(10))
            .await
            .unwrap();
        let [Event::Leased { row, .. }] = events[..] else {
            panic!("{events:?}");
        };
        // Leased, not yet queued here: the crawl isn't over meanwhile.
        let waited = tokio::time::timeout(Duration::from_millis(400), crawler.next(10)).await;
        assert!(waited.is_err(), "{waited:?}");
        assert_eq!(crawler.resubmit(request(url), row), Submitted::Queued);
        let failed = tokio::time::timeout(Duration::from_secs(10), crawler.next(10))
            .await
            .unwrap();
        assert!(matches!(failed[..], [Event::Failed { .. }]), "{failed:?}");
        crawler.done(&[7]);
        // The page could have led to more: the crawl ends only after a
        // whole grace of quiet again.
        let started = Instant::now();
        let ended = tokio::time::timeout(Duration::from_secs(10), crawler.next(10)).await;
        assert!(ended.unwrap().is_empty());
        assert!(started.elapsed() >= grace / 2, "{:?}", started.elapsed());
    }

    #[test]
    fn a_saved_login_keeps_its_user_and_domain_and_finds_its_password() {
        let old = format!("old-{}", std::process::id());
        let new = format!("new-{}", std::process::id());
        let other = format!("other-{}", std::process::id());
        let mut asked = request("http://shop.test/a");
        asked.outgoing.policy.auth = Some(crate::Auth::new("ann", old.clone(), None));
        let mut saved = pending_of(&asked, "{}", 0);
        // What a queue member and a checkpoint's row are made of.
        let json = serde_json::to_string(&saved).unwrap();
        assert!(!json.contains(&old), "{json}");
        assert!(json.contains("ann"), "{json}");

        let mut lost = saved.clone();
        let settings = CrawlSettings::default();
        assert_eq!(
            unmask_login(&settings, &mut lost).as_deref(),
            Some("ann at http://shop.test/a")
        );
        assert_eq!(lost.policy.auth, None);

        let settings = CrawlSettings {
            saved_logins: vec![
                crate::Auth::new("ann", other, Some("shop.test".into())),
                crate::Auth::new("ann", new.clone(), None),
            ],
            ..CrawlSettings::default()
        };
        assert_eq!(unmask_login(&settings, &mut saved), None);
        assert_eq!(
            saved.policy.auth,
            Some(crate::Auth::new("ann", new.clone(), None))
        );
    }

    fn request(url: &str) -> CrawlRequest {
        CrawlRequest {
            id: 7,
            url: url.into(),
            priority: 0,
            headers: Vec::new(),
            dont_filter: false,
            depth: 0,
            browser: false,
            outgoing: Outgoing::navigate(),
            retry_post: false,
            proxy: None,
            delta_key: Default::default(),
            cookies: Default::default(),
            steps: None,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_claimed_request_is_screened_and_seen_but_never_queued() {
        let settings = CrawlSettings {
            max_depth: Some(1),
            ..CrawlSettings::default()
        };
        let crawler = Crawler::new(
            FetchOptions::new(Profile::named("chrome").unwrap()),
            settings,
        )
        .unwrap();
        let url = "http://127.0.0.1:1/answered";
        assert_eq!(crawler.claim(request(url)), Submitted::Answered);
        // Seen: neither a second claim nor a submit gets past it.
        assert_eq!(crawler.claim(request(url)), Submitted::Duplicate);
        assert_eq!(crawler.submit(request(url)), Submitted::Duplicate);
        let deep = CrawlRequest {
            depth: 2,
            ..request("http://127.0.0.1:1/deep")
        };
        assert_eq!(crawler.claim(deep), Submitted::TooDeep);
        // Nothing was queued, so the crawl is over at once.
        assert_eq!(crawler.stats().queued, 0);
        assert_eq!(crawler.stats().served, 1);
        assert!(crawler.next(10).await.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_claimed_request_counts_toward_the_page_limits() {
        let settings = CrawlSettings {
            max_pages: Some(2),
            max_pages_per_domain: Some(1),
            ..CrawlSettings::default()
        };
        let crawler = Crawler::new(
            FetchOptions::new(Profile::named("chrome").unwrap()),
            settings,
        )
        .unwrap();
        assert_eq!(
            crawler.claim(request("http://a.test/1")),
            Submitted::Answered
        );
        assert_eq!(
            crawler.claim(request("http://a.test/2")),
            Submitted::DomainFull
        );
        assert_eq!(
            crawler.submit(request("http://a.test/3")),
            Submitted::DomainFull
        );
        assert_eq!(
            crawler.claim(request("http://b.test/1")),
            Submitted::Answered
        );
        assert!(crawler.capped());
        assert_eq!(crawler.claim(request("http://c.test/1")), Submitted::Capped);
        // Not seen: refused the same way again.
        assert_eq!(crawler.claim(request("http://c.test/1")), Submitted::Capped);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_panic_in_a_watched_task_ends_the_crawl_with_a_fatal_event() {
        let crawler = Crawler::new(
            FetchOptions::new(Profile::named("chrome").unwrap()),
            CrawlSettings::default(),
        )
        .unwrap();
        spawn_watched(crawler.shared.clone(), async { panic!("a bad member") });
        // Nothing is queued, so `next` says so at once until the event is in.
        let mut events = Vec::new();
        for _ in 0..500 {
            events = crawler.next(10).await;
            if !events.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        match &events[..] {
            [Event::Fatal { message }] => assert!(message.contains("a bad member"), "{message}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn proxies_requests_name_beyond_the_cap_are_counted_together() {
        let mut pool = Pool::new(&["http://pool:1".to_string()]);
        for i in 0..MAX_OTHER_PROXIES + 50 {
            pool.entry(&format!("http://u{i}:p@gateway:8000"));
        }
        assert_eq!(pool.entries.len(), 1 + MAX_OTHER_PROXIES + 1);
        assert_eq!(pool.entries.last().unwrap().url, OTHER_PROXIES);
        let k = pool.entry("http://late:p@gateway:8000");
        assert_eq!(pool.entries[k].url, OTHER_PROXIES);
        assert_eq!(pool.entry("http://u3:p@gateway:8000"), 4, "still its own");
    }

    #[test]
    fn the_client_maps_keep_their_most_recently_used() {
        let mut lru = Lru::new(2);
        lru.insert("a", 1);
        lru.insert("b", 2);
        assert_eq!(lru.get("a"), Some(1));
        lru.insert("c", 3);
        assert_eq!(
            (lru.get("a"), lru.get("b"), lru.get("c")),
            (Some(1), None, Some(3))
        );
    }

    #[test]
    fn a_pool_exhausted_only_for_chrome_isnt_called_exhausted() {
        let mut pool = Pool::new(&[
            "socks5://ann:secret@10.0.0.1:1080".to_string(),
            "http://10.0.0.2:8080".to_string(),
        ]);
        let now = Instant::now();
        pool.entries[1].until = Some(now + Duration::from_secs(60));
        // The one Chrome can use is benched; the other is healthy.
        assert_eq!(
            pool.pick(now, None, chrome_can_use),
            Picked::Soonest("http://10.0.0.2:8080".into(), false)
        );
        assert!(!pool.warned_exhausted);
        // With every proxy benched, it is exhausted, and says so once.
        pool.entries[0].until = Some(now + Duration::from_secs(90));
        assert_eq!(
            pool.pick(now, None, |_| true),
            Picked::Soonest("http://10.0.0.2:8080".into(), true)
        );
        assert_eq!(
            pool.pick(now, None, |_| true),
            Picked::Soonest("http://10.0.0.2:8080".into(), false)
        );
    }

    #[test]
    fn chrome_is_given_no_socks_proxy_with_a_login() {
        assert!(!chrome_can_use("socks5://ann:secret@10.0.0.1:1080"));
        assert!(chrome_can_use("socks5://10.0.0.1:1080"));
        assert!(chrome_can_use("http://ann:secret@10.0.0.1:8080"));
        let pool = Pool::new(&[
            "socks5://ann:secret@10.0.0.1:1080".to_string(),
            "http://10.0.0.2:8080".to_string(),
        ]);
        let now = Instant::now();
        assert_eq!(pool.choose(now, chrome_can_use), Some((1, true)));
        let socks = Pool::new(&["socks5h://ann:secret@10.0.0.1:1080".to_string()]);
        assert_eq!(socks.choose(now, chrome_can_use), None);
    }

    #[test]
    fn throttle_moves_halfway_to_the_target_and_only_success_lowers_it() {
        let s = CrawlSettings {
            target_concurrency: 2.0,
            ..CrawlSettings::default()
        };
        let ms = Duration::from_millis;
        assert_eq!(adjust_delay(&s, ms(1000), ms(400), Some(200)), ms(600));
        assert_eq!(
            adjust_delay(&s, ms(1000), ms(400), Some(500)),
            ms(1000),
            "a failure can't lower it"
        );
        assert_eq!(
            adjust_delay(&s, ms(100), ms(4000), Some(404)),
            ms(1050),
            "but can raise it"
        );
        assert_eq!(adjust_delay(&s, ms(300), ms(10), Some(429)), ms(600));
        assert_eq!(
            adjust_delay(&s, ms(100), ms(10), Some(429)),
            ms(500),
            "a 429 raises it to at least half a second"
        );
        assert_eq!(
            adjust_delay(&s, ms(50_000), ms(200_000), Some(200)),
            ms(60_000),
            "clamped to the max"
        );
        assert_eq!(
            adjust_delay(&s, ms(100), ms(10), None),
            ms(100),
            "a network error can't lower it"
        );
    }

    #[tokio::test]
    async fn a_panic_in_guarded_work_becomes_the_fallback() {
        let value = guarded(async { panic!("parser bug") }, |why| {
            assert!(why.contains("panic"), "{why}");
            7
        })
        .await;
        assert_eq!(value, 7);
        assert_eq!(guarded(async { 1 }, |_| 2).await, 1);
    }

    #[test]
    fn queue_order_is_priority_then_arrival() {
        let q = |priority, seq| Queued {
            priority,
            seq,
            request: CrawlRequest {
                id: seq,
                url: String::new(),
                priority,
                headers: vec![],
                dont_filter: false,
                depth: 0,
                browser: false,
                outgoing: Outgoing::navigate(),
                retry_post: false,
                proxy: None,
                delta_key: DeltaKey::Fingerprint,
                cookies: Default::default(),
                steps: None,
            },
            host: String::new(),
            origin: String::new(),
            path: String::new(),
            hops: 0,
            passed: None,
            attempts: 0,
            in_browser: false,
            keep: None,
            blocked: None,
        };
        let mut heap: BinaryHeap<Queued> =
            [q(0, 1), q(5, 2), q(0, 3), q(5, 4)].into_iter().collect();
        let order: Vec<u64> = std::iter::from_fn(|| heap.pop().map(|q| q.seq)).collect();
        assert_eq!(order, [2, 4, 1, 3]);
    }
}
