"""Sessions and cookies under the caller's control: a Client's cookies and
storage state, a crawl's request cookies and named jars, Chrome's storage
state, and cookies handed between Chrome and the HTTP client."""

import json
import os
import stat
import sys
from http.server import BaseHTTPRequestHandler
from urllib.parse import parse_qsl, urlsplit

import pytest
from local_server import serve

import netweir


class Handler(BaseHTTPRequestHandler):
    """/login?user=x sets user=x; /set?k=v sets any cookie; /echo answers
    with the Cookie header it got; /local sets localStorage; everything
    is logged with the Cookie it carried."""

    protocol_version = "HTTP/1.1"
    seen: list = []

    def log_message(self, *args):
        pass

    def do_GET(self):
        url = urlsplit(self.path)
        query = dict(parse_qsl(url.query))
        cookie = self.headers.get("Cookie", "")
        Handler.seen.append((url.path, cookie))
        headers = [("Content-Type", "text/html")]
        body = b"ok"
        if url.path == "/login":
            headers.append(("Set-Cookie", f"user={query['user']}; Path=/"))
        elif url.path == "/set":
            for k, v in query.items():
                headers.append(("Set-Cookie", f"{k}={v}; Path=/"))
        elif url.path == "/echo":
            body = json.dumps({"cookie": cookie}).encode()
        elif url.path == "/local":
            body = b"<script>localStorage.setItem('cart', '[1,2]')</script>"
        self.send_response(200)
        for k, v in headers:
            self.send_header(k, v)
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


@pytest.fixture(autouse=True)
def clean():
    Handler.seen.clear()


def carried(path):
    return [c for p, c in Handler.seen if p == path]


def private(path):
    if sys.platform != "win32":
        assert stat.S_IMODE(os.stat(path).st_mode) == 0o600


async def test_a_clients_cookies_can_be_read_set_and_removed(base):
    async with netweir.Client() as client:
        await client.get(f"{base}/login?user=ada")
        assert client.cookies.get("user") == "ada"
        assert client.cookies.get("user", domain="127.0.0.1") == "ada"
        assert client.cookies.get("nope") is None
        [c] = client.cookies.all()
        assert (c["domain"], c["path"], c["expires"]) == ("127.0.0.1", "/", -1)
        client.cookies.set("theme", "dark", domain="127.0.0.1")
        assert len(client.cookies) == 2
        page = await client.get(f"{base}/echo")
        assert json.loads(page.text)["cookie"] == "user=ada; theme=dark"
        client.cookies.delete("user")
        assert [c["name"] for c in client.cookies.all()] == ["theme"]
        client.cookies.clear()
        assert len(client.cookies) == 0
        assert not client.cookies


async def test_a_clients_storage_state_is_saved_privately_and_loads_in_another(base, tmp_path):
    path = tmp_path / "state.json"
    async with netweir.Client() as client:
        await client.get(f"{base}/login?user=ada")
        state = client.storage_state(path)
    private(path)
    assert json.loads(path.read_text()) == state
    assert state["origins"] == []
    [c] = state["cookies"]
    assert (c["name"], c["value"], c["httpOnly"]) == ("user", "ada", False)
    for source in (path, str(path), state):
        async with netweir.Client(storage_state=source) as again:
            page = await again.get(f"{base}/echo")
            assert json.loads(page.text)["cookie"] == "user=ada"


async def test_cookies_in_playwrights_spelling_go_in_as_they_are(base):
    async with netweir.Client() as client:
        client.cookies.add(
            [
                {
                    "name": "pw",
                    "value": "1",
                    "domain": "127.0.0.1",
                    "path": "/",
                    "httpOnly": True,
                    "sameSite": "Lax",
                    "expires": -1,
                }
            ]
        )
        [c] = client.cookies.all()
        assert (c["http_only"], c["same_site"]) == (True, "Lax")
        page = await client.get(f"{base}/echo")
        assert json.loads(page.text)["cookie"] == "pw=1"


