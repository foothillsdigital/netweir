//! A running Chrome and its browser contexts.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::conn::{Connection, GONE, Handler};
use crate::environment::{Environment, Geolocation, Size};
use crate::events::Downloads;
use crate::network::Route;
use crate::page::{ContextRules, Cookie, Page, PageInner, WaitUntil, cookies_from, cookies_to};
use crate::{Error, Result, launch};

/// Decides whether a page may fetch a document, given its URL.
pub type Guard = Arc<
    dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
        + Send
        + Sync,
>;

/// For a Chrome major version (such as "154"), the `Sec-CH-UA` value its
/// pages should present, if not Chrome's own.
pub type Brands = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

#[derive(Clone)]
pub struct LaunchOptions {
    /// The browser to launch.
    pub engine: Engine,
    /// For Firefox: start it so pages can't tell it's driven, as Chrome
    /// always is. That needs a Firefox `netweir install firefox`
    /// installed. False drives any Firefox in automation mode instead,
    /// where `navigator.webdriver` is true.
    pub stealth: bool,
    /// The browser's executable; found with [`crate::find_chrome`] or
    /// [`crate::find_firefox`] when None.
    pub executable: Option<PathBuf>,
    pub headless: bool,
    /// More command-line switches for Chrome.
    pub args: Vec<String>,
    /// The default limit for navigations and actions.
    pub timeout: Duration,
    /// An `http://`, `https://` or `socks5://` proxy for everything Chrome
    /// fetches. For an HTTP(S) proxy, a username and password in it are
    /// given when it asks; Chrome can't log in to a SOCKS proxy, so one
    /// with a login is refused.
    pub proxy: Option<String>,
    /// The brands pages present. Chrome for Testing calls itself Chromium
    /// alone, where Google Chrome of the same version lists itself too.
    pub brands: Option<Brands>,
    /// A profile directory to use and keep, instead of a fresh one that's
    /// removed on close: cookies and storage survive to the next launch.
    /// Pages from [`Browser::new_page`] then share it. Chrome's "Preload
    /// pages" setting is turned off in it, and stays off.
    pub user_data_dir: Option<PathBuf>,
    /// How long a person takes to react, least and most: no dialog is
    /// answered sooner after it opened, and a tab brought to the front
    /// waits as long before it's used, a time picked at random between
    /// the two each time. Shorter is faster, and a page timing how long
    /// `confirm()` kept it waiting can tell.
    pub dialog_pause: (Duration, Duration),
    /// Every page's environment, unless its context or the page itself
    /// says otherwise. Given here, a screen, pixel ratio, user agent or
    /// language is Chrome's own from the start, so even service workers,
    /// which no override reaches, agree with it.
    pub environment: Environment,
    /// Puts each worker script request's User-Agent and Accept-Language
    /// back where Chrome sends them. The user agent override every page
    /// has (it keeps the client hints the --user-agent switch empties)
    /// moves them earlier among the headers of a dedicated or shared
    /// worker's script, which a server reading header order can tell.
    /// Off by default: it pauses every request of every page in the Fetch
    /// domain: a page with 300 subresources took 118 ms to load where it
    /// took 93, and one with frames and workers took 292 ms where it took
    /// 200.
    pub worker_header_order: bool,
}

impl std::fmt::Debug for LaunchOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchOptions")
            .field("engine", &self.engine)
            .field("stealth", &self.stealth)
            .field("executable", &self.executable)
            .field("headless", &self.headless)
            .field("args", &self.args)
            .field("timeout", &self.timeout)
            .field("proxy", &self.proxy.as_ref().map(|_| "…"))
            .field("brands", &self.brands.as_ref().map(|_| "…"))
            .field("user_data_dir", &self.user_data_dir)
            .field("dialog_pause", &self.dialog_pause)
            .field("environment", &self.environment)
            .field("worker_header_order", &self.worker_header_order)
            .finish()
    }
}

impl Default for LaunchOptions {
    fn default() -> LaunchOptions {
        LaunchOptions {
            engine: Engine::Chrome,
            stealth: true,
            executable: None,
            headless: true,
            args: Vec::new(),
            timeout: Duration::from_secs(30),
            proxy: None,
            brands: None,
            user_data_dir: None,
            dialog_pause: (Duration::from_millis(500), Duration::from_millis(1500)),
            environment: Environment::default(),
            worker_header_order: false,
        }
    }
}

/// A browser Netweir drives.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Engine {
    #[default]
    Chrome,
    Firefox,
}

impl Engine {
    pub fn name(self) -> &'static str {
        match self {
            Engine::Chrome => "Chrome",
            Engine::Firefox => "Firefox",
        }
    }
}

/// What a [`Browser`] is: which engine, its version, and whether pages
/// can tell it's driven (`stealth` false: Firefox in automation mode).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineInfo {
    pub engine: Engine,
    pub version: String,
    pub stealth: bool,
}

/// What every page says it is: the user agent without the headless
/// marker, and the client hints Chrome reports for itself.
#[derive(Debug, Clone)]
pub(crate) struct Identity {
    pub user_agent: String,
    pub metadata: Value,
}

/// A running Chrome. Cheap to clone; closes when [`Browser::close`] is
/// called or the last clone is dropped.
#[derive(Clone)]
pub struct Browser {
    pub(crate) inner: Arc<Inner>,
}

/// What DevTools' Performance panel records.
const TRACE_CATEGORIES: &[&str] = &[
    "-*",
    "devtools.timeline",
    "v8.execute",
    "disabled-by-default-devtools.timeline",
    "disabled-by-default-devtools.timeline.frame",
    "toplevel",
    "blink.console",
    "blink.user_timing",
    "latencyInfo",
    "disabled-by-default-devtools.timeline.stack",
    "disabled-by-default-v8.cpu_profiler",
];

pub(crate) struct Inner {
    pub conn: Connection,
    /// The Chrome netweir launched; None for one it connected to.
    child: Mutex<Option<Child>>,
    /// What ends the launched Chrome if netweir goes without closing it.
    lifeline: Mutex<Option<crate::lifeline::Lifeline>>,
    /// The launched Chrome's profile, removed when it closes unless it's
    /// one the caller gave.
    profile: Option<PathBuf>,
    /// Whether the profile is the caller's, to keep: then pages open in
    /// its own context, the one whose cookies and storage Chrome saves.
    keep_profile: bool,
    /// The profile netweir made, if it did; it goes when this does.
    temp_profile: Mutex<Option<launch::TempProfile>>,
    /// Contexts netweir made, for closing a browser it connected to,
    /// whose other contexts aren't netweir's to close.
    pub(crate) contexts: Mutex<Vec<String>>,
    /// Told the trace's stream when Chrome has finished writing it.
    tracing_done: Arc<Mutex<Option<oneshot::Sender<String>>>>,
    version: String,
    pub identity: Option<Identity>,
    pub timeout: Duration,
    /// The proxy's username and password, for its challenges.
    pub proxy_login: Option<(String, String)>,
    /// What the launched Chrome has lately written to stderr.
    pub said: Option<launch::Said>,
    /// See [`LaunchOptions::dialog_pause`].
    pub pause: (Duration, Duration),
    /// Every page netweir has, for finding the opener of a popup.
    pub pages: Arc<Mutex<Vec<Weak<PageInner>>>>,
    /// Pages netweir made and is attaching to itself, whose attaching
    /// the browser's session hears too.
    pub attaching: Arc<Mutex<HashSet<String>>>,
    /// Downloads under way, and each context's folder for them.
    pub downloads: Arc<Downloads>,
    /// Every page's environment, from [`LaunchOptions::environment`].
    pub environment: Environment,
    /// Whether netweir launched it headless, so its screens and windows
    /// are netweir's to arrange, and nobody else's to see.
    pub arranges: bool,
    /// The screen pages are shown on unless they name another, and its
    /// pixel ratio.
    pub screen: Size,
    pub screen_scale: f64,
    /// See [`LaunchOptions::worker_header_order`].
    pub worker_header_order: bool,
    /// A window size given as a --window-size switch: every page's
    /// window, unless its environment names a viewport.
    pub window_size: Option<(u32, u32)>,
    /// The user agent Chrome was launched with (`--user-agent`), if
    /// netweir knows it.
    pub agent: Option<String>,
    /// The languages Chrome shows page script with no override, in its
    /// own context (first) and in contexts netweir makes, once read. The
    /// two differ: launched with `--accept-lang=de-DE,de`, a context made
    /// over CDP shows `de-DE,de,en-US,en`, page and worker alike.
    pub own_languages: Mutex<[Option<String>; 2]>,
    /// Held while a screen is looked for, and added if there's none, so
    /// two pages wanting one don't add two.
    pub screens: tokio::sync::Mutex<()>,
    /// Chrome's own user agent and client hints, read the first time a
    /// page needs them, for a browser with no `identity`.
    own: tokio::sync::OnceCell<Option<Identity>>,
    /// False only for a Firefox driven in automation mode.
    pub stealth: bool,
    /// Firefox's own user context, which its session is confined to (or
    /// "default" in automation mode); None for Chrome.
    pub default_context: Option<String>,
    /// Where Firefox opened its first window, and its size (x, y, width,
    /// height), and whether it's maximized: what each page's window
    /// copies, as a person's would be.
    pub first_window: Option<([i64; 4], bool)>,
}

/// The fields of an [`Inner`] that only a launch fills in.
fn bare(options: &LaunchOptions) -> (Environment, Size, f64) {
    let env = options.environment.clone();
    let screen = env.screen.unwrap_or(crate::window::DEFAULT_SCREEN);
    let scale = env.device_scale_factor.unwrap_or(1.0);
    (env, screen, scale)
}

/// The user agent override for a page with `env` in a context whose own
/// languages are `own` (as Chrome shows them with no override), worked
/// out from what Chrome says it is, and read from Chrome first if netweir
/// doesn't know that yet, and the page's languages. None leaves the page
/// as Chrome makes it.
/// Boxed, since reading what Chrome says opens a page, which comes here.
/// What `user_agent_for` gives.
pub(crate) struct Overrides {
    /// The page's, its frames' and its dedicated workers'.
    pub user_agent: Option<Value>,
    /// Service and shared workers', whose user agent is always the
    /// --user-agent switch's: the page's where it's that, and otherwise
    /// the browser's own with the page's languages, so a worker's
    /// `navigator.userAgentData` says what its `navigator.userAgent` does.
    pub switched: Option<Value>,
    /// The page's languages.
    pub languages: String,
}

type Overriding<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Overrides>> + Send + 'a>>;

pub(crate) fn user_agent_for<'a>(
    browser: &'a Arc<Inner>,
    env: &'a Environment,
    own: &'a str,
) -> Overriding<'a> {
    Box::pin(user_agent_of(browser, env, own))
}

