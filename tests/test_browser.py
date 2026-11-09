"""The browser driver from Python. Skipped without Chrome, unless
NETWEIR_REQUIRE_CHROME is set. With NETWEIR_TEST_ENGINE=firefox, the
tests that use the ``browser`` fixture drive Firefox instead (skipped
without it, unless NETWEIR_REQUIRE_FIREFOX is set), and those that are
Chrome's alone say why in their skip."""

import asyncio
import base64
import contextlib
import json
import os
import re
import socket
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest
from local_server import serve, socket_state

import netweir

ENGINE = os.environ.get("NETWEIR_TEST_ENGINE", "chrome")
assert ENGINE in ("chrome", "firefox"), f"NETWEIR_TEST_ENGINE is {ENGINE!r}"


def chrome_only(why):
    """Skips a test on Firefox, saying why."""
    return pytest.mark.skipif(ENGINE == "firefox", reason=f"Chrome only: {why}")


def skip_if_missing(e):
    """Skips the test when the engine under test isn't installed, unless
    it's required."""
    missing, required = {
        "chrome": ("no Chrome found", "NETWEIR_REQUIRE_CHROME"),
        "firefox": ("no Firefox", "NETWEIR_REQUIRE_FIREFOX"),
    }[ENGINE]
    if missing in str(e) and not os.environ.get(required):
        pytest.skip(str(e))


PAGES = {
    "/": b"""<!DOCTYPE html><title>shop</title>
<ul id=list></ul>
<script>
  for (const name of ['tea', 'cake']) {
    document.querySelector('#list').insertAdjacentHTML('beforeend', `<li>${name}</li>`);
  }
</script>
<button id=more style="display:none" onclick="document.querySelector('#list').insertAdjacentHTML('beforeend', '<li>jam</li>')">more</button>
<script>setTimeout(() => document.querySelector('#more').style.display = '', 200)</script>
<a id=away href="/missing">away</a>
<form onsubmit="event.preventDefault(); window.searched = document.querySelector('#q').value">
  <input id=q>
</form>
""",
    # An endless feed: the next page of items is fetched as you near the end.
    "/feed": b"""<!DOCTYPE html>
<style>li { height: 150px; }</style>
<ul id=list></ul>
<script>
  let next = 1, loading = false, done = false;
  async function more() {
    if (loading || done) return;
    loading = true;
    const r = await fetch('/api/items?page=' + next);
    if (r.ok) {
      const items = (await r.json()).items;
      for (const name of items) {
        document.querySelector('#list').insertAdjacentHTML('beforeend', `<li>${name}</li>`);
      }
      next++;
    } else {
      done = true;
    }
    loading = false;
  }
  addEventListener('scroll', () => {
    if (scrollY + innerHeight > document.documentElement.scrollHeight - 300) more();
  });
  more();
</script>
""",
    "/form": b"""<!DOCTYPE html>
<style>#tip { display: none; position: absolute } #help:hover + #tip { display: block }</style>
<form method=post action=/upload enctype=multipart/form-data>
  <select name=size id=size onchange="window.changed = this.value">
    <option value=s>Small</option><option value=m>Medium</option><option value=l>Large</option>
  </select>
  <select name=extras id=extras multiple>
    <option value=milk>Milk</option><option value=sugar>Sugar</option><option value=lemon>Lemon</option>
  </select>
  <input type=checkbox name=gift id=gift value=yes>
  <input type=file name=doc id=doc>
  <span id=help>?</span><span id=tip>a tip</span>
  <button id=send>send</button>
</form>
""",
    # Grows whenever you near the end, forever.
    "/endless": b"""<!DOCTYPE html><div id=feed style="height: 3000px"></div>
<script>
  addEventListener('scroll', () => {
    const s = document.scrollingElement;
    if (scrollY > s.scrollHeight - s.clientHeight - 100) {
      setTimeout(() => feed.style.height = (feed.offsetHeight + 2000) + 'px', 50);
    }
  });
</script>
""",
    "/inner": b"""<!DOCTYPE html><title>inner</title>
<script>window.clicks = 0</script>
<button id=b onclick="clicks++">b</button>
<input id=t>
<select id=s><option value=a>A</option><option value=b>B</option></select>
<input id=c type=checkbox>
""",
    # A frame from this site, and one from another, which Chrome runs in a
    # process of its own.
    "/outer": b"""<!DOCTYPE html><title>outer</title>
<iframe name=same src="/inner"></iframe>
<script>
  document.write(`<iframe name=other src="http://localhost:${location.port}/inner"></iframe>`);
</script>
""",
    "/asks": b"""<!DOCTYPE html><title>asks</title>
<button id=b onclick="window.answer = prompt('name?', 'ann')">ask</button>
""",
    "/opens": b"""<!DOCTYPE html><title>opens</title>
<a id=a target=_blank href="/popped">open</a>
<a id=dl href="/prices.csv">prices</a>
<a id=stall href="/stall">stalls</a>
""",
    "/popped": b"""<!DOCTYPE html><title>popped</title><p id=p>popped up</p>
<script>
  fetch('/api/items?page=1').then(r => r.text()).then(t => document.querySelector('#p').dataset.api = t)
</script>
""",
}

