//! Bindings for netweir-browser. Every method that talks to the browser
//! returns an awaitable.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use netweir_browser::{
    Browser as CoreBrowser, Context as CoreContext, Continue, Dialog as CoreDialog,
    Download as CoreDownload, Error, Frame as CoreFrame, LaunchOptions, NetFailure,
    Page as CorePage, PageEvent, Pick, Request as CoreRequest, Response as CoreResponse,
    Route as CoreRoute, WaitUntil,
};
use netweir_dom::Document;
use pyo3::exceptions::{
    PyFileNotFoundError, PyIsADirectoryError, PyRuntimeError, PyTypeError, PyValueError,
};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList};

use crate::Node;

/// Raises the Python exception that matches `e`.
pub(crate) fn raise(e: Error) -> PyErr {
    // Called on Rust threads too, so counted for the atexit hook (aio.rs).
    crate::aio::attach(|py| {
        let made = (|| -> PyResult<PyErr> {
            let errors = py.import("netweir._errors")?;
            let value = match &e {
                Error::Invalid(m) => return Ok(PyValueError::new_err(m.clone())),
                Error::Timeout(_) => errors.getattr("BrowserTimeout")?.call1((e.to_string(),))?,
                Error::ScrollTimeout { rounds, .. } => {
                    let error = errors.getattr("BrowserTimeout")?.call1((e.to_string(),))?;
                    error.setattr("rounds", rounds)?;
                    error
                }
                Error::Navigation { .. } => {
                    // The kind comes from Chrome's net error alone; the
                    // message has all of it, what Chrome said included.
                    let kind = match e.net_failure() {
                        Some(NetFailure::Timeout) => "timeout",
                        Some(NetFailure::Tls) => "tls",
                        Some(NetFailure::Connect) => "connect",
                        _ => "other",
                    };
                    errors.getattr("FetchError")?.call1((e.to_string(), kind))?
                }
                _ => errors.getattr("BrowserError")?.call1((e.to_string(),))?,
            };
            Ok(PyErr::from_value(value))
        })();
        made.unwrap_or_else(|err| err)
    })
}

fn seconds(name: &str, value: Option<f64>) -> PyResult<Option<Duration>> {
    match value {
        None => Ok(None),
        Some(t) if t > 0.0 => Duration::try_from_secs_f64(t)
            .map(Some)
            .map_err(|_| PyValueError::new_err(format!("{name} is too long: {t} seconds"))),
        Some(_) => Err(PyValueError::new_err(format!(
            "{name} must be a positive number of seconds"
        ))),
    }
}

/// Starts Chrome, or Firefox (`engine="firefox"`). Resolves to a Browser.
#[pyfunction]
#[pyo3(signature = (executable=None, headless=true, args=Vec::new(), timeout=30.0, proxy=None, connect=None, user_data_dir=None, dialog_pause=(0.5, 1.5), environment=None, worker_header_order=false, engine="chrome", stealth=true))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn launch<'py>(
    py: Python<'py>,
    executable: Option<PathBuf>,
    headless: bool,
    args: Vec<String>,
    timeout: f64,
    proxy: Option<String>,
    connect: Option<String>,
    user_data_dir: Option<PathBuf>,
    dialog_pause: (f64, f64),
    environment: Option<Bound<'py, PyDict>>,
    worker_header_order: bool,
    engine: &str,
    stealth: bool,
) -> PyResult<Bound<'py, PyAny>> {
    let engine = match engine {
        "chrome" => netweir_browser::Engine::Chrome,
        "firefox" => netweir_browser::Engine::Firefox,
        other => {
            return Err(PyValueError::new_err(format!(
                "engine must be \"chrome\" or \"firefox\", not {other:?}"
            )));
        }
    };
    let environment = crate::environment::environment_from(environment.as_ref())?;
    let (least, most) = dialog_pause;
    let dialog_pause = match (
        Duration::try_from_secs_f64(least),
        Duration::try_from_secs_f64(most),
    ) {
        (Ok(least), Ok(most)) if least <= most => (least, most),
        _ => {
            return Err(PyValueError::new_err(format!(
                "dialog_pause must be (least, most) seconds, 0 or more, least first, not \
                 {dialog_pause:?}"
            )));
        }
    };
    if connect.is_some()
        && (executable.is_some()
            || proxy.is_some()
            || !args.is_empty()
            || !headless
            || user_data_dir.is_some())
    {
        return Err(PyValueError::new_err(
            "connect= drives a browser that's already running, so executable, args, proxy, \
             headless and user_data_dir don't apply",
        ));
    }
    if args.iter().any(|a| a.starts_with("--user-data-dir")) && user_data_dir.is_some() {
        return Err(PyValueError::new_err(
            "give the profile as user_data_dir, not also as --user-data-dir in args",
        ));
    }
    let timeout = seconds("timeout", Some(timeout))?.expect("checked above");
    let options = LaunchOptions {
        engine,
        stealth,
        executable,
        headless,
        args,
        timeout,
        proxy,
        // Chrome for Testing presents itself as Google Chrome would.
        // Firefox sends no client hints, so has no brands.
        brands: (engine == netweir_browser::Engine::Chrome)
            .then(|| Arc::new(netweir_core::Profile::chrome_brands) as _),
        user_data_dir,
        dialog_pause,
        environment,
        worker_header_order,
    };
    crate::aio::future_into_py(py, async move {
        let inner = match connect {
            Some(url) => CoreBrowser::connect(&url, options).await,
            None => CoreBrowser::launch(options).await,
        }
        .map_err(raise)?;
        Ok(Browser { inner })
    })
}

/// A running Chrome or Firefox. Closing it closes every page.
#[pyclass(frozen, module = "netweir")]
pub struct Browser {
    inner: CoreBrowser,
}

#[pymethods]
impl Browser {
    /// The browser's version, such as "Chrome/154.0.8037.98" or
    /// "Firefox/159.0.1".
    #[getter]
    fn version(&self) -> String {
        self.inner.version().to_string()
    }

    /// Which browser this is: "chrome" or "firefox".
    #[getter]
    fn engine(&self) -> &'static str {
        match self.inner.engine_info().engine {
            netweir_browser::Engine::Chrome => "chrome",
            netweir_browser::Engine::Firefox => "firefox",
        }
    }

    /// Whether pages can't tell the browser is driven: false only for a
    /// Firefox launched with `stealth=False`, in automation mode.
    #[getter]
    fn stealth(&self) -> bool {
        self.inner.engine_info().stealth
    }

    #[getter]
    fn closed(&self) -> bool {
        self.inner.is_closed()
    }

    /// A page in a context of its own: it shares no cookies or storage.
    /// Keyword arguments give its environment, as `new_context` takes them.
    #[pyo3(signature = (**environment))]
    fn new_page<'py>(
        &self,
        py: Python<'py>,
        environment: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let env = crate::environment::environment_from(environment)?;
        let browser = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            let inner = browser.new_page_with(env).await.map_err(raise)?;
            Ok(BrowserPage::new(inner, None))
        })
    }

    /// A context whose pages share cookies and storage. `storage_state` (a
    /// path to a file `storage_state()` or Playwright saved, or its dict)
    /// starts it with those cookies and localStorage. Keyword arguments
    /// give its pages' environment: `device`, `viewport`, `screen`,
    /// `device_scale_factor`, `is_mobile`, `has_touch`, `locale`,
    /// `timezone_id`, `geolocation`, `permissions`, `color_scheme`,
    /// `reduced_motion`, `extra_http_headers`, `offline`, `user_agent` and
    /// `init_scripts`.
    #[pyo3(signature = (storage_state=None, **environment))]
    fn new_context<'py>(
        &self,
        py: Python<'py>,
        storage_state: Option<Bound<'py, PyAny>>,
        environment: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let env = crate::environment::environment_from(environment)?;
        let state = storage_state
            .filter(|s| !s.is_none())
            .map(|s| crate::cookies::storage_state_from(&s))
            .transpose()?;
        let browser = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            let inner = browser.new_context_with(env).await.map_err(raise)?;
            if let Some(state) = &state
                && let Err(e) = inner.set_storage_state(state).await
            {
                let _ = inner.close().await;
                return Err(raise(e));
            }
            Ok(BrowserContext {
                inner,
                network: OnceLock::new(),
            })
        })
    }

    /// Every DevTools method sent so far.
    fn methods_sent(&self) -> Vec<String> {
        self.inner.methods_sent()
    }

    /// Starts recording what the whole browser does, for `stop_tracing`.
    #[pyo3(signature = (*, categories=None, screenshots=false))]
    fn start_tracing<'py>(
        &self,
        py: Python<'py>,
        categories: Option<Vec<String>>,
        screenshots: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let browser = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            browser
                .start_tracing(categories.as_deref(), screenshots)
                .await
                .map_err(raise)
        })
    }

    /// Stops recording: the trace's JSON as bytes, also written to `path`
    /// if given.
    #[pyo3(signature = (path=None))]
    fn stop_tracing<'py>(
        &self,
        py: Python<'py>,
        path: Option<PathBuf>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let browser = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            let trace = browser.stop_tracing().await.map_err(raise)?;
            if let Some(path) = path {
                std::fs::write(&path, &trace)?;
            }
            crate::aio::attach(|py| Ok(PyBytes::new(py, &trace).unbind()))
        })
    }

    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let browser = self.inner.clone();
        crate::aio::future_into_py(py, async move { browser.close().await.map_err(raise) })
    }

    fn __aenter__<'py>(slf: Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        let me = slf.clone().unbind();
        crate::aio::future_into_py(slf.py(), async move { Ok(me) })
    }

    fn __aexit__<'py>(
        &self,
        py: Python<'py>,
        _kind: Bound<'py, PyAny>,
        _value: Bound<'py, PyAny>,
        _traceback: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.close(py)
    }

    fn __repr__(&self) -> String {
        format!("<Browser {}>", self.inner.version())
    }
}

/// Pages that share cookies and storage.
#[pyclass(frozen, module = "netweir")]
pub struct BrowserContext {
    inner: CoreContext,
    /// The context's routes, kept in Python (netweir._network).
    network: OnceLock<Py<PyAny>>,
}