async def test_a_cookie_chrome_would_refuse_is_refused_and_a_saved_state_loads_the_rest(
    base, caplog
):
    async with netweir.Client() as client:
        good = {"name": "ok", "value": "1", "domain": "127.0.0.1"}
        bad = {"name": "__Host-sid", "value": "s3cret", "domain": "127.0.0.1"}
        with pytest.raises(ValueError, match="__Host-") as raised:
            client.cookies.add([good, bad])
        assert "s3cret" not in str(raised.value)
        # All or nothing.
        assert client.cookies.all() == []
        with pytest.raises(ValueError, match="'='"):
            client.cookies.set("", "a=b", domain="127.0.0.1")
        # Each refusal names the cookie, or the type that wasn't a cookie.
        sid = {"name": "sid", "value": "s3cret"}
        with pytest.raises(ValueError, match='^the cookie "sid" needs a "domain"$') as raised:
            client.cookies.add([good, sid])
        assert "s3cret" not in str(raised.value)
        with pytest.raises(ValueError, match='^a cookie needs a "name"$'):
            client.cookies.add([{"value": "1", "domain": "127.0.0.1"}])
        with pytest.raises(
            ValueError,
            match="""^the cookie "sid": expires is seconds since the epoch, not 'tomorrow'$""",
        ):
            client.cookies.add([{**sid, "domain": "127.0.0.1", "expires": "tomorrow"}])
        with pytest.raises(TypeError, match="^each cookie must be a dict, not str$") as raised:
            client.cookies.add(["sid=s3cret"])
        assert "s3cret" not in str(raised.value)
        assert client.cookies.all() == []
    state = {"cookies": [good, bad, {**good, "name": "tab", "value": "a\tb"}], "origins": []}
    async with netweir.Client(storage_state=state) as client:
        assert [c["name"] for c in client.cookies.all()] == ["ok"]
    left_out = [r.getMessage() for r in caplog.records if "left out" in r.getMessage()]
    assert len(left_out) == 2 and "__Host-sid" in left_out[0]
    assert not any("s3cret" in m for m in left_out)


def test_a_request_with_a_cookie_chrome_would_refuse_is_an_error(base):
    class Spider(netweir.Spider):
        async def start(self):
            yield netweir.Request(
                f"{base}/echo", cookies=[{"name": "x", "value": "1", "same_site": "None"}]
            )

    with pytest.raises(ValueError, match="SameSite=None"):
        run(Spider, fail_fast=True)
    assert carried("/echo") == []


def run(spider_class, **settings):
    spider = spider_class()
    spider.settings = netweir.Settings(
        throttle=False,
        start_delay=0,
        obey_robots=False,
        obey_tdmrep=False,
        concurrency=1,
        per_domain=1,
        **settings,
    )
    return spider.run()


def test_a_requests_cookies_go_with_it_and_stay_in_its_jar(base):
    class Spider(netweir.Spider):
        async def start(self):
            yield netweir.Request(f"{base}/echo", cookies={"currency": "EUR", "n": 1})

        def parse(self, page):
            if page.url.endswith("/echo"):
                yield netweir.Request(f"{base}/set?x=1", callback=self.again)

        def again(self, page):
            pass

    run(Spider)
    assert carried("/echo") == ["currency=EUR; n=1"]
    assert carried("/set") == ["currency=EUR; n=1"]


def test_named_jars_keep_two_logins_apart_in_one_crawl(base):
    class Spider(netweir.Spider):
        async def start(self):
            for user in ("ada", "bob"):
                yield netweir.Request(f"{base}/login?user={user}", meta={"cookiejar": user})

        def parse(self, page):
            if "/login" in page.url:
                # Carried on, as in Scrapy, by passing the meta along.
                yield netweir.Request(
                    f"{base}/echo?as={page.meta['cookiejar']}",
                    meta={"cookiejar": page.meta["cookiejar"]},
                    callback=self.me,
                )

        def me(self, page):
            pass

    run(Spider)
    assert sorted(carried("/echo")) == ["user=ada", "user=bob"]


def test_deltafetch_skips_a_page_only_for_the_jar_it_gave_items_in(base, tmp_path):
    delta = str(tmp_path / "delta")

    class Spider(netweir.Spider):
        users = ("ada",)

        async def start(self):
            for user in self.users:
                yield netweir.Request(f"{base}/login?user={user}", meta={"cookiejar": user})

        def parse(self, page):
            if "/login" in page.url:
                yield netweir.Request(
                    f"{base}/echo", meta={"cookiejar": page.meta["cookiejar"]}, callback=self.me
                )

        def me(self, page):
            yield {"cookie": json.loads(page.text)["cookie"]}

    run(Spider, deltafetch=delta)
    assert carried("/echo") == ["user=ada"]
    Handler.seen.clear()
    Spider.users = ("ada", "bob")
    run(Spider, deltafetch=delta)
    # Ada's page gave items last time; Bob's is his own and still to do.
    assert carried("/echo") == ["user=bob"]


