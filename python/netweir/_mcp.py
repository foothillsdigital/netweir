"""``netweir mcp``: Netweir's reading as tools for an AI agent, over the
Model Context Protocol on stdin and stdout. Needs the ``mcp`` package
(the ``netweir[mcp]`` extra)."""

from __future__ import annotations

import asyncio
import sys
from typing import Annotated, Any
from urllib.parse import urlsplit


def _auto_types() -> dict[str, Any]:
    """The auto-extracted item types, by the name ``page_type()`` gives
    them (``product``, ``job-posting``, ...)."""
    from netweir import auto

    return {
        cls._schema: cls
        for cls in vars(auto).values()
        if isinstance(cls, type) and getattr(cls, "_schema", None)
    }


INSTRUCTIONS = """\
Netweir fetches web pages the way Chrome does and reads them. Start with
read_page for a page's text as Markdown. Use chunks to cut a long page
into pieces with their headings, extract for a product, job posting,
article, listing or forum thread as structured data, select for what a
CSS selector finds, and links for where a page leads. Set browser=true
for a page that builds itself in JavaScript; it's slower."""


try:
    from pydantic import Field
except ImportError:  # without the extra; _module() says what to install
    Count = Results = Overlap = int
else:
    # Limits the SDK checks before a tool runs. At module level, where it
    # reads the tools' annotations.
    Count = Annotated[int, Field(ge=1, le=1_000_000)]
    Results = Annotated[int, Field(ge=1, le=10_000)]
    Overlap = Annotated[int, Field(ge=0, le=1_000_000)]

#: What a page's Content-Type may be: HTML, or text read as such. A
#: response without one is read too, as a browser would sniff it.
_READABLE = ("text/html", "application/xhtml+xml", "text/plain")


def _module():
    try:
        from mcp.server import MCPServer
        from mcp.server.mcpserver.exceptions import ToolError
    except ImportError:
        raise ImportError("netweir mcp needs the MCP SDK: pip install 'netweir[mcp]'") from None
    return MCPServer, ToolError