#[pymethods]
impl BrowserContext {
    /// A page of the context. Keyword arguments change its environment
    /// from the context's, as `Browser.new_context` takes them, except
    /// `timezone_id` and `locale`, which are the context's.
    #[pyo3(signature = (**environment))]
    fn new_page<'py>(
        slf: Bound<'py, Self>,
        environment: Option<&Bound<'py, PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let env = crate::environment::environment_from(environment)?;
        let context = slf.get().inner.clone();
        let me = slf.clone().unbind();
        crate::aio::future_into_py(slf.py(), async move {
            match context.new_page_with(env).await {
                Ok(inner) => Ok(BrowserPage::new(inner, Some(me))),
                Err(e) => {
                    crate::aio::release(me);
                    Err(raise(e))
                }
            }
        })
    }

    /// Routes every page of the context: each request whose URL matches
    /// `pattern` (a glob, a compiled regex, or a callable taking the URL)
    /// goes to `handler`, called with a Route to decide about.
    fn route<'py>(
        slf: &Bound<'py, Self>,
        pattern: Bound<'py, PyAny>,
        handler: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        network(slf.py(), &slf.get().network, None)?.call_method1("route", (slf, pattern, handler))
    }

    /// Answers requests from the HAR file at `path`, with nothing sent to
    /// the network. A request it doesn't hold fails, or with
    /// `not_found="fallback"` goes on as usual.
    #[pyo3(signature = (path, *, url=None, not_found="abort"))]
    fn route_from_har<'py>(
        slf: &Bound<'py, Self>,
        path: PathBuf,
        url: Option<Bound<'py, PyAny>>,
        not_found: &str,
    ) -> PyResult<Bound<'py, PyAny>> {
        let py = slf.py();
        let url = match url {
            Some(url) => url,
            None => "**/*".into_pyobject(py)?.into_any(),
        };
        network(py, &slf.get().network, None)?
            .call_method1("route_from_har", (slf, path, url, not_found))
    }

    /// Removes the routes with `pattern` (only the one with `handler`, if
    /// given).
    #[pyo3(signature = (pattern, handler=None))]
    fn unroute<'py>(
        slf: &Bound<'py, Self>,
        pattern: Bound<'py, PyAny>,
        handler: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        network(slf.py(), &slf.get().network, None)?
            .call_method1("unroute", (slf, pattern, handler))
    }

    fn _route<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let context = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            let rx = context.route().await.map_err(raise)?;
            Ok(Routes::new(rx))
        })
    }

    fn _unroute<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let context = self.inner.clone();
        crate::aio::future_into_py(py, async move { context.unroute().await.map_err(raise) })
    }

    /// Stops handing requests to the context's routes, at once.
    fn _drop_routes(&self) {
        self.inner.drop_routes();
    }

    /// The context's routes, for its pages to fall back on.
    fn _network<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        network(py, &self.network, None)
    }

    fn cookies<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let context = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            let cookies = context.cookies().await.map_err(raise)?;
            crate::aio::attach(|py| crate::cookies::cookie_list(py, &cookies))
        })
    }

    fn set_cookies<'py>(
        &self,
        py: Python<'py>,
        cookies: Vec<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let cookies = cookies
            .iter()
            .map(|c| crate::cookies::cookie_from(c, false))
            .collect::<PyResult<Vec<_>>>()?;
        let context = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            context.set_cookies(&cookies).await.map_err(raise)
        })
    }

    /// The context's cookies and the localStorage of every origin its
    /// pages loaded, as a dict in Playwright's storage state form; with
    /// `path`, also written there, readable by its owner alone.
    #[pyo3(signature = (path=None))]
    fn storage_state<'py>(
        &self,
        py: Python<'py>,
        path: Option<PathBuf>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let context = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            let state = context.storage_state().await.map_err(raise)?;
            crate::aio::attach(|py| crate::cookies::storage_state_out(py, &state, path))
        })
    }

    /// Grants permissions (such as "geolocation") to every page of the
    /// context, for `origin` or for every origin.
    #[pyo3(signature = (permissions, origin=None))]
    fn grant_permissions<'py>(
        &self,
        py: Python<'py>,
        permissions: Bound<'py, PyAny>,
        origin: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let permissions = permission_names(&permissions)?;
        let context = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            context
                .grant_permissions(&permissions, origin.as_deref())
                .await
                .map_err(raise)
        })
    }

    /// Takes back every permission granted in the context.
    fn clear_permissions<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let context = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            context.clear_permissions().await.map_err(raise)
        })
    }

    /// Where every page of the context is, as {"latitude", "longitude",
    /// "accuracy"}; None for Chrome's own idea, which is nowhere.
    fn set_geolocation<'py>(
        &self,
        py: Python<'py>,
        geolocation: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let at = geolocation
            .filter(|g| !g.is_none())
            .map(|g| crate::environment::geolocation_from(&g))
            .transpose()?;
        let context = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            context.set_geolocation(at).await.map_err(raise)
        })
    }

    /// Takes every page of the context off the network, or back on.
    fn set_offline<'py>(&self, py: Python<'py>, offline: bool) -> PyResult<Bound<'py, PyAny>> {
        let context = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            context.set_offline(offline).await.map_err(raise)
        })
    }

    /// Headers every page of the context sends with every request, after
    /// Chrome's own.
    fn set_extra_http_headers<'py>(
        &self,
        py: Python<'py>,
        headers: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let headers = crate::environment::headers_from(&headers)?;
        let context = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            context.set_extra_headers(&headers).await.map_err(raise)
        })
    }

    /// Runs a script (or the file at `path`) in every document of every
    /// page of the context, before the document's own scripts.
    #[pyo3(signature = (script=None, *, path=None))]
    fn add_init_script<'py>(
        &self,
        py: Python<'py>,
        script: Option<String>,
        path: Option<PathBuf>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let script = init_script(script, path)?;
        let context = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            context.add_init_script(&script).await.map_err(raise)
        })
    }

    /// Closes the context and every page in it.
    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let context = self.inner.clone();
        crate::aio::future_into_py(py, async move { context.close().await.map_err(raise) })
    }

    fn __aenter__<'py>(slf: Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        let me = slf.clone().unbind();
        crate::aio::future_into_py(slf.py(), async move { Ok(me) })
    }

    fn __aexit__<'py>(
        &self,
        py: Python<'py>,
        _kind: Bound<'py, PyAny>,
        _value: Bound<'py, PyAny>,
        _traceback: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.close(py)
    }
}

/// A page in Chrome.
#[pyclass(frozen, module = "netweir")]
pub struct BrowserPage {
    inner: CorePage,
    /// For a page a crawl fetched: closing it frees its place in the
    /// crawl's pool.
    live: Option<netweir_core::LivePage>,
    /// The shared context the page was opened in, whose routes it falls
    /// back on.
    context: Option<Py<BrowserContext>>,
    /// For a popup, the page that opened it, whose routes it falls back
    /// on (and, through it, its context's).
    opener: Option<Py<BrowserPage>>,
    /// The page's listeners, waiters and routes, kept in Python
    /// (netweir._network).
    network: OnceLock<Py<PyAny>>,
}

impl BrowserPage {
    fn new(inner: CorePage, context: Option<Py<BrowserContext>>) -> BrowserPage {
        BrowserPage {
            inner,
            live: None,
            context,
            opener: None,
            network: OnceLock::new(),
        }
    }

    pub(crate) fn from_crawl(live: netweir_core::LivePage) -> BrowserPage {
        BrowserPage {
            inner: live.page().clone(),
            live: Some(live),
            context: None,
            opener: None,
            network: OnceLock::new(),
        }
    }

    /// `go_back` (`by` -1) or `go_forward` (1).
    fn go_by<'py>(
        &self,
        py: Python<'py>,
        by: i8,
        wait: &str,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let wait: WaitUntil = wait.parse().map_err(raise)?;
        let timeout = seconds("timeout", timeout)?;
        let page = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            let went = if by < 0 {
                page.go_back(wait, timeout).await
            } else {
                page.go_forward(wait, timeout).await
            };
            Ok(went
                .map_err(raise)?
                .map(|inner| BrowserResponse::new(inner, &page)))
        })
    }

    fn network<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let parent = match (&self.opener, &self.context) {
            (Some(opener), _) => Some(opener.get().network(py)?),
            (None, Some(c)) => Some(network(py, &c.get().network, None)?),
            (None, None) => None,
        };
        network(py, &self.network, parent)
    }
}

/// The netweir._network.Network kept in `slot`, made the first time.
fn network<'py>(
    py: Python<'py>,
    slot: &OnceLock<Py<PyAny>>,
    parent: Option<Bound<'py, PyAny>>,
) -> PyResult<Bound<'py, PyAny>> {
    if let Some(n) = slot.get() {
        return Ok(n.bind(py).clone());
    }
    let made = py
        .import("netweir._network")?
        .getattr("Network")?
        .call1((parent,))?;
    // Another thread may have got there first; its one is kept.
    Ok(slot.get_or_init(|| made.unbind()).bind(py).clone())
}

/// What the caller passed as headers: a dict, or (name, value) pairs.
fn header_pairs(headers: &Bound<'_, PyAny>) -> PyResult<Vec<(String, String)>> {
    let items = match headers.cast::<PyDict>() {
        Ok(d) => d.items().into_any(),
        Err(_) => headers.clone(),
    };
    items
        .try_iter()?
        .map(|pair| pair?.extract::<(String, String)>())
        .collect::<PyResult<_>>()
        .map_err(|_| {
            PyTypeError::new_err("headers must be a dict, or (name, value) pairs, of strings")
        })
}

/// What the caller passed as a body: bytes, or a str sent as UTF-8.
fn body_bytes(body: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
    if let Ok(s) = body.extract::<String>() {
        return Ok(s.into_bytes());
    }
    body.extract::<Vec<u8>>()
        .map_err(|_| PyTypeError::new_err("body must be bytes or a str"))
}

