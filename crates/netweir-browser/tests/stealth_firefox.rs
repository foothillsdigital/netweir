//! A Netweir Firefox against one nobody drives: the same install and
//! profile settings, compared on what about 45 page-visible properties
//! say, headless and headed. Any difference fails.

mod common;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use netweir_browser::{LaunchOptions, WaitUntil};
use serde_json::Value;

/// Reads what a page can see and posts it to /report?who=<its #hash>.
const FINGERPRINT: &str = r#"<!doctype html><title>fp</title><script>
addEventListener('load', async () => {
  await new Promise(r => setTimeout(r, 1500));
  const n = navigator, o = {};
  for (const k of ['webdriver','userAgent','language','languages','platform','hardwareConcurrency','maxTouchPoints','cookieEnabled','doNotTrack','pdfViewerEnabled','globalPrivacyControl','buildID','oscpu','vendor','product','productSub','appVersion']) { try { o['n.'+k] = n[k]; } catch(e) { o['n.'+k] = 'ERR'; } }
  o.plugins = n.plugins.length; o.mimeTypes = n.mimeTypes.length;
  Object.assign(o, {sw: screen.width, sh: screen.height, aw: screen.availWidth, ah: screen.availHeight, cd: screen.colorDepth, dpr: devicePixelRatio, iw: innerWidth, ih: innerHeight, ow: outerWidth, oh: outerHeight, sx: screenX, sy: screenY});
  o.vis = document.visibilityState; o.focus = document.hasFocus(); o.hidden = document.hidden;
  for (const bar of ['toolbar','menubar','personalbar','statusbar','scrollbars','locationbar']) { try { o['bar.'+bar] = window[bar].visible; } catch(e) { o['bar.'+bar] = 'ERR'; } }
  Object.assign(o, {isx: window.mozInnerScreenX, isy: window.mozInnerScreenY, sl: screen.left, st: screen.top, vvw: visualViewport.width, vvh: visualViewport.height, cw: document.documentElement.clientWidth, ch: document.documentElement.clientHeight});
  o.tz = Intl.DateTimeFormat().resolvedOptions().timeZone; o.locale = Intl.DateTimeFormat().resolvedOptions().locale;
  o.historyLen = history.length; o.opener = !!window.opener; o.frameElem = window.frameElement === null;
  o.notif = typeof Notification !== 'undefined' ? Notification.permission : 'none';
  try { o.perm = (await n.permissions.query({name: 'notifications'})).state; } catch (e) { o.perm = 'ERR'; }
  o.storageEst = !!n.storage; o.serviceWorker = !!n.serviceWorker; o.webgl = (() => { const g = document.createElement('canvas').getContext('webgl'); if (!g) return 'none'; const d = g.getExtension('WEBGL_debug_renderer_info'); return d ? g.getParameter(d.UNMASKED_RENDERER_WEBGL) : g.getParameter(g.RENDERER); })();
  o.wdDesc = Object.getOwnPropertyDescriptor(Navigator.prototype, 'webdriver')?.get?.toString();
  o.navKeys = Object.keys(Object.getOwnPropertyDescriptors(Navigator.prototype)).length;
  o.winKeys = Object.getOwnPropertyNames(window).length;
  o.popupBlocked = (() => { const w = window.open('about:blank'); const b = !w; if (w) w.close(); return b; })();
  o.mql = matchMedia('(prefers-color-scheme: dark)').matches; o.reducedMotion = matchMedia('(prefers-reduced-motion: reduce)').matches;
  o.fonts = await (async () => { const fs = ['Arial','Helvetica Neue','Menlo','Monaco','Courier New','Times New Roman','Comic Sans MS']; return fs.filter(f => document.fonts.check('12px "'+f+'"')).length; })();
  fetch('/report?who=' + location.hash.slice(1), {method: 'POST', body: JSON.stringify(o)});
});
</script>"#;

type Reports = Arc<(Mutex<HashMap<String, Value>>, Condvar)>;

/// Serves the page at /fp and keeps each report by who sent it.
fn serve() -> (String, Reports) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let reports: Reports = Arc::default();
    let kept = reports.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let kept = kept.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                let mut length = 0;
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.split_once(':')
                        && k.eq_ignore_ascii_case("content-length")
                    {
                        length = v.trim().parse().unwrap_or(0);
                    }
                }
                let path = line.split(' ').nth(1).unwrap_or("/").to_string();
                if let Some(who) = path.strip_prefix("/report?who=") {
                    let mut body = vec![0; length];
                    let _ = reader.read_exact(&mut body);
                    if let Ok(report) = serde_json::from_slice(&body) {
                        let (lock, told) = &*kept;
                        lock.lock().unwrap().insert(who.to_string(), report);
                        told.notify_all();
                    }
                    let _ =
                        stream.write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
                    return;
                }
                let (status, body) = if path == "/fp" {
                    ("200 OK", FINGERPRINT)
                } else {
                    ("404 Not Found", "")
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    (base, reports)
}

fn wait_for(reports: &Reports, who: &str) -> Value {
    let (lock, told) = &**reports;
    let (reports, timeout) = told
        .wait_timeout_while(lock.lock().unwrap(), Duration::from_secs(90), |r| {
            !r.contains_key(who)
        })
        .unwrap();
    assert!(!timeout.timed_out(), "no report from the {who} Firefox");
    reports[who].clone()
}

/// The same install started by hand, with a fresh profile set up as
/// Netweir sets up its own, opening `url`; ended when dropped.
struct Undriven(std::process::Child);