PRICES = b"name,price\ntea,3\n"

# What the server was sent to /upload, and how often it served each page of
# items; the server's threads write them.
UPLOADS: list[bytes] = []
SERVED: dict[str, int] = {}
LOCK = threading.Lock()
# How many times Chrome hung up on /stall, which sends ten bytes of a
# million and waits for that.
HUNG_UP = []


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        with LOCK:
            UPLOADS.append(body)
        reply = f"<p id=got>{len(body)} bytes</p>".encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(reply)))
        self.end_headers()
        self.wfile.write(reply)

    def do_GET(self):
        if self.path.startswith("/api/items?page="):
            page = int(self.path.rsplit("=", 1)[1])
            with LOCK:
                SERVED[self.path] = SERVED.get(self.path, 0) + 1
            if page > 3:
                self.send_response(404)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            body = json.dumps({"items": [f"item {page}.{i}" for i in range(8)]}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if self.path == "/empty":
            self.send_response(204)
            self.end_headers()
            return
        if self.path == "/prices.csv":
            self.send_response(200)
            self.send_header("Content-Type", "text/csv")
            self.send_header("Content-Disposition", 'attachment; filename="prices.csv"')
            self.send_header("Content-Length", str(len(PRICES)))
            self.end_headers()
            self.wfile.write(PRICES)
            return
        if self.path == "/stall":
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Content-Disposition", 'attachment; filename="big.bin"')
            self.send_header("Content-Length", "1000000")
            self.end_headers()
            self.wfile.write(b"0123456789")
            self.wfile.flush()
            self.connection.settimeout(30)
            with contextlib.suppress(OSError):
                self.connection.recv(1)
            with LOCK:
                HUNG_UP.append(self.path)
            return
        body = PAGES.get(self.path)
        self.send_response(200 if body else 404)
        self.send_header("Content-Type", "text/html")
        if self.path == "/":
            self.send_header("Set-Cookie", "visit=1; Path=/")
        body = body or b"<p>missing</p>"
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class Proxy(BaseHTTPRequestHandler):
    """Wants the login user:secret, and answers every request itself."""

    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.headers.get("Proxy-Authorization") != "Basic dXNlcjpzZWNyZXQ=":
            self.send_response(407)
            self.send_header("Proxy-Authenticate", 'Basic realm="test"')
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        body = f"<p id=via>{self.path}</p>".encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


@pytest.fixture(scope="module")
def base():
    servers = serve(Handler)
    yield f"http://127.0.0.1:{servers[0].server_port}"
    for server in servers:
        server.shutdown()
        server.server_close()


@pytest.fixture
async def browser():
    try:
        b = await netweir.browser(timeout=30, engine=ENGINE)
    except netweir.BrowserError as e:
        skip_if_missing(e)
        raise
    yield b
    await b.close()


async def test_a_rendered_page_reads_back_as_a_node(browser, base):
    page = await browser.new_page()
    response = await page.goto(base + "/")
    assert (response.status, response.url) == (200, base + "/")
    assert await page.title() == "shop"
    root = await page.parse()
    assert root.css("li::text").getall() == ["tea", "cake"]
    assert "<li>cake</li>" in await page.content()


async def test_actions_wait_and_evaluate_returns_python_values(browser, base):
    async with await browser.new_page() as page:
        await page.goto(base + "/")
        await page.click("#more")
        await page.wait_for("li:nth-child(3)")
        await page.fill("#q", "scones")
        await page.press("Enter")
        assert await page.evaluate("searched") == "scones"
        # Waiting starts before the click, so the navigation can't slip past.
        moved = asyncio.ensure_future(page.wait_for_navigation())
        await page.click("#away")
        moved = await moved
        assert (moved.status, moved.url) == (404, base + "/missing")
        assert await page.evaluate(
            "() => ({n: 1, f: 1.5, ok: true, none: null, list: [1, 'a']})"
        ) == {
            "n": 1,
            "f": 1.5,
            "ok": True,
            "none": None,
            "list": [1, "a"],
        }
    assert page.closed


async def test_going_back_forward_and_reloading_return_responses(browser, base):
    async with await browser.new_page() as page:
        await page.goto(base + "/")
        await page.goto(base + "/missing")
        back = await page.go_back()
        assert (back.status, back.url) == (200, base + "/")
        forward = await page.go_forward(wait="domcontentloaded", timeout=10)
        assert (forward.status, forward.url) == (404, base + "/missing")
        assert await page.go_forward() is None
        again = await page.reload()
        assert (again.status, again.url) == (404, base + "/missing")


async def test_press_holds_the_modifiers_named_before_the_key(browser, base):
    async with await browser.new_page() as page:
        await page.goto(base + "/")
        await page.fill("#q", "scones")
        await page.press("ControlOrMeta+A")
        await page.press("Backspace")
        assert await page.evaluate("document.querySelector('#q').value") == ""
        with pytest.raises(ValueError, match="unknown modifier"):
            await page.press("Hyper+a")


@chrome_only("HTML5 drag and drop, which Firefox doesn't begin from WebDriver's mouse")
async def test_drag_drops_one_element_onto_another(browser, base):
    async with await browser.new_page() as page:
        await page.goto(base + "/")
        await page.evaluate("""() => {
            document.body.insertAdjacentHTML('beforeend',
                '<div id=item draggable=true>item</div><div id=bin style="height: 50px">bin</div>');
            item.ondragstart = e => e.dataTransfer.setData('text/plain', 'item');
            bin.ondragover = e => e.preventDefault();
            bin.ondrop = e => { e.preventDefault(); window.dropped = e.dataTransfer.getData('text/plain'); };
        }""")
        await page.drag("#item", "#bin")
        assert await page.evaluate("window.dropped") == "item"


async def test_failures_raise_what_they_are(browser, base):
    page = await browser.new_page()
    await page.goto(base + "/")
    with pytest.raises(netweir.BrowserTimeout, match="visible") as timed_out:
        await page.wait_for("#q-missing", timeout=0.5)
    assert isinstance(timed_out.value, TimeoutError)
    with pytest.raises(netweir.BrowserTimeout, match="nothing matched"):
        await page.click("#nothing-here", timeout=0.5)
    with pytest.raises(netweir.BrowserError, match="boom"):
        await page.evaluate("(() => { throw new Error('boom') })()")
    with pytest.raises(ValueError, match="wait must be"):
        await page.goto(base + "/", wait="soon")
    closed = socket.socket()
    closed.bind(("127.0.0.1", 0))
    port = closed.getsockname()[1]
    closed.close()
    with pytest.raises(netweir.FetchError, match="REFUSED") as refused:
        await page.goto(f"http://127.0.0.1:{port}/")
    assert refused.value.kind == "connect"
    # Chrome abandons a navigation to a 204; the server was reached.
    with pytest.raises(netweir.FetchError, match="ERR_ABORTED") as aborted:
        await page.goto(base + "/empty")
    assert aborted.value.kind == "other"
    for bad in (0, -1, float("inf"), 1e300):
        with pytest.raises(ValueError, match="timeout"):
            await page.click("#q", timeout=bad)


async def test_cookies_are_dicts_and_stay_in_their_context(browser, base):
    page = await browser.new_page()
    await page.goto(base + "/")
    cookies = await page.cookies()
    assert [(c["name"], c["value"], c["path"]) for c in cookies] == [("visit", "1", "/")]
    assert await (await browser.new_page()).cookies() == []

    async with await browser.new_context() as context:
        a, b = await context.new_page(), await context.new_page()
        await a.set_cookies([{"name": "k", "value": "v", "domain": "127.0.0.1"}])
        assert [(c["name"], c["value"], c["path"]) for c in await b.cookies()] == [("k", "v", "/")]
        with pytest.raises(ValueError, match="domain"):
            await a.set_cookies([{"name": "k", "value": "v"}])


async def test_screenshots_are_png_and_can_go_to_a_file(browser, base, tmp_path):
    page = await browser.new_page()
    await page.goto(base + "/")
    png = await page.screenshot(tmp_path / "shot.png", full_page=True)
    assert png.startswith(b"\x89PNG") and (tmp_path / "shot.png").read_bytes() == png


async def test_a_page_saves_as_a_pdf_and_can_go_to_a_file(browser, base, tmp_path):
    page = await browser.new_page()
    await page.goto(base + "/")
    pdf = await page.pdf(tmp_path / "page.pdf", format="a4", landscape=True, margin=0.5)
    assert pdf.startswith(b"%PDF-") and (tmp_path / "page.pdf").read_bytes() == pdf
    with pytest.raises(ValueError, match="the formats are"):
        await page.pdf(format="napkin")
    with pytest.raises(ValueError, match="width and height together"):
        await page.pdf(width=3)
    with pytest.raises(ValueError, match="width and height together"):
        await page.pdf(format="a4", width=3, height=3)


def _har(*entries):
    return {"log": {"version": "1.2", "entries": list(entries)}}


def _entry(url, text="", status=200, method="GET", posted=None, encoding=None, headers=()):
    request = {"method": method, "url": url, "headers": []}
    if posted is not None:
        request["postData"] = {"mimeType": "text/plain", "text": posted}
    content = {"mimeType": "text/html", "text": text}
    if encoding:
        content["encoding"] = encoding
    return {
        "request": request,
        "response": {
            "status": status,
            "headers": [{"name": k, "value": v} for k, v in headers],
            "content": content,
        },
    }


async def test_a_har_file_answers_requests_and_nothing_reaches_the_network(browser, tmp_path):
    # Nothing listens at this address: only the file can answer. (Not a
    # port browsers refuse outright, such as 9, which Firefox does before
    # any route sees the request.)
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        site = f"http://127.0.0.1:{s.getsockname()[1]}"
    html = """<title>From the file</title><script>
      globalThis.go = async (url, init) => {
        try {
          const r = await fetch(url, init);
          return [r.status, r.headers.get("x-kept"), await r.text()];
        } catch (e) { return "failed"; }
      };
    </script>"""
    path = tmp_path / "shop.har"
    path.write_text(
        json.dumps(
            _har(
                _entry(
                    site + "/",
                    html,
                    headers=[
                        ("Content-Type", "text/html"),
                        # The text is decoded: these would break the page.
                        ("Content-Encoding", "gzip"),
                        ("Content-Length", "1"),
                        (":status", "200"),
                    ],
                ),
                _entry(site + "/poll", "first", headers=[("X-Kept", "yes")]),
                _entry(site + "/poll", "second"),
                _entry(site + "/raw", base64.b64encode(b"\xffbytes").decode(), encoding="base64"),
                _entry(site + "/q", "for a", method="POST", posted="a"),
                _entry(site + "/q", "for b", method="POST", posted="b"),
                _entry(site + "/blocked", status=0),
                _entry(site + "/moved", status=302, headers=[("Location", "/gone")]),
                _entry(site + "/gone", "not found", status=404),
            )
        )
    )
    page = await browser.new_page()
    await page.route_from_har(path)
    await page.goto(site + "/#part")
    assert await page.title() == "From the file"
    assert await page.evaluate("go('/poll')") == [200, "yes", "first"]
    assert (await page.evaluate("go('/poll')"))[2] == "second"
    assert (await page.evaluate("go('/poll')"))[2] == "second"
    assert await page.evaluate("fetch('/raw').then(r => r.bytes()).then(b => b[0])") == 255
    ask = "go('/q', {method: 'POST', body: '%s'})"
    assert (await page.evaluate(ask % "b"))[2] == "for b"
    assert (await page.evaluate(ask % "a"))[2] == "for a"
    assert (await page.evaluate("go('/gone')"))[0] == 404
    assert (await page.evaluate("go('/moved')"))[0] == 404
    assert await page.evaluate("go('/blocked')") == "failed"
    assert await page.evaluate("go('/not-in-the-file')") == "failed"


async def test_a_har_file_can_leave_what_it_lacks_to_the_network(browser, base, tmp_path):
    path = tmp_path / "part.har"
    path.write_text(json.dumps(_har(_entry(base + "/only", "from the file"))))
    page = await browser.new_page()
    await page.route_from_har(path, url="**/only", not_found="fallback")
    await page.goto(base + "/")
    assert await page.evaluate("fetch('/only').then(r => r.text())") == "from the file"
    # Another page's context shares nothing with this one's route.
    other = await browser.new_context()
    await other.route_from_har(path, not_found="fallback")
    tab = await other.new_page()
    response = await tab.goto(base + "/")
    assert response.status == 200
    await other.close()


async def test_a_file_that_isnt_a_har_says_so(browser, tmp_path):
    page = await browser.new_page()
    path = tmp_path / "x.har"
    for text in ("not json", "{}", '{"log": {"entries": [{"request": {}}]}}'):
        path.write_text(text)
        with pytest.raises(ValueError, match="isn't a HAR file"):
            await page.route_from_har(path)
    with pytest.raises(FileNotFoundError):
        await page.route_from_har(tmp_path / "missing.har")
    path.write_text(json.dumps(_har()))
    with pytest.raises(ValueError, match="not_found"):
        await page.route_from_har(path, not_found="maybe")


@chrome_only("tracing, which is Chrome's DevTools format")
async def test_a_trace_goes_to_a_file_and_is_chromes_json(browser, base, tmp_path):
    page = await browser.new_page()
    await browser.start_tracing(screenshots=True)
    await page.goto(base + "/")
    trace = await browser.stop_tracing(tmp_path / "trace.json")
    assert (tmp_path / "trace.json").read_bytes() == trace
    assert json.loads(trace)["traceEvents"]
    with pytest.raises(netweir.BrowserError):
        await browser.stop_tracing()


async def test_the_browser_can_be_awaited_or_used_as_a_context_manager():
    try:
        async with netweir.browser(engine=ENGINE) as b:
            assert b.version.startswith({"chrome": "Chrome/", "firefox": "Firefox/"}[ENGINE])
            assert (b.engine, b.stealth) == (ENGINE, True)
            page = await b.new_page()
    except netweir.BrowserError as e:
        skip_if_missing(e)
        raise
    assert b.closed
    with pytest.raises(netweir.BrowserError):
        await page.goto("about:blank")
    with pytest.raises(netweir.BrowserError):
        await b.new_page()
    assert not any(
        m in b.methods_sent() for m in ("Runtime.enable", "Console.enable", "Log.enable")
    )


async def test_the_engine_is_chosen_by_name_and_firefox_refuses_what_it_lacks():
    with pytest.raises(ValueError, match='engine must be "chrome" or "firefox"'):
        await netweir.browser(engine="safari")
    with pytest.raises(ValueError, match="stealth: false is for Firefox"):
        await netweir.browser(stealth=False)
    with pytest.raises(netweir.BrowserError, match="proxy isn't available on Firefox"):
        await netweir.browser(engine="firefox", proxy="http://127.0.0.1:1")
    with pytest.raises(netweir.BrowserError, match="connect isn't available on Firefox"):
        await netweir.browser(engine="firefox", connect="ws://127.0.0.1:1/")


@chrome_only("a proxy, which Netweir doesn't give Firefox yet")
async def test_a_proxy_with_a_login_carries_the_pages():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Proxy)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        try:
            b = await netweir.browser(proxy=f"http://user:secret@127.0.0.1:{server.server_port}")
        except netweir.BrowserError as e:
            if "no Chrome found" in str(e) and not os.environ.get("NETWEIR_REQUIRE_CHROME"):
                pytest.skip(str(e))
            raise
        async with b:
            page = await b.new_page()
            # Not a loopback address, which Chrome would fetch directly.
            response = await page.goto("http://shop.test/item")
            assert response.status == 200
            assert (await page.parse()).css("#via::text").get() == "http://shop.test/item"
    finally:
        server.shutdown()
        server.server_close()