def server(
    profile: str = "chrome",
    proxy: str | None = None,
    timeout: float = 30.0,
    engine: str = "chrome",
):
    """The MCP server, its tools fetching with a ``netweir.Client`` made
    from ``profile``, ``proxy`` and ``timeout``, and with one browser
    (Chrome, or Firefox with ``engine="firefox"``), started the first time
    a tool asks for it."""
    if engine not in ("chrome", "firefox"):
        raise ValueError(f'engine must be "chrome" or "firefox", not {engine!r}')
    if engine == "firefox" and proxy:
        raise ValueError("a proxy can't go with Firefox yet: Firefox takes no proxy")
    import netweir
    from netweir import Blocked, BrowserError, FetchError
    from netweir._cli import _utf8
    from netweir._native import _response

    MCPServer, ToolError = _module()
    mcp = MCPServer("netweir", instructions=INSTRUCTIONS)
    client = netweir.Client(profile, proxy, timeout)
    chrome: dict[str, Any] = {}

    async def in_chrome(url: str) -> netweir.Page:
        if "browser" not in chrome:
            chrome["browser"] = await netweir.browser(proxy=proxy, timeout=timeout, engine=engine)
        try:
            tab = await chrome["browser"].new_page()
        except BrowserError:
            # Chrome went away (it crashed, or was closed): the next call
            # starts another.
            chrome.pop("browser", None)
            raise
        try:
            response = await tab.goto(url)
            content = (await tab.content()).encode()
            headers = _utf8(response.headers)
            return netweir.Page(_response(tab.url, response.status, "browser", headers, content))
        finally:
            try:
                await tab.close()
            except BrowserError:
                chrome.pop("browser", None)

    async def fetch(url: str, browser: bool) -> netweir.Page:
        if "://" not in url:
            url = "https://" + url
        # Chrome opens file: and other schemes the HTTP client refuses,
        # and a page an agent reads could ask it for a local file.
        if urlsplit(url).scheme.lower() not in ("http", "https"):
            raise ToolError(f"{url} isn't a web page: only http and https URLs are fetched")
        try:
            page = await in_chrome(url) if browser else await client.get(url)
        except Blocked as e:
            raise ToolError(f"{url} is behind bot protection ({e}); try browser=true") from None
        except (FetchError, BrowserError) as e:
            raise ToolError(f"{url} couldn't be fetched: {e}") from None
        if page.status >= 400:
            raise ToolError(f"{url} answered {page.status}")
        kind = (page.headers.get("content-type") or "").split(";")[0].strip().lower()
        if kind and kind not in _READABLE:
            raise ToolError(f"{url} is {kind}, not a web page")
        return page

    async def off_loop(work):
        """``work()`` on a thread, so parsing a big page doesn't hold up
        the other calls."""
        return await asyncio.to_thread(work)

    @mcp.tool()
    async def read_page(
        url: str, main_only: bool = True, max_chars: Count = 20_000, browser: bool = False
    ) -> dict[str, Any]:
        """A web page as Markdown: its main content (the article, post,
        product description or docs page, without menus, ads and
        comments), or the whole page with main_only=false or when it has
        no main content. Cut at max_chars, with truncated=true. Also its
        final URL, title and page_type (product, article, job-posting,
        ...; other when it's none)."""
        page = await fetch(url, browser)

        def read() -> dict[str, Any]:
            markdown = (page.markdown(main_only=True) if main_only else None) or page.markdown()
            return {
                "url": page.url,
                "title": page.css("title::text").get(),
                "page_type": page.page_type(),
                "markdown": markdown[:max_chars],
                "truncated": len(markdown) > max_chars,
            }

        return await off_loop(read)

    @mcp.tool()
    async def chunks(
        url: str, max_chars: Count = 2000, overlap: Overlap = 0, browser: bool = False
    ) -> list[dict[str, Any]]:
        """A web page's main content as Markdown, cut at its headings into
        chunks of at most max_chars characters, each with its id, text,
        headings (the path of headings above it) and index."""
        page = await fetch(url, browser)
        try:
            found = await off_loop(lambda: page.chunks(max_chars, overlap=overlap))
        except ValueError as e:
            raise ToolError(str(e)) from None
        return [c.to_dict() for c in found]

    @mcp.tool()
    async def extract(url: str, type: str = "auto", browser: bool = False) -> dict[str, Any]:
        """A web page read as structured data, without selectors: a
        product (name, price, currency, availability, brand, ...), a
        product-list, a job-posting, an article, an article-list, a
        forum-thread, a business-place (address, phone, opening hours), a
        real-estate listing, a social-media-post, a serp (search
        results), page-content, or a navigation page (its items and next
        page). type="auto" reads it as whatever it looks like."""
        types = _auto_types()
        if type != "auto" and type not in types:
            raise ToolError(f"type is auto or one of {', '.join(types)}, not {type!r}")
        page = await fetch(url, browser)
        if type == "auto":
            type = await off_loop(page.page_type)
            if type == "other":
                raise ToolError(
                    f"{url} doesn't look like a product, listing, job, article or thread; "
                    "give type to read it as one, or use read_page"
                )
        cls = types[type]
        found = await off_loop(lambda: cls.extract(page))
        if all(v in (None, [], {}, "") for k, v in found.items() if k not in ("url", "metadata")):
            raise ToolError(f"{url} has nothing that reads as a {type}")
        return {"type": type, "item": found}

    @mcp.tool()
    async def select(
        url: str, css: str, max_results: Results = 50, browser: bool = False
    ) -> list[str]:
        """What a CSS selector finds on a web page, up to max_results: each
        element's text as a reader sees it, or the strings ::text and
        ::attr(name) give."""
        page = await fetch(url, browser)

        def run() -> list[str]:
            found = page.css(css)
            return [h if isinstance(h, str) else h.clean_text() for h in found[:max_results]]

        try:
            return await off_loop(run)
        except ValueError as e:
            raise ToolError(f"{css!r} isn't a selector Netweir reads: {e}") from None

    @mcp.tool()
    async def links(
        url: str, max_results: Results = 200, browser: bool = False
    ) -> list[dict[str, str]]:
        """The links on a web page, up to max_results, each with its
        absolute url and its text, in page order, without repeats."""
        page = await fetch(url, browser)

        def run() -> list[dict[str, str]]:
            seen: set[str] = set()
            out = []
            for a in page.css("a[href]"):
                href = page.urljoin(a.attrs.get("href", ""))
                if not href.startswith(("http://", "https://")) or href in seen:
                    continue
                seen.add(href)
                out.append({"url": href, "text": a.clean_text()})
                if len(out) >= max_results:
                    break
            return out

        return await off_loop(run)

    return mcp


def main(args) -> int:
    """Runs the server on stdin and stdout until its client goes."""
    try:
        mcp = server(args.profile or "chrome", args.proxy, args.timeout, args.engine or "chrome")
    except (ImportError, ValueError) as e:
        print(f"netweir: {e}", file=sys.stderr)
        return 2
    mcp.run()
    return 0