fn delegate<'py>(
    slf: &Bound<'py, BrowserPage>,
    method: &str,
    args: impl pyo3::call::PyCallArgs<'py>,
) -> PyResult<Bound<'py, PyAny>> {
    slf.get().network(slf.py())?.call_method1(method, args)
}

#[pymethods]
impl BrowserPage {
    /// The current URL.
    #[getter]
    fn url(&self) -> String {
        self.inner.url()
    }

    /// The viewport as {"width", "height"}, if netweir sized the window.
    #[getter]
    fn viewport_size<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyDict>>> {
        self.inner
            .viewport()
            .map(|s| crate::environment::size_to(py, s))
            .transpose()
    }

    /// Resizes the window so the viewport is {"width", "height"}.
    fn set_viewport_size<'py>(
        &self,
        py: Python<'py>,
        viewport: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let size = crate::environment::size_from("viewport", &viewport)?;
        let page = self.inner.clone();
        crate::aio::future_into_py(
            py,
            async move { page.set_viewport(size).await.map_err(raise) },
        )
    }

    /// Headers the page, its frames and its workers send with every
    /// request, after Chrome's own, in place of those it had.
    fn set_extra_http_headers<'py>(
        &self,
        py: Python<'py>,
        headers: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let headers = crate::environment::headers_from(&headers)?;
        let page = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            page.set_extra_headers(&headers).await.map_err(raise)
        })
    }

    /// The CSS media type ("screen", "print") and what prefers-color-scheme
    /// ("light", "dark", "no-preference"), prefers-reduced-motion ("reduce",
    /// "no-preference"), forced-colors ("active", "none") and
    /// prefers-contrast ("more", "no-preference") match, as Playwright's
    /// emulate_media takes them: one left out stays as it is, and "null"
    /// takes its emulation away.
    #[pyo3(signature = (*, media=None, color_scheme=None, reduced_motion=None, forced_colors=None, contrast=None))]
    fn emulate_media<'py>(
        &self,
        py: Python<'py>,
        media: Option<String>,
        color_scheme: Option<String>,
        reduced_motion: Option<String>,
        forced_colors: Option<String>,
        contrast: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        use crate::environment::media_setting;
        let change = netweir_browser::MediaChange {
            media: media_setting(media)?,
            color_scheme: media_setting(color_scheme)?,
            reduced_motion: media_setting(reduced_motion)?,
            forced_colors: media_setting(forced_colors)?,
            contrast: media_setting(contrast)?,
        };
        let page = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            page.emulate_media(&change).await.map_err(raise)
        })
    }

    /// Runs a script (or the file at `path`) in every document the page
    /// and its frames load from now on, before the document's own.
    #[pyo3(signature = (script=None, *, path=None))]
    fn add_init_script<'py>(
        &self,
        py: Python<'py>,
        script: Option<String>,
        path: Option<PathBuf>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let script = init_script(script, path)?;
        let page = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            page.add_init_script(&script).await.map_err(raise)
        })
    }

    /// Where the page is, as {"latitude", "longitude", "accuracy"}; None
    /// for Chrome's own idea, which is nowhere. The page's origin needs
    /// the "geolocation" permission too.
    fn set_geolocation<'py>(
        &self,
        py: Python<'py>,
        geolocation: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let at = geolocation
            .filter(|g| !g.is_none())
            .map(|g| crate::environment::geolocation_from(&g))
            .transpose()?;
        let page = self.inner.clone();
        crate::aio::future_into_py(
            py,
            async move { page.set_geolocation(at).await.map_err(raise) },
        )
    }

    /// Takes the page off the network, or back on.
    fn set_offline<'py>(&self, py: Python<'py>, offline: bool) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        crate::aio::future_into_py(
            py,
            async move { page.set_offline(offline).await.map_err(raise) },
        )
    }

    /// Grants permissions in the page's context, for `origin` or for
    /// every origin.
    #[pyo3(signature = (permissions, origin=None))]
    fn grant_permissions<'py>(
        &self,
        py: Python<'py>,
        permissions: Bound<'py, PyAny>,
        origin: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let permissions = permission_names(&permissions)?;
        let page = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            page.grant_permissions(&permissions, origin.as_deref())
                .await
                .map_err(raise)
        })
    }

    /// Takes back every permission granted in the page's context.
    fn clear_permissions<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        crate::aio::future_into_py(
            py,
            async move { page.clear_permissions().await.map_err(raise) },
        )
    }

    #[getter]
    fn closed(&self) -> bool {
        self.inner.is_closed()
    }

    /// Navigates and waits for "domcontentloaded", "load" or
    /// "networkidle". Resolves to a BrowserResponse.
    #[pyo3(signature = (url, wait="load", timeout=None))]
    fn goto<'py>(
        &self,
        py: Python<'py>,
        url: String,
        wait: &str,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let wait: WaitUntil = wait.parse().map_err(raise)?;
        let timeout = seconds("timeout", timeout)?;
        let page = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            let inner = page.goto(&url, wait, timeout).await.map_err(raise)?;
            Ok(BrowserResponse::new(inner, &page))
        })
    }

    /// Waits for a navigation away from the document showing now to reach
    /// `wait`. Resolves to the BrowserResponse of the new document.
    #[pyo3(signature = (wait="load", timeout=None))]
    fn wait_for_navigation<'py>(
        &self,
        py: Python<'py>,
        wait: &str,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let wait: WaitUntil = wait.parse().map_err(raise)?;
        let timeout = seconds("timeout", timeout)?;
        let page = self.inner.clone();
        // The document showing now, read before anything is awaited, so a
        // navigation that starts while the future waits to run still
        // counts.
        let after = page.response();
        crate::aio::future_into_py(py, async move {
            let inner = page
                .wait_for_navigation(&after, wait, timeout)
                .await
                .map_err(raise)?;
            Ok(BrowserResponse::new(inner, &page))
        })
    }

    /// Waits for the document showing now, or one that replaced it, to
    /// reach `wait`: for a popup just opened, the one it was opened for.
    /// Resolves to the BrowserResponse of the document showing then.
    #[pyo3(signature = (wait="load", timeout=None))]
    fn wait_for_load<'py>(
        &self,
        py: Python<'py>,
        wait: &str,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let wait: WaitUntil = wait.parse().map_err(raise)?;
        let timeout = seconds("timeout", timeout)?;
        let page = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            let inner = page.wait_for_load(wait, timeout).await.map_err(raise)?;
            Ok(BrowserResponse::new(inner, &page))
        })
    }

    /// Goes back one entry in the history and waits for `wait`. Resolves
    /// to the BrowserResponse of the document showing then, or None when
    /// there's nothing to go back to.
    #[pyo3(signature = (wait="load", timeout=None))]
    fn go_back<'py>(
        &self,
        py: Python<'py>,
        wait: &str,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.go_by(py, -1, wait, timeout)
    }

    /// Goes forward one entry in the history, as `go_back` goes back.
    #[pyo3(signature = (wait="load", timeout=None))]
    fn go_forward<'py>(
        &self,
        py: Python<'py>,
        wait: &str,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.go_by(py, 1, wait, timeout)
    }

    /// Loads the document again and waits for `wait`. Resolves to its
    /// BrowserResponse.
    #[pyo3(signature = (wait="load", timeout=None))]
    fn reload<'py>(
        &self,
        py: Python<'py>,
        wait: &str,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let wait: WaitUntil = wait.parse().map_err(raise)?;
        let timeout = seconds("timeout", timeout)?;
        let page = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            let inner = page.reload(wait, timeout).await.map_err(raise)?;
            Ok(BrowserResponse::new(inner, &page))
        })
    }

    /// The HTML as Chrome now has it.
    fn content<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        crate::aio::future_into_py(py, async move { page.content().await.map_err(raise) })
    }

    /// The page as it is now, parsed into a netweir Node.
    fn parse<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            let html = page.content().await.map_err(raise)?;
            let doc = tokio::task::spawn_blocking(move || Document::parse(&html))
                .await
                .map_err(|e| PyValueError::new_err(e.to_string()))?;
            Ok(Node::root_of(Arc::new(doc)))
        })
    }

    fn title<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        crate::aio::future_into_py(py, async move { page.title().await.map_err(raise) })
    }

    /// Runs `js` in the page: an expression, or a function, which is
    /// called. Resolves to the result as Python values.
    fn evaluate<'py>(&self, py: Python<'py>, js: String) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            let value = page.evaluate(&js).await.map_err(raise)?;
            crate::aio::attach(|py| to_python(py, &value))
        })
    }

    #[pyo3(signature = (selector, timeout=None))]
    fn click<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let timeout = seconds("timeout", timeout)?;
        let page = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            page.click(&selector, timeout).await.map_err(raise)
        })
    }

    /// Double-clicks the element (taps it twice on a phone).
    #[pyo3(signature = (selector, timeout=None))]
    fn dblclick<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let timeout = seconds("timeout", timeout)?;
        let page = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            page.dblclick(&selector, timeout).await.map_err(raise)
        })
    }

    #[pyo3(signature = (selector, text, timeout=None))]
    fn fill<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        text: String,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let timeout = seconds("timeout", timeout)?;
        let page = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            page.fill(&selector, &text, timeout).await.map_err(raise)
        })
    }

    /// Types `text` a key at a time; see `Frame::type_text`.
    #[pyo3(signature = (selector, text, delay=None, timeout=None))]
    fn r#type<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        text: String,
        delay: Option<Bound<'py, PyAny>>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let timeout = seconds("timeout", timeout)?;
        let delay = delay_of(delay)?;
        let page = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            page.type_text(&selector, &text, delay, timeout)
                .await
                .map_err(raise)
        })
    }

    /// Presses a key in whatever has focus: a character, or a name such
    /// as "Enter", "Tab" or "ArrowDown", after any of the modifiers
    /// "Shift", "Control", "Alt", "Meta" and "ControlOrMeta" (Meta on a
    /// Mac), joined by "+": "ControlOrMeta+A", "Shift+Tab".
    fn press<'py>(&self, py: Python<'py>, key: String) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        crate::aio::future_into_py(py, async move { page.press(&key).await.map_err(raise) })
    }

    #[pyo3(signature = (selector, state="visible".to_string(), timeout=None))]
    fn wait_for<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        state: String,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let timeout = seconds("timeout", timeout)?;
        let page = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            page.wait_for(&selector, &state, timeout)
                .await
                .map_err(raise)
        })
    }

    #[pyo3(signature = (selector, timeout=None))]
    fn hover<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        act::hover(py, self.inner.main_frame(), selector, timeout)
    }

    /// Drags the element `source` onto `target` with the mouse: anything
    /// that follows the mouse, or an HTML5 drag and drop.
    #[pyo3(signature = (source, target, timeout=None))]
    fn drag<'py>(
        &self,
        py: Python<'py>,
        source: String,
        target: String,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        act::drag(py, self.inner.main_frame(), source, target, timeout)
    }

    /// Scrolls with the mouse wheel. Resolves to how many times more of
    /// the page loaded, when scrolling to the bottom; otherwise 0.
    #[pyo3(signature = (selector=None, *, x=0.0, y=None, times=None, idle=2.0, timeout=None))]
    #[allow(clippy::too_many_arguments)]
    fn scroll<'py>(
        &self,
        py: Python<'py>,
        selector: Option<String>,
        x: f64,
        y: Option<f64>,
        times: Option<u32>,
        idle: f64,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let how = act::Scroll {
            selector,
            x,
            y,
            times,
            idle,
            timeout,
        };
        act::scroll(py, self.inner.main_frame(), how)
    }

    /// Chooses options of a <select>. Resolves to the values selected.
    #[pyo3(signature = (selector, value=None, *, label=None, index=None, timeout=None))]
    fn select_option<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        value: Option<Bound<'py, PyAny>>,
        label: Option<Bound<'py, PyAny>>,
        index: Option<Bound<'py, PyAny>>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let pick = act::pick(value, label, index)?;
        act::select_option(py, self.inner.main_frame(), selector, pick, timeout)
    }

    #[pyo3(signature = (selector, timeout=None))]
    fn check<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        act::set_checked(py, self.inner.main_frame(), selector, true, timeout)
    }

    #[pyo3(signature = (selector, timeout=None))]
    fn uncheck<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        act::set_checked(py, self.inner.main_frame(), selector, false, timeout)
    }

    #[pyo3(signature = (selector, files, timeout=None))]
    fn set_input_files<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        files: Bound<'py, PyAny>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        act::set_input_files(py, self.inner.main_frame(), selector, &files, timeout)
    }

    /// Every frame of the page, its own first. Resolves to BrowserFrames.
    fn frames<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            let frames = page.frames().await.map_err(raise)?;
            Ok(frames
                .into_iter()
                .map(|inner| BrowserFrame { inner })
                .collect::<Vec<_>>())
        })
    }

    /// The first frame with this `name`, and whose URL matches `url` (a
    /// glob or a compiled regex); None if there's none.
    #[pyo3(signature = (name=None, url=None))]
    fn frame<'py>(
        &self,
        py: Python<'py>,
        name: Option<String>,
        url: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let url = url.map(|u| u.unbind());
        let page = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            let frames = page.frames().await;
            // `url` is let go of in here, attached.
            crate::aio::attach(move |py| -> PyResult<Option<BrowserFrame>> {
                for inner in frames.map_err(raise)? {
                    if name.as_deref().is_some_and(|n| n != inner.name()) {
                        continue;
                    }
                    if let Some(url) = &url {
                        let url = url.bind(py);
                        let matched = match url.extract::<String>() {
                            Ok(glob) => netweir_browser::glob_match(&glob, inner.url()),
                            Err(_) => url.call_method1("search", (inner.url(),))?.is_truthy()?,
                        };
                        if !matched {
                            continue;
                        }
                    }
                    return Ok(Some(BrowserFrame { inner }));
                }
                Ok(None)
            })
        })
    }

    /// PNG bytes, or with a `quality` (1 to 100) JPEG ones, also written
    /// to `path` if given.
    #[pyo3(signature = (path=None, full_page=false, quality=None))]
    fn screenshot<'py>(
        &self,
        py: Python<'py>,
        path: Option<PathBuf>,
        full_page: bool,
        quality: Option<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        if quality.is_some_and(|q| !(1..=100).contains(&q)) {
            return Err(PyValueError::new_err("quality is from 1 to 100"));
        }
        let page = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            let png = page.capture(full_page, quality).await.map_err(raise)?;
            if let Some(path) = path {
                std::fs::write(&path, &png)?;
            }
            crate::aio::attach(|py| Ok(PyBytes::new(py, &png).unbind()))
        })
    }

    /// The page as PDF bytes, also written to `path` if given. Lengths are
    /// in inches.
    #[pyo3(signature = (path=None, *, landscape=false, print_background=false, scale=None, format=None, width=None, height=None, margin=None, page_ranges=None))]
    #[allow(clippy::too_many_arguments)]
    fn pdf<'py>(
        &self,
        py: Python<'py>,
        path: Option<PathBuf>,
        landscape: bool,
        print_background: bool,
        scale: Option<f64>,
        format: Option<&str>,
        width: Option<f64>,
        height: Option<f64>,
        margin: Option<f64>,
        page_ranges: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let paper = match (format, width, height) {
            (None, None, None) => None,
            (None, Some(width), Some(height)) => Some((width, height)),
            (Some(format), None, None) => Some(paper_size(format)?),
            _ => {
                return Err(PyValueError::new_err(
                    "a page's size is format, or width and height together",
                ));
            }
        };
        let options = netweir_browser::PdfOptions {
            landscape,
            print_background,
            scale,
            paper,
            margin,
            page_ranges,
        };
        let page = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            let pdf = page.pdf(&options).await.map_err(raise)?;
            if let Some(path) = path {
                std::fs::write(&path, &pdf)?;
            }
            crate::aio::attach(|py| Ok(PyBytes::new(py, &pdf).unbind()))
        })
    }

    /// The cookies of the page's context, as dicts.
    fn cookies<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            let cookies = page.cookies().await.map_err(raise)?;
            crate::aio::attach(|py| crate::cookies::cookie_list(py, &cookies))
        })
    }

    fn set_cookies<'py>(
        &self,
        py: Python<'py>,
        cookies: Vec<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let cookies = cookies
            .iter()
            .map(|c| crate::cookies::cookie_from(c, false))
            .collect::<PyResult<Vec<_>>>()?;
        let page = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            page.set_cookies(&cookies).await.map_err(raise)
        })
    }

    /// The page's context's cookies and the localStorage of the origins
    /// its documents came from, as a dict in Playwright's storage state
    /// form; with `path`, also written there, readable by its owner alone.
    #[pyo3(signature = (path=None))]
    fn storage_state<'py>(
        &self,
        py: Python<'py>,
        path: Option<PathBuf>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            let state = page.storage_state().await.map_err(raise)?;
            crate::aio::attach(|py| crate::cookies::storage_state_out(py, &state, path))
        })
    }

    /// Calls `callback` (a function or a coroutine function) with each
    /// response the page gets.
    fn on_response<'py>(
        slf: &Bound<'py, Self>,
        callback: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        delegate(slf, "on", (slf, "response", callback))
    }

    fn remove_response_listener<'py>(
        slf: &Bound<'py, Self>,
        callback: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        delegate(slf, "off", ("response", callback))
    }

    /// Calls `callback` with each request the page makes.
    fn on_request<'py>(
        slf: &Bound<'py, Self>,
        callback: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        delegate(slf, "on", (slf, "request", callback))
    }

    fn remove_request_listener<'py>(
        slf: &Bound<'py, Self>,
        callback: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        delegate(slf, "off", ("request", callback))
    }

    /// Waits for a response whose URL matches `pattern` (a glob or a
    /// compiled regex), or for which `pattern` (a callable) is true.
    #[pyo3(signature = (pattern, timeout=None))]
    fn wait_for_response<'py>(
        slf: &Bound<'py, Self>,
        pattern: Bound<'py, PyAny>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        seconds("timeout", timeout)?;
        delegate(slf, "wait", (slf, "response", pattern, timeout))
    }

    #[pyo3(signature = (pattern, timeout=None))]
    fn wait_for_request<'py>(
        slf: &Bound<'py, Self>,
        pattern: Bound<'py, PyAny>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        seconds("timeout", timeout)?;
        delegate(slf, "wait", (slf, "request", pattern, timeout))
    }

    /// `wait_for_response` as an async context manager, so the wait starts
    /// before what makes the request: `async with page.expect_response(p)
    /// as info:`, then `await info.value`.
    #[pyo3(signature = (pattern, timeout=None))]
    fn expect_response<'py>(
        slf: &Bound<'py, Self>,
        pattern: Bound<'py, PyAny>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        seconds("timeout", timeout)?;
        delegate(slf, "expect", (slf, "response", pattern, timeout))
    }

    #[pyo3(signature = (pattern, timeout=None))]
    fn expect_request<'py>(
        slf: &Bound<'py, Self>,
        pattern: Bound<'py, PyAny>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        seconds("timeout", timeout)?;
        delegate(slf, "expect", (slf, "request", pattern, timeout))
    }

    /// Each request whose URL matches `pattern` (a glob, a compiled regex,
    /// or a callable taking the URL) goes to `handler`, called with a
    /// Route to decide about.
    fn route<'py>(
        slf: &Bound<'py, Self>,
        pattern: Bound<'py, PyAny>,
        handler: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        delegate(slf, "route", (slf, pattern, handler))
    }

    /// Answers requests from the HAR file at `path`, with nothing sent to
    /// the network. A request it doesn't hold fails, or with
    /// `not_found="fallback"` goes on as usual.
    #[pyo3(signature = (path, *, url=None, not_found="abort"))]
    fn route_from_har<'py>(
        slf: &Bound<'py, Self>,
        path: PathBuf,
        url: Option<Bound<'py, PyAny>>,
        not_found: &str,
    ) -> PyResult<Bound<'py, PyAny>> {
        let py = slf.py();
        let url = match url {
            Some(url) => url,
            None => "**/*".into_pyobject(py)?.into_any(),
        };
        delegate(slf, "route_from_har", (slf, path, url, not_found))
    }

    /// Removes the routes with `pattern` (only the one with `handler`, if
    /// given).
    #[pyo3(signature = (pattern, handler=None))]
    fn unroute<'py>(
        slf: &Bound<'py, Self>,
        pattern: Bound<'py, PyAny>,
        handler: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        delegate(slf, "unroute", (slf, pattern, handler))
    }

    /// Fails, before they leave Chrome, requests of the resource types
    /// given and those whose URLs match one of the globs in `urls`.
    #[pyo3(signature = (resource_types=vec!["image".to_string(), "font".to_string(), "media".to_string()], urls=Vec::new()))]
    fn block<'py>(
        &self,
        py: Python<'py>,
        resource_types: Vec<String>,
        urls: Vec<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            let types: Vec<&str> = resource_types.iter().map(String::as_str).collect();
            page.block(&types, &urls).await.map_err(raise)
        })
    }

    /// Calls `callback` (a function or a coroutine function) with each
    /// dialog the page opens, a BrowserDialog. One the listeners leave
    /// unanswered is dismissed once they return (`beforeunload` is
    /// accepted); without listeners, every dialog is.
    fn on_dialog<'py>(
        slf: &Bound<'py, Self>,
        callback: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        delegate(slf, "on_dialog", (slf, callback))
    }

    fn remove_dialog_listener<'py>(
        slf: &Bound<'py, Self>,
        callback: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        delegate(slf, "off_dialog", (callback,))
    }

    /// Calls `callback` with each page this one opens (a popup or a new
    /// tab), a BrowserPage, once it's running with this page's routes.
    fn on_popup<'py>(
        slf: &Bound<'py, Self>,
        callback: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        delegate(slf, "on", (slf, "popup", callback))
    }

    fn remove_popup_listener<'py>(
        slf: &Bound<'py, Self>,
        callback: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        delegate(slf, "off", ("popup", callback))
    }

    /// Waits for the next page this one opens.
    #[pyo3(signature = (timeout=None))]
    fn wait_for_popup<'py>(
        slf: &Bound<'py, Self>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        seconds("timeout", timeout)?;
        delegate(slf, "wait", (slf, "popup", slf.py().None(), timeout))
    }

    /// `wait_for_popup` as an async context manager: `async with
    /// page.expect_popup() as info:`, then `await info.value`.
    #[pyo3(signature = (timeout=None))]
    fn expect_popup<'py>(
        slf: &Bound<'py, Self>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        seconds("timeout", timeout)?;
        delegate(slf, "expect", (slf, "popup", slf.py().None(), timeout))
    }

    /// Calls `callback` with each download the page starts, a
    /// BrowserDownload.
    fn on_download<'py>(
        slf: &Bound<'py, Self>,
        callback: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        delegate(slf, "on", (slf, "download", callback))
    }

    fn remove_download_listener<'py>(
        slf: &Bound<'py, Self>,
        callback: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        delegate(slf, "off", ("download", callback))
    }

    /// Waits for a download whose URL matches `pattern` (a glob, a
    /// compiled regex, or a callable taking the download); any download
    /// without one.
    #[pyo3(signature = (pattern=None, timeout=None))]
    fn wait_for_download<'py>(
        slf: &Bound<'py, Self>,
        pattern: Option<Bound<'py, PyAny>>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        seconds("timeout", timeout)?;
        delegate(slf, "wait", (slf, "download", pattern, timeout))
    }

    #[pyo3(signature = (pattern=None, timeout=None))]
    fn expect_download<'py>(
        slf: &Bound<'py, Self>,
        pattern: Option<Bound<'py, PyAny>>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        seconds("timeout", timeout)?;
        delegate(slf, "expect", (slf, "download", pattern, timeout))
    }

    /// Hears every request, response, popup and download from now on.
    fn _listen(slf: &Bound<'_, Self>) -> NetworkEvents {
        NetworkEvents {
            rx: Arc::new(tokio::sync::Mutex::new(slf.get().inner.listen())),
            page: slf.get().inner.clone(),
            owner: slf.clone().unbind(),
        }
    }

    /// Takes every dialog from now on, until closed or asked again.
    fn _dialogs(&self) -> Dialogs {
        Dialogs {
            rx: Arc::new(tokio::sync::Mutex::new(self.inner.dialogs())),
        }
    }

    fn _route<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            let rx = page.route().await.map_err(raise)?;
            Ok(Routes::new(rx))
        })
    }

    fn _unroute<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let page = self.inner.clone();
        crate::aio::future_into_py(py, async move { page.unroute().await.map_err(raise) })
    }

    /// Stops handing requests to the page's routes, at once.
    fn _drop_routes(&self) {
        self.inner.drop_routes();
    }

    /// The browser's default limit, in seconds.
    #[getter]
    fn _timeout(&self) -> f64 {
        self.inner.default_timeout().as_secs_f64()
    }

    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let (page, live) = (self.inner.clone(), self.live.clone());
        crate::aio::future_into_py(py, async move {
            match live {
                Some(live) => {
                    live.close().await;
                    Ok(())
                }
                None => page.close().await.map_err(raise),
            }
        })
    }

    fn __aenter__<'py>(slf: Bound<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        let me = slf.clone().unbind();
        crate::aio::future_into_py(slf.py(), async move { Ok(me) })
    }

    fn __aexit__<'py>(
        &self,
        py: Python<'py>,
        _kind: Bound<'py, PyAny>,
        _value: Bound<'py, PyAny>,
        _traceback: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        self.close(py)
    }

    fn __repr__(&self) -> String {
        format!("<BrowserPage {}>", self.inner.url())
    }
}