@pytest.mark.parametrize("per_jar", [False, True])
def test_an_items_files_download_in_the_jar_of_the_page_it_came_from(base, tmp_path, per_jar):
    import hashlib

    class Spider(netweir.Spider):
        pipelines = [netweir.files(tmp_path, per_jar=per_jar)]

        async def start(self):
            for user in ("ada", "bob"):
                yield netweir.Request(f"{base}/login?user={user}", meta={"cookiejar": user})

        def parse(self, page):
            if "/login" in page.url:
                user = page.meta["cookiejar"]
                yield netweir.Request(
                    f"{base}/echo?page={user}", meta={"cookiejar": user}, callback=self.item
                )

        def item(self, page):
            # Both users' items name the same file.
            yield {"file_urls": [f"{base}/file.txt"]}

    run(Spider)
    url = f"{base}/file.txt"
    sha1 = lambda key: hashlib.sha1(key.encode()).hexdigest()  # noqa: E731
    stored = sorted(p.name for p in (tmp_path / "full").iterdir())
    if per_jar:
        # Each user's own, stored apart.
        assert sorted(carried("/file.txt")) == ["user=ada", "user=bob"]
        assert stored == sorted(sha1(j + "\0" + url) + ".txt" for j in ('"ada"', '"bob"'))
    else:
        # Once, as Scrapy stores it, in the jar of the first item to name it.
        assert carried("/file.txt") in (["user=ada"], ["user=bob"])
        assert stored == [f"{sha1(url)}.txt"]


def test_jar_names_are_told_apart_by_type_as_in_scrapy(base):
    class Spider(netweir.Spider):
        async def start(self):
            yield netweir.Request(f"{base}/login?user=one", meta={"cookiejar": 1})

        def parse(self, page):
            if "/login" in page.url:
                yield netweir.Request(f"{base}/echo?a", meta={"cookiejar": "1"}, callback=self.me)
                yield netweir.Request(f"{base}/echo?b", meta={"cookiejar": 1}, callback=self.me)

        def me(self, page):
            pass

    run(Spider)
    assert sorted(carried("/echo")) == ["", "user=one"]


@pytest.mark.parametrize("saving", [False, True])
def test_a_request_whose_cookies_or_jar_cant_work_is_skipped_not_the_crawl(base, tmp_path, saving):
    class Spider(netweir.Spider):
        async def start(self):
            yield netweir.Request(
                f"{base}/echo?bad", cookies=[{"name": "x", "value": "1", "same_site": "None"}]
            )
            yield netweir.Request(
                f"{base}/echo?odd", cookies=[{"name": "x", "value": "1", "expires": object()}]
            )
            yield netweir.Request(f"{base}/echo?jar", meta={"cookiejar": object()})
            yield netweir.Request(f"{base}/echo?good")

        def parse(self, page):
            pass

    settings = {"checkpoint": str(tmp_path / "state")} if saving else {}
    stats = run(Spider, **settings)
    assert carried("/echo") == [""]
    assert stats["callback_errors"] == 3


def test_dont_merge_cookies_leaves_the_jar_out(base):
    class Spider(netweir.Spider):
        async def start(self):
            yield netweir.Request(f"{base}/login?user=ada")

        def parse(self, page):
            if "/login" in page.url:
                yield netweir.Request(
                    f"{base}/set?tracker=1",
                    cookies={"only": "this"},
                    meta={"dont_merge_cookies": True},
                    callback=self.then,
                )

        def then(self, page):
            yield netweir.Request(f"{base}/echo", callback=self.done)

        def done(self, page):
            pass

    run(Spider)
    assert carried("/set") == ["only=this"]
    assert carried("/echo") == ["user=ada"]


def test_request_cookies_are_kept_out_of_its_repr():
    r = netweir.Request("https://e.com/", cookies={"sid": "s3cret"})
    assert "s3cret" not in repr(r)
    assert r.cookies == [{"name": "sid", "value": "s3cret"}]
    with pytest.raises(TypeError, match="cookies"):
        netweir.Request("https://e.com/", cookies="sid=1")


