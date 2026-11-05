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
        timeout: Duration::from_secs(30),
        ..firefox_options()
    })
    .await
    .err()
    .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(5),
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
    unsupported(page.route().await.map(drop), "route");
    unsupported(page.block(&["image"], &[]).await, "block");
    unsupported(
        browser
            .new_page_with(netweir_browser::Environment {
                locale: Some("de-DE".into()),
                ..Default::default()
            })
            .await
            .map(drop),
        "environment",
    );
    let context = browser.new_context().await.unwrap();
    unsupported(context.cookies().await.map(drop), "cookies");
    // A set: the context was made with a method new_page had sent.
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
    page.goto(&server.url, netweir_browser::WaitUntil::NetworkIdle, None)
        .await
        .unwrap();
    let took = started.elapsed();
    assert_eq!(page.evaluate("window.done").await.unwrap(), "ok");
    assert!(took < Duration::from_secs(3), "{took:?}");
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