async fn user_agent_of(browser: &Arc<Inner>, env: &Environment, own: &str) -> Result<Overrides> {
    // A locale of the page's or its context's own; the browser's is the
    // one Chrome already shows, in each context as it computes it there.
    let languages = match &env.locale {
        Some(l) if Some(l) != browser.environment.locale.as_ref() => {
            env.languages().unwrap_or_default()
        }
        _ => own.to_string(),
    };
    let own_languages = languages == own;
    let full = browser
        .version
        .trim_start_matches("HeadlessChrome/")
        .trim_start_matches("Chrome/");
    let wanted = env.changes_user_agent() || !own_languages;
    let base = match (&browser.identity, wanted) {
        (Some(i), _) => Some(i.clone()),
        (None, false) => {
            return Ok(Overrides {
                user_agent: None,
                switched: None,
                languages,
            });
        }
        (None, true) => browser
            .own
            .get_or_try_init(|| async {
                let me = Browser {
                    inner: browser.clone(),
                };
                let metadata = me.own_metadata().await?;
                let version = browser.call("", "Browser.getVersion", json!({})).await?;
                // Hints emptied by someone's --user-agent switch would be
                // sent as empty headers.
                let emptied = metadata["architecture"] == "" && metadata["platformVersion"] == "";
                Ok::<_, Error>((!emptied).then(|| Identity {
                    user_agent: str_of(&version, "userAgent").replace("HeadlessChrome/", "Chrome/"),
                    metadata,
                }))
            })
            .await?
            .clone(),
    };
    let base = base.as_ref().map(|i| (i.user_agent.as_str(), &i.metadata));
    // Chrome's own are left to it: an override's Accept-Language goes
    // out after User-Agent, where Chrome's own is last.
    let given = (!own_languages && !languages.is_empty()).then_some(languages.as_str());
    let user_agent = env.user_agent_override(base, full, given, own_languages)?;
    let agent = |o: &Option<Value>| {
        o.as_ref()
            .and_then(|o| o["userAgent"].as_str().map(str::to_string))
    };
    let switched = if browser.agent.is_none() || agent(&user_agent) == browser.agent {
        user_agent.clone()
    } else {
        let plain = Environment {
            phone: None,
            user_agent: None,
            mobile: None,
            ..env.clone()
        };
        plain.user_agent_override(base, full, given, own_languages)?
    };
    Ok(Overrides {
        user_agent,
        switched,
        languages,
    })
}

