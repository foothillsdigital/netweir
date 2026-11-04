#![allow(dead_code)]

use std::path::PathBuf;

/// The Chrome to test with, or None (and a note) when there isn't one, so
/// the browser tests skip on machines without Chrome. CI sets
/// NETWEIR_REQUIRE_CHROME to make that a failure instead.
pub fn chrome() -> Option<PathBuf> {
    if engine() == netweir_browser::Engine::Firefox {
        eprintln!("skipped on Firefox: the test launches Chrome itself");
        return None;
    }
    match netweir_browser::find_chrome() {
        Ok(path) => Some(path),
        Err(e) if std::env::var_os("NETWEIR_REQUIRE_CHROME").is_some() => panic!("{e}"),
        Err(e) => {
            eprintln!("skipped: {e}");
            None
        }
    }
}

/// The Firefox to test with: one `netweir install firefox` installed, or
/// None (and a note) when there isn't one. CI sets NETWEIR_REQUIRE_FIREFOX
/// to make that a failure instead.
pub fn firefox() -> Option<PathBuf> {
    match netweir_browser::find_firefox() {
        Ok(path) => Some(path),
        Err(e) if std::env::var_os("NETWEIR_REQUIRE_FIREFOX").is_some() => panic!("{e}"),
        Err(e) => {
            eprintln!("skipped: {e}");
            None
        }
    }
}

/// Launches the Firefox at `executable` with `options` otherwise, once
/// it's this test's turn.
pub async fn launch_firefox(
    executable: PathBuf,
    options: netweir_browser::LaunchOptions,
) -> TestBrowser {
    let turn = turn();
    let browser = netweir_browser::Browser::launch(netweir_browser::LaunchOptions {
        engine: netweir_browser::Engine::Firefox,
        executable: Some(executable),
        timeout: std::time::Duration::from_secs(60),
        ..options
    })
    .await
    .unwrap();
    TestBrowser {
        browser,
        _turn: turn,
    }
}

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: String,
}

pub fn html(body: &str) -> Reply {
    Reply {
        status: 200,
        headers: vec![("Content-Type", "text/html; charset=utf-8".into())],
        body: body.to_string(),
    }
}

/// Each request's path and headers.
pub type Seen = Arc<Mutex<Vec<(String, Vec<(String, String)>)>>>;

/// A local HTTP/1.1 server for the given paths; anything else is a 404.
/// Records each request's path and headers. Stops listening when dropped.
pub struct Server {
    pub url: String,
    pub seen: Seen,
    _listening: Listening,
}

impl Server {
    /// The path of each request so far, in order.
    pub fn paths(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|(p, _)| p.clone())
            .collect()
    }

    pub fn headers_for(&self, path: &str) -> Vec<(String, String)> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, h)| h.clone())
            .unwrap_or_default()
    }
}

pub fn serve(routes: Vec<(&'static str, Reply)>) -> Server {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let listening = listen(move |mut stream| {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        // Chrome opens connections ahead of need and closes some
        // unused: one that sends nothing is no request for "/".
        if !matches!(reader.read_line(&mut line), Ok(n) if n > 0) {
            return;
        }
        let path = line.split(' ').nth(1).unwrap_or("/").to_string();
        let mut headers = Vec::new();
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                break;
            }
            if let Some((k, v)) = h.trim_end().split_once(':') {
                headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
        }
        log.lock().unwrap().push((path.clone(), headers));
        let missing = Reply {
            status: 404,
            headers: vec![("Content-Type", "text/html".into())],
            body: "<p>not here</p>".into(),
        };
        let reply = routes
            .iter()
            .find(|(p, _)| *p == path)
            .map(|(_, r)| r)
            .unwrap_or(&missing);
        // A test asks for a slow answer with this header, which
        // isn't sent.
        let mut reply_headers = reply.headers.clone();
        if let Some(i) = reply_headers.iter().position(|(k, _)| *k == "X-Delay-Ms") {
            let (_, ms) = reply_headers.remove(i);
            std::thread::sleep(std::time::Duration::from_millis(ms.parse().unwrap()));
        }
        let mut out = format!(
            "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
            reply.status,
            reply.body.len()
        );
        for (k, v) in &reply_headers {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        out.push_str("\r\n");
        out.push_str(&reply.body);
        let _ = stream.write_all(out.as_bytes());
    });
    Server {
        url: format!("http://127.0.0.1:{}", listening.port),
        seen,
        _listening: listening,
    }
}

