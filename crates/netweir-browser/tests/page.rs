mod common;

use std::time::Duration;

use common::{Reply, browser, html, serve};
use netweir_browser::{Error, PdfOptions, WaitUntil};
use serde_json::json;

const SHORT: Option<Duration> = Some(Duration::from_millis(800));

#[tokio::test]
async fn goto_reports_the_documents_status_headers_and_final_url() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![
        (
            "/",
            Reply {
                status: 200,
                headers: vec![
                    ("Content-Type", "text/html".into()),
                    ("X-Test", "yes".into()),
                ],
                body: "<title>home</title>".into(),
            },
        ),
        (
            "/moved",
            Reply {
                status: 302,
                headers: vec![("Location", "/".into())],
                body: String::new(),
            },
        ),
    ]);
    let page = browser.new_page().await.unwrap();
    let r = page
        .goto(&format!("{}/", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    assert_eq!(r.status, 200);
    assert!(
        r.headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case("x-test") && v == "yes"),
        "{:?}",
        r.headers
    );
    assert_eq!(page.title().await.unwrap(), "home");

    let r = page
        .goto(&format!("{}/missing", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    assert_eq!(r.status, 404, "an HTTP error is returned, not raised");

    let r = page
        .goto(
            &format!("{}/moved", server.url),
            WaitUntil::DomContentLoaded,
            None,
        )
        .await
        .unwrap();
    assert_eq!((r.status, r.url.clone()), (200, format!("{}/", server.url)));
    assert_eq!(page.url(), format!("{}/", server.url));
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_refused_connection_is_a_navigation_error() {
    let Some(browser) = browser().await else {
        return;
    };
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let page = browser.new_page().await.unwrap();
    let err = page
        .goto(&format!("http://127.0.0.1:{port}/"), WaitUntil::Load, None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Navigation { error: m, .. } if m.contains("ERR_CONNECTION_REFUSED")),
        "{err}"
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn content_is_the_page_after_its_scripts_ran() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![(
        "/",
        html(
            "<!DOCTYPE html><body><script>document.body.insertAdjacentHTML('beforeend', '<p id=made>by script</p>')</script></body>",
        ),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let html = page.content().await.unwrap();
    assert!(html.starts_with("<!DOCTYPE html>"), "{html}");
    assert!(html.contains(r#"<p id="made">by script</p>"#), "{html}");
    browser.close().await.unwrap();
}

#[tokio::test]
async fn evaluate_runs_in_the_page_and_its_helpers_stay_hidden() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html("<script>var pageVar = 41</script>"))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    assert_eq!(page.evaluate("pageVar + 1").await.unwrap(), json!(42));
    assert_eq!(
        page.evaluate("() => [1, 'a', {b: null}]").await.unwrap(),
        json!([1, "a", {"b": null}])
    );
    assert_eq!(
        page.evaluate("new Promise(r => setTimeout(() => r('later'), 10))")
            .await
            .unwrap(),
        json!("later")
    );
    assert_eq!(page.evaluate("undefined").await.unwrap(), json!(null));
    let err = page
        .evaluate("(() => { throw new Error('boom') })()")
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Script(m) if m.contains("boom")),
        "{err}"
    );
    // The helpers live in an isolated world.
    page.content().await.unwrap();
    assert_eq!(
        page.evaluate("typeof __netweir").await.unwrap(),
        json!("undefined")
    );
    browser.close().await.unwrap();
}

const ACTIONS: &str = r#"<!DOCTYPE html>
<style>
  button { width: 120px; height: 40px; }
  #wrap { position: relative; width: 120px; height: 40px; }
  #cover { position: absolute; left: 0; top: 0; width: 100%; height: 100%; }
</style>
<script>
  window.clicks = [];
  const later = (f) => setTimeout(f, 300);
  addEventListener('click', (e) => e.target.id && clicks.push(e.target.id));
</script>
<button id=shows style="display:none">a</button>
<button id=enables disabled>b</button>
<button id=never style="visibility:hidden">c</button>
<div id=wrap><button id=covered>d</button><div id=cover></div></div>
<form id=form onsubmit="event.preventDefault(); window.submitted = document.querySelector('#name').value">
  <input id=name value="old">
</form>
<input id=fixed readonly value="x">
<script>
  later(() => {
    document.querySelector('#shows').style.display = '';
    document.querySelector('#enables').disabled = false;
  });
</script>
"#;

#[tokio::test]
async fn click_waits_until_the_element_can_be_clicked() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html(ACTIONS))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.click("#shows", None).await.unwrap();
    page.click("#enables", None).await.unwrap();

    let err = page.click("#never", SHORT).await.unwrap_err();
    assert!(
        matches!(&err, Error::Timeout(m) if m.contains("visible")),
        "{err}"
    );
    let err = page.click("#covered", SHORT).await.unwrap_err();
    assert!(
        matches!(&err, Error::Timeout(m) if m.contains("covering")),
        "{err}"
    );
    let err = page.click("#nothing", SHORT).await.unwrap_err();
    assert!(
        matches!(&err, Error::Timeout(m) if m.contains("nothing matched")),
        "{err}"
    );

    page.evaluate("document.querySelector('#cover').remove()")
        .await
        .unwrap();
    page.click("#covered", None).await.unwrap();
    assert_eq!(
        page.evaluate("clicks").await.unwrap(),
        json!(["shows", "enables", "covered"])
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn fill_replaces_the_text_and_press_sends_keys() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html(ACTIONS))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.evaluate("document.querySelector('#cover').remove()")
        .await
        .unwrap();
    page.fill("#name", "hello", None).await.unwrap();
    page.press("!").await.unwrap();
    page.press("Enter").await.unwrap();
    assert_eq!(page.evaluate("submitted").await.unwrap(), json!("hello!"));
    page.fill("#name", "", None).await.unwrap();
    assert_eq!(
        page.evaluate("document.querySelector('#name').value")
            .await
            .unwrap(),
        json!("")
    );
    let err = page.fill("#fixed", "y", SHORT).await.unwrap_err();
    assert!(
        matches!(&err, Error::Timeout(m) if m.contains("editable")),
        "{err}"
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn wait_for_follows_the_element_through_its_states() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![(
        "/",
        html(
            "<script>setTimeout(() => document.body.insertAdjacentHTML('beforeend', '<p id=late>here</p>'), 300)</script><p id=gone>x</p><script>setTimeout(() => document.querySelector('#gone').remove(), 300)</script>",
        ),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.wait_for("#late", "visible", None).await.unwrap();
    page.wait_for("#gone", "detached", None).await.unwrap();
    page.wait_for("#gone", "hidden", None).await.unwrap();
    let err = page
        .wait_for("#never", "attached", SHORT)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Timeout(_)));
    assert!(matches!(
        page.wait_for("#late", "shown", None).await,
        Err(Error::Invalid(_))
    ));
    browser.close().await.unwrap();
}

#[tokio::test]
async fn screenshots_are_png() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![(
        "/",
        html("<div style='height:3000px;background:red'></div>"),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let viewport = page.screenshot(false).await.unwrap();
    let full = page.screenshot(true).await.unwrap();
    assert!(viewport.starts_with(b"\x89PNG\r\n\x1a\n"));
    // Height is bytes 20..24 of the IHDR chunk.
    let height = |png: &[u8]| u32::from_be_bytes(png[20..24].try_into().unwrap());
    assert!(
        height(&full) >= 3000 && height(&viewport) < 3000,
        "{} {}",
        height(&full),
        height(&viewport)
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn pages_keep_their_cookies_to_their_own_context() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![(
        "/",
        Reply {
            status: 200,
            headers: vec![
                ("Content-Type", "text/html".into()),
                ("Set-Cookie", "session=abc; Path=/; HttpOnly".into()),
            ],
            body: "<p>hi</p>".into(),
        },
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let cookies = page.cookies().await.unwrap();
    assert!(
        cookies
            .iter()
            .any(|c| c.name == "session" && c.value == "abc" && c.http_only),
        "{cookies:?}"
    );

    let other = browser.new_page().await.unwrap();
    assert!(
        other.cookies().await.unwrap().is_empty(),
        "a new page shares nothing"
    );

    let context = browser.new_context().await.unwrap();
    let (a, b) = (
        context.new_page().await.unwrap(),
        context.new_page().await.unwrap(),
    );
    a.set_cookies(&cookies).await.unwrap();
    assert_eq!(
        b.cookies().await.unwrap(),
        cookies,
        "pages in one context share cookies"
    );
    context.close().await.unwrap();
    page.close().await.unwrap();
    assert!(page.is_closed());
    browser.close().await.unwrap();
}

#[tokio::test]
async fn closing_a_page_ends_a_navigation_waiting_on_it() {
    let Some(browser) = browser().await else {
        return;
    };
    // Accepts and never answers.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", silent.local_addr().unwrap());
    let page = browser.new_page().await.unwrap();
    let waiting = tokio::spawn({
        let page = page.clone();
        async move {
            page.goto(&url, WaitUntil::Load, Some(Duration::from_secs(20)))
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = std::time::Instant::now();
    page.close().await.unwrap();
    let r = waiting.await.unwrap();
    assert!(matches!(r, Err(Error::PageClosed)), "{r:?}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn evaluate_returns_values_json_cant_hold() {
    let Some(browser) = browser().await else {
        return;
    };
    let page = browser.new_page().await.unwrap();
    assert_eq!(page.evaluate("NaN").await.unwrap(), json!("NaN"));
    assert_eq!(
        page.evaluate("-Infinity").await.unwrap(),
        json!("-Infinity")
    );
    assert_eq!(
        page.evaluate("10n ** 20n").await.unwrap(),
        json!("100000000000000000000")
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn fill_refuses_a_select() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![(
        "/",
        html("<select id=s><option>a</option></select>"),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let err = page.fill("#s", "a", SHORT).await.unwrap_err();
    assert!(
        matches!(&err, Error::Timeout(m) if m.contains("editable")),
        "{err}"
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn an_endless_timeout_is_capped_not_a_panic() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html("<p id=x>x</p>"))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, Some(Duration::MAX))
        .await
        .unwrap();
    page.click("#x", Some(Duration::MAX)).await.unwrap();
    page.wait_for("#x", "visible", Some(Duration::MAX))
        .await
        .unwrap();
    browser.close().await.unwrap();
}

#[tokio::test]
async fn wait_for_navigation_follows_a_script_that_moves_the_page_on() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![
        (
            "/",
            html("<script>setTimeout(() => location.href = '/next', 300)</script>"),
        ),
        ("/next", html("<title>next</title>")),
    ]);
    let page = browser.new_page().await.unwrap();
    let first = page
        .goto(&format!("{}/", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    assert_eq!(first.url, format!("{}/", server.url));
    let next = page
        .wait_for_navigation(&first, WaitUntil::Load, None)
        .await
        .unwrap();
    assert_eq!(
        (next.status, next.url.clone()),
        (200, format!("{}/next", server.url))
    );
    assert_eq!(page.response(), next);
    // Already past `first`: nothing more to wait for.
    let again = page
        .wait_for_navigation(&first, WaitUntil::Load, SHORT)
        .await
        .unwrap();
    assert_eq!(again, next);
    let err = page
        .wait_for_navigation(&next, WaitUntil::Load, SHORT)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Timeout(_)), "{err}");
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_guard_sees_every_hop_and_can_stop_one_before_it_is_fetched() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![
        (
            "/start",
            Reply {
                status: 302,
                headers: vec![("Location", "/secret".into())],
                body: String::new(),
            },
        ),
        (
            "/fine",
            Reply {
                status: 302,
                headers: vec![("Location", "/end".into())],
                body: String::new(),
            },
        ),
        ("/end", html("<title>end</title>")),
        ("/secret", html("<p>secret</p>")),
    ]);
    let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = asked.clone();
    let guard: netweir_browser::Guard = std::sync::Arc::new(move |url: String| {
        log.lock().unwrap().push(url.clone());
        Box::pin(async move { !url.ends_with("/secret") })
    });
    let page = browser.new_guarded_page(guard).await.unwrap();

    let r = page
        .goto(&format!("{}/fine", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    assert_eq!(r.url, format!("{}/end", server.url));
    assert_eq!(
        *asked.lock().unwrap(),
        [
            format!("{}/fine", server.url),
            format!("{}/end", server.url)
        ]
    );

    let err = page
        .goto(&format!("{}/start", server.url), WaitUntil::Load, None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Navigation { error: m, .. } if m.contains("BLOCKED_BY_CLIENT")),
        "{err}"
    );
    assert!(
        !server
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|(p, _)| p == "/secret"),
        "the refused hop was never requested"
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_page_that_never_answers_times_out_instead_of_hanging() {
    let Some(executable) = common::chrome() else {
        return;
    };
    let browser = common::launch(netweir_browser::LaunchOptions {
        executable: Some(executable),
        timeout: Duration::from_secs(5),
        ..Default::default()
    })
    .await;
    // Once loaded, the page's script never lets go of the renderer, so
    // nothing more is ever run in it, netweir's helpers included.
    let server = serve(vec![(
        "/",
        html("<p>busy</p><script>setTimeout(() => { for (;;) {} }, 50)</script>"),
    )]);
    let page = browser.new_page().await.unwrap();
    // A slow runner may take longer than the timeout to load it.
    page.goto(
        &format!("{}/", server.url),
        WaitUntil::Load,
        Some(Duration::from_secs(60)),
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = std::time::Instant::now();
    let outcome = tokio::time::timeout(Duration::from_secs(40), page.content())
        .await
        .expect("content() waited on Chrome with no end");
    assert!(matches!(outcome, Err(Error::Timeout(_))), "{outcome:?}");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    let _ = browser.close().await;
}

#[tokio::test]
async fn goto_waits_as_long_as_it_is_told_not_the_browsers_timeout() {
    let Some(executable) = common::chrome() else {
        return;
    };
    let browser = common::launch(netweir_browser::LaunchOptions {
        executable: Some(executable),
        timeout: Duration::from_secs(5),
        ..Default::default()
    })
    .await;
    let mut slow = html("<p>slow</p>");
    slow.headers.push(("X-Delay-Ms", "7000".into()));
    let server = serve(vec![("/slow", slow)]);
    let page = browser.new_page().await.unwrap();
    let r = page
        .goto(
            &format!("{}/slow", server.url),
            WaitUntil::Load,
            Some(Duration::from_secs(30)),
        )
        .await;
    assert_eq!(r.map(|r| r.status).ok(), Some(200));
    let _ = browser.close().await;
}

#[tokio::test]
async fn a_failed_navigation_carries_what_chrome_said_without_changing_its_kind() {
    let Some(executable) = common::chrome() else {
        return;
    };
    // Verbose, so Chrome always has something to say.
    let browser = common::launch(netweir_browser::LaunchOptions {
        executable: Some(executable),
        args: vec!["--enable-logging=stderr".into(), "--v=1".into()],
        ..Default::default()
    })
    .await;
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let page = browser.new_page().await.unwrap();
    let err = page
        .goto(&format!("http://127.0.0.1:{port}/"), WaitUntil::Load, None)
        .await
        .unwrap_err();
    let Error::Navigation { error, said, .. } = &err else {
        panic!("{err}");
    };
    assert_eq!(error, "net::ERR_CONNECTION_REFUSED");
    assert!(said.as_deref().is_some_and(|s| !s.is_empty()), "{err}");
    assert!(err.to_string().contains("Chrome said lately:"), "{err}");
    assert_eq!(
        err.net_failure(),
        Some(netweir_browser::NetFailure::Connect)
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn back_and_forward_return_each_documents_response_and_none_at_the_ends() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/one", html("<title>one</title>"))]);
    let (one, two) = (format!("{}/one", server.url), format!("{}/two", server.url));
    let page = browser.new_page().await.unwrap();
    page.goto(&one, WaitUntil::Load, None).await.unwrap();
    page.goto(&two, WaitUntil::Load, None).await.unwrap();

    let r = page.go_back(WaitUntil::Load, None).await.unwrap().unwrap();
    assert_eq!((r.url.as_str(), r.status), (one.as_str(), 200));
    assert_eq!(page.url(), one);
    assert_eq!(page.title().await.unwrap(), "one");
    // A new Chrome page's history starts at the blank page it opened on;
    // Firefox's replaces that page with the first it loads.
    if common::engine() == netweir_browser::Engine::Chrome {
        let r = page.go_back(WaitUntil::Load, None).await.unwrap().unwrap();
        assert_eq!((r.url.as_str(), r.status), ("about:blank", 0));
        assert!(
            page.go_back(WaitUntil::Load, None).await.unwrap().is_none(),
            "the blank page has nothing before it"
        );
        assert_eq!(page.url(), "about:blank");
        page.go_forward(WaitUntil::Load, None).await.unwrap();
    } else {
        assert!(page.go_back(WaitUntil::Load, None).await.unwrap().is_none());
    }
    assert_eq!(page.url(), one);

    let r = page
        .go_forward(WaitUntil::DomContentLoaded, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((r.url.as_str(), r.status), (two.as_str(), 404));
    assert_eq!(page.url(), two);
    assert!(
        page.go_forward(WaitUntil::Load, None)
            .await
            .unwrap()
            .is_none()
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn back_over_a_pushed_entry_stays_in_the_document() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html("<script>window.mark = 'kept'</script>"))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.evaluate("history.pushState({}, '', '/pushed')")
        .await
        .unwrap();
    page.evaluate("location.hash = 'part'").await.unwrap();

    let r = page.go_back(WaitUntil::Load, SHORT).await.unwrap().unwrap();
    let pushed = format!("{}/pushed", server.url);
    assert_eq!((r.url.as_str(), r.status), (pushed.as_str(), 0));
    assert_eq!(page.url(), pushed);
    let r = page.go_back(WaitUntil::Load, SHORT).await.unwrap().unwrap();
    let home = format!("{}/", server.url);
    assert_eq!((r.url.as_str(), r.status), (home.as_str(), 0));
    assert_eq!(page.url(), home);
    assert_eq!(
        page.evaluate("window.mark").await.unwrap(),
        json!("kept"),
        "no new document was loaded"
    );
    let r = page
        .go_forward(WaitUntil::Load, SHORT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(r.url, pushed);
    browser.close().await.unwrap();
}

#[tokio::test]
async fn back_to_a_page_chrome_kept_whole_returns_without_loading_it() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![
        (
            "/",
            html(
                "<script>
                  window.restored = false;
                  addEventListener('pageshow', e => { if (e.persisted) restored = true });
                </script>",
            ),
        ),
        ("/next", html("<p>next</p>")),
    ]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.goto(&format!("{}/next", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    // A page restored from the back-forward cache fires no load event:
    // waiting for one would run out the time.
    let r = page
        .go_back(WaitUntil::NetworkIdle, Some(Duration::from_secs(5)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(r.url, format!("{}/", server.url));
    assert_eq!(
        page.evaluate("restored").await.unwrap(),
        json!(true),
        "the page came back from the back-forward cache"
    );
    assert_eq!(
        server.paths().iter().filter(|p| *p == "/").count(),
        1,
        "{:?}",
        server.paths()
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn reload_loads_the_document_again_and_returns_its_response() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html("<title>again</title>"))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    page.evaluate("window.mark = 1").await.unwrap();
    let r = page.reload(WaitUntil::Load, None).await.unwrap();
    assert_eq!((r.url, r.status), (format!("{}/", server.url), 200));
    assert_eq!(page.evaluate("window.mark").await.unwrap(), json!(null));
    assert_eq!(
        server.paths().iter().filter(|p| *p == "/").count(),
        2,
        "{:?}",
        server.paths()
    );
    browser.close().await.unwrap();
}

/// A page Chrome won't keep in its back-forward cache, nor its HTTP
/// cache: going back to it loads it again.
fn uncached(body: &str) -> Reply {
    let mut r = html(body);
    r.headers.push(("Cache-Control", "no-store".into()));
    r
}

#[test]
fn a_dropped_server_doesnt_answer_on_a_connection_already_open() {
    use std::io::{Read, Write};
    let server = serve(vec![("/a", html("<p>a</p>"))]);
    let addr = server.url.trim_start_matches("http://").to_string();
    let mut idle = std::net::TcpStream::connect(&addr).unwrap();
    // Accepted before the drop, as a connection Chrome opened ahead of need.
    let mut probe = std::net::TcpStream::connect(&addr).unwrap();
    probe.write_all(b"GET /a HTTP/1.1\r\n\r\n").unwrap();
    let mut answer = Vec::new();
    // Windows may reset the connection once the answer is sent: the bytes read stay.
    let _ = probe.read_to_end(&mut answer);
    let answer = String::from_utf8_lossy(&answer);
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    drop(server);
    let _ = idle.write_all(b"GET /a HTTP/1.1\r\n\r\n");
    let mut late = Vec::new();
    let _ = idle.read_to_end(&mut late);
    assert!(late.is_empty(), "{}", String::from_utf8_lossy(&late));
}

#[tokio::test]
async fn back_to_a_page_that_cant_be_reached_is_a_navigation_error() {
    let Some(browser) = browser().await else {
        return;
    };
    // An unload handler keeps it out of the back-forward cache too.
    let gone = serve(vec![(
        "/a",
        uncached("<script>addEventListener('unload', () => {})</script>"),
    )]);
    let stays = serve(vec![("/b", html("<p>b</p>"))]);
    let page = browser.new_page().await.unwrap();
    let a = format!("{}/a", gone.url);
    page.goto(&a, WaitUntil::Load, None).await.unwrap();
    page.goto(&format!("{}/b", stays.url), WaitUntil::Load, None)
        .await
        .unwrap();
    drop(gone);
    let err = page.go_back(WaitUntil::Load, None).await.unwrap_err();
    assert!(
        matches!(&err, Error::Navigation { url, error, .. }
            if *url == a && error.contains("ERR_CONNECTION_REFUSED")),
        "{err}"
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn back_to_a_posted_form_chrome_wont_send_again_is_a_navigation_error() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![
        (
            "/",
            html(
                "<form method=post action=/sent><input name=q value=x><button id=go>go</button></form>",
            ),
        ),
        ("/sent", uncached("<p>sent</p>")),
        ("/next", html("<p>next</p>")),
    ]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    let before = page.response();
    page.click("#go", None).await.unwrap();
    page.wait_for_navigation(&before, WaitUntil::Load, None)
        .await
        .unwrap();
    page.goto(&format!("{}/next", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    let err = page.go_back(WaitUntil::Load, None).await.unwrap_err();
    assert!(
        matches!(&err, Error::Navigation { error, .. } if error.contains("ERR_CACHE_MISS")),
        "{err}"
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn back_waits_for_its_own_navigation_not_one_the_page_made_meanwhile() {
    let Some(browser) = browser().await else {
        return;
    };
    let mut slow = uncached("<script>addEventListener('unload', () => {})</script>");
    slow.headers.push(("X-Delay-Ms", "600".into()));
    let server = serve(vec![
        ("/a", slow),
        (
            "/b",
            html(
                "<script>setTimeout(() => history.replaceState({}, '', '/b?moved'), 150)</script>",
            ),
        ),
    ]);
    let page = browser.new_page().await.unwrap();
    let a = format!("{}/a", server.url);
    page.goto(&a, WaitUntil::Load, None).await.unwrap();
    page.goto(&format!("{}/b", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    // The page moves within itself while the way back is still loading.
    let r = page.go_back(WaitUntil::Load, None).await.unwrap().unwrap();
    assert_eq!((r.url.as_str(), r.status), (a.as_str(), 200));
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_history_step_that_never_commits_says_so_when_time_runs_out() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![
        (
            "/a",
            uncached("<script>addEventListener('unload', () => {})</script>"),
        ),
        ("/b", html("<p>b</p>")),
    ]);
    let page = browser.new_page().await.unwrap();
    page.goto(&format!("{}/a", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    page.goto(&format!("{}/b", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    // A page that asks before it's left, which needs a person to have
    // touched it first, and a listener that never answers.
    let _unanswered = page.dialogs();
    page.evaluate(
        "addEventListener('beforeunload', e => { e.preventDefault(); e.returnValue = '' })",
    )
    .await
    .unwrap();
    page.click("body", None).await.unwrap();
    let err = page.go_back(WaitUntil::Load, SHORT).await.unwrap_err();
    assert!(
        matches!(&err, Error::Timeout(m) if m.contains("may not have committed")),
        "{err}"
    );
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_page_saves_as_a_pdf_of_the_size_and_pages_asked_for() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![(
        "/",
        html("<p>one</p><p style='break-before: page'>two</p>"),
    )]);
    let page = browser.new_page().await.unwrap();
    page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
    // Chrome writes each page's dictionary uncompressed.
    let pages = |pdf: &[u8]| {
        String::from_utf8_lossy(pdf)
            .matches("/Type /Page\n")
            .count()
    };
    let whole = page.pdf(&PdfOptions::default()).await.unwrap();
    assert!(whole.starts_with(b"%PDF-"));
    assert_eq!(pages(&whole), 2);
    let first = page
        .pdf(&PdfOptions {
            page_ranges: Some("1".into()),
            landscape: true,
            paper: Some((4.0, 8.0)),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(pages(&first), 1);
    // 72 points to the inch, turned on its side.
    let text = String::from_utf8_lossy(&first);
    assert!(text.contains("/MediaBox [0 0 576 288]"));
    // What Chrome refuses is an error, not an empty file.
    let err = page
        .pdf(&PdfOptions {
            page_ranges: Some("nonsense".into()),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Protocol { .. }), "{err}");
    browser.close().await.unwrap();
}

#[tokio::test]
async fn a_trace_records_what_the_page_did_and_can_be_taken_again() {
    let Some(browser) = browser().await else {
        return;
    };
    let server = serve(vec![("/", html("<p>traced</p>"))]);
    let page = browser.new_page().await.unwrap();
    let err = browser.stop_tracing().await.unwrap_err();
    assert!(matches!(err, Error::Protocol { .. }), "{err}");
    for _ in 0..2 {
        browser.start_tracing(None, true).await.unwrap();
        let again = browser.start_tracing(None, false).await.unwrap_err();
        assert!(matches!(again, Error::Protocol { .. }), "{again}");
        page.goto(&server.url, WaitUntil::Load, None).await.unwrap();
        page.evaluate("console.timeStamp('netweir-mark')")
            .await
            .unwrap();
        let trace = browser.stop_tracing().await.unwrap();
        let trace: serde_json::Value = serde_json::from_slice(&trace).unwrap();
        let events = trace["traceEvents"].as_array().unwrap();
        let text = serde_json::to_string(events).unwrap();
        assert!(text.contains("netweir-mark"));
        assert!(text.contains(&server.url));
    }
    let only: Vec<String> = vec!["blink.user_timing".into()];
    browser.start_tracing(Some(&only), false).await.unwrap();
    page.evaluate("performance.mark('netweir-user-mark')")
        .await
        .unwrap();
    let trace = browser.stop_tracing().await.unwrap();
    assert!(String::from_utf8_lossy(&trace).contains("netweir-user-mark"));
    browser.close().await.unwrap();
}