impl Browser {
    pub async fn launch(mut options: LaunchOptions) -> Result<Browser> {
        if options.engine == Engine::Firefox {
            return Browser::launch_firefox(options).await;
        }
        if !options.stealth {
            return Err(Error::Invalid(
                "stealth: false is for Firefox; Chrome is always launched as a person's browser"
                    .into(),
            ));
        }
        options.args.extend(launch::env_switches());
        options.environment.validate()?;
        let executable = match options.executable.clone() {
            Some(path) => path,
            None => launch::find_chrome()?,
        };
        // An install from before netweir granted this is fixed in place.
        launch::grant_if_ours(&executable)?;
        // A profile given as a switch is the one Chrome would use (the
        // caller's switches come after netweir's, and Chrome takes the
        // last), so it's the one to prepare, and keep, as if given as
        // user_data_dir. Taken out of the switches, so the identity probe
        // doesn't open it too.
        let switched = launch::take_switches(&mut options.args, "user-data-dir")
            .pop()
            .map(PathBuf::from);
        match (switched, &options.user_data_dir) {
            (Some(switched), Some(given)) if switched != *given => {
                return Err(Error::Launch(format!(
                    "the profile is given twice, as user_data_dir ({}) and as --user-data-dir ({}); \
                     give it once",
                    given.display(),
                    switched.display()
                )));
            }
            (Some(switched), _) => options.user_data_dir = Some(switched),
            (None, _) => {}
        }
        // Chrome reads an empty --user-data-dir as none at all and opens
        // its own default profile, one netweir never prepared; and an
        // empty folder would be the working directory here.
        if options
            .user_data_dir
            .as_ref()
            .is_some_and(|dir| dir.as_os_str().to_string_lossy().trim().is_empty())
        {
            return Err(Error::Launch(
                "the profile folder (user_data_dir or --user-data-dir) is empty: name a folder"
                    .into(),
            ));
        }
        if let Some(dir) = &options.user_data_dir {
            launch::check_profile(&executable, dir)?;
        }
        let keep_profile = options.user_data_dir.is_some();
        // Profiles left by programs that ended without closing their
        // browser.
        launch::sweep_soon();
        // A profile of netweir's own is removed if the launch fails, on
        // whichever line.
        if let Some(dir) = &options.user_data_dir {
            Quitting::wait_for(dir).await;
        }
        let (profile, temp) = match &options.user_data_dir {
            Some(dir) => std::fs::create_dir_all(dir).map(|()| (dir.clone(), None)),
            None => launch::TempProfile::make().map(|t| (t.dir().to_path_buf(), Some(t))),
        }
        .map_err(|e| Error::Launch(format!("can't make a profile directory: {e}")))?;
        launch::turn_off_preloading(&profile, &options.args)?;
        if options.environment.is_mobile() {
            launch::open_pdfs_outside(&profile, &options.args)?;
        }
        let mut args = vec![
            format!("--user-data-dir={}", profile.display()),
            "--no-first-run".into(),
            "--no-default-browser-check".into(),
            // Leaves navigator.webdriver false.
            "--disable-blink-features=AutomationControlled".into(),
        ];
        args.extend(
            KEYRING
                .iter()
                .chain(FOREGROUND)
                .chain(AS_SHIPPED)
                .map(|a| a.to_string()),
        );
        // Chrome's client hints, when they had to be read before launch.
        let mut hints = None;
        let env = &options.environment;
        // The caller's own, if given.
        let mut agent = options
            .args
            .iter()
            .rev()
            .find_map(|a| a.strip_prefix("--user-agent="))
            .map(str::to_string);
        if options.headless {
            args.push("--headless".into());
            // Service workers take their user agent from this flag alone;
            // page overrides don't reach them. The flag empties the
            // high-entropy client hints, which each page's override puts
            // back.
            if !options.args.iter().any(|a| a.starts_with("--user-agent=")) {
                let (own, metadata) =
                    headless_identity(&executable, &options.args, options.timeout).await?;
                let own = own.replace("HeadlessChrome/", "Chrome/");
                // The environment's own, where it has one, so a service
                // worker says it too.
                let full = own
                    .split("Chrome/")
                    .nth(1)
                    .and_then(|v| v.split(' ').next())
                    .unwrap_or_default()
                    .to_string();
                let flag = env
                    .user_agent_override(Some((&own, &metadata)), &full, None, true)?
                    .and_then(|o| o["userAgent"].as_str().map(str::to_string))
                    .unwrap_or(own);
                args.push(format!("--user-agent={flag}"));
                agent = Some(flag);
                hints = Some(metadata);
            }
            // A real screen for the pages: the environment's, or one the
            // windows fit on.
            args.extend(crate::window::launch_switches(env, &options.args)?);
        } else if env.screen.is_some() || env.device_scale_factor.is_some() || env.is_mobile() {
            return Err(Error::Launch(
                "a screen, pixel ratio or phone needs a headless Chrome: a window on the real \
                 screen can't be shown on another"
                    .into(),
            ));
        }
        // The environment's languages as the browser's own, so Chrome
        // sends them as it would for a person whose languages they are.
        // With none, Chrome's own, which follow the machine's language as
        // its Intl does: forcing English on a German machine would show
        // navigator.language en-US beside an Intl in de-DE.
        if let Some(l) = env.languages()
            && !options.args.iter().any(|a| a.starts_with("--accept-lang"))
        {
            args.push(format!("--accept-lang={l}"));
        }
        let mut proxy_login = None;
        if let Some(proxy) = &options.proxy {
            let (server, login) = split_proxy(proxy)?;
            args.push(format!("--proxy-server={server}"));
            proxy_login = login;
        }
        args.extend(options.args.iter().cloned());
        // Chrome's Preload pages setting, turned off above, leaves
        // <link rel=prerender> fetching all the same; this holds that back
        // as Chrome's own experiment does for some of its users.
        let mut args = launch::enable_features(args, &["NoStatePrefetchHoldback"]);
        args.push("about:blank".into());
        let started = launch::start(&executable, &args)?;
        let said = started.said.clone();
        let conn = Connection::new(started.replies, started.commands, Some(&started.child));
        let (environment, screen, screen_scale) = bare(&options);
        let shown = profile.clone();
        let inner = Inner {
            conn,
            child: Mutex::new(Some(started.child)),
            lifeline: Mutex::new(Some(started.lifeline)),
            profile: Some(profile),
            keep_profile,
            temp_profile: Mutex::new(temp),
            contexts: Mutex::default(),
            tracing_done: Arc::default(),
            version: String::new(),
            identity: None,
            timeout: options.timeout,
            pause: options.dialog_pause,
            proxy_login,
            said: Some(said.clone()),
            pages: Arc::default(),
            attaching: Arc::default(),
            downloads: Arc::default(),
            environment,
            arranges: options.headless,
            screen,
            screen_scale,
            worker_header_order: options.worker_header_order,
            window_size: options
                .args
                .iter()
                .rev()
                .find_map(|a| a.strip_prefix("--window-size="))
                .and_then(|v| v.split_once(','))
                .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?))),
            agent,
            own_languages: Mutex::default(),
            screens: tokio::sync::Mutex::new(()),
            own: tokio::sync::OnceCell::new(),
            stealth: true,
            default_context: None,
            first_window: None,
        };
        let what = executable.display().to_string();
        // Google Chrome given its own default profile says so and runs on
        // without listening; that ends the launch at once, not at the
        // timeout.
        tokio::select! {
            finished = Browser::finish(inner, &options, &what, hints) => finished.map_err(|e| match e {
                Error::Launch(message) => Error::Launch(said.explain(message)),
                other => other,
            }),
            () = said.refused() => Err(launch::own_profile_refused(&shown)),
        }
    }

    async fn launch_firefox(options: LaunchOptions) -> Result<Browser> {
        options.environment.validate()?;
        if options.environment != Environment::default() {
            return Err(Error::Unsupported {
                engine: "Firefox",
                feature: "LaunchOptions.environment".into(),
            });
        }
        // Each is Chrome's, or not done on Firefox yet: refused rather
        // than quietly left out.
        let given = [
            ("LaunchOptions.proxy", options.proxy.is_some()),
            ("LaunchOptions.brands", options.brands.is_some()),
            (
                "LaunchOptions.worker_header_order",
                options.worker_header_order,
            ),
        ];
        if let Some((feature, _)) = given.iter().find(|(_, given)| *given) {
            return Err(Error::Unsupported {
                engine: "Firefox",
                feature: feature.to_string(),
            });
        }
        // A Firefox dropped with this profile may still be quitting, and
        // holds its lock until it has.
        if let Some(dir) = &options.user_data_dir {
            Quitting::wait_for(dir).await;
        }
        let started = crate::firefox::start(&options).await?;
        let (environment, screen, screen_scale) = bare(&options);
        let inner = Inner {
            conn: started.conn,
            child: Mutex::new(Some(started.process.child)),
            lifeline: Mutex::new(Some(started.process.lifeline)),
            profile: Some(started.profile),
            keep_profile: options.user_data_dir.is_some(),
            temp_profile: Mutex::new(started.temp),
            contexts: Mutex::default(),
            tracing_done: Arc::default(),
            version: started.version,
            identity: None,
            timeout: options.timeout,
            pause: options.dialog_pause,
            proxy_login: None,
            said: Some(started.process.said),
            pages: Arc::default(),
            attaching: Arc::default(),
            downloads: Arc::default(),
            environment,
            arranges: options.headless,
            screen,
            screen_scale,
            worker_header_order: false,
            window_size: None,
            agent: None,
            own_languages: Mutex::default(),
            screens: tokio::sync::Mutex::new(()),
            own: tokio::sync::OnceCell::new(),
            stealth: started.stealth,
            default_context: Some(started.default_context),
            first_window: started.first_window,
        };
        Ok(Browser {
            inner: Arc::new(inner),
        })
    }

    /// Drives a browser that's already running: a `ws://` DevTools URL, as
    /// Chrome prints it, or an `http://host:port` whose `/json/version`
    /// names one. `options` give the timeout and the brands; nothing is
    /// launched. Closing it closes what netweir opened and leaves the
    /// browser running.
    pub async fn connect(url: &str, options: LaunchOptions) -> Result<Browser> {
        // Every Firefox Netweir didn't start is in automation mode, and
        // one it did start holds its one session already.
        if options.engine == Engine::Firefox {
            return Err(Error::Unsupported {
                engine: "Firefox",
                feature: "Browser.connect".into(),
            });
        }
        options.environment.validate()?;
        let (environment, screen, screen_scale) = bare(&options);
        let ws = if url.starts_with("http://") {
            devtools_url(url).await?
        } else {
            url.to_string()
        };
        let late = || {
            Error::Launch(format!(
                "{ws} didn't answer in {}s",
                options.timeout.as_secs_f64()
            ))
        };
        let refused = |e: String| Error::Launch(format!("can't connect to {ws}: {e}"));
        let conn = if ws.starts_with("wss://") {
            let socket = tokio::time::timeout(options.timeout, secure_websocket(&ws))
                .await
                .map_err(|_| late())?
                .map_err(refused)?;
            Connection::over_websocket(socket, crate::conn::Dialect::Cdp)
        } else {
            let (socket, _) = tokio::time::timeout(
                options.timeout,
                tokio_tungstenite::connect_async(ws.as_str()),
            )
            .await
            .map_err(|_| late())?
            .map_err(|e| refused(e.to_string()))?;
            Connection::over_websocket(socket, crate::conn::Dialect::Cdp)
        };
        let inner = Inner {
            conn,
            child: Mutex::new(None),
            lifeline: Mutex::default(),
            profile: None,
            keep_profile: false,
            temp_profile: Mutex::default(),
            contexts: Mutex::default(),
            tracing_done: Arc::default(),
            version: String::new(),
            identity: None,
            timeout: options.timeout,
            pause: options.dialog_pause,
            proxy_login: None,
            said: None,
            pages: Arc::default(),
            attaching: Arc::default(),
            downloads: Arc::default(),
            environment,
            arranges: false,
            screen,
            screen_scale,
            worker_header_order: options.worker_header_order,
            window_size: None,
            agent: None,
            own_languages: Mutex::default(),
            screens: tokio::sync::Mutex::new(()),
            own: tokio::sync::OnceCell::new(),
            stealth: true,
            default_context: None,
            first_window: None,
        };
        Browser::finish(inner, &options, &ws, None).await
    }

    /// Asks the browser what it is, and works out what its pages should
    /// say they are.
    async fn finish(
        mut inner: Inner,
        options: &LaunchOptions,
        what: &str,
        hints: Option<Value>,
    ) -> Result<Browser> {
        let version = match tokio::time::timeout(
            options.timeout,
            inner.conn.call("", "Browser.getVersion", json!({})),
        )
        .await
        {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(Error::Launch(format!("{what} didn't answer: {e}"))),
            Err(_) => {
                let limit = options.timeout.as_secs_f64();
                return Err(Error::Launch(format!("{what} didn't answer in {limit}s")));
            }
        };
        inner.version = str_of(&version, "product");
        // Anything that answers in JSON could get this far; a browser says
        // what it is.
        if inner.version.is_empty() {
            return Err(Error::Launch(format!(
                "{what} isn't a browser's DevTools: it didn't say what it is"
            )));
        }
        let user_agent = str_of(&version, "userAgent");
        let mut browser = Browser {
            inner: Arc::new(inner),
        };
        let full = browser
            .inner
            .version
            .trim_start_matches("HeadlessChrome/")
            .trim_start_matches("Chrome/")
            .to_string();
        let major = full.split('.').next().unwrap_or_default().to_string();
        // A list that doesn't parse would leave pages with no brands at
        // all; Chrome's own are better.
        let brands = options
            .brands
            .as_ref()
            .and_then(|b| b(&major))
            .filter(|header| !parse_brands(header).is_empty());
        // A headless Chrome netweir launched has a clean user agent from the
        // --user-agent flag and the hints it read before the flag emptied
        // them, which each page's override restores.
        let mut identity = None;
        if hints.is_some() || user_agent.contains("HeadlessChrome/") || brands.is_some() {
            let probed = hints.is_some();
            let metadata = match hints {
                Some(hints) => hints,
                None => browser.own_metadata().await?,
            };
            // A Chrome running with a --user-agent flag of someone else's
            // reports emptied hints, and an override would send them as
            // empty headers; it's left as it is.
            let emptied = metadata["architecture"] == "" && metadata["platformVersion"] == "";
            if probed || !emptied {
                identity = Some(metadata);
            }
        }
        if let Some(mut metadata) = identity {
            if let Some(header) = brands {
                let list = parse_brands(&header);
                metadata["brands"] = list
                    .iter()
                    .map(|(brand, version)| json!({"brand": brand, "version": version}))
                    .collect();
                metadata["fullVersion"] = Value::from(full.as_str());
                // A brand at Chrome's own major version carries its full
                // version; the made-up one, as in Chrome, is N.0.0.0.
                metadata["fullVersionList"] = list
                    .iter()
                    .map(|(brand, version)| {
                        let full = if *version == major {
                            full.clone()
                        } else {
                            format!("{version}.0.0.0")
                        };
                        json!({"brand": brand, "version": full})
                    })
                    .collect();
            }
            let identity = Identity {
                user_agent: user_agent.replace("HeadlessChrome/", "Chrome/"),
                metadata,
            };
            // Nothing else holds the Arc yet.
            Arc::get_mut(&mut browser.inner)
                .expect("the browser isn't shared before launch returns")
                .identity = Some(identity);
        }
        browser.watch().await?;
        Ok(browser)
    }

    /// Has Chrome hold each new page until the browser's session has
    /// looked at it: a popup of one of netweir's pages is set up as its
    /// opener is before it runs anything, and anything else is let go at
    /// once. Download progress comes here too.
    async fn watch(&self) -> Result<()> {
        let conn = self.inner.conn.clone();
        let (pages, downloads) = (self.inner.pages.clone(), self.inner.downloads.clone());
        let attaching = self.inner.attaching.clone();
        let tracing_done = self.inner.tracing_done.clone();
        let take_done = move || {
            tracing_done
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
        };
        let handler: Handler = Arc::new(move |method, params| match method {
            "Target.attachedToTarget" => popped(&conn, &pages, &attaching, params),
            "Browser.downloadWillBegin" => unclaimed(&pages, &downloads, params),
            "Browser.downloadProgress" => downloads.progress(params),
            "Tracing.tracingComplete" => {
                if let (Some(done), Some(stream)) = (take_done(), params["stream"].as_str()) {
                    let _ = done.send(stream.to_string());
                }
            }
            GONE => {
                downloads.gone();
                // Dropped, so whoever waits for a trace hears it won't come.
                take_done();
            }
            _ => {}
        });
        self.inner.conn.on("", handler);
        // Shared workers too: no page's session is told of one, since it
        // belongs to no page, so it's held here until it has the
        // environment of the context it's in.
        let attach = json!({
            "autoAttach": true,
            "waitForDebuggerOnStart": true,
            "flatten": true,
            "filter": [{"type": "page"}, {"type": "shared_worker"}],
        });
        match self.inner.call("", "Target.setAutoAttach", attach).await {
            Ok(_) => Ok(()),
            // Something that speaks CDP but not all of it: pages work,
            // popups aren't taken on.
            Err(Error::Protocol { message, .. }) => {
                crate::warn(&format!(
                    "the browser won't hold new pages ({message}), so popups can't be read"
                ));
                Ok(())
            }
            Err(e) => Err(Error::Launch(format!("the browser didn't answer: {e}"))),
        }
    }

    /// The client hints Chrome reports for itself, read where no override
    /// is in force and the hints are visible: a secure page, which
    /// `navigator.userAgentData` needs. That's chrome://version, which
    /// Chrome serves itself and keeps out of history. chrome-headless-shell
    /// serves no chrome:// pages, so there it's an https page a route
    /// answers, which never leaves Chrome.
    async fn own_metadata(&self) -> Result<Value> {
        const HINTS: &str = "navigator.userAgentData && navigator.userAgentData\
            .getHighEntropyValues(['architecture', 'bitness', 'formFactors', \
            'fullVersionList', 'model', 'platformVersion', 'uaFullVersion', 'wow64'])";
        let page = Page::open(
            self.inner.clone(),
            None,
            false,
            None,
            None,
            Environment::default(),
        )
        .await?;
        let read = async {
            if page
                .goto("chrome://version", WaitUntil::Load, None)
                .await
                .is_ok()
            {
                let values = page.evaluate(HINTS).await?;
                if values.is_object() {
                    return Ok(values);
                }
            }
            let mut routes = page.route().await?;
            tokio::spawn(async move {
                while let Some(route) = routes.recv().await {
                    let _ = route.fulfill(200, &[], b"").await;
                }
            });
            page.goto("https://netweir.invalid/", WaitUntil::Load, None)
                .await?;
            page.evaluate(HINTS).await
        }
        .await;
        page.close().await?;
        let values = read?;
        Ok(json!({
            "brands": values["brands"],
            "fullVersionList": values["fullVersionList"],
            "platform": values["platform"],
            "platformVersion": values["platformVersion"],
            "architecture": values["architecture"],
            "model": values["model"],
            "mobile": values["mobile"],
            "bitness": values["bitness"],
            "wow64": values["wow64"],
            "formFactors": values["formFactors"],
            // Deprecated, but still a hint Chrome gives (`uaFullVersion`,
            // `Sec-CH-UA-Full-Version`); left out, it was empty.
            "fullVersion": values["uaFullVersion"],
        }))
    }

    /// Which browser this is, its version, and whether pages can tell
    /// it's driven.
    pub fn engine_info(&self) -> EngineInfo {
        let engine = self.inner.engine();
        let version = match engine {
            Engine::Chrome => self.inner.version.split('/').nth(1).unwrap_or_default(),
            Engine::Firefox => self.inner.version.trim_start_matches("Firefox/"),
        };
        EngineInfo {
            engine,
            version: version.to_string(),
            stealth: self.inner.stealth,
        }
    }

    /// The browser's product string, such as `Chrome/154.0.8037.98` or
    /// `Firefox/159.0.1`.
    pub fn version(&self) -> &str {
        &self.inner.version
    }

    /// Whether this is chrome-headless-shell, which names itself
    /// `HeadlessChrome/` in its product string (Chrome itself says
    /// `Chrome/`, headless or not; a `--user-agent` switch doesn't change
    /// it). The shell preloads what a page's speculation rules name
    /// whatever its "Preload pages" setting says, outside every session
    /// netweir has.
    pub fn is_headless_shell(&self) -> bool {
        self.inner.version.starts_with("HeadlessChrome/")
    }

    /// The launched Chrome's profile directory; None for a browser netweir
    /// connected to.
    pub fn profile_dir(&self) -> Option<&Path> {
        self.inner.profile.as_deref()
    }

    /// True once closed, and also once Chrome has exited or crashed (or,
    /// for a browser netweir connected to, the connection has).
    pub fn is_closed(&self) -> bool {
        self.inner.conn.is_closed() || (self.inner.profile.is_some() && self.child().is_none())
    }

    /// Chrome's process id, while it runs.
    pub fn pid(&self) -> Option<u32> {
        self.child().as_ref().map(|c| c.id())
    }

    fn child(&self) -> std::sync::MutexGuard<'_, Option<Child>> {
        self.inner.child.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A page in a context of its own: no cookies or storage shared with
    /// any other page. Closing it closes the context. With a
    /// [`LaunchOptions::user_data_dir`], it's a page of the profile's own
    /// context instead, so what it keeps is saved there.
    pub async fn new_page(&self) -> Result<Page> {
        self.new_page_with(Environment::default()).await
    }

    /// [`Browser::new_page`], in `env` (over the browser's own
    /// environment). Its permissions are granted in the page's context.
    pub async fn new_page_with(&self, env: Environment) -> Result<Page> {
        if self.inner.keep_profile {
            // The profile's own context, which every such page shares.
            shares_process(
                &env,
                &self.inner.environment,
                "With a profile to keep, every page shares the profile's own context: give it \
                 to the browser's environment",
            )?;
        }
        let env = env.over(&self.inner.environment);
        added_screen(&env, &self.inner.environment)?;
        if self.inner.keep_profile {
            if self.inner.engine() == Engine::Chrome {
                self.inner.allow_downloads(None).await?;
            }
            return Page::open(self.inner.clone(), None, false, None, None, env).await;
        }
        let context = self.create_context().await?;
        Page::open(self.inner.clone(), Some(context), true, None, None, env).await
    }

    /// A page in a context of its own, with a
    /// [`LaunchOptions::user_data_dir`] too: nothing it keeps reaches the
    /// profile, nor any other page.
    pub async fn new_page_apart(&self) -> Result<Page> {
        let context = self.create_context().await?;
        let env = self.inner.environment.clone();
        Page::open(self.inner.clone(), Some(context), true, None, None, env).await
    }

    /// Like [`Browser::new_page`], but every document the page's main
    /// frame is about to fetch (each hop of a redirect included) is put
    /// to `guard` first, and one it refuses isn't fetched: navigating there
    /// fails with `ERR_BLOCKED_BY_CLIENT`.
    pub async fn new_guarded_page(&self, guard: Guard) -> Result<Page> {
        let context = self.create_context().await?;
        let env = self.inner.environment.clone();
        Page::open(
            self.inner.clone(),
            Some(context),
            true,
            Some(guard),
            None,
            env,
        )
        .await
    }

    /// A context whose pages share cookies and storage with each other.
    pub async fn new_context(&self) -> Result<Context> {
        self.new_context_with(Environment::default()).await
    }

    /// [`Browser::new_context`], whose pages have `env` (over the
    /// browser's own environment) unless they say otherwise.
    pub async fn new_context_with(&self, env: Environment) -> Result<Context> {
        if env != Environment::default() {
            self.inner.chrome_only("a context's environment")?;
        }
        let env = env.over(&self.inner.environment);
        env.validate()?;
        added_screen(&env, &self.inner.environment)?;
        let id = self.create_context().await?;
        let context = Context {
            browser: self.inner.clone(),
            id,
            rules: Arc::default(),
            env: Arc::new(Mutex::new(env.clone())),
        };
        if let Err(e) =
            crate::page::grant(&self.inner, Some(&context.id), &env.permissions, None).await
        {
            let _ = context.close().await;
            return Err(e);
        }
        Ok(context)
    }

    async fn create_context(&self) -> Result<String> {
        if self.inner.engine() == Engine::Firefox {
            // Netweir launched this Firefox, so the context goes with it.
            let r = self
                .inner
                .call("", "browser.createUserContext", json!({}))
                .await?;
            return Ok(str_of(&r, "userContext"));
        }
        // Gone when the connection is, however netweir lets go of it.
        let r = self
            .inner
            .call(
                "",
                "Target.createBrowserContext",
                json!({"disposeOnDetach": true}),
            )
            .await?;
        let id = str_of(&r, "browserContextId");
        self.inner
            .contexts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(id.clone());
        self.inner.allow_downloads(Some(&id)).await?;
        Ok(id)
    }

    /// Every CDP method sent so far, for tests that check nothing
    /// detectable was.
    pub fn methods_sent(&self) -> Vec<String> {
        self.inner.conn.methods_sent()
    }

    /// Asks Chrome to quit, waits for it (killing it after 10 seconds),
    /// and removes its profile directory, unless it was given one.
    /// Starts recording what the whole browser does, every page of it, for
    /// [`Browser::stop_tracing`]. `categories` are Chrome's trace categories,
    /// in place of those DevTools' Performance panel records.
    pub async fn start_tracing(
        &self,
        categories: Option<&[String]>,
        screenshots: bool,
    ) -> Result<()> {
        self.inner.chrome_only("Browser.start_tracing")?;
        let mut included: Vec<&str> = match categories {
            Some(categories) => categories.iter().map(String::as_str).collect(),
            None => TRACE_CATEGORIES.to_vec(),
        };
        if screenshots {
            included.push("disabled-by-default-devtools.screenshot");
        }
        let params = json!({
            "transferMode": "ReturnAsStream",
            "traceConfig": {"includedCategories": included},
        });
        self.inner.call("", "Tracing.start", params).await?;
        Ok(())
    }

    /// Stops recording, and returns the trace: JSON that Chrome's Performance
    /// panel and Perfetto open.
    pub async fn stop_tracing(&self) -> Result<Vec<u8>> {
        self.inner.chrome_only("Browser.stop_tracing")?;
        let inner = &self.inner;
        let (done, stream) = oneshot::channel();
        *inner.tracing_done.lock().unwrap_or_else(|e| e.into_inner()) = Some(done);
        inner.call("", "Tracing.end", json!({})).await?;
        let stream = tokio::time::timeout(inner.timeout, stream)
            .await
            .map_err(|_| {
                inner.explain(Error::Timeout(format!(
                    "Chrome didn't finish its trace within {:?}",
                    inner.timeout
                )))
            })?
            .map_err(|_| Error::Protocol {
                method: "Tracing.end".into(),
                message: "the browser went before its trace was finished".into(),
            })?;
        let mut trace = Vec::new();
        loop {
            let read = inner.call("", "IO.read", json!({"handle": stream})).await?;
            let data = read["data"].as_str().unwrap_or_default();
            if read["base64Encoded"].as_bool().unwrap_or(false) {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|e| Error::Protocol {
                        method: "IO.read".into(),
                        message: e.to_string(),
                    })?;
                trace.extend_from_slice(&bytes);
            } else {
                trace.extend_from_slice(data.as_bytes());
            }
            if read["eof"].as_bool().unwrap_or(true) {
                break;
            }
        }
        inner
            .call("", "IO.close", json!({"handle": stream}))
            .await?;
        Ok(trace)
    }

    pub async fn close(&self) -> Result<()> {
        let inner = self.inner.clone();
        if inner.profile.is_none() {
            // Connected, not launched: close what netweir opened, and go.
            let contexts =
                std::mem::take(&mut *inner.contexts.lock().unwrap_or_else(|e| e.into_inner()));
            for id in contexts {
                let dispose = inner.conn.call(
                    "",
                    "Target.disposeBrowserContext",
                    json!({"browserContextId": id}),
                );
                // A browser that doesn't answer won't hold the close up.
                let _ = tokio::time::timeout(Duration::from_secs(5), dispose).await;
            }
            inner.conn.shut();
            return Ok(());
        }
        if inner
            .child
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
        {
            return Ok(());
        }
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            inner.conn.call("", inner.close_method(), json!({})),
        )
        .await;
        tokio::task::spawn_blocking(move || inner.shut_down(Duration::from_secs(10)))
            .await
            .map_err(|e| Error::Launch(format!("closing the browser failed: {e}")))?
    }
}