impl Undriven {
    fn start(executable: &Path, profile: &Path, headless: bool, url: &str) -> Undriven {
        std::fs::create_dir_all(profile).unwrap();
        std::fs::write(profile.join("user.js"), netweir_browser::user_js(profile)).unwrap();
        let mut command = std::process::Command::new(executable);
        command
            .arg("-profile")
            .arg(profile)
            // So ending the process ends the browser on Windows too.
            .args(["-no-remote", "-new-instance", "-wait-for-browser"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if headless {
            command.arg("-headless");
        }
        command.arg(url);
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        Undriven(command.spawn().unwrap())
    }
}

impl Drop for Undriven {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.0.id() as libc::pid_t), libc::SIGKILL);
        }
        #[cfg(windows)]
        let _ = std::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &self.0.id().to_string()])
            .output();
        let _ = self.0.wait();
    }
}

/// Whether a headed Firefox can open a window here.
fn has_display() -> bool {
    !cfg!(target_os = "linux") || std::env::var_os("DISPLAY").is_some()
}

async fn compare(headless: bool) {
    let Some(executable) = common::firefox() else {
        return;
    };
    if !headless && !has_display() {
        assert!(
            std::env::var_os("NETWEIR_REQUIRE_FIREFOX").is_none(),
            "a headed Firefox needs a display"
        );
        eprintln!("skipped: no display for a headed Firefox");
        return;
    }
    let _turn = common::turns_for(2);
    let (base, reports) = serve();
    let mode = if headless { "headless" } else { "headed" };
    let scratch =
        std::env::temp_dir().join(format!("netweir-ffstealth-{mode}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    let plain = format!("plain-{mode}");
    let undriven = Undriven::start(
        &executable,
        &scratch.join("undriven"),
        headless,
        &format!("{base}/fp#{plain}"),
    );
    let theirs = wait_for(&reports, &plain);
    drop(undriven);

    let browser = netweir_browser::Browser::launch(LaunchOptions {
        engine: netweir_browser::Engine::Firefox,
        executable: Some(executable),
        headless,
        timeout: Duration::from_secs(60),
        ..LaunchOptions::default()
    })
    .await
    .unwrap();
    let page = browser.new_page().await.unwrap();
    let ours = format!("netweir-{mode}");
    page.goto(&format!("{base}/fp#{ours}"), WaitUntil::Load, None)
        .await
        .unwrap();
    let report = tokio::task::spawn_blocking({
        let reports = reports.clone();
        move || wait_for(&reports, &ours)
    })
    .await
    .unwrap();
    browser.close().await.unwrap();
    let _ = std::fs::remove_dir_all(&scratch);

    let (theirs, ours) = (theirs.as_object().unwrap(), report.as_object().unwrap());
    // Focus depends on which app the system has in front, which neither
    // Firefox controls: a person's window loses it whenever they switch
    // apps. So it's recorded but not compared.
    let mut differences: Vec<String> = theirs
        .keys()
        .chain(ours.keys())
        .filter(|k| *k != "focus")
        .filter(|k| theirs.get(*k) != ours.get(*k))
        .map(|k| {
            format!(
                "{k}: undriven {}, Netweir {}",
                theirs.get(k).unwrap_or(&Value::Null),
                ours.get(k).unwrap_or(&Value::Null)
            )
        })
        .collect();
    differences.sort();
    differences.dedup();
    assert!(
        differences.is_empty(),
        "{mode}: a page can tell Netweir's Firefox from an undriven one:\n{}",
        differences.join("\n")
    );
    assert_eq!(ours["n.webdriver"], false);
}

#[tokio::test]
async fn a_headless_netweir_firefox_looks_like_one_nobody_drives() {
    compare(true).await;
}

#[tokio::test]
async fn a_headed_netweir_firefox_looks_like_one_nobody_drives() {
    compare(false).await;
}

/// Every BiDi method Netweir sends, each one a page can't see: making and
/// closing contexts and windows, reading the browser's windows and
/// frames, putting a new window where the first one is before anything
/// loads in it, navigating as a person would (to a URL, back, forward,
/// reload), and script in a page's own world (evaluate, what the caller
/// asked for) or netweir's sandbox, including the one there in each
/// document that only listens. A method added here needs that said of it.
#[tokio::test]
async fn netweir_sends_firefox_only_these_methods() {
    let Some(executable) = common::firefox() else {
        return;
    };
    let browser = common::launch_firefox(executable, LaunchOptions::default()).await;
    let server = common::serve(vec![("/", common::html("<title>t</title><p>x</p>"))]);
    let page = browser.new_page().await.unwrap();
    page.goto(&format!("{}/", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    page.goto(&format!("{}/#part", server.url), WaitUntil::Load, None)
        .await
        .unwrap();
    page.go_back(WaitUntil::Load, None).await.unwrap();
    page.reload(WaitUntil::NetworkIdle, None).await.unwrap();
    page.evaluate("1").await.unwrap();
    page.content().await.unwrap();
    page.title().await.unwrap();
    let context = browser.new_context().await.unwrap();
    context.new_page().await.unwrap();
    context.close().await.unwrap();
    page.close().await.unwrap();
    let allowed = [
        "browser.createUserContext",
        "browser.getClientWindows",
        "browser.setClientWindowState",
        "browsingContext.getTree",
        "browser.removeUserContext",
        "browsingContext.close",
        "browsingContext.create",
        "browsingContext.navigate",
        "browsingContext.reload",
        "browsingContext.traverseHistory",
        "script.addPreloadScript",
        "script.callFunction",
        "session.new",
        "session.subscribe",
    ];
    let sent = browser.methods_sent();
    let extra: Vec<&String> = sent
        .iter()
        .filter(|m| !allowed.contains(&m.as_str()))
        .collect();
    assert!(extra.is_empty(), "sent {extra:?}");
}