/// Listens on 127.0.0.1 at a free port and, where the machine has it, on
/// [::1] at the same one, each connection handled on a thread of its own,
/// until what it returns is dropped.
/// Pages reach test servers as localhost too, which Windows tries as ::1
/// first: with nothing listening there, every such request started with a
/// refused connection.
fn listen(handle: impl Fn(TcpStream) + Send + Sync + 'static) -> Listening {
    let v4 = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = v4.local_addr().unwrap().port();
    let v6 = TcpListener::bind(("::1", port)).ok();
    let handle = Arc::new(handle);
    let stop = Arc::new(AtomicBool::new(false));
    let open = Arc::new(Mutex::new(HashMap::new()));
    let mut addrs = Vec::new();
    let mut accepting = Vec::new();
    for listener in std::iter::once(v4).chain(v6) {
        addrs.push(listener.local_addr().unwrap());
        let handle = handle.clone();
        let stop = stop.clone();
        let open = open.clone();
        accepting.push(std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let peer = stream.peer_addr().ok();
                if let (Some(peer), Ok(clone)) = (peer, stream.try_clone()) {
                    open.lock().unwrap().insert(peer, clone);
                }
                let handle = handle.clone();
                let open = open.clone();
                std::thread::spawn(move || {
                    handle(stream);
                    // Held here too, this would keep the connection open
                    // after its answer, and a client reading to the end
                    // would wait for ever.
                    if let Some(peer) = peer {
                        open.lock().unwrap().remove(&peer);
                    }
                });
            }
        }));
    }
    Listening {
        port,
        stop,
        addrs,
        accepting,
        open,
    }
}

/// A test server's listeners. Without this they'd stay open, each with a
/// thread, until the test binary ended.
struct Listening {
    port: u16,
    stop: Arc<AtomicBool>,
    addrs: Vec<std::net::SocketAddr>,
    accepting: Vec<std::thread::JoinHandle<()>>,
    /// Each connection being handled. Chrome opens some ahead of need, and
    /// a handler waiting on one would answer after the server was dropped.
    open: Arc<Mutex<HashMap<std::net::SocketAddr, TcpStream>>>,
}

impl Drop for Listening {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wakes each accept, which then sees the flag and ends its loop,
        // closing the listener.
        for addr in &self.addrs {
            let _ = TcpStream::connect(addr);
        }
        for thread in self.accepting.drain(..) {
            let _ = thread.join();
        }
        for (_, stream) in self.open.lock().unwrap().drain() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

/// A headless browser for a test, or None when there's no Chrome.
/// A turn with Chrome. Only two tests have a Chrome at once: a small CI
/// runner with one each, all at once, takes longer over a page than a test
/// waits. Blocking, so the helpers that start Chrome by hand can take one.
pub struct Turn(usize);

const ALL_TURNS: usize = 2;

static TURNS: (Mutex<usize>, std::sync::Condvar) =
    (Mutex::new(ALL_TURNS), std::sync::Condvar::new());

pub fn turn() -> Turn {
    turns(1)
}

/// Every turn there is, for a test whose Chromes are its alone. They're
/// taken at once: two such tests taking them one by one could each hold
/// one and wait on the other for good, and every other test with them.
pub fn all_turns() -> Turn {
    turns(ALL_TURNS)
}

/// `n` turns at once, for a test that runs `n` browsers together.
pub fn turns_for(n: usize) -> Turn {
    turns(n)
}

fn turns(n: usize) -> Turn {
    report_socket_state_on_enobufs();
    let mut free = TURNS.0.lock().unwrap_or_else(|e| e.into_inner());
    while *free < n {
        free = TURNS.1.wait(free).unwrap_or_else(|e| e.into_inner());
    }
    *free -= n;
    Turn(n)
}

impl Drop for Turn {
    fn drop(&mut self) {
        *TURNS.0.lock().unwrap_or_else(|e| e.into_inner()) += self.0;
        // All, as a waiter for more than is free waits on.
        TURNS.1.notify_all();
    }
}

/// Windows runners now and then refuse a connection to the test server
/// with WSAENOBUFS. What ran out is only visible while it lasts, so a test
/// that panics that way prints the machine's state as
/// tests/socket_state.ps1 reports it. Nothing anywhere else.
fn report_socket_state_on_enobufs() {
    if !cfg!(windows) {
        return;
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            previous(info);
            if !info.to_string().contains("ERR_NO_BUFFER_SPACE") {
                return;
            }
            let script =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/socket_state.ps1");
            let said = std::process::Command::new("powershell")
                .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
                .arg(script)
                .output();
            match said {
                Ok(out) => eprintln!("socket state:\n{}", String::from_utf8_lossy(&out.stdout)),
                Err(e) => eprintln!("socket state: couldn't read it: {e}"),
            }
        }));
    });
}