/// For `Target.attachedToTarget` on the browser's session. A new page
/// Chrome holds: one opened by a page of netweir's, or by a frame from
/// another site in one, is a popup, taken on by its opener; anything else
/// (a page netweir made, which it attaches to itself, or someone else's
/// tab) is let go, and its session kept but unused. Left, it would take
/// the page offline back online: a session leaving a renderer clears
/// `Network.overrideNetworkState` for every page in that process (Blink's
/// network agent clears it as it goes, whether or not it set it). This
/// session's leaving reaches the renderer by another pipe than the page's
/// own commands, so it could land after the page had gone offline: a
/// page in an offline context read online now and then on Windows CI.
/// A page that was there already, which Chrome attaches without holding
/// (a tab of a browser netweir connected to, say), is left, before
/// netweir sets anything on it. Netweir
/// attaching to a page it made is heard here as well, and kept.
fn popped(
    conn: &Connection,
    pages: &Mutex<Vec<Weak<PageInner>>>,
    attaching: &Mutex<HashSet<String>>,
    params: &Value,
) {
    let info = &params["targetInfo"];
    let session = str_of(params, "sessionId");
    if info["type"] == "shared_worker" {
        return shared_worker(
            conn,
            pages,
            &session,
            info,
            params["waitingForDebugger"] == true,
        );
    }
    if params["waitingForDebugger"] != true {
        let ours = attaching
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&str_of(info, "targetId"));
        if !ours {
            conn.send("", "Target.detachFromTarget", json!({"sessionId": session}));
        }
        return;
    }
    let opener = info["openerId"].as_str().and_then(|id| {
        let pages = pages.lock().unwrap_or_else(|e| e.into_inner());
        pages
            .iter()
            .filter_map(Weak::upgrade)
            .map(|inner| Page { inner })
            .find(|page| page.opened(id))
    });
    match opener {
        Some(opener) => {
            let runtime = opener.runtime();
            adopt_now(
                &runtime,
                Page::adopt(opener, str_of(info, "targetId"), session),
            );
        }
        None => conn.send(&session, "Runtime.runIfWaitingForDebugger", json!({})),
    }
}