/// A frame of a page: its own document, or one in an <iframe>, from the
/// page's site or another. Selectors find elements in its document.
#[pyclass(frozen, module = "netweir")]
pub struct BrowserFrame {
    inner: CoreFrame,
}

#[pymethods]
impl BrowserFrame {
    /// Its name attribute; "" for the page's own frame.
    #[getter]
    fn name(&self) -> &str {
        self.inner.name()
    }

    /// Its URL when it was found.
    #[getter]
    fn url(&self) -> &str {
        self.inner.url()
    }

    fn content<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let frame = self.inner.clone();
        crate::aio::future_into_py(py, async move { frame.content().await.map_err(raise) })
    }

    fn parse<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let frame = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            let html = frame.content().await.map_err(raise)?;
            let doc = tokio::task::spawn_blocking(move || Document::parse(&html))
                .await
                .map_err(|e| PyValueError::new_err(e.to_string()))?;
            Ok(Node::root_of(Arc::new(doc)))
        })
    }

    fn title<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let frame = self.inner.clone();
        crate::aio::future_into_py(py, async move { frame.title().await.map_err(raise) })
    }

    fn evaluate<'py>(&self, py: Python<'py>, js: String) -> PyResult<Bound<'py, PyAny>> {
        let frame = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            let value = frame.evaluate(&js).await.map_err(raise)?;
            crate::aio::attach(|py| to_python(py, &value))
        })
    }

    #[pyo3(signature = (selector, timeout=None))]
    fn click<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let timeout = seconds("timeout", timeout)?;
        let frame = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            frame.click(&selector, timeout).await.map_err(raise)
        })
    }

    /// Double-clicks the element (taps it twice on a phone).
    #[pyo3(signature = (selector, timeout=None))]
    fn dblclick<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let timeout = seconds("timeout", timeout)?;
        let frame = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            frame.dblclick(&selector, timeout).await.map_err(raise)
        })
    }

    #[pyo3(signature = (selector, text, timeout=None))]
    fn fill<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        text: String,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let timeout = seconds("timeout", timeout)?;
        let frame = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            frame.fill(&selector, &text, timeout).await.map_err(raise)
        })
    }

    #[pyo3(signature = (selector, text, delay=None, timeout=None))]
    fn r#type<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        text: String,
        delay: Option<Bound<'py, PyAny>>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let timeout = seconds("timeout", timeout)?;
        let delay = delay_of(delay)?;
        let frame = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            frame
                .type_text(&selector, &text, delay, timeout)
                .await
                .map_err(raise)
        })
    }

    #[pyo3(signature = (selector, state="visible".to_string(), timeout=None))]
    fn wait_for<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        state: String,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let timeout = seconds("timeout", timeout)?;
        let frame = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            frame
                .wait_for(&selector, &state, timeout)
                .await
                .map_err(raise)
        })
    }

    #[pyo3(signature = (selector, timeout=None))]
    fn hover<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        act::hover(py, self.inner.clone(), selector, timeout)
    }

    #[pyo3(signature = (source, target, timeout=None))]
    fn drag<'py>(
        &self,
        py: Python<'py>,
        source: String,
        target: String,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        act::drag(py, self.inner.clone(), source, target, timeout)
    }

    #[pyo3(signature = (selector=None, *, x=0.0, y=None, times=None, idle=2.0, timeout=None))]
    #[allow(clippy::too_many_arguments)]
    fn scroll<'py>(
        &self,
        py: Python<'py>,
        selector: Option<String>,
        x: f64,
        y: Option<f64>,
        times: Option<u32>,
        idle: f64,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let how = act::Scroll {
            selector,
            x,
            y,
            times,
            idle,
            timeout,
        };
        act::scroll(py, self.inner.clone(), how)
    }

    #[pyo3(signature = (selector, value=None, *, label=None, index=None, timeout=None))]
    fn select_option<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        value: Option<Bound<'py, PyAny>>,
        label: Option<Bound<'py, PyAny>>,
        index: Option<Bound<'py, PyAny>>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let pick = act::pick(value, label, index)?;
        act::select_option(py, self.inner.clone(), selector, pick, timeout)
    }

    #[pyo3(signature = (selector, timeout=None))]
    fn check<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        act::set_checked(py, self.inner.clone(), selector, true, timeout)
    }

    #[pyo3(signature = (selector, timeout=None))]
    fn uncheck<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        act::set_checked(py, self.inner.clone(), selector, false, timeout)
    }

    #[pyo3(signature = (selector, files, timeout=None))]
    fn set_input_files<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        files: Bound<'py, PyAny>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        act::set_input_files(py, self.inner.clone(), selector, &files, timeout)
    }

    fn __repr__(&self) -> String {
        format!(
            "<BrowserFrame {:?} {}>",
            self.inner.name(),
            self.inner.url()
        )
    }
}