/// A launched Chrome, with its turn, for the test.
pub struct TestBrowser {
    browser: netweir_browser::Browser,
    _turn: Turn,
}

impl std::ops::Deref for TestBrowser {
    type Target = netweir_browser::Browser;
    fn deref(&self) -> &netweir_browser::Browser {
        &self.browser
    }
}

/// Launches Chrome with `options`, once it's this test's turn.
///
/// A test's short timeout is for the wait it's about, and since a launch
/// keeps to the same timeout, a busy runner's Chrome could miss it before
/// the test began. Here launching is only setup, so a launch that timed
/// out gets two more tries; the tests about launching call
/// `Browser::launch` themselves.
pub async fn launch(options: netweir_browser::LaunchOptions) -> TestBrowser {
    let turn = turn();
    let mut tries = 0;
    let browser = loop {
        match netweir_browser::Browser::launch(options.clone()).await {
            Err(netweir_browser::Error::Launch(m)) if m.contains("didn't answer") && tries < 2 => {
                tries += 1;
            }
            other => break other.unwrap(),
        }
    };
    TestBrowser {
        browser,
        _turn: turn,
    }
}

/// The engine the suites run on: Chrome, or Firefox when
/// NETWEIR_TEST_ENGINE is `firefox`.
pub fn engine() -> netweir_browser::Engine {
    match std::env::var("NETWEIR_TEST_ENGINE").as_deref() {
        Err(_) | Ok("chrome") => netweir_browser::Engine::Chrome,
        Ok("firefox") => netweir_browser::Engine::Firefox,
        Ok(other) => panic!("NETWEIR_TEST_ENGINE is {other:?}: it can be chrome or firefox"),
    }
}

/// Tests of the Chrome suites that can't pass on Firefox yet, by name,
/// with what they're waiting for.
pub const FIREFOX_SKIPS: &[(&str, &str)] = &[
    (
        "drag_to_a_target_out_of_view_with_the_source_says_so_and_presses_nothing",
        "HTML5 drag and drop, which Firefox doesn't begin from WebDriver's mouse",
    ),
    (
        "drag_drops_an_item_by_html5_drag_and_drop",
        "HTML5 drag and drop, which Firefox doesn't begin from WebDriver's mouse",
    ),
    (
        "a_frame_drags_within_itself_wherever_it_sits_in_the_page",
        "HTML5 drag and drop, which Firefox doesn't begin from WebDriver's mouse",
    ),
    (
        "a_download_a_service_worker_answers_is_its_pages",
        "downloads (phase 4)",
    ),
    (
        "a_download_nobody_holds_is_cancelled",
        "downloads (phase 4)",
    ),
    (
        "a_download_is_saved_in_its_contexts_own_folder",
        "downloads (phase 4)",
    ),
    (
        "a_download_from_a_frame_on_another_site_is_its_pages",
        "downloads (phase 4)",
    ),
    (
        "a_cancelled_or_broken_download_says_it_didnt_finish",
        "downloads (phase 4)",
    ),
    (
        "a_popup_from_a_frame_on_another_site_is_its_pages",
        "popups, told of by the page's events (phase 4)",
    ),
    (
        "choosing_on_an_opener_behind_its_popup_brings_it_back_first",
        "popups, told of by the page's events (phase 4)",
    ),
    (
        "waiting_on_an_opener_behind_its_popup_brings_it_back_first",
        "popups, told of by the page's events (phase 4)",
    ),
    (
        "a_blank_popup_a_script_writes_into_is_read",
        "popups, told of by the page's events (phase 4)",
    ),
    (
        "a_page_that_asks_before_it_unloads_is_left_and_closed",
        "dialogs (phase 3, task 6)",
    ),
    (
        "a_dialog_from_a_frame_on_another_site_is_heard_and_answered",
        "dialogs (phase 3, task 6)",
    ),
    (
        "a_dialog_nobody_answers_is_dismissed_as_a_person_would",
        "dialogs (phase 3, task 6)",
    ),
    (
        "a_failing_step_continues_stops_or_fails_as_it_says",
        "steps (phase 3, task 8)",
    ),
    (
        "a_guard_sees_every_hop_and_can_stop_one_before_it_is_fetched",
        "network control (phase 4)",
    ),
    (
        "a_listener_answers_a_dialog_and_reads_what_it_says",
        "dialogs (phase 3, task 6)",
    ),
    (
        "a_page_saves_as_a_pdf_of_the_size_and_pages_asked_for",
        "screenshots and PDF (phase 3, task 7)",
    ),
    (
        "a_popup_from_a_link_is_read_and_carries_its_openers_rules",
        "network control (phase 4)",
    ),
    (
        "a_popup_of_a_guarded_page_is_guarded_from_its_first_request",
        "network control (phase 4)",
    ),
    (
        "a_popup_of_a_shared_context_is_routed_by_the_context",
        "network control (phase 4)",
    ),
    (
        "a_popup_on_another_site_keeps_its_openers_route",
        "network control (phase 4)",
    ),
    (
        "a_trace_records_what_the_page_did_and_can_be_taken_again",
        "CDP-format tracing, which Firefox has none of",
    ),
    (
        "control_or_meta_a_selects_the_field_on_each_system_and_backspace_empties_it",
        "environments (phase 4)",
    ),
    (
        "double_clicks_xpath_and_two_waits_for_one_url",
        "steps (phase 3, task 8)",
    ),
    (
        "pages_keep_their_cookies_to_their_own_context",
        "network control (phase 4)",
    ),
    (
        "screenshots_are_png",
        "screenshots and PDF (phase 3, task 7)",
    ),
    (
        "steps_answer_dialogs_act_in_frames_and_run_calls",
        "steps (phase 3, task 8)",
    ),
    (
        "steps_click_type_and_wait_with_the_page_own_actions",
        "steps (phase 3, task 8)",
    ),
    (
        "the_steps_stop_at_their_cap_whatever_their_on_error",
        "steps (phase 3, task 8)",
    ),
    (
        "undo_and_redo_take_back_typing_on_each_system",
        "environments (phase 4)",
    ),
    (
        "word_and_line_shortcuts_edit_the_field_as_each_system_does",
        "environments (phase 4)",
    ),
];