def test_request_cookies_take_a_generator_bytes_and_drop_none_as_scrapy(caplog):
    pairs = netweir.Request("https://e.com/", cookies=(c for c in [{"name": "a", "value": "1"}]))
    assert pairs.cookies == [{"name": "a", "value": "1"}]
    r = netweir.Request("https://e.com/", cookies={"a": b"1", "b": None, b"c": "3"})
    assert r.cookies == [{"name": "a", "value": "1"}, {"name": "c", "value": "3"}]
    r = netweir.Request(
        "https://e.com/", cookies=[{"name": "a", "value": b"1"}, {"name": "b", "value": None}]
    )
    assert r.cookies == [{"name": "a", "value": "1"}]
    dropped = [m for m in caplog.messages if "has no value" in m]
    assert len(dropped) == 2 and all("'b'" in m for m in dropped)


def test_a_resumed_crawl_is_still_logged_in_and_keeps_waiting_requests_cookies(base, tmp_path):
    state = tmp_path / "state"

    class Spider(netweir.Spider):
        async def start(self):
            yield netweir.Request(f"{base}/login?user=ada", meta={"cookiejar": "ada"})

        def parse(self, page):
            if "/login" in page.url:
                yield netweir.Request(
                    f"{base}/echo", meta={"cookiejar": "ada"}, cookies={"extra": "1"}
                )

    # The first run stops after the login; the /echo it queued waits.
    run(Spider, checkpoint=str(state), max_pages=1)
    assert carried("/echo") == []
    private(state / "crawl.sqlite3")
    run(Spider, checkpoint=str(state))
    assert carried("/echo") == ["user=ada; extra=1"]


@pytest.fixture
async def browser():
    try:
        b = await netweir.browser(timeout=30)
    except netweir.BrowserError as e:
        if "no Chrome found" in str(e) and not os.environ.get("NETWEIR_REQUIRE_CHROME"):
            pytest.skip(str(e))
        raise
    yield b
    await b.close()


async def test_log_in_with_chrome_then_carry_on_over_http(browser, base):
    page = await browser.new_page()
    await page.goto(f"{base}/login?user=ada")
    async with netweir.Client() as client:
        client.cookies.add(await page.cookies())
        reply = await client.get(f"{base}/echo")
    assert json.loads(reply.text)["cookie"] == "user=ada"


async def test_cookies_from_http_go_into_chrome(browser, base):
    async with netweir.Client() as client:
        await client.get(f"{base}/login?user=bob")
        context = await browser.new_context()
        await context.set_cookies(client.cookies.all())
    page = await context.new_page()
    await page.goto(f"{base}/echo")
    assert carried("/echo") == ["user=bob"]


async def test_a_contexts_storage_state_with_local_storage_goes_to_a_new_one(
    browser, base, tmp_path
):
    context = await browser.new_context()
    page = await context.new_page()
    await page.goto(f"{base}/login?user=ada")
    await page.goto(f"{base}/local")
    path = tmp_path / "state.json"
    state = await context.storage_state(path=path)
    private(path)
    assert json.loads(path.read_text()) == state
    assert state["origins"] == [
        {"origin": base, "localStorage": [{"name": "cart", "value": "[1,2]"}]}
    ]
    fresh = await browser.new_context(storage_state=path)
    page = await fresh.new_page()
    await page.goto(f"{base}/echo")
    assert carried("/echo") == ["user=ada"]
    assert await page.evaluate("localStorage.getItem('cart')") == "[1,2]"
    # The same file loads into the HTTP client.
    async with netweir.Client(storage_state=path) as client:
        reply = await client.get(f"{base}/echo")
    assert json.loads(reply.text)["cookie"] == "user=ada"


def test_a_crawls_browser_request_takes_its_jar_and_gives_it_back(base):
    try:
        import asyncio

        async def probe():
            async with netweir.browser():
                pass

        if not os.environ.get("NETWEIR_REQUIRE_CHROME"):
            asyncio.run(probe())
    except netweir.BrowserError as e:
        pytest.skip(str(e))

    class Spider(netweir.Spider):
        async def start(self):
            yield netweir.Request(f"{base}/login?user=ada", meta={"cookiejar": "ada"})

        async def parse(self, page):
            if "/login" in page.url:
                yield netweir.Request(
                    f"{base}/echo?in=chrome",
                    browser=True,
                    meta={"cookiejar": "ada"},
                    callback=self.in_chrome,
                )

        async def in_chrome(self, page):
            # Chrome sets one more, which goes back to the jar on close.
            await page.browser.goto(f"{base}/set?from=chrome")
            await page.browser.close()
            yield netweir.Request(
                f"{base}/echo?after", meta={"cookiejar": "ada"}, callback=self.after
            )

        def after(self, page):
            pass

    run(Spider)
    assert carried("/echo") == ["user=ada", "user=ada; from=chrome"]