/// The pause between keys `type` takes: seconds, or (least, most), and
/// a person's by default.
fn delay_of(delay: Option<Bound<'_, PyAny>>) -> PyResult<(Duration, Duration)> {
    let Some(delay) = delay.filter(|d| !d.is_none()) else {
        return Ok(netweir_browser::steps::TYPING);
    };
    let secs = |v: f64| {
        Duration::try_from_secs_f64(v)
            .map_err(|_| PyValueError::new_err("delay must be seconds, 0 or more"))
    };
    if let Ok(one) = delay.extract::<f64>() {
        let d = secs(one)?;
        return Ok((d, d));
    }
    let (least, most): (f64, f64) = delay
        .extract()
        .map_err(|_| PyTypeError::new_err("delay is seconds, or a (least, most) pair of them"))?;
    let (least, most) = (secs(least)?, secs(most)?);
    if least > most {
        return Err(PyValueError::new_err("delay's least is more than its most"));
    }
    Ok((least, most))
}

/// Steps from their JSON, a ValueError saying what's wrong with them.
pub(crate) fn steps_from(json: &str) -> PyResult<Arc<[netweir_browser::steps::Step]>> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| PyValueError::new_err(format!("steps must be JSON: {e}")))?;
    netweir_browser::steps::parse(&value)
        .map(Into::into)
        .map_err(PyValueError::new_err)
}