/// For a shared worker the browser's session holds: it gets the user agent
/// and environment of a page of netweir's in its context, as a page's
/// dedicated worker does, before it runs, and its session is kept, since
/// overrides go with it. One in no context of netweir's pages (a tab of
/// a browser netweir connected to) is let go and left.
fn shared_worker(
    conn: &Connection,
    pages: &Mutex<Vec<Weak<PageInner>>>,
    session: &str,
    info: &Value,
    held: bool,
) {
    let context = info["browserContextId"].as_str().unwrap_or_default();
    let env = {
        let pages = pages.lock().unwrap_or_else(|e| e.into_inner());
        let live: Vec<_> = pages.iter().filter_map(Weak::upgrade).collect();
        live.iter()
            .find(|p| p.context.as_deref() == Some(context))
            // A page of the browser's own context, whose id netweir
            // doesn't keep.
            .or_else(|| live.iter().find(|p| p.context.is_none()))
            .map(|p| crate::page::lock_env(&p.env).clone())
    };
    let Some(env) = env else {
        if held {
            conn.send(session, "Runtime.runIfWaitingForDebugger", json!({}));
        }
        conn.send("", "Target.detachFromTarget", json!({"sessionId": session}));
        return;
    };
    conn.send(session, "Network.enable", json!({}));
    if let Some(user_agent) = &env.switched_user_agent {
        conn.send(
            session,
            "Network.setUserAgentOverride",
            crate::page::for_workers(user_agent, &env.own_languages),
        );
    }
    for (method, params) in env.env.commands(crate::environment::Kind::Worker) {
        conn.send(session, method, params);
    }
    if held {
        conn.send(session, "Runtime.runIfWaitingForDebugger", json!({}));
    }
}

/// Runs `adopting` here, on the reader thread, as far as it goes without
/// waiting, and the rest on `runtime`. A same-site popup shares its
/// opener's renderer, and while Chrome holds it the opener is stopped
/// inside `window.open()`, its timers with it. Here the popup's setup is
/// sent and the popup let go before Chrome's next message is read, with
/// no wait for the runtime's thread to be free and scheduled. On a
/// Windows runner kept busy, that wait was 35 to 50 ms of every popup.
fn adopt_now(
    runtime: &tokio::runtime::Handle,
    adopting: impl std::future::Future<Output = ()> + Send + 'static,
) {
    let _entered = runtime.enter();
    let mut adopting = Box::pin(adopting);
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    if adopting.as_mut().poll(&mut cx).is_pending() {
        runtime.spawn(adopting);
    }
}

/// For `Browser.downloadWillBegin`, which Chrome sends straight after
/// the `Page.downloadWillBegin` of the page's own session, when there is
/// one. A download none told of was started by a frame from another
/// site, or by one inside it, whose sessions don't have the Page domain:
/// it's told to the frame's page, whose listeners would otherwise never
/// hear of it, and which cancels it if nobody holds it. One that's no
/// page's of netweir's (a tab of a browser netweir connected to) is
/// left.
fn unclaimed(pages: &Mutex<Vec<Weak<PageInner>>>, downloads: &Arc<Downloads>, params: &Value) {
    let guid = str_of(params, "guid");
    if downloads.knows(&guid) {
        return;
    }
    let frame = str_of(params, "frameId");
    let pages: Vec<Page> = {
        let pages = pages.lock().unwrap_or_else(|e| e.into_inner());
        pages
            .iter()
            .filter_map(Weak::upgrade)
            .map(|inner| Page { inner })
            .collect()
    };
    if let Some(page) = pages.iter().find(|page| page.opened(&frame)) {
        page.download_began(params);
        return;
    }
    // A frame inside one from another site: only that frame's process
    // knows it, so each page's frames are asked.
    let Some(runtime) = pages.first().map(Page::runtime) else {
        return;
    };
    downloads.look_for(&guid);
    let (downloads, params) = (downloads.clone(), params.clone());
    runtime.spawn(async move {
        for page in pages {
            let Ok(frames) = page.frames().await else {
                continue;
            };
            if frames.iter().any(|f| f.id() == frame) {
                page.download_began(&params);
                return;
            }
        }
        downloads.forget(&guid);
    });
}

impl Inner {
    /// Gives `context` (None for the default one) a folder of its own for
    /// downloads, and has Chrome save them there.
    pub(crate) async fn allow_downloads(&self, context: Option<&str>) -> Result<()> {
        let folder = self
            .downloads
            .folder(context)
            .map_err(|e| Error::Launch(format!("can't make a folder for downloads: {e}")))?;
        let mut params = json!({
            "behavior": "allowAndName",
            "downloadPath": folder.display().to_string(),
            "eventsEnabled": true,
        });
        if let Some(id) = context {
            params["browserContextId"] = Value::from(id);
        }
        match self.call("", "Browser.setDownloadBehavior", params).await {
            Ok(_) => Ok(()),
            Err(Error::Protocol { message, .. }) => {
                crate::warn(&format!(
                    "the browser won't save downloads where netweir reads them: {message}"
                ));
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Sends `method` and waits for the answer, for no longer than the
    /// timeout, with what Chrome has lately said in the error if it
    /// doesn't come.
    pub(crate) async fn call(&self, session: &str, method: &str, params: Value) -> Result<Value> {
        self.conn
            .call_within(self.timeout, session, method, params)
            .await
            .map_err(|e| self.explain(e))
    }

    /// A timeout, with what Chrome has lately said after it: why Chrome
    /// stopped answering is often there.
    pub(crate) fn explain(&self, e: Error) -> Error {
        match (e, &self.said) {
            (Error::Timeout(m), Some(said)) => Error::Timeout(said.lately(m)),
            (e, _) => e,
        }
    }

    fn shut_down(&self, grace: Duration) -> Result<()> {
        let Some(mut child) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            // Connected, not launched: let go of the browser. Its contexts
            // go with the connection (disposeOnDetach).
            self.conn.shut();
            return Ok(());
        };
        self.conn.shut();
        let deadline = Instant::now() + grace;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
            }
        }
        let lifeline = self
            .lifeline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(lifeline) = lifeline {
            lifeline.process_gone();
        }
        self.downloads.remove_all();
        let removed = match &self.profile {
            Some(profile) if !self.keep_profile => remove_profile(profile),
            _ => Ok(()),
        };
        drop(
            self.temp_profile
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take(),
        );
        removed
    }
}

impl Inner {
    pub(crate) fn engine(&self) -> Engine {
        match self.conn.dialect() {
            crate::conn::Dialect::Cdp => Engine::Chrome,
            crate::conn::Dialect::Bidi => Engine::Firefox,
        }
    }

    /// What asks the browser to quit.
    pub(crate) fn close_method(&self) -> &'static str {
        match self.engine() {
            Engine::Chrome => "Browser.close",
            Engine::Firefox => "browser.close",
        }
    }

    /// For what Netweir can't do on Firefox, or can't do there yet.
    pub(crate) fn chrome_only(&self, feature: &str) -> Result<()> {
        match self.engine() {
            Engine::Chrome => Ok(()),
            engine => Err(Error::Unsupported {
                engine: engine.name(),
                feature: feature.to_string(),
            }),
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // A profile to keep: Chrome is asked to quit, as close() asks, so
        // it writes out what it hasn't yet, and is let go without waiting.
        // A thread waits for it, killing it after ten seconds, and only
        // then lets go of the lifeline; if the program ends first, the
        // lifeline asks again, as gently as the system allows. The writer
        // is a thread, not a task, so the ask reaches Chrome with or
        // without a runtime.
        if self.keep_profile
            && let Some(mut child) = self
                .child
                .get_mut()
                .unwrap_or_else(|e| e.into_inner())
                .take()
        {
            self.conn.send("", self.close_method(), json!({}));
            self.conn.shut();
            let lifeline = self
                .lifeline
                .get_mut()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            let quitting = Quitting::start(self.profile.as_deref().unwrap_or(Path::new("")));
            let _ = std::thread::Builder::new()
                .name("netweir-chrome-quit".into())
                .spawn(move || {
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                    if let Some(lifeline) = lifeline {
                        lifeline.process_gone();
                    }
                    drop(quitting);
                });
            return;
        }
        let _ = self.shut_down(Duration::ZERO);
    }
}

/// A kept profile whose Chrome, its browser dropped, is still quitting.
/// A Chrome launched with it meanwhile would find the old one, hand its
/// pages over to it and exit, so a launch waits for it first.
struct Quitting(PathBuf);

/// The profiles being quit, by the folder as the system names it, each as
/// many times as Chromes quit with it.
static QUITTING: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

impl Quitting {
    fn start(profile: &Path) -> Quitting {
        let profile = std::fs::canonicalize(profile).unwrap_or_else(|_| profile.to_path_buf());
        QUITTING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(profile.clone());
        Quitting(profile)
    }

    /// Waits until no Chrome is quitting with `profile`: ten seconds at
    /// most, as long as one is given before it's killed.
    async fn wait_for(profile: &Path) {
        let profile = std::fs::canonicalize(profile).unwrap_or_else(|_| profile.to_path_buf());
        while QUITTING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&profile)
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for Quitting {
    fn drop(&mut self) {
        let mut quitting = QUITTING.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(at) = quitting.iter().position(|p| *p == self.0) {
            quitting.remove(at);
        }
    }
}

/// Chrome's helper processes can hold files in the profile for a moment
/// after the browser exits, and Windows won't delete an open file. On
/// Windows they've been ended and waited for by now (see
/// [`lifeline::Lifeline::process_gone`]); what's left is a scanner or an
/// indexer with a file open for a moment, which a few seconds' retrying
/// outlasts. A file still held after that is said in the error, path and
/// all, rather than waited on for good: Chrome is gone by then, and the
/// directory is in the system's temporary folder.
fn remove_profile(dir: &Path) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                return Err(Error::Launch(format!(
                    "can't remove the profile directory {}: {e}",
                    dir.display()
                )));
            }
        }
    }
}

/// Keeps Chrome away from the system keyring, where a fresh profile goes
/// for the key it encrypts cookies with. A headless Chrome can't show the
/// prompt that asks for access, so it waits on one nobody can answer.
/// Nothing a page can see changes.
#[cfg(target_os = "macos")]
const KEYRING: &[&str] = &["--use-mock-keychain"];
#[cfg(all(unix, not(target_os = "macos")))]
const KEYRING: &[&str] = &["--password-store=basic"];
#[cfg(not(unix))]
const KEYRING: &[&str] = &[];

/// Keeps every page at full speed. Chrome treats a page that isn't the
/// visible tab as background, slows its timers, and on macOS asks the OS
/// to run its renderer at low priority; on a busy machine a new page could
/// take over 30 seconds to answer at all. Puppeteer and Playwright pass
/// the same switches.
const FOREGROUND: &[&str] = &[
    "--disable-renderer-backgrounding",
    "--disable-backgrounding-occluded-windows",
    "--disable-background-timer-throttling",
];

