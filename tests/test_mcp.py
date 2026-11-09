"""netweir mcp: the tools an agent gets, driven through an in-memory MCP
client against a local site."""

import asyncio
import sys
import threading
import types
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

pytest.importorskip("mcp")

from mcp import Client  # noqa: E402

import netweir  # noqa: E402
from netweir import _mcp  # noqa: E402

PRODUCT = """<html><head><title>Arc lamp</title>
<script type="application/ld+json">{"@context": "https://schema.org", "@type": "Product",
"name": "Arc lamp", "offers": {"@type": "Offer", "price": "49.00", "priceCurrency": "GBP"}}</script>
</head><body><nav><a href="/">Home</a> <a href="/lamps">Lamps</a> <a href="/lamps">Lamps</a>
<a href="mailto:hi@lumo.example">Mail</a></nav>
<main><h1>Arc lamp</h1><p class="lead">A tall lamp that leans over a sofa or a desk, made
of brushed steel, with a linen shade and a dimmer on the cord.</p>
<h2>Care</h2><p>Dust the shade with a soft brush. Wipe the steel with a dry cloth, and never
use polish on it, as polish clouds the brushed finish over time.</p></main>
<footer>Lumo Ltd</footer></body></html>"""

PAGES = {"/arc": PRODUCT, "/empty": "<html><body><p>Hello.</p></body></html>"}
FILES = {"/manual.pdf": ("application/pdf", b"%PDF-1.4 binary")}


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.path in FILES:
            kind, data = FILES[self.path]
            self.send_response(200)
            self.send_header("Content-Type", kind)
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            return
        found = PAGES.get(self.path)
        data = (found or "missing").encode()
        self.send_response(200 if found else 404)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


@pytest.fixture(scope="module")
def base():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()
    server.server_close()


def call(tool, **arguments):
    async def go():
        async with Client(_mcp.server(timeout=10)) as client:
            return await client.call_tool(tool, arguments)

    return asyncio.run(go())


def ok(tool, **arguments):
    result = call(tool, **arguments)
    assert not result.is_error, result.content
    return result.structured_content


def test_the_tools_an_agent_sees():
    async def go():
        async with Client(_mcp.server()) as client:
            return await client.list_tools()

    tools = {t.name: t for t in asyncio.run(go()).tools}
    assert set(tools) == {"read_page", "chunks", "extract", "select", "links"}
    assert all(t.description for t in tools.values())


def test_read_page_gives_the_main_content_as_markdown(base):
    page = ok("read_page", url=f"{base}/arc")
    assert page["title"] == "Arc lamp"
    assert page["page_type"] == "product"
    assert page["markdown"].startswith("# Arc lamp")
    assert "Lumo Ltd" not in page["markdown"]
    assert page["truncated"] is False
    whole = ok("read_page", url=f"{base}/arc", main_only=False, max_chars=40)
    assert len(whole["markdown"]) == 40 and whole["truncated"] is True


def test_chunks_cut_the_page_at_its_headings(base):
    found = ok("chunks", url=f"{base}/arc", max_chars=200)["result"]
    assert [c["headings"] for c in found] == [["Arc lamp"], ["Arc lamp", "Care"]]
    assert all(len(c["text"]) <= 200 for c in found)


def test_extract_reads_what_the_page_is(base):
    got = ok("extract", url=f"{base}/arc")
    assert got["type"] == "product"
    assert (got["item"]["name"], got["item"]["price"]) == ("Arc lamp", "49.00")
    assert call("extract", url=f"{base}/arc", type="recipe").is_error


def test_select_and_links(base):
    assert ok("select", url=f"{base}/arc", css="h2::text")["result"] == ["Care"]
    assert ok("select", url=f"{base}/arc", css="p.lead", max_results=1)["result"][0].startswith(
        "A tall lamp"
    )
    links = ok("links", url=f"{base}/arc")["result"]
    assert [link["url"] for link in links] == [f"{base}/", f"{base}/lamps"]
    assert links[1]["text"] == "Lamps"