/// Checks steps (JSON) when they're written, rather than when a page
/// reaches them: each step, and with `cap` (seconds), that they ask for
/// no more time than that outright.
#[pyfunction]
#[pyo3(signature = (json, cap=None))]
pub(crate) fn _check_steps(json: &str, cap: Option<f64>) -> PyResult<()> {
    let steps = steps_from(json)?;
    let cap = cap
        .map(Duration::try_from_secs_f64)
        .transpose()
        .map_err(|_| PyValueError::new_err("browser_steps_max must be a number of seconds"))?;
    netweir_browser::steps::within(&steps, cap).map_err(PyValueError::new_err)
}

/// A paper format's width and height in inches.
fn paper_size(format: &str) -> PyResult<(f64, f64)> {
    Ok(match format.to_ascii_lowercase().as_str() {
        "letter" => (8.5, 11.0),
        "legal" => (8.5, 14.0),
        "tabloid" => (11.0, 17.0),
        "a3" => (11.69, 16.54),
        "a4" => (8.27, 11.69),
        "a5" => (5.83, 8.27),
        _ => {
            return Err(PyValueError::new_err(format!(
                "unknown format {format:?}; the formats are Letter, Legal, Tabloid, A3, A4 and A5"
            )));
        }
    })
}

/// How each step went, as Python dicts: `action`, `status` ("ok",
/// "failed" or "skipped"), `error`, `elapsed` (seconds) and `result`
/// (a screenshot's or a PDF's bytes).
pub(crate) fn outcomes_py<'py>(
    py: Python<'py>,
    outcomes: &[netweir_browser::steps::Outcome],
) -> PyResult<Bound<'py, PyList>> {
    let list = PyList::empty(py);
    for o in outcomes {
        let d = PyDict::new(py);
        d.set_item("action", &o.action)?;
        d.set_item("status", o.status.as_str())?;
        d.set_item("error", &o.error)?;
        d.set_item("elapsed", o.elapsed.as_secs_f64())?;
        match (&o.image, &o.value) {
            (Some(image), _) => d.set_item("result", PyBytes::new(py, image))?,
            (None, Some(v)) => d.set_item("result", to_python(py, v)?)?,
            (None, None) => d.set_item("result", py.None())?,
        }
        list.append(d)?;
    }
    Ok(list)
}

/// The actions a page and its frames share, on a frame.
mod act {
    use super::*;

    pub(super) fn hover<'py>(
        py: Python<'py>,
        frame: CoreFrame,
        selector: String,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let timeout = seconds("timeout", timeout)?;
        crate::aio::future_into_py(py, async move {
            frame.hover(&selector, timeout).await.map_err(raise)
        })
    }

    pub(super) fn drag<'py>(
        py: Python<'py>,
        frame: CoreFrame,
        source: String,
        target: String,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let timeout = seconds("timeout", timeout)?;
        crate::aio::future_into_py(py, async move {
            frame.drag(&source, &target, timeout).await.map_err(raise)
        })
    }

    pub(super) struct Scroll {
        pub selector: Option<String>,
        pub x: f64,
        pub y: Option<f64>,
        pub times: Option<u32>,
        pub idle: f64,
        pub timeout: Option<f64>,
    }

    pub(super) fn scroll(
        py: Python<'_>,
        frame: CoreFrame,
        how: Scroll,
    ) -> PyResult<Bound<'_, PyAny>> {
        let timeout = seconds("timeout", how.timeout)?;
        let idle = Duration::try_from_secs_f64(how.idle)
            .map_err(|_| PyValueError::new_err("idle must be a number of seconds, 0 or more"))?;
        if !how.x.is_finite() || !how.y.unwrap_or(0.0).is_finite() {
            return Err(PyValueError::new_err("x and y must be numbers of pixels"));
        }
        crate::aio::future_into_py(py, async move {
            frame
                .scroll(
                    how.selector.as_deref(),
                    how.x,
                    how.y,
                    how.times,
                    idle,
                    timeout,
                )
                .await
                .map_err(raise)
        })
    }

    /// One of value, label or index, each one or a list.
    pub(super) fn pick(
        value: Option<Bound<'_, PyAny>>,
        label: Option<Bound<'_, PyAny>>,
        index: Option<Bound<'_, PyAny>>,
    ) -> PyResult<Pick> {
        fn many<'py, T: for<'a> FromPyObject<'a, 'py>>(
            v: &Bound<'py, PyAny>,
            what: &str,
        ) -> PyResult<Vec<T>> {
            if let Ok(one) = v.extract::<T>() {
                return Ok(vec![one]);
            }
            v.try_iter()?
                .map(|item| item?.extract::<T>().map_err(Into::into))
                .collect::<PyResult<Vec<T>>>()
                .map_err(|_| PyTypeError::new_err(format!("{what} must be one, or a list")))
        }
        match (value, label, index) {
            (Some(v), None, None) => Ok(Pick::Values(many(&v, "value")?)),
            (None, Some(l), None) => Ok(Pick::Labels(many(&l, "label")?)),
            (None, None, Some(i)) => Ok(Pick::Indexes(many(&i, "index")?)),
            _ => Err(PyValueError::new_err("give one of value, label or index")),
        }
    }

    pub(super) fn select_option(
        py: Python<'_>,
        frame: CoreFrame,
        selector: String,
        pick: Pick,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'_, PyAny>> {
        let timeout = seconds("timeout", timeout)?;
        crate::aio::future_into_py(py, async move {
            frame
                .select_option(&selector, &pick, timeout)
                .await
                .map_err(raise)
        })
    }

    pub(super) fn set_checked(
        py: Python<'_>,
        frame: CoreFrame,
        selector: String,
        on: bool,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'_, PyAny>> {
        let timeout = seconds("timeout", timeout)?;
        crate::aio::future_into_py(py, async move {
            frame
                .set_checked(&selector, on, timeout)
                .await
                .map_err(raise)
        })
    }

    pub(super) fn set_input_files<'py>(
        py: Python<'py>,
        frame: CoreFrame,
        selector: String,
        files: &Bound<'py, PyAny>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let timeout = seconds("timeout", timeout)?;
        // A path, or a list of them.
        let files: Vec<PathBuf> = match files.extract::<PathBuf>() {
            Ok(one) => vec![one],
            Err(_) => files
                .try_iter()?
                .map(|f| f?.extract::<PathBuf>())
                .collect::<PyResult<_>>()
                .map_err(|_| PyTypeError::new_err("files must be a path, or a list of paths"))?,
        };
        if let Some(dir) = files.iter().find(|f| f.is_dir()) {
            return Err(PyIsADirectoryError::new_err(format!(
                "{} is a directory, not a file",
                dir.display()
            )));
        }
        if let Some(missing) = files.iter().find(|f| !f.is_file()) {
            return Err(PyFileNotFoundError::new_err(format!(
                "no file at {}",
                missing.display()
            )));
        }
        crate::aio::future_into_py(py, async move {
            frame
                .set_input_files(&selector, &files, timeout)
                .await
                .map_err(raise)
        })
    }
}

/// A response the page got: the main document's to a navigation, or any
/// other the page's listeners heard about.
#[pyclass(frozen, module = "netweir")]
pub struct BrowserResponse {
    inner: CoreResponse,
    /// The page that got it, which has its body.
    page: CorePage,
}

impl BrowserResponse {
    fn new(inner: CoreResponse, page: &CorePage) -> BrowserResponse {
        BrowserResponse {
            inner,
            page: page.clone(),
        }
    }

    fn read<'py>(
        &self,
        py: Python<'py>,
        then: fn(Python<'_>, Vec<u8>) -> PyResult<Py<PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let (page, response) = (self.page.clone(), self.inner.clone());
        crate::aio::future_into_py(py, async move {
            let body = page.body(&response).await.map_err(raise)?;
            crate::aio::attach(|py| then(py, body))
        })
    }
}

#[pymethods]
impl BrowserResponse {
    #[getter]
    fn url(&self) -> &str {
        &self.inner.url
    }

    /// 0 when there was no HTTP response (about:, data:).
    #[getter]
    fn status(&self) -> u16 {
        self.inner.status
    }

    #[getter]
    fn headers(&self) -> Vec<(String, String)> {
        self.inner.headers.clone()
    }

    /// The request's method.
    #[getter]
    fn method(&self) -> &str {
        &self.inner.method
    }