@chrome_only("connecting to a running browser, which Netweir does only with Chrome")
async def test_a_running_chrome_can_be_driven_and_is_left_running(base, tmp_path):
    from netweir._native import find_chrome

    try:
        executable = find_chrome()
    except netweir.BrowserError as e:
        if not os.environ.get("NETWEIR_REQUIRE_CHROME"):
            pytest.skip(str(e))
        raise
    chrome = subprocess.Popen(
        [
            executable,
            "--headless",
            "--remote-debugging-port=0",
            "--no-first-run",
            f"--user-data-dir={tmp_path}",
            "about:blank",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        ws = next(
            line.split("DevTools listening on ", 1)[1].strip()
            for line in chrome.stderr
            if "DevTools listening on " in line
        )
        # Whatever Chrome writes from here on, it never waits on a full pipe.
        threading.Thread(target=chrome.stderr.read, daemon=True).start()
        async with netweir.browser(connect=ws) as b:
            page = await b.new_page()
            await page.goto(base + "/")
            assert await page.title() == "shop"
        assert chrome.poll() is None, "Chrome kept running"
        with pytest.raises(ValueError, match="already running"):
            await netweir.browser(connect=ws, proxy="http://127.0.0.1:1")
        with pytest.raises(ValueError, match="already running"):
            await netweir.browser(connect=ws, headless=False)
    finally:
        _kill_tree(chrome)


def _kill_tree(process):
    """Kills a Chrome this test started, helpers too, and fails rather
    than waiting for good if it doesn't go: once on a Windows runner,
    Chrome was killed and its handle never signalled, and the job sat
    there until its time ran out."""
    if sys.platform == "win32":
        # Its helpers, while they can still be found as its children.
        subprocess.run(
            ["taskkill", "/F", "/T", "/PID", str(process.pid)], capture_output=True, timeout=60
        )
    process.kill()
    try:
        process.wait(timeout=60)
    except subprocess.TimeoutExpired:
        left = ""
        if sys.platform == "win32":
            left = subprocess.run(
                ["powershell", "-NoProfile", "-Command",
                 f"(Get-Process -Id {process.pid}).Threads | Format-Table Id,ThreadState,"
                 "WaitReason -AutoSize | Out-String -Width 200; Get-CimInstance Win32_Process"
                 " -Filter \"Name='chrome.exe'\" | Format-Table ProcessId,ParentProcessId"
                 " -AutoSize | Out-String -Width 200"],
                capture_output=True, text=True, timeout=120,
            ).stdout  # fmt: skip
        pytest.fail(f"Chrome (pid {process.pid}) still ran 60 s after it was killed\n{left}")


async def test_scrolling_an_endless_feed_reads_every_page_of_its_api(browser, base):
    page = await browser.new_page()
    await page.goto(base + "/feed")
    await page.wait_for("li")
    items = []
    while True:
        async with page.expect_response("**/api/items?*") as info:
            loaded = await page.scroll(times=1)
        if not loaded:
            break
        items += (await (await info.value).json())["items"]
    assert items == [f"item {page}.{i}" for page in (2, 3) for i in range(8)]
    assert len((await page.parse()).css("li")) == 24
    assert await page.scroll() == 0, "nothing more comes"
    with LOCK:
        assert SERVED.get("/api/items?page=4") == 1, "the page stopped asking once it ran out"
    await page.evaluate("scrollTo(0, 0)")
    await page.scroll("li:last-child")
    assert await page.evaluate(
        "(() => { const r = document.querySelector('li:last-child').getBoundingClientRect(); "
        "return r.top >= 0 && r.bottom <= innerHeight; })()"
    )
    await page.scroll(y=-200)
    with pytest.raises(netweir.BrowserTimeout, match="nothing matched"):
        await page.scroll("#nothing", timeout=0.5)


async def test_a_form_is_filled_with_choices_and_a_file_and_sent(browser, base, tmp_path):
    upload = tmp_path / "notes.txt"
    upload.write_bytes(b"the bytes of the file")
    page = await browser.new_page()
    await page.goto(base + "/form")

    assert await page.select_option("#size", "m") == ["m"]
    assert await page.evaluate("changed") == "m"
    assert await page.select_option("#size", label="Large") == ["l"]
    assert await page.select_option("#size", index=0) == ["s"]
    assert await page.select_option("#extras", ["milk", "lemon"]) == ["milk", "lemon"]
    with pytest.raises(netweir.BrowserTimeout, match='no option has the value "xl"'):
        await page.select_option("#size", "xl", timeout=0.5)
    with pytest.raises(ValueError, match="one of value, label or index"):
        await page.select_option("#size", "m", label="Medium")

    await page.check("#gift")
    await page.check("#gift")
    assert await page.evaluate("document.querySelector('#gift').checked")
    await page.uncheck("#gift")
    await page.uncheck("#gift")
    assert not await page.evaluate("document.querySelector('#gift').checked")
    await page.check("#gift")

    await page.hover("#help")
    await page.wait_for("#tip")

    with pytest.raises(FileNotFoundError, match="missing.txt"):
        await page.set_input_files("#doc", tmp_path / "missing.txt")
    await page.set_input_files("#doc", upload)
    moved = asyncio.ensure_future(page.wait_for_navigation())
    await page.click("#send")
    await moved
    assert await page.title() == "" and "bytes" in await page.content()
    with LOCK:
        sent = UPLOADS[-1]
    assert b"the bytes of the file" in sent and b'filename="notes.txt"' in sent
    assert b'name="size"\r\n\r\ns' in sent and b'name="gift"\r\n\r\nyes' in sent
    assert b"milk" in sent and b"lemon" in sent and b"sugar" not in sent
    assert not any(
        m in browser.methods_sent()
        for m in ("Runtime.enable", "Console.enable", "Log.enable", "DOM.enable")
    )


async def test_frames_from_this_site_and_others_take_actions(browser, base):
    page = await browser.new_page()
    await page.goto(base + "/outer")
    frames = await page.frames()
    assert [f.name for f in frames] == ["", "same", "other"]
    assert frames[0].url == base + "/outer"
    other = await page.frame(url="http://localhost:*/inner")
    assert other is not None and other.name == "other"
    assert await page.frame(url=re.compile(r"127\.0\.0\.1:\d+/inner$")) is not None
    assert await page.frame("nowhere") is None
    for name in ("same", "other"):
        frame = await page.frame(name)
        assert frame is not None and await frame.title() == "inner"
        await frame.click("#b")
        assert await frame.evaluate("clicks") == 1
        await frame.fill("#t", "hello")
        assert (await frame.parse()).css("#t").get() is not None
        assert await frame.evaluate("document.querySelector('#t').value") == "hello"
        assert await frame.select_option("#s", "b") == ["b"]
        await frame.check("#c")
        await frame.hover("#b")
        with pytest.raises(netweir.BrowserTimeout, match="#missing"):
            await frame.click("#missing", timeout=0.5)
    assert await page.evaluate("typeof clicks") == "undefined"


async def test_a_profile_given_to_the_browser_keeps_its_cookies_and_stays(base, tmp_path):
    profile = tmp_path / "profile"
    for visit in range(2):
        try:
            b = await netweir.browser(user_data_dir=profile, engine=ENGINE)
        except netweir.BrowserError as e:
            skip_if_missing(e)
            raise
        async with b:
            page = await b.new_page()
            if visit == 0:
                await page.goto(base + "/")
                await page.evaluate("document.cookie = 'kept=yes; max-age=3600; path=/'")
            else:
                cookies = {c["name"]: c["value"] for c in await page.cookies()}
                assert cookies.get("kept") == "yes", cookies
        assert profile.is_dir()
    with pytest.raises(ValueError, match="already running"):
        await netweir.browser(connect="ws://127.0.0.1:1/", user_data_dir=profile)


@chrome_only("it starts the Chrome find_chrome() finds")
async def test_a_profile_under_home_is_found_there(tmp_path, monkeypatch):
    from netweir._native import find_chrome

    # The Chrome is found before home moves: netweir's own install is
    # found through home, and this is about the profile, not the Chrome.
    try:
        executable = find_chrome()
    except netweir.BrowserError as e:
        if not os.environ.get("NETWEIR_REQUIRE_CHROME"):
            pytest.skip(str(e))
        raise
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setenv("USERPROFILE", str(tmp_path))
    # Windows finds a user's local app data under USERPROFILE, and when
    # that folder is missing, Google Chrome can't tell its default profile
    # from any other, so it takes every profile for the default and
    # refuses to be driven. A real home has the folder.
    (tmp_path / "AppData" / "Local").mkdir(parents=True)
    monkeypatch.chdir(tmp_path)
    b = await netweir.browser(executable, user_data_dir="~/kept-profile")
    await b.close()
    assert (tmp_path / "kept-profile").is_dir()
    assert not (tmp_path / "~").exists()


async def test_a_directory_isnt_a_file_to_upload(browser, base, tmp_path):
    page = await browser.new_page()
    await page.goto(base + "/form")
    with pytest.raises(IsADirectoryError, match="directory"):
        await page.set_input_files("#doc", tmp_path)


async def test_a_feed_still_growing_when_time_runs_out_says_how_far_it_got(browser, base):
    page = await browser.new_page()
    await page.goto(base + "/endless")
    # A turn of the wheel from the bottom, so the first round is done in
    # moments however slow the machine; the next is a long way down.
    await page.evaluate(
        "scrollTo(0, document.scrollingElement.scrollHeight - document.scrollingElement.clientHeight - 200)"
    )
    with pytest.raises(netweir.BrowserTimeout, match="scrolling to the bottom") as stopped:
        await page.scroll(timeout=2)
    assert stopped.value.rounds >= 1
    with pytest.raises(netweir.BrowserTimeout) as other:
        await page.wait_for("#nothing", timeout=0.2)
    assert other.value.rounds is None


@pytest.mark.skipif(os.name != "nt", reason="the report is read with PowerShell on Windows")
async def test_a_windows_socket_report_names_the_browser_this_process_started(browser):
    # What a test failing with ERR_NO_BUFFER_SPACE reports (see conftest.py).
    said = socket_state()
    assert "TCP: " in said and "Nonpaged pool: " in said, said
    assert re.search(rf"browser \d+ started \S+ by python[^(\s]*\({os.getpid()}\)", said), said


async def test_a_dialog_nobody_answers_is_dismissed_after_a_human_pause(browser, base):
    page = await browser.new_page()
    await page.goto(base + "/asks")
    started = asyncio.get_running_loop().time()
    assert await asyncio.wait_for(page.evaluate("confirm('sure?')"), 10) is False
    assert asyncio.get_running_loop().time() - started >= 0.5
    with pytest.raises(TypeError, match="pattern"):
        await page.wait_for_response(None)


async def test_the_pause_before_a_dialog_is_answered_can_be_set(base):
    with pytest.raises(ValueError, match="dialog_pause"):
        await netweir.browser(dialog_pause=(1.0, 0.5))
    # Longer than the default's longest (1.5 s), so only the pause set
    # here can keep the page waiting this long. A bound the other way
    # would be the machine's to keep: on a busy Windows runner the round
    # trips around a 50 ms pause alone took over 500 ms.
    pause = 2.0
    try:
        browser = await netweir.browser(dialog_pause=(pause, pause + 0.01), engine=ENGINE)
    except netweir.BrowserError as e:
        skip_if_missing(e)
        raise
    async with browser:
        page = await browser.new_page()
        await page.goto(base + "/asks")
        started = asyncio.get_running_loop().time()
        assert await page.evaluate("confirm('slow?')") is False
        assert asyncio.get_running_loop().time() - started >= pause


async def test_a_dialog_listener_plain_or_async_answers_it(browser, base):
    page = await browser.new_page()
    await page.goto(base + "/asks")
    seen = []

    def plain(dialog):
        seen.append((dialog.type, dialog.message, dialog.default_value, dialog.url))
        dialog.accept("bob")

    page.on_dialog(plain)
    await page.click("#b")
    assert await _soon(page, "window.answer") == "bob"
    assert seen == [("prompt", "name?", "ann", base + "/asks")]
    page.remove_dialog_listener(plain)

    async def dismisses(dialog):
        await asyncio.sleep(0.1)
        dismissing = dialog.dismiss()
        seen.append(dialog.handled)
        with pytest.raises(netweir.BrowserError, match="already answered"):
            await dialog.accept()
        await dismissing

    page.on_dialog(dismisses)
    assert await asyncio.wait_for(page.evaluate("confirm('sure?')"), 10) is False
    assert seen[-1] is True
    page.remove_dialog_listener(dismisses)

    def leaves_it(dialog):
        seen.append("heard")

    page.on_dialog(leaves_it)
    assert await asyncio.wait_for(page.evaluate("prompt('again?')"), 10) is None
    assert seen[-1] == "heard"


async def _soon(page, js, timeout=10.0):
    """``js`` once it's something other than None."""
    deadline = asyncio.get_running_loop().time() + timeout
    while (value := await page.evaluate(js)) is None:
        if asyncio.get_running_loop().time() > deadline:
            break
        await asyncio.sleep(0.05)
    return value


async def test_a_popup_is_read_and_routed_like_its_opener(browser, base):
    page = await browser.new_page()
    await page.route("**/api/items*", lambda route: route.fulfill(body="routed"))
    await page.goto(base + "/opens")
    heard = []
    page.on_popup(heard.append)
    async with page.expect_popup() as info:
        await page.click("#a")
    popup = await info.value
    response = await popup.wait_for_load()
    assert (response.status, popup.url) == (200, base + "/popped")
    assert "popped up" in await popup.content()
    assert await _soon(popup, "document.querySelector('#p').dataset.api") == "routed"
    assert not await popup.evaluate("navigator.userAgent.includes('Headless')")
    assert heard == [popup]
    page.remove_popup_listener(heard.append)
    waiting = asyncio.ensure_future(page.wait_for_popup(timeout=10))
    await page.click("#a")
    second = await waiting
    await second.wait_for_load()
    assert second.url == base + "/popped"
    await popup.close()
    assert popup.closed


async def test_a_download_is_heard_saved_and_goes_with_its_page(browser, base, tmp_path):
    page = await browser.new_page()
    await page.goto(base + "/opens")
    async with page.expect_download("**/*.csv") as info:
        await page.click("#dl")
    download = await info.value
    assert download.suggested_filename == "prices.csv"
    assert download.url == base + "/prices.csv"
    assert await download.body() == PRICES
    assert await download.failure() is None
    await download.save_as(tmp_path / "prices.csv")
    assert (tmp_path / "prices.csv").read_bytes() == PRICES
    kept = await download.path()
    assert os.path.exists(kept)
    await page.close()
    assert not os.path.exists(kept), "the page's context took its downloads with it"
    with pytest.raises(netweir.BrowserTimeout, match="download"):
        other = await browser.new_page()
        await other.goto(base + "/opens")
        await other.wait_for_download(timeout=0.5)


@chrome_only("cancelling a download, which Firefox can't")
async def test_waiting_on_a_stalled_download_gives_up_at_the_timeout(browser, base, tmp_path):
    page = await browser.new_page()
    await page.goto(base + "/opens")
    async with page.expect_download() as info:
        await page.click("#stall")
    download = await info.value
    for waiting in (
        download.path(timeout=0.3),
        download.body(timeout=0.3),
        download.save_as(tmp_path / "big.bin", timeout=0.3),
        download.failure(timeout=0.3),
    ):
        with pytest.raises(netweir.BrowserTimeout, match="download"):
            await asyncio.wait_for(waiting, 10)
    assert not (tmp_path / "big.bin").exists()
    # Giving up on it leaves it going, for cancel() to stop.
    await download.cancel()
    assert await download.failure() == "canceled"


async def _hung_up(n, timeout=10.0):
    """Whether Chrome has hung up on /stall ``n`` times, within ``timeout``."""
    deadline = asyncio.get_running_loop().time() + timeout
    while len(HUNG_UP) < n:
        if asyncio.get_running_loop().time() > deadline:
            return False
        await asyncio.sleep(0.05)
    return True


@chrome_only("cancelling a download, which Firefox can't")
async def test_a_download_nobody_listens_for_is_cancelled(browser, base):
    page = await browser.new_page()
    await page.goto(base + "/opens")
    before = len(HUNG_UP)
    # Heard, but by nobody who wants downloads.
    page.on_request(lambda request: None)
    await page.click("#stall")
    assert await _hung_up(before + 1), "a download nobody listened for went on"
    kept = []
    page.on_download(kept.append)
    await page.click("#stall")
    await asyncio.sleep(1)
    assert len(HUNG_UP) == before + 1, "a download held went on"
    await kept.pop().cancel()
    assert await _hung_up(before + 2)


async def test_a_har_a_crawl_wrote_replays_in_chrome(browser, base, tmp_path):
    path = tmp_path / "crawl.har"

    class Spider(netweir.Spider):
        start_urls = [base + "/"]
        settings = netweir.Settings(har=str(path))

        def parse(self, page):
            pass

    await asyncio.to_thread(Spider().run)
    har = json.loads(path.read_text())
    entry = next(e for e in har["log"]["entries"] if e["request"]["url"] == base + "/")
    # Marked, to show the answer came from the file and not the server.
    entry["response"]["content"]["text"] += "<p id=replayed>from the file</p>"
    path.write_text(json.dumps(har))
    page = await browser.new_page()
    await page.route_from_har(path)
    await page.goto(base + "/")
    assert (
        await page.evaluate("document.querySelector('#replayed')?.textContent") == "from the file"
    )
