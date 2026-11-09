"""Crawls that use Chrome: requests that ask for it, and a site that keeps
blocking until a browser passes its challenge. Skipped without Chrome,
unless NETWEIR_REQUIRE_CHROME is set. With NETWEIR_TEST_ENGINE=firefox
they use Firefox instead (browser_engine="firefox"), skipped without it
unless NETWEIR_REQUIRE_FIREFOX is set."""

import asyncio
import json
import os
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

import netweir

ENGINE = os.environ.get("NETWEIR_TEST_ENGINE", "chrome")
REQUIRED = {"chrome": "NETWEIR_REQUIRE_CHROME", "firefox": "NETWEIR_REQUIRE_FIREFOX"}[ENGINE]
RENDERED = b"""<!DOCTYPE html><ul></ul><script>
for (const name of ['tea', 'cake']) document.querySelector('ul').insertAdjacentHTML('beforeend', `<li>${name}</li>`);
</script>"""

CHALLENGE = b"""<title>Just a moment...</title><script>
setTimeout(() => { document.cookie = 'pass=1; path=/'; location.reload() }, 200)
</script>"""


class Handler(BaseHTTPRequestHandler):
    hits: list[tuple[str, str]] = []

    def log_message(self, *args):
        pass

    def do_GET(self):
        Handler.hits.append((self.path, self.headers.get("User-Agent", "")))
        if self.path.startswith("/guarded") and "pass=1" not in self.headers.get("Cookie", ""):
            self.send_response(403)
            self.send_header("cf-mitigated", "challenge")
            body = CHALLENGE
        else:
            self.send_response(200)
            body = RENDERED
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


@pytest.fixture(scope="module")
def base():
    if not os.environ.get(REQUIRED):
        try:
            asyncio.run(_probe())
        except netweir.BrowserError as e:
            pytest.skip(str(e))
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()
    server.server_close()


async def _probe():
    async with netweir.browser(engine=ENGINE):
        pass


def settings(**kw):
    return netweir.Settings(
        throttle=False,
        start_delay=0,
        obey_robots=False,
        obey_tdmrep=False,
        retries=1,
        backoff_base=0.01,
        backoff_max=0.05,
        **{"browser_engine": ENGINE, **kw},
    )


def test_a_browser_request_reaches_its_callback_rendered_with_the_page_open(base):
    seen = {}

    class Shop(netweir.Spider):
        async def start(self):
            yield netweir.Request(base + "/", browser=True)
            yield netweir.Request(base + "/plain")

        async def parse(self, page):
            seen[page.url] = (
                page.css("li::text").getall(),
                page.browser is not None
                and await page.browser.evaluate("document.querySelectorAll('li').length"),
            )
            self.kept = getattr(self, "kept", []) + [page.browser]

    spider = Shop()
    spider.settings = settings()
    stats = spider.run()
    assert seen[base + "/"] == (["tea", "cake"], 2)
    assert seen[base + "/plain"] == ([], False), "only the request that asked used Chrome"
    assert stats["browser_fetches"] == 1
    assert all(p is None or p.closed for p in spider.kept), "pages close after the callback"


def test_pages_fetched_in_chrome_are_left_out_of_the_warc_and_har_files(base, tmp_path):
    from warcio.archiveiterator import ArchiveIterator

    class Shop(netweir.Spider):
        async def start(self):
            yield netweir.Request(base + "/rendered", browser=True)
            yield netweir.Request(base + "/plain")

        def parse(self, page):
            pass

    spider = Shop()
    spider.settings = settings(warc=str(tmp_path / "c.warc"), har=str(tmp_path / "c.har"))
    assert spider.run()["browser_fetches"] == 1
    with open(tmp_path / "c.warc", "rb") as f:
        uris = [
            r.rec_headers.get_header("WARC-Target-URI")
            for r in ArchiveIterator(f)
            if r.rec_type == "response"
        ]
    assert uris == [base + "/plain"]
    with open(tmp_path / "c.har", encoding="utf-8") as f:
        entries = json.load(f)["log"]["entries"]
    assert [e["request"]["url"] for e in entries] == [base + "/plain"]


def test_on_block_gets_through_in_chrome_and_carries_on_over_http(base):
    names = []

    class Guarded(netweir.Spider):
        async def start(self):
            yield netweir.Request(base + "/guarded/1", callback=self.first)

        def first(self, page):
            names.extend(page.css("li::text").getall())
            yield page.follow("/guarded/2", callback=self.second)

        def second(self, page):
            assert page.browser is None, "fetched over HTTP with the browser's cookie"
            names.extend(page.css("li::text").getall())

    Handler.hits.clear()
    spider = Guarded()
    spider.settings = settings(browser="on_block")
    stats = spider.run()
    # Over HTTP, the cookie goes with the profile of the browser that earned it.
    agent = dict(Handler.hits)["/guarded/2"]
    assert {"chrome": "Chrome/", "firefox": "Firefox/"}[ENGINE] in agent, agent
    # The second page came over HTTP, so its script never ran.
    assert names == ["tea", "cake"]
    assert (stats["browser_fetches"], stats["browser_unblocked"]) == (1, 1)
    assert stats["blocked"] == 2  # the first request's two tries over HTTP


def test_browser_settings_are_checked():
    with pytest.raises(ValueError, match="browser must be"):
        netweir.Settings(browser="sometimes")
    with pytest.raises(ValueError, match="browser_pages"):
        netweir.Settings(browser_pages=0)
    with pytest.raises(ValueError, match='browser_engine must be "chrome", "firefox" or "match"'):
        netweir.Settings(browser_engine="safari")


@pytest.mark.parametrize(
    "proxy", [{"proxy": "http://127.0.0.1:1"}, {"proxies": ("http://127.0.0.1:1",)}]
)
@pytest.mark.parametrize(
    "engine", [{"browser_engine": "firefox"}, {"browser_engine": "match", "profile": "firefox"}]
)
def test_firefox_isnt_taken_through_a_crawls_proxy(proxy, engine):
    class Shop(netweir.Spider):
        async def start(self):
            yield netweir.Request("http://127.0.0.1:1/", browser=True)

    spider = Shop()
    spider.settings = netweir.Settings(**proxy, **engine)
    with pytest.raises(ValueError, match="Firefox can't go through a proxy in a crawl yet"):
        spider.run()


@pytest.mark.skipif(ENGINE != "firefox", reason="Firefox's own: it needs Firefox installed")
def test_match_fetches_in_the_browser_the_profile_is(base):
    agents = []

    class Shop(netweir.Spider):
        async def start(self):
            yield netweir.Request(base + "/", browser=True)

        async def parse(self, page):
            agents.append(await page.browser.evaluate("navigator.userAgent"))

    spider = Shop()
    spider.settings = settings(profile="firefox", browser_engine="match")
    assert spider.run()["browser_fetches"] == 1
    assert len(agents) == 1 and "Firefox/" in agents[0], agents


def test_a_resumed_crawl_still_fetches_browser_requests_in_chrome(base, tmp_path):
    class Stop(Exception):
        pass

    runs = []

    class Once(netweir.Spider):
        async def start(self):
            yield netweir.Request(base + "/again", browser=True)

        def parse(self, page):
            runs.append(page.browser is not None)
            if len(runs) == 1:
                raise Stop

    spider = Once()
    spider.settings = settings(checkpoint=str(tmp_path / "state"), fail_fast=True)
    with pytest.raises(Stop):
        spider.run()
    spider = Once()
    spider.settings = settings(checkpoint=str(tmp_path / "state"))
    spider.run()
    assert runs == [True, True]