    /// "document", "xhr", "fetch", "image" and so on.
    #[getter]
    fn resource_type(&self) -> &str {
        &self.inner.resource_type
    }

    /// The body as bytes, once it has all arrived.
    fn body<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.read(py, |py, body| {
            Ok(PyBytes::new(py, &body).into_any().unbind())
        })
    }

    /// The body as text, read as UTF-8.
    fn text<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.read(py, |py, body| {
            Ok(String::from_utf8_lossy(&body)
                .into_pyobject(py)?
                .into_any()
                .unbind())
        })
    }

    /// The body parsed as JSON.
    fn json<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.read(py, |py, body| {
            Ok(py
                .import("json")?
                .call_method1("loads", (PyBytes::new(py, &body),))?
                .unbind())
        })
    }

    fn __repr__(&self) -> String {
        format!("<BrowserResponse {} {}>", self.inner.status, self.inner.url)
    }
}

/// A request the page made.
#[pyclass(frozen, module = "netweir")]
pub struct BrowserRequest {
    inner: CoreRequest,
}

#[pymethods]
impl BrowserRequest {
    #[getter]
    fn url(&self) -> &str {
        &self.inner.url
    }

    #[getter]
    fn method(&self) -> &str {
        &self.inner.method
    }

    #[getter]
    fn headers(&self) -> Vec<(String, String)> {
        self.inner.headers.clone()
    }

    /// "document", "xhr", "fetch", "image" and so on.
    #[getter]
    fn resource_type(&self) -> &str {
        &self.inner.resource_type
    }

    /// The body sent with it, as bytes; None when there was none.
    #[getter]
    fn post_data<'py>(&self, py: Python<'py>) -> Option<Bound<'py, PyBytes>> {
        self.inner.post_data.as_ref().map(|b| PyBytes::new(py, b))
    }

    fn __repr__(&self) -> String {
        format!("<BrowserRequest {} {}>", self.inner.method, self.inner.url)
    }
}

/// A page's requests and responses, as they happen, for netweir._network.
#[pyclass(frozen, module = "netweir._native")]
pub struct NetworkEvents {
    rx: Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<PageEvent>>>,
    page: CorePage,
    /// The page's own Python object, a popup's opener.
    owner: Py<BrowserPage>,
}

#[pymethods]
impl NetworkEvents {
    /// Stops listening: the page sends nothing more here. A `next`
    /// still waiting finishes first.
    fn close(&self) {
        let rx = self.rx.clone();
        pyo3_async_runtimes::tokio::get_runtime().spawn(async move { rx.lock().await.close() });
    }

    /// Resolves to the next BrowserRequest, BrowserResponse, BrowserPage
    /// (a popup) or BrowserDownload, or None once the page has closed.
    fn next<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let (rx, page, owner) = (self.rx.clone(), self.page.clone(), self.owner.clone_ref(py));
        crate::aio::future_into_py(py, async move {
            let event = rx.lock().await.recv().await;
            crate::aio::attach(move |py| -> PyResult<Py<PyAny>> {
                Ok(match event {
                    None => py.None(),
                    Some(PageEvent::Request(inner)) => {
                        Py::new(py, BrowserRequest { inner })?.into_any()
                    }
                    Some(PageEvent::Response(inner)) => {
                        Py::new(py, BrowserResponse::new(inner, &page))?.into_any()
                    }
                    Some(PageEvent::Popup(inner)) => {
                        let mut popup = BrowserPage::new(inner, None);
                        popup.opener = Some(owner);
                        Py::new(py, popup)?.into_any()
                    }
                    Some(PageEvent::Download(inner)) => {
                        Py::new(py, BrowserDownload { inner })?.into_any()
                    }
                })
            })
        })
    }
}

/// The dialogs a page opens, for netweir._network.
#[pyclass(frozen, module = "netweir._native")]
pub struct Dialogs {
    rx: Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<CoreDialog>>>,
}

#[pymethods]
impl Dialogs {
    /// Stops taking dialogs: those not yet taken, and those to come, are
    /// answered the default way.
    fn close(&self) {
        let rx = self.rx.clone();
        pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
            let mut rx = rx.lock().await;
            rx.close();
            while rx.try_recv().is_ok() {}
        });
    }

    /// Resolves to the next BrowserDialog, or None once the page has
    /// closed or another took over.
    fn next<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let rx = self.rx.clone();
        crate::aio::future_into_py(py, async move {
            let dialog = rx.lock().await.recv().await;
            crate::aio::attach(|py| -> PyResult<Py<PyAny>> {
                Ok(match dialog {
                    None => py.None(),
                    Some(inner) => Py::new(py, BrowserDialog { inner })?.into_any(),
                })
            })
        })
    }
}

/// A dialog the page is waiting on: an alert, a confirm, a prompt or a
/// beforeunload. Answer it once, with `accept()` or `dismiss()`; the
/// answer reaches the page once a person could have given it.
#[pyclass(frozen, module = "netweir")]
pub struct BrowserDialog {
    inner: CoreDialog,
}

#[pymethods]
impl BrowserDialog {
    /// "alert", "confirm", "prompt" or "beforeunload".
    #[getter(r#type)]
    fn kind(&self) -> &str {
        self.inner.kind()
    }

    #[getter]
    fn message(&self) -> &str {
        self.inner.message()
    }

    /// The text a prompt offers; empty for the others.
    #[getter]
    fn default_value(&self) -> &str {
        self.inner.default_value()
    }

    /// The URL of the frame that opened it.
    #[getter]
    fn url(&self) -> &str {
        self.inner.url()
    }

    /// Whether it's been answered.
    #[getter]
    fn handled(&self) -> bool {
        self.inner.is_answered()
    }

    /// Presses OK, with `prompt_text` in a prompt's box (its default if
    /// None); for beforeunload, leaves the page. It counts as answered
    /// at once, so a plain listener can call it without awaiting.
    #[pyo3(signature = (prompt_text=None))]
    fn accept<'py>(
        &self,
        py: Python<'py>,
        prompt_text: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let answer = self.inner.accept(prompt_text.as_deref());
        crate::aio::future_into_py(py, async move { answer.await.map_err(raise) })
    }

    /// Presses Cancel; for beforeunload, stays on the page.
    fn dismiss<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let answer = self.inner.dismiss();
        crate::aio::future_into_py(py, async move { answer.await.map_err(raise) })
    }

    /// Answers it the default way, unless it's been answered.
    fn _release(&self) {
        self.inner.release();
    }

    fn __repr__(&self) -> String {
        format!(
            "<BrowserDialog {} {:?}>",
            self.inner.kind(),
            self.inner.message()
        )
    }
}

/// A file the page downloads, into a folder of its context's own that
/// goes when the context closes.
#[pyclass(frozen, module = "netweir")]
pub struct BrowserDownload {
    inner: CoreDownload,
}

#[pymethods]
impl BrowserDownload {
    #[getter]
    fn url(&self) -> &str {
        self.inner.url()
    }

    /// The name the server or the page gave the file.
    #[getter]
    fn suggested_filename(&self) -> &str {
        self.inner.suggested_filename()
    }

    /// Waits for it to finish, and resolves to the path of the file.
    /// Raises BrowserError if it didn't finish, and BrowserTimeout if it
    /// hasn't after `timeout` seconds (the browser's if None), leaving it
    /// going.
    #[pyo3(signature = (timeout=None))]
    fn path<'py>(&self, py: Python<'py>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let timeout = seconds("timeout", timeout)?;
        let download = self.inner.clone();
        crate::aio::future_into_py(
            py,
            async move { download.path(timeout).await.map_err(raise) },
        )
    }

    /// Waits for it to finish, as `path` does, and copies the file to
    /// `path`.
    #[pyo3(signature = (path, timeout=None))]
    fn save_as<'py>(
        &self,
        py: Python<'py>,
        path: PathBuf,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let timeout = seconds("timeout", timeout)?;
        let download = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            let from = download.path(timeout).await.map_err(raise)?;
            tokio::task::spawn_blocking(move || std::fs::copy(from, path))
                .await
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))??;
            Ok(())
        })
    }

    /// Waits for it to finish, as `path` does, and resolves to the file's
    /// bytes.
    #[pyo3(signature = (timeout=None))]
    fn body<'py>(&self, py: Python<'py>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let timeout = seconds("timeout", timeout)?;
        let download = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            let from = download.path(timeout).await.map_err(raise)?;
            let body = tokio::task::spawn_blocking(move || std::fs::read(from))
                .await
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))??;
            crate::aio::attach(|py| Ok(PyBytes::new(py, &body).unbind()))
        })
    }

    /// Waits for it to end, and resolves to None if it finished, or why
    /// it didn't ("canceled", for one cancelled or whose connection
    /// broke). Raises BrowserTimeout as `path` does.
    #[pyo3(signature = (timeout=None))]
    fn failure<'py>(&self, py: Python<'py>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let timeout = seconds("timeout", timeout)?;
        let download = self.inner.clone();
        crate::aio::future_into_py(
            py,
            async move { download.failure(timeout).await.map_err(raise) },
        )
    }

    /// Stops it, if it hasn't finished.
    fn cancel<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let download = self.inner.clone();
        crate::aio::future_into_py(py, async move { download.cancel().await.map_err(raise) })
    }

    fn __repr__(&self) -> String {
        format!("<BrowserDownload {}>", self.inner.url())
    }
}

/// Requests the routes of a page or a context get to decide about, for
/// netweir._network.
#[pyclass(frozen, module = "netweir._native")]
pub struct Routes {
    rx: Arc<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<CoreRoute>>>,
}

impl Routes {
    fn new(rx: tokio::sync::mpsc::UnboundedReceiver<CoreRoute>) -> Routes {
        Routes {
            rx: Arc::new(tokio::sync::Mutex::new(rx)),
        }
    }
}

#[pymethods]
impl Routes {
    /// Resolves to the next Route, or None once routing has stopped.
    fn next<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let rx = self.rx.clone();
        crate::aio::future_into_py(py, async move {
            let route = rx.lock().await.recv().await;
            crate::aio::attach(|py| -> PyResult<Py<PyAny>> {
                Ok(match route {
                    None => py.None(),
                    Some(inner) => Py::new(py, Route { inner })?.into_any(),
                })
            })
        })
    }
}

