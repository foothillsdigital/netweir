//! Launching Firefox: the stealth start of a Firefox `netweir install
//! firefox` installed, automation mode when asked for, and the failures in
//! between.

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use netweir_browser::{Browser, Engine, Error, LaunchOptions};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("netweir-fft-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn firefox_options() -> LaunchOptions {
    LaunchOptions {
        engine: Engine::Firefox,
        timeout: Duration::from_secs(60),
        ..LaunchOptions::default()
    }
}

/// Whether anything of the browser started as `pid` with `profile` still
/// runs: its process group on macOS and Linux (helpers and watcher too);
/// on Windows, where the process started can be a launcher that hands
/// over to another, the process and anything with the profile on its
/// command line.
fn running(pid: u32, profile: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        let _ = profile;
        unsafe { libc::kill(-(pid as libc::pid_t), 0) == 0 }
    }
    #[cfg(windows)]
    {
        let query = format!(
            "@(Get-CimInstance Win32_Process | Where-Object {{ $_.ProcessId -ne $PID -and \
             ($_.ProcessId -eq {pid} -or ($_.CommandLine -and $_.CommandLine.Contains('{}'))) }}).Count",
            profile.display()
        );
        let out = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &query])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim() != "0"
    }
}

fn gone_within(pid: u32, profile: &std::path::Path, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while running(pid, profile) {
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    true
}

#[tokio::test]
async fn firefox_launches_and_says_what_it_is() {
    let Some(executable) = common::firefox() else {
        return;
    };
    let browser = common::launch_firefox(executable, LaunchOptions::default()).await;
    let info = browser.engine_info();
    assert_eq!(info.engine, Engine::Firefox);
    assert!(info.stealth);
    let major: u32 = info.version.split('.').next().unwrap().parse().unwrap();
    assert!(major >= 159, "{info:?}");
    assert_eq!(browser.version(), format!("Firefox/{}", info.version));
    let pid = browser.pid().unwrap();
    let profile = browser.profile_dir().unwrap().to_path_buf();
    browser.close().await.unwrap();
    assert!(gone_within(pid, &profile, Duration::from_secs(15)));
    assert!(!profile.exists(), "a profile Netweir made goes with it");
}

/// A Firefox that exits at once fails the launch at once, saying what it
/// wrote, rather than at the timeout.
#[cfg(unix)]
#[tokio::test]
async fn a_firefox_that_exits_fails_the_launch_at_once() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("exits");
    let fake = dir.join("firefox");
    std::fs::write(&fake, "#!/bin/sh\necho boom >&2\nexit 1\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let started = Instant::now();
    let err = Browser::launch(LaunchOptions {
        executable: Some(fake),
        stealth: false,
        timeout: Duration::from_secs(60),
        ..firefox_options()
    })
    .await
    .err()
    .unwrap();
    // Well before the timeout, with room for a busy machine.
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    let Error::Launch(message) = &err else {
        panic!("{err:?}")
    };
    assert!(message.contains("exited"), "{message}");
    assert!(message.contains("boom"), "{message}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_firefox_netweir_didnt_install_needs_stealth_false() {
    let dir = scratch("theirs-exe");
    let theirs = dir.join(if cfg!(windows) {
        "firefox.exe"
    } else {
        "firefox"
    });
    std::fs::write(&theirs, "").unwrap();
    let err = Browser::launch(LaunchOptions {
        executable: Some(theirs.clone()),
        ..firefox_options()
    })
    .await
    .err()
    .unwrap();
    let message = err.to_string();
    assert!(message.contains("netweir install firefox"), "{message}");
    assert!(message.contains("stealth: false"), "{message}");
    // Nothing was written beside it.
    assert!(!dir.join("mozilla.cfg").exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn automation_mode_is_only_by_asking() {
    let Some(executable) = common::firefox() else {
        return;
    };
    let browser = common::launch_firefox(
        executable,
        LaunchOptions {
            stealth: false,
            ..LaunchOptions::default()
        },
    )
    .await;
    assert!(!browser.engine_info().stealth);
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_profile_netweir_didnt_make_is_refused() {
    let dir = scratch("theirs-profile");
    std::fs::write(dir.join("prefs.js"), "// a person's\n").unwrap();
    for stealth in [true, false] {
        let err = Browser::launch(LaunchOptions {
            user_data_dir: Some(dir.clone()),
            stealth,
            executable: common::firefox(),
            ..firefox_options()
        })
        .await
        .err()
        .unwrap();
        assert!(err.to_string().contains("didn't make"), "{err}");
    }
    assert_eq!(
        std::fs::read_to_string(dir.join("prefs.js")).unwrap(),
        "// a person's\n"
    );
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn a_stale_port_file_is_not_read() {
    let Some(executable) = common::firefox() else {
        return;
    };
    let dir = scratch("stale");
    let first = common::launch_firefox(
        executable.clone(),
        LaunchOptions {
            user_data_dir: Some(dir.clone()),
            ..LaunchOptions::default()
        },
    )
    .await;
    first.close().await.unwrap();
    drop(first);
    // A port nothing listens on, as a crashed launch would leave it.
    std::fs::write(dir.join("netweir-port"), "1").unwrap();
    std::fs::write(
        dir.join("netweir-port.failed"),
        "NETWEIR-FIREFOX failed: old",
    )
    .unwrap();
    let second = common::launch_firefox(
        executable,
        LaunchOptions {
            user_data_dir: Some(dir.clone()),
            ..LaunchOptions::default()
        },
    )
    .await;
    assert!(second.engine_info().stealth);
    second.close().await.unwrap();
    assert!(dir.exists(), "a kept profile stays");
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_firefoxes_run_at_once() {
    let Some(executable) = common::firefox() else {
        return;
    };
    let _turn = common::turns_for(2);
    let launch = || {
        Browser::launch(LaunchOptions {
            executable: Some(executable.clone()),
            ..firefox_options()
        })
    };
    let (a, b) = tokio::join!(launch(), launch());
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_ne!(a.pid(), b.pid());
    for browser in [&a, &b] {
        assert!(browser.engine_info().stealth);
        browser.close().await.unwrap();
    }
}

#[test]
fn a_dropped_firefox_ends_its_processes() {
    let Some(executable) = common::firefox() else {
        return;
    };
    let _turn = common::turn();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let browser = runtime
        .block_on(Browser::launch(LaunchOptions {
            executable: Some(executable),
            ..firefox_options()
        }))
        .unwrap();
    let pid = browser.pid().unwrap();
    let profile = browser.profile_dir().unwrap().to_path_buf();
    drop(browser);
    drop(runtime);
    // The watcher asks Firefox to quit and gives it ten seconds.
    assert!(
        gone_within(pid, &profile, Duration::from_secs(20)),
        "Firefox {pid} still runs"
    );
}

async fn firefox_browser() -> Option<common::TestBrowser> {
    let executable = common::firefox()?;
    Some(common::launch_firefox(executable, LaunchOptions::default()).await)
}

#[tokio::test]
async fn firefox_page_navigates_and_evaluates() {
    let Some(browser) = firefox_browser().await else {
        return;
    };
    let server = common::serve(vec![(
        "/",
        common::html(
            "<!DOCTYPE html><html><head><title>hi</title></head><body><p>x</p></body></html>",
        ),
    )]);
    let page = browser.new_page().await.unwrap();
    let url = format!("{}/", server.url);
    let response = page
        .goto(&url, netweir_browser::WaitUntil::Load, None)
        .await
        .unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.url, url);
    assert!(
        response
            .headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("content-type") && v.starts_with("text/html")),
        "{:?}",
        response.headers
    );
    assert_eq!(page.response(), response);
    assert_eq!(page.url(), url);
    assert_eq!(page.title().await.unwrap(), "hi");
    assert_eq!(
        page.content().await.unwrap(),
        "<!DOCTYPE html><html><head><title>hi</title></head><body><p>x</p></body></html>"
    );
    assert_eq!(
        page.evaluate("document.querySelector('p').textContent")
            .await
            .unwrap(),
        "x"
    );
    // In the page's own world, as on Chrome.
    page.evaluate("window.mark = 7").await.unwrap();
    assert_eq!(page.evaluate("window.mark").await.unwrap(), 7);
    assert_eq!(page.evaluate("navigator.webdriver").await.unwrap(), false);
    page.goto(&url, netweir_browser::WaitUntil::DomContentLoaded, None)
        .await
        .unwrap();
    page.close().await.unwrap();
    assert!(page.is_closed());
    assert!(matches!(page.evaluate("1").await, Err(Error::PageClosed)));
}

/// What evaluate gives on Firefox is what it gives on Chrome.
#[tokio::test]
async fn evaluate_gives_what_chrome_gives() {
    let (Some(firefox), Some(chrome)) = (firefox_browser().await, common::browser().await) else {
        return;
    };
    let (f, c) = (
        firefox.new_page().await.unwrap(),
        chrome.new_page().await.unwrap(),
    );
    for js in [
        "1",
        "'a'",
        "null",
        "undefined",
        "true",
        "[1, {a: 2}]",
        "NaN",
        "-0",
        "Infinity",
        "10n",
        "new Date(0)",
        "({f() {}, n: 1, u: undefined})",
        "[() => 1, undefined, 2]",
        "Promise.resolve(3)",
        "() => 4",
        "async () => 5",
        "new Map([[1, 2]])",
        "({nested: {deeper: [1, 'two', {three: 3}]}})",
    ] {
        let (on_firefox, on_chrome) = (f.evaluate(js).await, c.evaluate(js).await);
        assert_eq!(
            on_firefox.as_ref().unwrap(),
            on_chrome.as_ref().unwrap(),
            "{js}"
        );
    }
    for js in [
        "(() => { throw new Error('no') })()",
        "Promise.reject(new Error('no'))",
    ] {
        for (engine, page) in [("Firefox", &f), ("Chrome", &c)] {
            let err = page.evaluate(js).await.unwrap_err();
            assert!(
                matches!(&err, Error::Script(m) if m.contains("no")),
                "{engine}: {js}: {err:?}"
            );
        }
    }
}

#[tokio::test]
async fn pages_and_contexts_keep_their_cookies_apart() {
    let Some(browser) = firefox_browser().await else {
        return;
    };
    let server = common::serve(vec![("/", common::html("<p>cookies</p>"))]);
    let url = format!("{}/", server.url);
    let load = netweir_browser::WaitUntil::Load;
    let set = "document.cookie = 'k=v; path=/'";
    let read = "document.cookie";
    let (a, b) = (
        browser.new_page().await.unwrap(),
        browser.new_page().await.unwrap(),
    );
    a.goto(&url, load, None).await.unwrap();
    b.goto(&url, load, None).await.unwrap();
    a.evaluate(set).await.unwrap();
    assert_eq!(a.evaluate(read).await.unwrap(), "k=v");
    assert_eq!(
        b.evaluate(read).await.unwrap(),
        "",
        "pages of their own share nothing"
    );

    let context = browser.new_context().await.unwrap();
    let (c1, c2) = (
        context.new_page().await.unwrap(),
        context.new_page().await.unwrap(),
    );
    c1.goto(&url, load, None).await.unwrap();
    c2.goto(&url, load, None).await.unwrap();
    c1.evaluate(set).await.unwrap();
    assert_eq!(
        c2.evaluate(read).await.unwrap(),
        "k=v",
        "a context's pages share"
    );
    context.close().await.unwrap();
    // Its pages went with it.
    assert!(c1.evaluate("1").await.is_err());
}

#[tokio::test]
async fn every_firefox_page_is_visible() {
    let Some(browser) = firefox_browser().await else {
        return;
    };
    let (a, b) = (
        browser.new_page().await.unwrap(),
        browser.new_page().await.unwrap(),
    );
    let context = browser.new_context().await.unwrap();
    let c = context.new_page().await.unwrap();
    for page in [&a, &b, &c] {
        assert_eq!(
            page.evaluate("document.visibilityState").await.unwrap(),
            "visible"
        );
    }
}

#[tokio::test]
async fn a_kept_profile_keeps_firefox_s_cookies() {
    let Some(executable) = common::firefox() else {
        return;
    };
    let dir = scratch("kept");
    let server = common::serve(vec![("/", common::html("<p>kept</p>"))]);
    let url = format!("{}/", server.url);
    let load = netweir_browser::WaitUntil::Load;
    for round in 0..2 {
        let browser = common::launch_firefox(
            executable.clone(),
            LaunchOptions {
                user_data_dir: Some(dir.clone()),
                ..LaunchOptions::default()
            },
        )
        .await;
        let page = browser.new_page().await.unwrap();
        page.goto(&url, load, None).await.unwrap();
        if round == 0 {
            page.evaluate("document.cookie = 'kept=yes; path=/; max-age=3600'")
                .await
                .unwrap();
        } else {
            assert_eq!(page.evaluate("document.cookie").await.unwrap(), "kept=yes");
        }
        browser.close().await.unwrap();
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn unported_methods_are_unsupported_and_send_nothing() {
    let Some(browser) = firefox_browser().await else {
        return;
    };
    let page = browser.new_page().await.unwrap();
    let before = browser.methods_sent();
    let unsupported = |r: Result<(), Error>, what: &str| match r {
        Err(Error::Unsupported {
            engine: "Firefox",
            feature,
        }) => assert!(feature.contains(what), "{feature}"),
        other => panic!("{what}: {other:?}"),
    };
    unsupported(
        page.emulate_media(&netweir_browser::MediaChange::default())
            .await,
        "emulate_media",
    );
    unsupported(
        browser
            .new_page_with(netweir_browser::Environment {
                reduced_motion: Some(netweir_browser::ReducedMotion::Reduce),
                ..Default::default()
            })
            .await
            .map(drop),
        "reduced motion",
    );
    unsupported(browser.start_tracing(None, false).await, "start_tracing");
    let sent = browser.methods_sent();
    assert_eq!(sent, before, "nothing was sent for them");
    assert!(
        sent.iter()
            .all(|m| m.starts_with(|c: char| c.is_ascii_lowercase())),
        "{sent:?}"
    );
}

#[tokio::test]
async fn goto_a_dead_host_is_a_navigation_error() {
    let Some(browser) = firefox_browser().await else {
        return;
    };
    let page = browser.new_page().await.unwrap();
    let err = page
        .goto(
            "http://127.0.0.1:1/",
            netweir_browser::WaitUntil::Load,
            None,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Navigation { url, error, .. } if url == "http://127.0.0.1:1/" && !error.is_empty()),
        "{err:?}"
    );
}

/// A Firefox dropped with a kept profile quits in its own time; a launch
/// with that profile straight after waits for it rather than finding the
/// profile locked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kept_profile_is_free_again_after_a_drop() {
    let Some(executable) = common::firefox() else {
        return;
    };
    let dir = scratch("dropped-kept");
    let options = LaunchOptions {
        executable: Some(executable),
        user_data_dir: Some(dir.clone()),
        ..firefox_options()
    };
    let _turn = common::turn();
    drop(Browser::launch(options.clone()).await.unwrap());
    let again = Browser::launch(options).await.unwrap();
    assert!(again.engine_info().stealth);
    again.close().await.unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn connecting_to_firefox_is_unsupported() {
    let err = Browser::connect("ws://127.0.0.1:1/session", firefox_options())
        .await
        .err()
        .unwrap();
    assert!(
        matches!(&err, Error::Unsupported { engine: "Firefox", feature } if feature == "Browser.connect"),
        "{err:?}"
    );
}

/// Answers every request after `delay`, from any origin.
fn slow(delay: Duration) -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            std::thread::spawn(move || {
                let mut buf = [0; 4096];
                let _ = stream.read(&mut buf);
                std::thread::sleep(delay);
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                );
            });
        }
    });
    url
}

#[tokio::test]
async fn firefox_waits_for_network_idle() {
    let Some(browser) = firefox_browser().await else {
        return;
    };
    let slow = slow(Duration::from_secs(1));
    let page_html = format!(
        "<script>fetch('{slow}').then(r => r.text()).then(t => {{ window.done = t }})</script>"
    );
    let server = common::serve(vec![(
        "/",
        common::html(Box::leak(page_html.into_boxed_str())),
    )]);
    let page = browser.new_page().await.unwrap();
    let started = Instant::now();
    // Idle ends the wait: a page that never went idle would time out.
    page.goto(
        &server.url,
        netweir_browser::WaitUntil::NetworkIdle,
        Some(Duration::from_secs(15)),
    )
    .await
    .unwrap();
    let took = started.elapsed();
    assert_eq!(page.evaluate("window.done").await.unwrap(), "ok");
    assert!(took >= Duration::from_secs(1), "{took:?}");
}

#[tokio::test]
async fn back_forward_and_reload_on_firefox() {
    let Some(browser) = firefox_browser().await else {
        return;
    };
    let server = common::serve(vec![
        ("/a", common::html("<p>a</p>")),
        ("/b", common::html("<p>b</p>")),
    ]);
    let (a, b) = (format!("{}/a", server.url), format!("{}/b", server.url));
    let page = browser.new_page().await.unwrap();
    let load = netweir_browser::WaitUntil::Load;
    page.goto(&a, load, None).await.unwrap();
    page.goto(&b, load, None).await.unwrap();
    let back = page.go_back(load, None).await.unwrap().unwrap();
    assert_eq!((back.url.as_str(), back.status), (a.as_str(), 200));
    assert_eq!(page.url(), a);
    let forward = page.go_forward(load, None).await.unwrap().unwrap();
    assert_eq!(forward.url, b);
    // A #fragment of the document showing moves within it, loading nothing.
    let part = page
        .goto(&format!("{b}#part"), load, Some(Duration::from_secs(5)))
        .await
        .unwrap();
    assert_eq!(part.url, b, "the document's own response");
    assert_eq!(page.url(), format!("{b}#part"));
    // Firefox asks for the site's icon now and then too.
    let fetches = || server.paths().iter().filter(|p| *p == "/b").count();
    let fetched = fetches();
    let reloaded = page.reload(load, None).await.unwrap();
    assert_eq!(reloaded.status, 200);
    assert_eq!(fetches(), fetched + 1, "{:?}", server.paths());
    assert!(page.go_forward(load, None).await.unwrap().is_none());
    let within = page.go_back(load, None).await.unwrap().unwrap();
    assert_eq!((within.url.as_str(), within.status), (b.as_str(), 0));
    page.go_back(load, None).await.unwrap();
    assert_eq!(page.url(), a);
    // Firefox keeps no entry for the blank page a window opens on.
    assert!(page.go_back(load, None).await.unwrap().is_none());
}

#[tokio::test]
async fn back_to_a_cached_page_returns() {
    let Some(browser) = firefox_browser().await else {
        return;
    };
    let server = common::serve(vec![
        ("/a", common::html("<p>a</p>")),
        ("/b", common::html("<p>b</p>")),
    ]);
    let (a, b) = (format!("{}/a", server.url), format!("{}/b", server.url));
    let page = browser.new_page().await.unwrap();
    let load = netweir_browser::WaitUntil::Load;
    page.goto(&a, load, None).await.unwrap();
    page.evaluate("window.kept = 1").await.unwrap();
    page.goto(&b, load, None).await.unwrap();
    let back = page
        .go_back(load, Some(Duration::from_secs(5)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((back.url.as_str(), back.status), (a.as_str(), 200));
    assert_eq!(
        page.evaluate("window.kept").await.unwrap(),
        1,
        "came from the cache"
    );
}

#[tokio::test]
async fn wait_for_navigation_sees_a_script_redirect() {
    let Some(browser) = firefox_browser().await else {
        return;
    };
    let server = common::serve(vec![
        (
            "/",
            common::html("<script>setTimeout(() => location = '/next', 300)</script>"),
        ),
        ("/next", common::html("<p>next</p>")),
    ]);
    let page = browser.new_page().await.unwrap();
    let load = netweir_browser::WaitUntil::Load;
    let first = page.goto(&server.url, load, None).await.unwrap();
    let next = page.wait_for_navigation(&first, load, None).await.unwrap();
    assert_eq!(next.url, format!("{}/next", server.url));
    assert_eq!(next.status, 200);
}

#[tokio::test]
async fn firefox_helper_survives_its_document_going() {
    let Some(browser) = firefox_browser().await else {
        return;
    };
    let server = common::serve(vec![
        (
            "/",
            common::html("<script>setTimeout(() => location = '/next', 200)</script>"),
        ),
        (
            "/next",
            common::html(
                "<script>setTimeout(() => document.body.innerHTML = '<p id=late>x</p>', 200)</script>",
            ),
        ),
    ]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, netweir_browser::WaitUntil::Load, None)
        .await
        .unwrap();
    // The wait began in the first document, and its element is in the
    // second.
    page.wait_for("#late", "visible", Some(Duration::from_secs(5)))
        .await
        .unwrap();
    assert_eq!(page.url(), format!("{}/next", server.url));
}

#[tokio::test]
async fn a_firefox_frame_has_its_name_and_its_own_document() {
    let Some(browser) = firefox_browser().await else {
        return;
    };
    let other = common::serve(vec![(
        "/inner",
        common::html("<title>inner</title><p id=in>in</p>"),
    )]);
    let page_html = format!("<iframe name=box src='{}/inner'></iframe>", other.url);
    let server = common::serve(vec![(
        "/",
        common::html(Box::leak(page_html.into_boxed_str())),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, netweir_browser::WaitUntil::Load, None)
        .await
        .unwrap();
    let frames = page.frames().await.unwrap();
    assert_eq!(frames.len(), 2);
    let inner = &frames[1];
    assert_eq!(inner.name(), "box");
    assert_eq!(inner.parent_id(), Some(frames[0].id()));
    assert_eq!(inner.title().await.unwrap(), "inner");
    assert_eq!(
        inner
            .evaluate("document.getElementById('in').textContent")
            .await
            .unwrap(),
        "in"
    );
    page.evaluate("document.querySelector('iframe').remove()")
        .await
        .unwrap();
    assert!(
        matches!(inner.title().await, Err(Error::Gone(_))),
        "removed"
    );
}

#[tokio::test]
async fn an_html5_drag_is_unsupported_and_presses_nothing() {
    let Some(browser) = firefox_browser().await else {
        return;
    };
    let server = common::serve(vec![(
        "/",
        common::html(
            "<div id=item draggable=true style='width:60px;height:30px'>item</div>\
             <div id=handle style='width:60px;height:30px'>handle</div>\
             <div id=bin style='width:200px;height:80px'>bin</div>\
             <script>window.downs = 0; addEventListener('mousedown', () => downs++)</script>",
        ),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, netweir_browser::WaitUntil::Load, None)
        .await
        .unwrap();
    let r = page.drag("#item", "#bin", None).await;
    assert!(
        matches!(&r, Err(Error::Unsupported { feature, .. }) if feature.contains("HTML5")),
        "{r:?}"
    );
    assert_eq!(page.evaluate("downs").await.unwrap(), 0);
    // What only follows the mouse is dragged as on Chrome.
    page.drag("#handle", "#bin", None).await.unwrap();
    assert_eq!(page.evaluate("downs").await.unwrap(), 1);
}

#[tokio::test]
async fn firefox_typed_text_is_trusted() {
    let Some(browser) = firefox_browser().await else {
        return;
    };
    let server = common::serve(vec![(
        "/",
        common::html(
            "<input id=t><script>window.seen = []; \
             t.addEventListener('input', e => seen.push([e.data, e.isTrusted]))</script>",
        ),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, netweir_browser::WaitUntil::Load, None)
        .await
        .unwrap();
    page.type_text("#t", "aé", (Duration::ZERO, Duration::from_millis(1)), None)
        .await
        .unwrap();
    assert_eq!(page.evaluate("t.value").await.unwrap(), "aé");
    assert_eq!(
        page.evaluate("seen").await.unwrap(),
        serde_json::json!([["a", true], ["é", true]])
    );
}

#[tokio::test]
async fn an_alert_from_a_click_doesnt_hang_the_page() {
    let Some(browser) = firefox_browser().await else {
        return;
    };
    let server = common::serve(vec![(
        "/",
        common::html("<button id=b onclick=\"alert('clicked'); window.after = 1\">b</button>"),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, netweir_browser::WaitUntil::Load, None)
        .await
        .unwrap();
    // Nobody listening: dismissed at once, as a person would.
    page.click("#b", Some(Duration::from_secs(10)))
        .await
        .unwrap();
    // Firefox runs a script while its alert is open, so the page is
    // waited on to carry on.
    let carried_on =
        "new Promise(r => { const t = () => window.after ? r(1) : setTimeout(t, 20); t(); })";
    let after = tokio::time::timeout(Duration::from_secs(10), page.evaluate(carried_on))
        .await
        .expect("the page waited on the alert");
    assert_eq!(after.unwrap(), 1);
    // Listening: the alert is handed over, and answering it lets the page
    // go on.
    let mut dialogs = page.dialogs();
    page.evaluate("window.after = 0").await.unwrap();
    page.click("#b", None).await.unwrap();
    let dialog = tokio::time::timeout(Duration::from_secs(10), dialogs.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!((dialog.kind(), dialog.message()), ("alert", "clicked"));
    dialog.accept(None).await.unwrap();
    assert_eq!(page.evaluate(carried_on).await.unwrap(), 1);
}

#[tokio::test]
async fn firefox_full_page_screenshot_is_the_page_s_size() {
    let Some(browser) = firefox_browser().await else {
        return;
    };
    let server = common::serve(vec![(
        "/",
        common::html("<style>body { margin: 0 }</style><div style='height: 3000px'></div>"),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, netweir_browser::WaitUntil::Load, None)
        .await
        .unwrap();
    let ratio = page
        .evaluate("devicePixelRatio")
        .await
        .unwrap()
        .as_f64()
        .unwrap();
    // A PNG's height is in its header, after the width.
    let height = |png: &[u8]| u32::from_be_bytes(png[20..24].try_into().unwrap());
    let whole = page.screenshot(true).await.unwrap();
    assert_eq!(f64::from(height(&whole)), 3000.0 * ratio);
    let view = page.screenshot(false).await.unwrap();
    let inner = page
        .evaluate("innerHeight")
        .await
        .unwrap()
        .as_f64()
        .unwrap();
    assert_eq!(f64::from(height(&view)), inner * ratio);
    let jpeg = page.capture(false, Some(50)).await.unwrap();
    assert_eq!(&jpeg[..2], [0xFF, 0xD8], "a JPEG");
}

/// A context's init script runs before a Firefox page's own scripts, even
/// where the page's CSP forbids eval; its extra headers go with each
/// request; offline takes the page off the network and puts it back.
#[tokio::test]
async fn a_firefox_page_has_its_init_scripts_headers_and_offline() {
    let Some(browser) = firefox_browser().await else {
        return;
    };
    let mut strict = common::html("<script src=/own.js></script>");
    strict
        .headers
        .push(("Content-Security-Policy", "script-src 'self'".into()));
    let server = common::serve(vec![
        ("/", strict),
        ("/own.js", common::html("var own = typeof early;")),
        ("/ping", common::html("pong")),
    ]);
    let context = browser.new_context().await.unwrap();
    context
        .add_init_script("var early = document.readyState;")
        .await
        .unwrap();
    context
        .set_extra_headers(&[("X-Netweir-Test".into(), "yes".into())])
        .await
        .unwrap();
    let page = context.new_page().await.unwrap();
    let load = netweir_browser::WaitUntil::Load;
    page.goto(&format!("{}/", server.url), load, None)
        .await
        .unwrap();
    assert_eq!(
        page.evaluate("[early, own]").await.unwrap(),
        serde_json::json!(["loading", "string"])
    );
    let sent = server.headers_for("/");
    assert!(
        sent.iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("x-netweir-test") && v == "yes"),
        "{sent:?}"
    );
    let ping =
        "fetch('/ping').then(() => [navigator.onLine, 'ok'], () => [navigator.onLine, 'failed'])";
    page.set_offline(true).await.unwrap();
    assert_eq!(
        page.evaluate(ping).await.unwrap(),
        serde_json::json!([false, "failed"])
    );
    page.set_offline(false).await.unwrap();
    assert_eq!(
        page.evaluate(ping).await.unwrap(),
        serde_json::json!([true, "ok"])
    );
    browser.close().await.unwrap();
}

/// A Firefox page given an environment has it from its first document:
/// locale, time zone, user agent (in the page and its requests), screen,
/// viewport, pixel ratio and where it is; a permission granted to an
/// origin is the page's until it's taken back. What Firefox can't give
/// says so.
#[tokio::test]
async fn a_firefox_page_has_the_environment_it_was_given() {
    let Some(browser) = firefox_browser().await else {
        return;
    };
    let server = common::serve(vec![("/", common::html("<p>here</p>"))]);
    let env = netweir_browser::Environment {
        locale: Some("de-DE".into()),
        timezone: Some("Europe/Berlin".into()),
        user_agent: Some(
            "Mozilla/5.0 (X11; Linux x86_64; rv:159.0) Gecko/20100101 Firefox/159.0".into(),
        ),
        screen: Some(netweir_browser::Size {
            width: 1440,
            height: 900,
        }),
        viewport: Some(netweir_browser::Size {
            width: 800,
            height: 600,
        }),
        device_scale_factor: Some(2.0),
        geolocation: Some(netweir_browser::Geolocation {
            latitude: 52.52,
            longitude: 13.4,
            accuracy: 10.0,
        }),
        ..Default::default()
    };
    let context = browser.new_context_with(env).await.unwrap();
    let page = context.new_page().await.unwrap();
    page.goto(
        &format!("{}/", server.url),
        netweir_browser::WaitUntil::Load,
        None,
    )
    .await
    .unwrap();
    let seen = page
        .evaluate(
            "[navigator.language, Intl.DateTimeFormat().resolvedOptions().timeZone, \
              navigator.userAgent.includes('Linux'), screen.width, screen.height, \
              innerWidth, innerHeight, devicePixelRatio]",
        )
        .await
        .unwrap();
    assert_eq!(
        seen,
        serde_json::json!(["de-DE", "Europe/Berlin", true, 1440, 900, 800, 600, 2])
    );
    let sent = server.headers_for("/");
    let header = |name: &str| {
        sent.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    assert!(header("user-agent").contains("Linux"), "{sent:?}");

    // A permission the system has no say in, granted and taken back.
    let origin = server.url.trim_end_matches('/').to_string();
    let state = |name: &str| {
        format!("navigator.permissions.query({{name: '{name}'}}).then((p) => p.state)")
    };
    context
        .grant_permissions(&["persistent-storage".into()], Some(&origin))
        .await
        .unwrap();
    assert_eq!(
        page.evaluate(&state("persistent-storage")).await.unwrap(),
        serde_json::json!("granted")
    );
    context.clear_permissions().await.unwrap();
    assert_eq!(
        page.evaluate(&state("persistent-storage")).await.unwrap(),
        serde_json::json!("prompt")
    );
    // Where it is: Firefox asks the system too, and a machine with
    // location turned off (a CI runner's) leaves geolocation at prompt
    // whatever is granted, so the place is read only where it can be.
    context
        .grant_permissions(&["geolocation".into()], Some(&origin))
        .await
        .unwrap();
    if page.evaluate(&state("geolocation")).await.unwrap() == serde_json::json!("granted") {
        // With a time limit of the page's own: a position that never came
        // held the whole run up on macOS, and geolocation's own timeout
        // only starts once the system has answered.
        let at = tokio::time::timeout(
            Duration::from_secs(20),
            page.evaluate(
                "new Promise((ok, no) => { \
                   setTimeout(() => no('no position in 10 seconds'), 10000); \
                   navigator.geolocation.getCurrentPosition( \
                     (p) => ok([p.coords.latitude, p.coords.longitude]), \
                     (e) => no(e.code + ' ' + e.message), {timeout: 10000}); })",
            ),
        )
        .await
        .expect("the page never answered for its position")
        .unwrap();
        assert_eq!(at, serde_json::json!([52.52, 13.4]));
    }
    context.clear_permissions().await.unwrap();

    for (env, what) in [
        (
            netweir_browser::Environment {
                color_scheme: Some(netweir_browser::ColorScheme::Dark),
                ..Default::default()
            },
            "colour scheme",
        ),
        (
            netweir_browser::Environment {
                forced_colors: Some(netweir_browser::ForcedColors::Active),
                ..Default::default()
            },
            "forced colours",
        ),
    ] {
        match browser.new_page_with(env).await {
            Err(netweir_browser::Error::Unsupported { feature, .. }) => {
                assert!(feature.contains(what), "{feature}")
            }
            other => panic!("{what}: {:?}", other.map(drop)),
        }
    }
    assert!(
        context
            .grant_permissions(&["geolocation".into()], None)
            .await
            .is_err()
    );
    browser.close().await.unwrap();
}

/// A double click on Firefox is one: the page hears dblclick, by the time
/// the click returns. Firefox counts clicks only within one chain of
/// actions, so two clicks sent apart are two single clicks.
#[tokio::test]
async fn a_firefox_double_click_is_heard_as_one() {
    let Some(browser) = firefox_browser().await else {
        return;
    };
    let server = common::serve(vec![(
        "/",
        common::html(
            "<!DOCTYPE html><p id=cell ondblclick=\"this.textContent = 'edited' + (++window.n)\" \
             onclick=\"window.clicks += 1\">cell</p><script>window.n = 0; window.clicks = 0</script>",
        ),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, netweir_browser::WaitUntil::Load, None)
        .await
        .unwrap();
    for n in 1..=3 {
        page.dblclick("#cell", None).await.unwrap();
        assert_eq!(
            page.evaluate("[document.querySelector('#cell').textContent, window.clicks]")
                .await
                .unwrap(),
            serde_json::json!([format!("edited{n}"), 2 * n])
        );
    }
    browser.close().await.unwrap();
}

/// What a popup's first document saw and sent, for `opener`'s
/// environment: Firefox gives a popup none of its opener's, so netweir
/// does, before the popup's first request goes.
async fn popup_sees(page: netweir_browser::Page) -> (serde_json::Value, Vec<(String, String)>) {
    let server = common::serve(vec![
        (
            "/",
            common::html("<a id=open href=/popup target=_blank>open</a>"),
        ),
        (
            "/popup",
            common::html(
                "<script>window.first = [navigator.userAgent, navigator.language, \
                 Intl.DateTimeFormat().resolvedOptions().timeZone, window.inited ?? null];</script>",
            ),
        ),
    ]);
    page.goto(&server.url, netweir_browser::WaitUntil::Load, None)
        .await
        .unwrap();
    let mut events = page.listen();
    page.click("#open", None).await.unwrap();
    let popup = loop {
        match tokio::time::timeout(Duration::from_secs(20), events.recv())
            .await
            .expect("no popup")
        {
            Some(netweir_browser::PageEvent::Popup(p)) => break p,
            Some(_) => {}
            None => panic!("the page went away"),
        }
    };
    popup
        .wait_for_load(netweir_browser::WaitUntil::Load, None)
        .await
        .unwrap();
    let first = popup.evaluate("first").await.unwrap();
    (first, server.headers_for("/popup"))
}

fn popup_env() -> netweir_browser::Environment {
    netweir_browser::Environment {
        locale: Some("de-DE".into()),
        timezone: Some("Asia/Tokyo".into()),
        user_agent: Some(
            "Mozilla/5.0 (X11; Linux x86_64; rv:159.0) Gecko/20100101 Firefox/159.0".into(),
        ),
        extra_headers: vec![("X-Netweir-Test".into(), "popup".into())],
        init_scripts: vec!["window.inited = 'yes'".into()],
        ..Default::default()
    }
}

fn assert_opener_env(first: serde_json::Value, headers: Vec<(String, String)>) {
    let env = popup_env();
    assert_eq!(
        first,
        serde_json::json!([env.user_agent, "de-DE", "Asia/Tokyo", "yes"])
    );
    let header = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    assert_eq!(header("user-agent"), env.user_agent);
    assert_eq!(header("accept-language").as_deref(), Some("de-DE"));
    assert_eq!(header("x-netweir-test").as_deref(), Some("popup"));
}

#[tokio::test]
async fn a_firefox_popup_has_its_openers_environment_from_its_first_request() {
    let Some(browser) = firefox_browser().await else {
        return;
    };
    let page = browser.new_page_with(popup_env()).await.unwrap();
    let (first, headers) = popup_sees(page).await;
    assert_opener_env(first, headers);
    browser.close().await.unwrap();
}

/// A context's pages share their user context, so the popup's first
/// request is held while it's given its opener's environment.
#[tokio::test]
async fn a_firefox_popup_in_a_shared_context_has_its_openers_environment_from_its_first_request() {
    let Some(browser) = firefox_browser().await else {
        return;
    };
    let context = browser.new_context_with(popup_env()).await.unwrap();
    let page = context.new_page().await.unwrap();
    let (first, headers) = popup_sees(page).await;
    assert_opener_env(first, headers);
    browser.close().await.unwrap();
}

/// A page's intercept is the whole browser's on Firefox: another page's
/// requests go on at once, and never reach the first page's routes.
#[tokio::test]
async fn a_firefox_pages_routes_see_only_its_own_requests() {
    let Some(browser) = firefox_browser().await else {
        return;
    };
    let server = common::serve(vec![
        (
            "/",
            common::html("<img src=/a.png><script>fetch('/api')</script>"),
        ),
        ("/api", common::html("api")),
    ]);
    let routed = browser.new_page().await.unwrap();
    let mut routes = routed.route().await.unwrap();
    let other = browser.new_page().await.unwrap();
    other
        .goto(&server.url, netweir_browser::WaitUntil::Load, None)
        .await
        .unwrap();
    // The script's fetch may only go out after the load, and later still
    // on a busy machine: waited for, rather than the network going quiet,
    // which it can seem to before Firefox has said the fetch began.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !server.paths().contains(&"/api".to_string()) {
        assert!(
            Instant::now() < deadline,
            "another page's request never went on: {:?}",
            server.paths()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        routes.try_recv().is_err(),
        "another page's request reached the routes"
    );
    browser.close().await.unwrap();
}

/// On Chrome a page's environment is its own: a change to the opener
/// reaches neither the popups it has open nor their pages, and a popup
/// opened later starts from the opener's as it is then, whoever opened
/// it. Firefox keeps one environment for the opener and its popups, so
/// netweir keeps them apart.
#[tokio::test]
async fn a_firefox_popups_environment_is_its_own_after_it_opens() {
    let Some(browser) = firefox_browser().await else {
        return;
    };
    let mut routes = vec![(
        "/",
        common::html("<a id=open href=/popup target=_blank>open</a>"),
    )];
    for path in [
        "/popup",
        "/opener-1",
        "/first-1",
        "/opener-2",
        "/first-2",
        "/second",
    ] {
        routes.push((
            path,
            common::html("<a id=open href=/second target=_blank>open</a>"),
        ));
    }
    let server = common::serve(routes);
    let header = |path: &str| {
        server
            .headers_for(path)
            .into_iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("x-netweir-test"))
            .map(|(_, v)| v)
    };
    let env = netweir_browser::Environment {
        extra_headers: vec![("X-Netweir-Test".into(), "opened".into())],
        ..Default::default()
    };
    let page = browser.new_page_with(env).await.unwrap();
    page.goto(&server.url, netweir_browser::WaitUntil::Load, None)
        .await
        .unwrap();
    let opened = |page: netweir_browser::Page| async move {
        let mut events = page.listen();
        page.click("#open", None).await.unwrap();
        loop {
            match tokio::time::timeout(Duration::from_secs(20), events.recv())
                .await
                .expect("no popup")
            {
                Some(netweir_browser::PageEvent::Popup(p)) => break p,
                Some(_) => {}
                None => panic!("the page went away"),
            }
        }
    };
    let go = |page: &netweir_browser::Page, path: &str| {
        let (page, url) = (page.clone(), format!("{}{path}", server.url));
        async move {
            page.goto(&url, netweir_browser::WaitUntil::Load, None)
                .await
                .unwrap();
        }
    };
    let first = opened(page.clone()).await;
    first
        .wait_for_load(netweir_browser::WaitUntil::Load, None)
        .await
        .unwrap();
    assert_eq!(header("/popup").as_deref(), Some("opened"));

    // Changed on the opener: not on its popup.
    page.set_extra_headers(&[("X-Netweir-Test".into(), "changed".into())])
        .await
        .unwrap();
    go(&page, "/opener-1").await;
    go(&first, "/first-1").await;
    assert_eq!(header("/opener-1").as_deref(), Some("changed"));
    assert_eq!(header("/first-1").as_deref(), Some("opened"));

    // Taken away on the opener: the popup keeps its own.
    page.set_extra_headers(&[]).await.unwrap();
    go(&page, "/opener-2").await;
    go(&first, "/first-2").await;
    assert_eq!(header("/opener-2"), None);
    assert_eq!(header("/first-2").as_deref(), Some("opened"));

    // A popup's popup starts from its opener's as it is now.
    first
        .set_extra_headers(&[("X-Netweir-Test".into(), "popup's".into())])
        .await
        .unwrap();
    let second = opened(first.clone()).await;
    second
        .wait_for_load(netweir_browser::WaitUntil::Load, None)
        .await
        .unwrap();
    assert_eq!(header("/second").as_deref(), Some("popup's"));
    browser.close().await.unwrap();
}