/// Chrome as Google ships it to people. Chrome for Testing shows an
/// infobar saying what it is, which takes 56 pixels from every page's
/// viewport (a window 143 pixels taller than its page, where a person's
/// Chrome is 87), and applies the field trial config its builds carry,
/// which a person's Chrome doesn't: with it, page script saw one language
/// (`["en-US"]`, against `["en-US", "en"]`) and documents sent
/// Accept-Language before Upgrade-Insecure-Requests, where Google Chrome
/// sends it last, as netweir's HTTP profiles do. Google Chrome itself
/// has neither, and takes both switches as having nothing to turn off.
const AS_SHIPPED: &[&str] = &["--disable-infobars", "--disable-field-trial-config"];

/// The switches each probe was started with, for the tests that count
/// them.
#[cfg(test)]
static PROBED: Mutex<Vec<Vec<String>>> = Mutex::new(Vec::new());

/// What a headless Chrome at `executable` says it is: its user agent and
/// its client hints, which only asking it tells exactly, and only before
/// a --user-agent flag empties the hints. The first time, it's started once
/// to ask.
async fn headless_identity(
    executable: &Path,
    extra: &[String],
    timeout: Duration,
) -> Result<(String, Value)> {
    // By the file as it is now, so a Chrome updated in place is asked
    // again, and by the switches it's started with.
    type Key = (PathBuf, Option<std::time::SystemTime>, u64, Vec<String>);
    type Answer = Arc<tokio::sync::OnceCell<Result<(String, Value)>>>;
    // One probe for each key at a time, whose outcome every launch waiting
    // on it takes: each asking on its own started a Chrome apiece at the
    // moment a fresh machine is slowest to start one, and two at once on
    // a new Windows runner both sat over 30 seconds on their first
    // Target.createTarget. An answer is kept; a failure goes to those
    // already waiting and no further, so the next launch asks again.
    static KNOWN: Mutex<Option<HashMap<Key, Answer>>> = Mutex::new(None);
    let meta = std::fs::metadata(executable).ok();
    let key: Key = (
        executable.to_path_buf(),
        meta.as_ref().and_then(|m| m.modified().ok()),
        meta.as_ref().map_or(0, |m| m.len()),
        // Switches can change what Chrome says it is.
        extra.to_vec(),
    );
    let answer = KNOWN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .entry(key.clone())
        .or_default()
        .clone();
    let outcome = answer
        .get_or_init(|| ask_identity(executable, extra, timeout))
        .await
        .clone();
    if outcome.is_err() {
        let mut known = KNOWN.lock().unwrap_or_else(|e| e.into_inner());
        let map = known.get_or_insert_with(HashMap::new);
        if map.get(&key).is_some_and(|a| Arc::ptr_eq(a, &answer)) {
            map.remove(&key);
        }
    }
    outcome
}

/// [`probe_identity`], again with a new Chrome if the first sat on its
/// first page: on a busy Mac runner one now and then never answers, and
/// the next does. A second such wait is a launch that failed, whichever
/// call inside it ran out of time.
async fn ask_identity(
    executable: &Path,
    extra: &[String],
    timeout: Duration,
) -> Result<(String, Value)> {
    again_if_stuck(|| probe_identity(executable, extra, timeout)).await
}

async fn again_if_stuck<T, F: Future<Output = Result<T>>>(probe: impl Fn() -> F) -> Result<T> {
    match probe().await {
        Err(Error::Timeout(_)) => probe().await.map_err(|e| match e {
            Error::Timeout(message) => Error::Launch(message),
            other => other,
        }),
        other => other,
    }
}

/// Starts a headless Chrome at `executable` to ask what it is.
async fn probe_identity(
    executable: &Path,
    extra: &[String],
    timeout: Duration,
) -> Result<(String, Value)> {
    #[cfg(test)]
    PROBED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(extra.to_vec());
    let temp = launch::TempProfile::make()
        .map_err(|e| Error::Launch(format!("can't make a profile directory: {e}")))?;
    let profile = temp.dir().to_path_buf();
    // The caller's switches too: a Chrome that only starts with, say,
    // --no-sandbox must have it here as well.
    let mut args = vec![
        format!("--user-data-dir={}", profile.display()),
        "--no-first-run".into(),
        "--headless".into(),
    ];
    args.extend(
        KEYRING
            .iter()
            .chain(FOREGROUND)
            .chain(AS_SHIPPED)
            .map(|a| a.to_string()),
    );
    args.extend(extra.iter().cloned());
    args.push("about:blank".into());
    let started = launch::start(executable, &args)?;
    let said = started.said.clone();
    let probe = Browser {
        inner: Arc::new(Inner {
            conn: Connection::new(started.replies, started.commands, Some(&started.child)),
            child: Mutex::new(Some(started.child)),
            lifeline: Mutex::new(Some(started.lifeline)),
            profile: Some(profile),
            keep_profile: false,
            temp_profile: Mutex::new(Some(temp)),
            contexts: Mutex::default(),
            tracing_done: Arc::default(),
            version: String::new(),
            identity: None,
            timeout: Duration::from_secs(30),
            pause: LaunchOptions::default().dialog_pause,
            proxy_login: None,
            said: Some(said.clone()),
            pages: Arc::default(),
            attaching: Arc::default(),
            downloads: Arc::default(),
            environment: Environment::default(),
            arranges: false,
            screen: crate::window::DEFAULT_SCREEN,
            screen_scale: 1.0,
            worker_header_order: false,
            window_size: None,
            agent: None,
            own_languages: Mutex::default(),
            screens: tokio::sync::Mutex::new(()),
            own: tokio::sync::OnceCell::new(),
            stealth: true,
            default_context: None,
            first_window: None,
        }),
    };
    let what = executable.display().to_string();
    let asked = async {
        let version = tokio::time::timeout(
            timeout,
            probe.inner.conn.call("", "Browser.getVersion", json!({})),
        )
        .await
        // A Timeout, so `again_if_stuck` tries a new Chrome: stuck probes
        // on a busy Mac runner sat here, with no renderer started yet.
        .map_err(|_| {
            Error::Timeout(format!(
                "{what} didn't answer in {}s",
                timeout.as_secs_f64()
            ))
        })?
        .map_err(|e| Error::Launch(format!("{what} didn't answer: {e}")))?;
        let metadata = probe.own_metadata().await?;
        Ok::<_, Error>((str_of(&version, "userAgent"), metadata))
    }
    .await;
    // What it said counts even if it then wouldn't close cleanly. One
    // that never answered isn't asked to close: dropping it ends it.
    if asked.is_ok() {
        let _ = probe.close().await;
    }
    asked.map_err(|e| match e {
        Error::Launch(message) => Error::Launch(said.explain(message)),
        Error::Timeout(message) => Error::Timeout(said.explain(message)),
        other => other,
    })
}

/// `host:port`, `host`, `[::1]:port` or `[::1]`, with 443 by default.
fn host_and_port(authority: &str) -> std::result::Result<(&str, u16), String> {
    let port = |p: &str| p.parse::<u16>().map_err(|e| format!("port {p:?}: {e}"));
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']').ok_or("an unclosed [ in the address")?;
        return match after.strip_prefix(':') {
            Some(p) => Ok((host, port(p)?)),
            None => Ok((host, 443)),
        };
    }
    match authority.rsplit_once(':') {
        Some((host, p)) => Ok((host, port(p)?)),
        None => Ok((authority, 443)),
    }
}

/// A `wss://` WebSocket over BoringSSL, the TLS library netweir's HTTP
/// client uses, trusting the roots Chrome trusts.
async fn secure_websocket(
    url: &str,
) -> std::result::Result<
    tokio_tungstenite::WebSocketStream<tokio_btls::SslStream<tokio::net::TcpStream>>,
    String,
> {
    use btls::ssl::{SslConnector, SslMethod};
    use btls::x509::X509;
    use btls::x509::store::X509StoreBuilder;

    let authority = url
        .trim_start_matches("wss://")
        .split('/')
        .next()
        .unwrap_or_default();
    let (host, port) = host_and_port(authority)?;
    let tcp = tokio::net::TcpStream::connect((host, port))
        .await
        .map_err(|e| e.to_string())?;
    static ROOTS: std::sync::LazyLock<Vec<X509>> = std::sync::LazyLock::new(|| {
        chromium_roots::TLS_SERVER_ROOT_CERTS
            .iter()
            .filter_map(|der| X509::from_der(der.as_ref()).ok())
            .collect()
    });
    let mut store = X509StoreBuilder::new().map_err(|e| e.to_string())?;
    for root in ROOTS.iter() {
        store.add_cert(root).map_err(|e| e.to_string())?;
    }
    let mut connector = SslConnector::builder(SslMethod::tls()).map_err(|e| e.to_string())?;
    connector.set_cert_store(store.build());
    let ssl = connector
        .build()
        .configure()
        .and_then(|c| c.into_ssl(host))
        .map_err(|e| e.to_string())?;
    let mut tls = tokio_btls::SslStream::new(ssl, tcp).map_err(|e| e.to_string())?;
    std::pin::Pin::new(&mut tls)
        .connect()
        .await
        .map_err(|e| match tls.ssl().verify_result() {
            Err(why) => format!("the certificate wasn't trusted: {}", why.error_string()),
            Ok(()) => e.to_string(),
        })?;
    let (socket, _) = tokio_tungstenite::client_async(url, tls)
        .await
        .map_err(|e| e.to_string())?;
    Ok(socket)
}

/// The WebSocket URL a browser's `http://host:port/json/version` names.
async fn devtools_url(base: &str) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let failed = |e: String| Error::Launch(format!("{base}: {e}"));
    let host = base
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or_default()
        .to_string();
    let read = async {
        let mut stream = tokio::net::TcpStream::connect(&host)
            .await
            .map_err(|e| failed(e.to_string()))?;
        let request =
            format!("GET /json/version HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|e| failed(e.to_string()))?;
        // Chrome keeps the connection open whatever the request says, so
        // the body is read by its length, not to the end.
        let mut reply = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(at) = reply.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&reply[..at]).to_ascii_lowercase();
                let ok = head
                    .lines()
                    .next()
                    .is_some_and(|status| status.split(' ').nth(1) == Some("200"));
                let length: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .ok_or_else(|| failed("no Content-Length in the reply".into()))?;
                if reply.len() >= at + 4 + length {
                    let body = reply[at + 4..at + 4 + length].to_vec();
                    if !ok {
                        // Chrome says why, such as a host name it won't
                        // serve DevTools to.
                        let why = String::from_utf8_lossy(&body).trim().to_string();
                        return Err(failed(format!("refused: {why}")));
                    }
                    return Ok::<_, Error>(body);
                }
            }
            let n = stream
                .read(&mut chunk)
                .await
                .map_err(|e| failed(e.to_string()))?;
            if n == 0 {
                return Err(failed("the reply ended early".into()));
            }
            reply.extend_from_slice(&chunk[..n]);
        }
    };
    let body = tokio::time::timeout(Duration::from_secs(30), read)
        .await
        .map_err(|_| failed("no answer in 30 seconds".into()))??;
    let version: Value =
        serde_json::from_slice(&body).map_err(|e| failed(format!("not DevTools: {e}")))?;
    version["webSocketDebuggerUrl"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| failed("no webSocketDebuggerUrl in /json/version".into()))
}