def test_a_page_that_isnt_there_is_a_tool_error(base):
    result = call("read_page", url=f"{base}/nowhere")
    assert result.is_error
    assert "404" in result.content[0].text
    assert call("select", url=f"{base}/arc", css="p[").is_error


def test_without_the_sdk_it_says_what_to_install(monkeypatch):
    monkeypatch.setitem(sys.modules, "mcp.server", None)
    with pytest.raises(ImportError, match=r"netweir\[mcp\]"):
        _mcp.server()


def test_only_web_pages_are_fetched(base):
    for url in ("file:///etc/hosts", "view-source:https://example.com/", "ftp://example.com/"):
        for browser in (False, True):
            result = call("read_page", url=url, browser=browser)
            assert result.is_error, url
            assert "only http and https" in result.content[0].text
    result = call("chunks", url=f"{base}/manual.pdf")
    assert result.is_error
    assert "application/pdf, not a web page" in result.content[0].text


def test_limits_are_checked_before_fetching(base):
    for tool, arguments in [
        ("read_page", {"max_chars": -5}),
        ("chunks", {"max_chars": 0}),
        ("select", {"css": "p", "max_results": -1}),
        ("links", {"max_results": 0}),
    ]:
        assert call(tool, url=f"{base}/arc", **arguments).is_error, (tool, arguments)


def test_a_type_with_nothing_to_read_is_a_tool_error(base):
    result = call("extract", url=f"{base}/empty", type="product")
    assert result.is_error
    assert "nothing that reads as a product" in result.content[0].text


class FakeTab:
    def __init__(self, html):
        self.html, self.url = html, None

    async def goto(self, url):
        self.url = url
        return types.SimpleNamespace(
            status=200, headers=[("Content-Type", "text/html; charset=iso-8859-1")]
        )

    async def content(self):
        return self.html

    async def close(self):
        pass


class FakeChrome:
    started = 0

    def __init__(self, dies):
        FakeChrome.started += 1
        self.dies = dies

    async def new_page(self):
        if self.dies:
            raise netweir.BrowserError("the browser has closed")
        return FakeTab("<html><body><main><h1>Café</h1><p>Crème brûlée, à la carte.</p></main>")


def test_the_browser_is_the_engine_asked_for(monkeypatch):
    asked = []
    monkeypatch.setattr(FakeChrome, "started", 0)

    async def browser(**kwargs):
        asked.append(kwargs["engine"])
        return FakeChrome(dies=False)

    monkeypatch.setattr(netweir, "browser", browser)

    async def go(engine):
        async with Client(_mcp.server(engine=engine)) as client:
            return await client.call_tool("read_page", {"url": "https://e.com/", "browser": True})

    assert not asyncio.run(go("firefox")).is_error
    assert asked == ["firefox"]
    with pytest.raises(ValueError, match="a proxy can't go with Firefox"):
        _mcp.server(proxy="http://127.0.0.1:1", engine="firefox")
    with pytest.raises(ValueError, match='engine must be "chrome" or "firefox"'):
        _mcp.server(engine="safari")


def test_chrome_pages_read_as_utf8_and_a_dead_chrome_is_replaced(monkeypatch):
    async def browser(**kwargs):
        return FakeChrome(dies=FakeChrome.started == 0)

    monkeypatch.setattr(netweir, "browser", browser)

    async def go():
        async with Client(_mcp.server()) as client:
            first = await client.call_tool("read_page", {"url": "https://e.com/", "browser": True})
            second = await client.call_tool("read_page", {"url": "https://e.com/", "browser": True})
            return first, second

    first, second = asyncio.run(go())
    assert first.is_error
    assert not second.is_error, second.content
    assert second.structured_content["markdown"].startswith("# Café")
    assert FakeChrome.started == 2