/// A request held in Chrome until a route handler decides about it:
/// `abort()`, `continue_()` or `fulfill()`, once. One the handler leaves
/// undecided goes on unchanged when the handler returns.
#[pyclass(frozen, module = "netweir")]
pub struct Route {
    inner: CoreRoute,
}

#[pymethods]
impl Route {
    #[getter]
    fn request(&self) -> BrowserRequest {
        BrowserRequest {
            inner: self.inner.request().clone(),
        }
    }

    /// Whether it's been aborted, continued or fulfilled.
    #[getter]
    fn handled(&self) -> bool {
        self.inner.is_decided()
    }

    /// Fails the request; the page sees a network error.
    fn abort<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let route = self.inner.clone();
        crate::aio::future_into_py(py, async move { route.abort().await.map_err(raise) })
    }

    /// Lets the request go on, changed as asked. `headers` replace all the
    /// request's headers.
    #[pyo3(signature = (url=None, method=None, headers=None, body=None))]
    fn continue_<'py>(
        &self,
        py: Python<'py>,
        url: Option<String>,
        method: Option<String>,
        headers: Option<Bound<'py, PyAny>>,
        body: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let with = Continue {
            url,
            method,
            headers: headers.as_ref().map(header_pairs).transpose()?,
            body: body.as_ref().map(body_bytes).transpose()?,
        };
        let route = self.inner.clone();
        crate::aio::future_into_py(
            py,
            async move { route.continue_(with).await.map_err(raise) },
        )
    }

    /// Answers the request without it leaving Chrome. `json` is sent as
    /// JSON, with an application/json content type unless `headers` give
    /// one.
    #[pyo3(signature = (status=200, headers=None, body=None, json=None))]
    fn fulfill<'py>(
        &self,
        py: Python<'py>,
        status: u16,
        headers: Option<Bound<'py, PyAny>>,
        body: Option<Bound<'py, PyAny>>,
        json: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let mut headers = headers
            .as_ref()
            .map(header_pairs)
            .transpose()?
            .unwrap_or_default();
        let body = match (body, json) {
            (Some(_), Some(_)) => {
                return Err(PyValueError::new_err("give body or json, not both"));
            }
            (Some(body), None) => body_bytes(&body)?,
            (None, Some(json)) => {
                if !headers
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
                {
                    headers.push(("Content-Type".into(), "application/json".into()));
                }
                py.import("json")?
                    .call_method1("dumps", (json,))?
                    .extract::<String>()?
                    .into_bytes()
            }
            (None, None) => Vec::new(),
        };
        let route = self.inner.clone();
        crate::aio::future_into_py(py, async move {
            route.fulfill(status, &headers, &body).await.map_err(raise)
        })
    }

    /// Lets the request go on unchanged, unless it's been decided.
    fn _release(&self) {
        self.inner.release();
    }

    fn __repr__(&self) -> String {
        let r = self.inner.request();
        format!("<Route {} {}>", r.method, r.url)
    }
}

/// Whether `url` matches the glob `pattern`, as routes and waiters match
/// them.
#[pyfunction]
fn _glob_match(pattern: &str, url: &str) -> bool {
    netweir_browser::glob_match(pattern, url)
}

/// A list of permission names; a lone str is refused, since it would be
/// taken letter by letter.
fn permission_names(v: &Bound<'_, PyAny>) -> PyResult<Vec<String>> {
    if v.is_instance_of::<pyo3::types::PyString>() {
        return Err(PyTypeError::new_err(
            "permissions is a list of names, such as [\"geolocation\"]",
        ));
    }
    v.extract()
}

/// An init script given as itself or as a file.
fn init_script(script: Option<String>, path: Option<PathBuf>) -> PyResult<String> {
    match (script, path) {
        (Some(s), None) => Ok(s),
        (None, Some(p)) => std::fs::read_to_string(&p).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                PyFileNotFoundError::new_err(format!("{}: no such file", p.display()))
            } else {
                PyValueError::new_err(format!("can't read {}: {e}", p.display()))
            }
        }),
        _ => Err(PyTypeError::new_err(
            "add_init_script takes a script or a path, one of them",
        )),
    }
}

/// JSON as Python values.
pub(crate) fn to_python(py: Python<'_>, value: &serde_json::Value) -> PyResult<Py<PyAny>> {
    use serde_json::Value;
    Ok(match value {
        Value::Null => py.None(),
        Value::Bool(b) => b.into_pyobject(py)?.to_owned().into_any().unbind(),
        Value::Number(n) => match n.as_i64() {
            Some(i) => i.into_pyobject(py)?.into_any().unbind(),
            None => n
                .as_f64()
                .unwrap_or(f64::NAN)
                .into_pyobject(py)?
                .into_any()
                .unbind(),
        },
        Value::String(s) => s.into_pyobject(py)?.into_any().unbind(),
        Value::Array(items) => {
            let list = PyList::empty(py);
            for item in items {
                list.append(to_python(py, item)?)?;
            }
            list.into_any().unbind()
        }
        Value::Object(map) => {
            let d = PyDict::new(py);
            for (k, v) in map {
                d.set_item(k, to_python(py, v)?)?;
            }
            d.into_any().unbind()
        }
    })
}

/// Installs the Chrome for Testing build matching netweir's newest Chrome
/// profile under `home` (`$NETWEIR_HOME` or `~/.netweir` by default), as
/// `index` lists it. Returns (version, executable, already installed).
#[pyfunction]
#[pyo3(signature = (headless_shell=false, index=netweir_core::install::CHROME_FOR_TESTING.to_string(), home=None))]
fn install_chrome(
    py: Python<'_>,
    headless_shell: bool,
    index: String,
    home: Option<PathBuf>,
) -> PyResult<(String, PathBuf, bool)> {
    let home = home.unwrap_or_else(netweir_browser::netweir_home);
    let runtime = pyo3_async_runtimes::tokio::get_runtime();
    py.detach(|| {
        runtime.block_on(netweir_core::install::install_chrome(
            &index,
            &home,
            headless_shell,
        ))
    })
    .map(|i| (i.version, i.executable, i.already))
    .map_err(browser_error)
}

/// Installs the Firefox Netweir pins (`netweir install firefox`) under
/// `home` (`$NETWEIR_HOME` or `~/.netweir` by default), from `base`, laid out as
/// Mozilla's release archive is. `version` and `sums`, (platform, SHA-256)
/// pairs, replace the pinned release's. Returns (version, executable,
/// already installed).
#[pyfunction]
#[pyo3(signature = (base=netweir_core::install::FIREFOX_RELEASES.to_string(), home=None, version=None, sums=None))]
fn install_firefox(
    py: Python<'_>,
    base: String,
    home: Option<PathBuf>,
    version: Option<String>,
    sums: Option<Vec<(String, String)>>,
) -> PyResult<(String, PathBuf, bool)> {
    let home = home.unwrap_or_else(netweir_browser::netweir_home);
    let pinned = netweir_core::install::FIREFOX;
    let sums: Option<Vec<(&str, &str)>> = sums
        .as_ref()
        .map(|s| s.iter().map(|(p, h)| (p.as_str(), h.as_str())).collect());
    let release = netweir_core::install::FirefoxRelease {
        version: version.as_deref().unwrap_or(pinned.version),
        sums: sums.as_deref().unwrap_or(pinned.sums),
    };
    let runtime = pyo3_async_runtimes::tokio::get_runtime();
    py.detach(|| {
        runtime.block_on(netweir_core::install::install_firefox(
            &base, &home, &release,
        ))
    })
    .map(|i| (i.version, i.executable, i.already))
    .map_err(browser_error)
}

/// An installer's failure, as BrowserError.
fn browser_error(e: String) -> PyErr {
    crate::aio::attach(|py| -> PyErr {
        match py
            .import("netweir._errors")
            .and_then(|m| m.getattr("BrowserError"))
            .and_then(|c| c.call1((e.clone(),)))
        {
            Ok(v) => PyErr::from_value(v),
            Err(err) => err,
        }
    })
}

/// The Chrome `netweir.browser()` would start, or BrowserError saying
/// where it looked.
#[pyfunction]
fn find_chrome() -> PyResult<PathBuf> {
    netweir_browser::find_chrome().map_err(raise)
}

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // What the browser carries on past goes where the rest of netweir's
    // warnings do. Called from Tokio's threads, which hold nothing the
    // event loop waits on.
    netweir_browser::on_warning(|message| {
        Python::attach(|py| {
            let _ = py
                .import("logging")
                .and_then(|l| l.call_method1("getLogger", ("netweir",)))
                .and_then(|log| log.call_method1("warning", (message,)));
        })
    });
    m.add_function(wrap_pyfunction!(launch, m)?)?;
    m.add_function(wrap_pyfunction!(find_chrome, m)?)?;
    m.add_function(wrap_pyfunction!(install_chrome, m)?)?;
    m.add_function(wrap_pyfunction!(install_firefox, m)?)?;
    m.add_class::<Browser>()?;
    m.add_class::<BrowserContext>()?;
    m.add_class::<BrowserPage>()?;
    m.add_function(wrap_pyfunction!(_check_steps, m)?)?;
    m.add_class::<BrowserFrame>()?;
    m.add_class::<BrowserResponse>()?;
    m.add_class::<BrowserRequest>()?;
    m.add_class::<Route>()?;
    m.add_class::<NetworkEvents>()?;
    m.add_class::<Dialogs>()?;
    m.add_class::<BrowserDialog>()?;
    m.add_class::<BrowserDownload>()?;
    m.add_class::<Routes>()?;
    m.add_function(wrap_pyfunction!(_glob_match, m)?)?;
    m.add_function(wrap_pyfunction!(crate::environment::_devices, m)?)?;
    m.add_function(wrap_pyfunction!(crate::environment::_check_environment, m)?)?;
    m.add(
        "_RESOURCE_TYPES",
        netweir_browser::resource_types().collect::<Vec<_>>(),
    )?;
    Ok(())
}