/// `"Chromium";v="154", "Google Chrome";v="154"` as (brand, version)
/// pairs.
fn parse_brands(header: &str) -> Vec<(String, String)> {
    header
        .split(',')
        .filter_map(|part| {
            let (brand, version) = part.trim().split_once(";v=")?;
            Some((
                brand.trim_matches('"').to_string(),
                version.trim_matches('"').to_string(),
            ))
        })
        .collect()
}

/// A proxy URL as Chrome takes it (`scheme://host:port`, which has no
/// room for a login), and the username and password that were in it.
fn split_proxy(proxy: &str) -> Result<(String, Option<(String, String)>)> {
    // Without the login: an error mustn't repeat a password.
    let shown = match proxy.rsplit_once('@') {
        Some((before, host)) => match before.split_once("://") {
            Some((scheme, _)) => format!("{scheme}://…@{host}"),
            None => format!("…@{host}"),
        },
        None => proxy.to_string(),
    };
    let bad = || Error::Invalid(format!("not a proxy URL: {shown}"));
    let (scheme, rest) = proxy.split_once("://").ok_or_else(bad)?;
    let rest = rest.trim_end_matches('/');
    let Some((login, host)) = rest.rsplit_once('@') else {
        return Ok((format!("{scheme}://{rest}"), None));
    };
    if host.is_empty() {
        return Err(bad());
    }
    if scheme.starts_with("socks") {
        return Err(Error::Invalid(format!(
            "Chrome can't log in to a SOCKS proxy ({shown}); a login works for http:// and \
             https:// proxies"
        )));
    }
    let (user, password) = login.split_once(':').unwrap_or((login, ""));
    Ok((
        format!("{scheme}://{host}"),
        Some((unescape(user), unescape(password))),
    ))
}