/// Why the running test can't pass on Firefox yet, if it can't. libtest
/// names each test's thread after the test.
fn firefox_skip() -> Option<&'static str> {
    let thread = std::thread::current();
    let name = thread.name()?;
    FIREFOX_SKIPS
        .iter()
        .find(|(test, _)| *test == name)
        .map(|(_, why)| *why)
}

pub async fn browser() -> Option<TestBrowser> {
    if engine() == netweir_browser::Engine::Firefox {
        if let Some(why) = firefox_skip() {
            eprintln!("skipped on Firefox: {why}");
            return None;
        }
        // As Chrome's below, rather than launch_firefox's longer timeout,
        // which is for launching alone: tests rely on the browser's.
        let turn = turn();
        let browser = netweir_browser::Browser::launch(netweir_browser::LaunchOptions {
            engine: netweir_browser::Engine::Firefox,
            executable: Some(firefox()?),
            timeout: std::time::Duration::from_secs(30),
            ..Default::default()
        })
        .await
        .unwrap();
        return Some(TestBrowser {
            browser,
            _turn: turn,
        });
    }
    let executable = chrome()?;
    Some(
        launch(netweir_browser::LaunchOptions {
            executable: Some(executable),
            // Generous: a small CI runner can take many seconds over a
            // page. Tests that expect a timeout set a short one themselves.
            timeout: std::time::Duration::from_secs(30),
            ..Default::default()
        })
        .await,
    )
}

/// An HTTP proxy that wants `user:secret` and answers every request itself
/// with a page naming the URL it was asked for.
pub struct Proxy {
    pub url: String,
    _listening: Listening,
}

pub fn proxy() -> Proxy {
    let listening = listen(|mut stream| {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let target = line.split(' ').nth(1).unwrap_or("").to_string();
            let mut authorised = false;
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).is_err() || h.trim().is_empty() {
                    break;
                }
                // "user:secret", base64-encoded.
                if h.to_ascii_lowercase().starts_with("proxy-authorization:")
                    && h.contains("dXNlcjpzZWNyZXQ=")
                {
                    authorised = true;
                }
            }
            let reply = if authorised {
                // Scripts, for the workers a page starts.
                let (kind, body) = if target.ends_with(".js") {
                    (
                        "application/javascript",
                        "postMessage('worker ran')".to_string(),
                    )
                } else {
                    ("text/html", format!("<p id=via>via proxy: {target}</p>"))
                };
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
            } else {
                "HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"test\"\r\nContent-Length: 0\r\n\r\n".to_string()
            };
            if stream.write_all(reply.as_bytes()).is_err() {
                return;
            }
        }
    });
    Proxy {
        url: format!("http://127.0.0.1:{}", listening.port),
        _listening: listening,
    }
}