/// Undoes URL percent-escapes, so a password can hold `@` or `:`.
fn unescape(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok());
        match (bytes[i], hex.and_then(|h| u8::from_str_radix(h, 16).ok())) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn str_of(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Pages that share cookies and storage.
#[derive(Clone)]
pub struct Context {
    browser: Arc<Inner>,
    id: String,
    /// What every page of the context routes.
    rules: Arc<ContextRules>,
    /// Its pages' environment, over the browser's.
    env: Arc<Mutex<Environment>>,
}

impl Context {
    pub async fn new_page(&self) -> Result<Page> {
        self.new_page_with(Environment::default()).await
    }

    /// A page of the context, with `env` over the context's environment.
    /// Its timezone and locale must be the context's: Chrome keeps one of
    /// each per renderer process, and pages of a context can share one.
    pub async fn new_page_with(&self, env: Environment) -> Result<Page> {
        let context = self.environment();
        shares_process(
            &env,
            &context,
            "Give it to the context, or open the page with Browser.new_page",
        )?;
        let env = env.over(&context);
        added_screen(&env, &self.browser.environment)?;
        Page::open(
            self.browser.clone(),
            Some(self.id.clone()),
            false,
            None,
            Some(self.rules.clone()),
            env,
        )
        .await
    }

    /// The context's environment.
    pub fn environment(&self) -> Environment {
        self.env.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn update(&self, change: impl Fn(&mut Environment)) {
        change(&mut self.env.lock().unwrap_or_else(|e| e.into_inner()));
    }

    /// Grants permissions to `origin` (such as "https://example.com"), or
    /// to every origin, for every page of the context.
    pub async fn grant_permissions(
        &self,
        permissions: &[String],
        origin: Option<&str>,
    ) -> Result<()> {
        self.browser.chrome_only("Context.grant_permissions")?;
        crate::page::grant(&self.browser, Some(&self.id), permissions, origin).await
    }

    /// Takes back every permission granted in the context.
    pub async fn clear_permissions(&self) -> Result<()> {
        self.browser.chrome_only("Context.clear_permissions")?;
        crate::page::clear_permissions(&self.browser, Some(&self.id)).await
    }

    /// See [`Page::set_geolocation`]; for every page of the context, those
    /// open now and those opened later.
    pub async fn set_geolocation(&self, at: Option<Geolocation>) -> Result<()> {
        self.browser.chrome_only("Context.set_geolocation")?;
        self.update(|e| e.geolocation = at);
        for page in self.rules.pages() {
            gone_is_fine(page.set_geolocation(at).await)?;
        }
        Ok(())
    }

    /// See [`Page::set_offline`]; for every page of the context.
    pub async fn set_offline(&self, offline: bool) -> Result<()> {
        self.browser.chrome_only("Context.set_offline")?;
        self.update(|e| e.offline = Some(offline));
        for page in self.rules.pages() {
            gone_is_fine(page.set_offline(offline).await)?;
        }
        Ok(())
    }

    /// See [`Page::set_extra_headers`]; for every page of the context.
    pub async fn set_extra_headers(&self, headers: &[(String, String)]) -> Result<()> {
        self.browser.chrome_only("Context.set_extra_headers")?;
        let mut env = self.environment();
        env.extra_headers = headers.to_vec();
        env.validate()?;
        self.update(|e| e.extra_headers = headers.to_vec());
        for page in self.rules.pages() {
            gone_is_fine(page.set_extra_headers(headers).await)?;
        }
        Ok(())
    }

    /// See [`Page::add_init_script`]; for every page of the context.
    pub async fn add_init_script(&self, script: &str) -> Result<()> {
        self.browser.chrome_only("Context.add_init_script")?;
        self.update(|e| e.init_scripts.push(script.to_string()));
        for page in self.rules.pages() {
            gone_is_fine(page.add_init_script(script).await)?;
        }
        Ok(())
    }

    /// Like [`Page::route`], for every page of the context, those open
    /// now and those opened later. A page's own routes come first.
    pub async fn route(&self) -> Result<tokio::sync::mpsc::UnboundedReceiver<Route>> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        crate::network::lock(&self.rules.rules).routes = Some(tx);
        self.refresh().await?;
        Ok(rx)
    }

    /// Stops the context's routing; requests waiting for a decision go on
    /// unchanged.
    pub async fn unroute(&self) -> Result<()> {
        crate::network::lock(&self.rules.rules).routes = None;
        self.refresh().await
    }

    /// Like [`Page::drop_routes`], for the context's routes.
    pub fn drop_routes(&self) {
        crate::network::lock(&self.rules.rules).routes = None;
    }

    async fn refresh(&self) -> Result<()> {
        for page in self.rules.pages() {
            match page.refresh_fetch().await {
                Ok(()) | Err(Error::PageClosed) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    pub async fn cookies(&self) -> Result<Vec<Cookie>> {
        self.browser.chrome_only("Context.cookies")?;
        context_cookies(&self.browser, Some(&self.id)).await
    }

    pub async fn set_cookies(&self, cookies: &[Cookie]) -> Result<()> {
        self.browser.chrome_only("Context.set_cookies")?;
        set_context_cookies(&self.browser, Some(&self.id), cookies).await
    }

    /// The context's cookies, and the localStorage of every origin its
    /// pages loaded a document from, as Playwright's `storageState` has
    /// them. Each origin is read in a page opened for it, whose requests
    /// never leave Chrome.
    pub async fn storage_state(&self) -> Result<crate::StorageState> {
        self.browser.chrome_only("Context.storage_state")?;
        let origins: Vec<String> = self
            .rules
            .origins
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect();
        crate::storage::state_of(&self.browser, Some(&self.id), &origins).await
    }

    /// Sets a storage state's cookies and localStorage in the context, as
    /// a new Playwright context made with `storage_state` has them.
    pub async fn set_storage_state(&self, state: &crate::StorageState) -> Result<()> {
        self.browser.chrome_only("Context.set_storage_state")?;
        crate::storage::set_state(&self.browser, Some(&self.id), &self.rules.origins, state).await
    }

    /// Closes the context and every page in it.
    pub async fn close(&self) -> Result<()> {
        let (method, params) = match self.browser.engine() {
            Engine::Chrome => (
                "Target.disposeBrowserContext",
                json!({"browserContextId": self.id}),
            ),
            Engine::Firefox => ("browser.removeUserContext", json!({"userContext": self.id})),
        };
        match self.browser.call("", method, params).await {
            Ok(_) | Err(Error::Closed) => Ok(()),
            // Already disposed.
            Err(Error::Protocol { .. }) => Ok(()),
            Err(e) => Err(e),
        }
        .inspect(|()| self.browser.downloads.remove(Some(&self.id)))
    }
}

/// A page that closed while it was told is no failure.
fn gone_is_fine(r: Result<()>) -> Result<()> {
    match r {
        Err(Error::PageClosed) => Ok(()),
        r => r,
    }
}

impl Inner {
    /// What Chrome adds to a request itself, as its switches say, for a
    /// page whose sessions have a user agent override: None where netweir
    /// knows neither. `own` is the context's own languages, as Chrome
    /// shows them.
    pub(crate) fn tidy(&self, own: &str) -> Option<crate::network::Tidy> {
        let accept_language = (!own.is_empty()).then(|| crate::environment::q_values(own));
        (self.agent.is_some() || accept_language.is_some()).then(|| crate::network::Tidy {
            user_agent: self.agent.clone(),
            accept_language,
        })
    }
}

/// Checks a page of a shared context (`shared`'s) asks for no timezone or
/// locale of its own: Chrome keeps one of each per renderer process, and
/// a context's pages of one site can share one.
fn shares_process(env: &Environment, shared: &Environment, instead: &str) -> Result<()> {
    for (what, own, theirs) in [
        ("timezone", &env.timezone, &shared.timezone),
        ("locale", &env.locale, &shared.locale),
    ] {
        if own.is_some() && own != theirs {
            return Err(Error::Invalid(format!(
                "a page's {what} must be its context's: Chrome keeps one per renderer process, \
                 and a context's pages of one site can share one. {instead}"
            )));
        }
    }
    Ok(())
}

/// Checks a screen that isn't the browser's own can be added, and says
/// once what a phone on one gives away.
fn added_screen(env: &Environment, browser: &Environment) -> Result<()> {
    if env.screen != browser.screen || env.device_scale_factor != browser.device_scale_factor {
        env.validate_added_screen()?;
    }
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if env.is_mobile()
        && !browser.is_mobile()
        && !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed)
    {
        crate::warn(
            "a phone given to one page or context is shown on a screen of its own beside the \
             browser's, so screen.isExtended is true and a frame from another site reads where \
             that screen is; its service and shared workers keep the browser's user agent, and \
             it keeps the browser's PDF viewer. Give the phone to the whole browser \
             (LaunchOptions::environment, or environment= in Python) where those matter",
        );
    }
    Ok(())
}

pub(crate) async fn context_cookies(browser: &Inner, context: Option<&str>) -> Result<Vec<Cookie>> {
    let mut params = json!({});
    if let Some(id) = context {
        params["browserContextId"] = Value::from(id);
    }
    let r = browser.call("", "Storage.getCookies", params).await?;
    Ok(cookies_from(&r["cookies"]))
}

pub(crate) async fn set_context_cookies(
    browser: &Inner,
    context: Option<&str>,
    cookies: &[Cookie],
) -> Result<()> {
    let mut params = json!({"cookies": cookies_to(cookies)});
    if let Some(id) = context {
        params["browserContextId"] = Value::from(id);
    }
    browser.call("", "Storage.setCookies", params).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_proxy_login_comes_off_the_url_chrome_gets() {
        assert_eq!(
            split_proxy("http://proxy:8080").unwrap(),
            ("http://proxy:8080".into(), None)
        );
        assert_eq!(
            split_proxy("http://ann:p%40ss%3Aword@10.0.0.1:1080/").unwrap(),
            (
                "http://10.0.0.1:1080".into(),
                Some(("ann".into(), "p@ss:word".into()))
            )
        );
        assert_eq!(
            split_proxy("socks5://10.0.0.1:1080").unwrap(),
            ("socks5://10.0.0.1:1080".into(), None)
        );
        assert!(split_proxy("proxy:8080").is_err());
        assert!(split_proxy("http://ann@").is_err());
    }

    #[test]
    fn chrome_cant_log_in_to_a_socks_proxy_so_it_isnt_asked_to() {
        let err = split_proxy("socks5://ann:secret@10.0.0.1:1080").unwrap_err();
        assert!(err.to_string().contains("SOCKS"), "{err}");
    }

    #[test]
    fn a_bad_proxy_url_doesnt_repeat_its_password() {
        for bad in ["http://ann:hunter2@", "ann:hunter2@proxy"] {
            let err = split_proxy(bad).unwrap_err().to_string();
            assert!(!err.contains("hunter2"), "{err}");
        }
    }

    #[test]
    fn wss_addresses_come_apart_into_host_and_port() {
        assert_eq!(host_and_port("example.com"), Ok(("example.com", 443)));
        assert_eq!(host_and_port("example.com:9222"), Ok(("example.com", 9222)));
        assert_eq!(host_and_port("[::1]"), Ok(("::1", 443)));
        assert_eq!(host_and_port("[::1]:9222"), Ok(("::1", 9222)));
        assert!(host_and_port("[::1").is_err());
        assert!(host_and_port("host:nope").is_err());
    }

    #[test]
    fn brands_that_dont_parse_leave_chromes_own() {
        assert!(parse_brands("nonsense").is_empty());
    }

    /// A switch Chrome ignores, new each time, so nothing is known yet
    /// for it and no other test's probe has it.
    fn fresh_switch() -> Vec<String> {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        vec![format!("--netweir-test-probe={}-{n}", std::process::id())]
    }

    /// How many probes have been started with these switches.
    #[tokio::test]
    async fn a_probe_stuck_once_is_asked_again_and_twice_is_a_failed_launch() {
        let run = |answers: Vec<Result<u8>>| async move {
            let answers = Mutex::new(answers.into_iter());
            let asked = std::sync::atomic::AtomicUsize::new(0);
            let outcome = again_if_stuck(|| {
                asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let answer = answers.lock().unwrap().next().unwrap();
                async move { answer }
            })
            .await;
            (outcome, asked.into_inner())
        };
        let stuck = || Err(Error::Timeout("Chrome didn't answer".into()));
        assert!(matches!(run(vec![Ok(1)]).await, (Ok(1), 1)));
        assert!(matches!(run(vec![stuck(), Ok(2)]).await, (Ok(2), 2)));
        let (twice, asked) = run(vec![stuck(), stuck()]).await;
        assert_eq!(asked, 2);
        assert!(
            matches!(&twice, Err(Error::Launch(m)) if m.contains("didn't answer")),
            "{twice:?}"
        );
        // Anything else is an answer: asking again wouldn't change it.
        let refused = Err(Error::Launch("refused".into()));
        assert!(matches!(
            run(vec![refused]).await,
            (Err(Error::Launch(_)), 1)
        ));
    }

    fn probes(extra: &[String]) -> usize {
        let probed = PROBED.lock().unwrap_or_else(|e| e.into_inner());
        probed.iter().filter(|p| p.as_slice() == extra).count()
    }

    #[tokio::test]
    async fn launches_at_once_ask_chrome_what_it_is_once() {
        let executable = match launch::find_chrome() {
            Ok(path) => path,
            Err(e) if std::env::var_os("NETWEIR_REQUIRE_CHROME").is_some() => panic!("{e}"),
            Err(e) => return eprintln!("skipped: {e}"),
        };
        let extra = fresh_switch();
        let (a, b) = tokio::join!(
            headless_identity(&executable, &extra, Duration::from_secs(30)),
            headless_identity(&executable, &extra, Duration::from_secs(30))
        );
        assert_eq!(a.unwrap(), b.unwrap());
        assert_eq!(probes(&extra), 1);
        // And the answer is kept.
        headless_identity(&executable, &extra, Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(probes(&extra), 1);
    }

    /// A "Chrome" in `dir` that runs `script`, run once before it's
    /// returned: macOS checks a new executable the first time it runs,
    /// which took up to 12 seconds with the other tests running, and the
    /// tests that time a launch mustn't time that.
    #[cfg(unix)]
    fn fake_chrome(dir: &Path, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        std::fs::create_dir_all(dir).unwrap();
        let fake = dir.join("chrome");
        std::fs::write(
            &fake,
            format!("#!/bin/sh\n[ \"$1\" = --once ] && exit\n{script}"),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Linux won't run a file another process holds open for writing,
        // and a process another test forks while the file is being
        // written holds it until that child execs: "Text file busy" for
        // a moment, then not.
        let mut tries = 0;
        let ran = loop {
            match std::process::Command::new(&fake).arg("--once").status() {
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && tries < 100 => {
                    tries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                other => break other,
            }
        };
        assert!(ran.unwrap().success());
        fake
    }

    /// A Chrome that says it won't be driven with its profile, and runs
    /// on, fails the launch at once rather than at the timeout.
    #[cfg(unix)]
    #[tokio::test]
    async fn chrome_refusing_its_profile_fails_the_launch_at_once() {
        let dir = std::env::temp_dir().join(format!("netweir-refuses-{}", std::process::id()));
        let fake = fake_chrome(
            &dir,
            "echo 'DevTools remote debugging requires a non-default data \
             directory. Specify this using --user-data-dir.' >&2\nexec sleep 60\n",
        );
        let started = Instant::now();
        let launched = Browser::launch(LaunchOptions {
            executable: Some(fake),
            headless: false,
            user_data_dir: Some(dir.join("profile")),
            ..LaunchOptions::default()
        })
        .await;
        let Err(Error::Launch(message)) = launched else {
            panic!("launched, or failed some other way");
        };
        assert!(message.contains("own default profile"), "{message}");
        assert!(message.contains("netweir install chrome"), "{message}");
        assert!(started.elapsed() < Duration::from_secs(10));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A Chrome that never answers is given the launch's `timeout`, not a
    /// fixed 30 seconds: a slow machine can wait longer, a script less.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_silent_chrome_is_given_the_launchs_timeout() {
        let dir = std::env::temp_dir().join(format!("netweir-silent-{}", std::process::id()));
        let fake = fake_chrome(&dir, "exec sleep 60\n");
        for headless in [true, false] {
            let started = Instant::now();
            let switch = fresh_switch();
            let launched = Browser::launch(LaunchOptions {
                executable: Some(fake.clone()),
                headless,
                args: switch.clone(),
                timeout: Duration::from_secs(1),
                ..LaunchOptions::default()
            })
            .await;
            let Err(Error::Launch(message)) = launched else {
                panic!("launched, or failed some other way");
            };
            assert!(message.contains("in 1s"), "{message}");
            // Headless asks a probe Chrome first, and a silent one twice.
            // Contains, not equals: $NETWEIR_CHROME_ARGS adds switches.
            let probed = PROBED
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .filter(|p| p.contains(&switch[0]))
                .count();
            assert_eq!(probed, if headless { 2 } else { 0 });
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "headless={headless}: {:?}",
                started.elapsed()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A probe that fails fails every launch waiting on it at once, and
    /// the next launch asks again.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_probe_fails_those_waiting_and_no_more() {
        let dir = std::env::temp_dir().join(format!("netweir-probe-{}", std::process::id()));
        // A "Chrome" that says nothing for a second, then exits.
        let fake = fake_chrome(&dir, "sleep 1\n");
        let extra = fresh_switch();
        let started = std::time::Instant::now();
        let (a, b, c) = tokio::join!(
            headless_identity(&fake, &extra, Duration::from_secs(30)),
            headless_identity(&fake, &extra, Duration::from_secs(30)),
            headless_identity(&fake, &extra, Duration::from_secs(30))
        );
        assert!(a.is_err() && b.is_err() && c.is_err());
        assert_eq!(probes(&extra), 1, "each waiting launch started its own");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert!(
            headless_identity(&fake, &extra, Duration::from_secs(30))
                .await
                .is_err()
        );
        assert_eq!(probes(&extra), 2, "the failure was kept for later launches");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_page_that_couldnt_be_attached_to_isnt_waited_for() {
        use std::io::{BufRead, Write};
        let (replies, mut chrome) = std::io::pipe().unwrap();
        let (heard, commands) = std::io::pipe().unwrap();
        let inner = Arc::new(Inner {
            conn: Connection::new(replies, commands, None),
            child: Mutex::new(None),
            lifeline: Mutex::new(None),
            profile: None,
            keep_profile: false,
            temp_profile: Mutex::new(None),
            contexts: Mutex::default(),
            tracing_done: Arc::default(),
            version: String::new(),
            identity: None,
            timeout: Duration::from_secs(5),
            pause: LaunchOptions::default().dialog_pause,
            proxy_login: None,
            said: None,
            pages: Arc::default(),
            attaching: Arc::default(),
            downloads: Arc::default(),
            environment: Environment::default(),
            arranges: false,
            screen: crate::window::DEFAULT_SCREEN,
            screen_scale: 1.0,
            worker_header_order: false,
            window_size: None,
            agent: None,
            own_languages: Mutex::default(),
            screens: tokio::sync::Mutex::new(()),
            own: tokio::sync::OnceCell::new(),
            stealth: true,
            default_context: None,
            first_window: None,
        });
        // A target that closes between being made and being attached to.
        std::thread::spawn(move || {
            let mut heard = std::io::BufReader::new(heard);
            let mut message = Vec::new();
            while heard.read_until(0, &mut message).unwrap_or(0) > 0 {
                message.pop();
                let command: Value = serde_json::from_slice(&message).unwrap();
                message.clear();
                let reply = match command["method"].as_str() {
                    Some("Target.createTarget") => {
                        json!({"id": command["id"], "result": {"targetId": "T"}})
                    }
                    _ => {
                        json!({"id": command["id"], "error": {"code": -32000, "message": "No target with given id found"}})
                    }
                };
                let mut reply = serde_json::to_vec(&reply).unwrap();
                reply.push(0);
                chrome.write_all(&reply).unwrap();
            }
        });
        assert!(
            Page::open(
                inner.clone(),
                None,
                false,
                None,
                None,
                Environment::default()
            )
            .await
            .is_err()
        );
        assert!(inner.attaching.lock().unwrap().is_empty());
    }
}
