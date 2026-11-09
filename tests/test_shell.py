"""netweir fetch, view and shell: the page netweir gets, from the command line."""

import json
import os
import signal
import subprocess
import sys
import threading
import time
import types
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

import netweir
from netweir import _cli

BLOCKS = Path(__file__).parent.parent / "crates/netweir-core/tests/fixtures/blocks"
BOM = b"\xef\xbb\xbf"
PAGE = (
    b"<html><head><title>Shop</title><link rel=stylesheet href=/style.css></head>"
    b"<body><h1>Books</h1><a href='/b'>b</a></body></html>"
)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    #: Set once /slow has been asked for.
    slow = threading.Event()

    def log_message(self, *args):
        pass

    def send(self, status, body, headers=()):
        self.send_response(status)
        for k, v in headers:
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path == "/page":
            self.send(200, PAGE, [("Content-Type", "text/html"), ("X-Seen", "a"), ("X-Seen", "b")])
        elif self.path == "/b":
            self.send(200, b"<h1>Second</h1>", [("Content-Type", "text/html")])
        elif self.path == "/shop/based":
            # A relative base, as Angular writes it, and UTF-8 said only in
            # the header.
            body = "<html><head><base href='/'></head><body>café</body></html>".encode()
            self.send(200, body, [("Content-Type", "text/html; charset=utf-8")])
        elif self.path == "/doctype":
            body = b"<!DOCTYPE html><html lang=en><title>t</title><p>x</p></html>"
            self.send(200, body, [("Content-Type", "text/html")])
        elif self.path == "/script-head":
            body = b"<p>x</p><script>document.write('<head>')</script>"
            self.send(200, body, [("Content-Type", "text/html")])
        elif self.path == "/latin-heading":
            body = "<h1>Café crème</h1><p>Un café.</p>".encode("latin-1")
            self.send(200, body, [("Content-Type", "text/html; charset=ISO-8859-1")])
        elif self.path == "/latin":
            body = "<p>café</p>".encode("latin-1")
            self.send(200, body, [("Content-Type", "text/html; charset=ISO-8859-1")])
        elif self.path == "/slow":
            Handler.slow.set()
            time.sleep(20)
            self.send(200, b"late")
        elif self.path == "/big":
            self.send(200, b"x" * 5_000_000, [("Content-Type", "text/plain")])
        elif self.path.startswith("http://proxied.test/"):
            # A request that came through this server as a proxy.
            self.send(200, b"via proxy", [("Content-Type", "text/plain")])
        elif self.path == "/search":
            body = b"<form action=/form method=post><input name=q><input type=submit name=go value=1></form>"
            self.send(200, body, [("Content-Type", "text/html")])
        elif self.path == "/scripted":
            body = b"<html><head></head><body><ul id=l></ul><script>l.innerHTML = '<li>caf\xc3\xa9</li>'</script></body></html>"
            self.send(200, body, [("Content-Type", "text/html; charset=utf-8")])
        elif self.path == "/headless":
            self.send(200, b"<p>no head</p>", [("Content-Type", "text/html")])
        elif self.path == "/redirect":
            self.send(302, b"", [("Location", "/page")])
        elif self.path == "/set-cookie":
            self.send(200, b"ok", [("Set-Cookie", "session=abc; Path=/")])
        elif self.path == "/echo":
            body = json.dumps(dict(self.headers.items())).encode()
            self.send(200, body, [("Content-Type", "application/json")])
        elif self.path == "/blocked":
            raw = (BLOCKS / "cloudflare-block.http").read_bytes()
            head, body = raw.split(b"\r\n\r\n", 1)
            lines = head.decode().split("\r\n")
            headers = [tuple(line.split(": ", 1)) for line in lines[1:]]
            headers = [(k, v) for k, v in headers if k.lower() != "content-length"]
            self.send(int(lines[0].split(" ")[1]), body, headers)
        else:
            self.send(404, b"<h1>not here</h1>", [("Content-Type", "text/html")])

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        self.send(200, self.rfile.read(length), [("Content-Type", "text/plain")])


@pytest.fixture(scope="module")
def base():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()
    server.server_close()


def saved(url: str) -> Path:
    """The file behind a file: URL, on any platform."""
    return Path(urllib.request.url2pathname(urllib.parse.urlparse(url).path))


@pytest.fixture
def opened(monkeypatch):
    """The paths view() would have opened in a browser."""
    paths = []
    monkeypatch.setattr(_cli.webbrowser, "open", lambda url, *a, **k: paths.append(url) or True)
    return paths


# --- fetch -----------------------------------------------------------------


def test_fetch_writes_the_body_unchanged(base, capsysbinary):
    assert _cli.main(["fetch", f"{base}/page"]) == 0
    assert capsysbinary.readouterr().out == PAGE


def test_fetch_headers_writes_the_status_and_headers_in_order(base, capsys):
    assert _cli.main(["fetch", "--headers", f"{base}/page"]) == 0
    lines = capsys.readouterr().out.splitlines()
    assert lines[0] == "HTTP/1.1 200"
    seen = [line for line in lines if line.lower().startswith("x-seen:")]
    assert seen == ["x-seen: a", "x-seen: b"]


def test_fetch_follows_redirects(base, capsysbinary):
    assert _cli.main(["fetch", f"{base}/redirect"]) == 0
    assert capsysbinary.readouterr().out == PAGE


def test_fetch_sends_extra_headers(base, capsysbinary):
    assert _cli.main(["fetch", "--header", "X-Token: 42", f"{base}/echo"]) == 0
    sent = {k.lower(): v for k, v in json.loads(capsysbinary.readouterr().out).items()}
    assert sent["x-token"] == "42"


def test_a_404_is_still_a_page(base, capsysbinary):
    assert _cli.main(["fetch", f"{base}/missing"]) == 0
    assert capsysbinary.readouterr().out == b"<h1>not here</h1>"


def test_a_block_is_shown_but_said_and_fails(base, capsysbinary):
    assert _cli.main(["fetch", f"{base}/blocked"]) == 1
    out = capsysbinary.readouterr()
    assert out.out  # the block page itself
    assert b"blocked" in out.err and b"cloudflare" in out.err


def test_a_failed_fetch_is_one_line_and_status_1(capsys):
    assert _cli.main(["fetch", "--timeout", "2", "http://127.0.0.1:1/"]) == 1
    err = capsys.readouterr().err
    assert err.startswith("netweir: ") and err.count("\n") == 1


@pytest.mark.parametrize(
    "argv",
    [
        ["fetch", "--profile", "netscape", "http://example.com/"],
        ["fetch", "--header", "no colon", "http://example.com/"],
        ["fetch", "--timeout", "0", "http://example.com/"],
    ],
)
def test_command_line_mistakes_are_status_2(argv, capsys):
    assert _cli.main(argv) == 2
    assert capsys.readouterr().err.startswith("netweir: ")


def in_chrome(argv, capture):
    """Runs a --browser command; skipped when there's no Chrome, unless
    NETWEIR_REQUIRE_CHROME is set."""
    status = _cli.main(argv)
    out = capture.readouterr()
    err = out.err.decode() if isinstance(out.err, bytes) else out.err
    if status == 1 and "no Chrome found" in err and not os.environ.get("NETWEIR_REQUIRE_CHROME"):
        pytest.skip(err)
    return status, out


def test_fetch_browser_gives_the_html_after_scripts(base, capsysbinary):
    status, out = in_chrome(["fetch", "--browser", f"{base}/scripted"], capsysbinary)
    assert status == 0
    assert "<li>café</li>" in out.out.decode()


def test_a_latin1_page_read_in_chrome_isnt_garbled(base, capsys):
    # Chrome gives its HTML as text; the page's own charset no longer
    # applies to it.
    argv = ["extract", "--browser", "--type", "article", f"{base}/latin-heading"]
    status, out = in_chrome(argv, capsys)
    assert status == 0
    assert "Café crème" in out.out


def test_fetch_browser_headers(base, capsys):
    status, out = in_chrome(["fetch", "--browser", "--headers", f"{base}/page"], capsys)
    assert status == 0
    lines = out.out.splitlines()
    assert lines[0] == "browser 200"
    # What's written is Chrome's HTML, in UTF-8 whatever the page's own
    # charset was, and the header says so.
    assert "content-type: text/html; charset=utf-8" in [line.lower() for line in lines]


def test_view_browser_says_its_html_is_utf8(base, opened, capsys):
    status, _ = in_chrome(["view", "--browser", f"{base}/scripted"], capsys)
    assert status == 0
    html = saved(opened[0]).read_bytes()
    assert html.startswith(BOM + b'<html><head><meta charset="utf-8"><base href=')
    assert "<li>café</li>" in html.decode()
    # A page with a <base> of its own gets the charset too.
    status, _ = in_chrome(["view", "--browser", f"{base}/shop/based"], capsys)
    assert status == 0
    assert saved(opened[1]).read_bytes().startswith(BOM + b'<html><head><meta charset="utf-8">')


def test_fetch_browser_renders_a_file(tmp_path, capsysbinary):
    path = tmp_path / "scripted.html"
    path.write_bytes(b"<ul id=l></ul><script>l.innerHTML = '<li>made</li>'</script>")
    status, out = in_chrome(["fetch", "--browser", str(path)], capsysbinary)
    assert status == 0
    assert b"<li>made</li>" in out.out


def test_browser_with_profile_or_header_is_a_mistake(capsys):
    assert _cli.main(["fetch", "--browser", "--profile", "firefox", "http://example.com/"]) == 2
    assert _cli.main(["view", "--browser", "--header", "A: b", "http://example.com/"]) == 2


def test_engine_picks_the_browser_and_is_a_browser_fetch(monkeypatch, capsys):
    asked = []

    class Started(Exception):
        pass

    def browser(**kwargs):
        asked.append(kwargs["engine"])
        raise Started

    monkeypatch.setattr(netweir, "browser", browser)
    for argv in (
        ["fetch", "--browser", "http://example.com/"],
        ["fetch", "--engine", "firefox", "http://example.com/"],
        ["view", "--browser", "--engine", "chrome", "http://example.com/"],
    ):
        with pytest.raises(Started):
            _cli.main(argv)
    assert asked == ["chrome", "firefox", "chrome"]
    argv = ["fetch", "--engine", "firefox", "--proxy", "http://127.0.0.1:1", "http://example.com/"]
    assert _cli.main(argv) == 2
    assert "--proxy can't go with --engine firefox" in capsys.readouterr().err
    with pytest.raises(SystemExit):
        _cli.main(["fetch", "--engine", "safari", "http://example.com/"])


def test_fetch_in_firefox_gives_the_html_after_scripts(base, capsysbinary):
    status = _cli.main(["fetch", "--engine", "firefox", f"{base}/scripted"])
    out = capsysbinary.readouterr()
    if status == 1 and b"no Firefox" in out.err and not os.environ.get("NETWEIR_REQUIRE_FIREFOX"):
        pytest.skip(out.err.decode())
    assert status == 0, out.err
    assert "<li>café</li>" in out.out.decode()


# --- view ------------------------------------------------------------------


def tags(base_href):
    return f'<meta charset="utf-8"><base href="{base_href}">'


def test_view_opens_the_page_with_a_base(base, opened, capsys):
    assert _cli.main(["view", f"{base}/redirect"]) == 0
    [url] = opened
    path = saved(url)
    html = path.read_text(encoding="utf-8-sig")
    # The final URL, first thing in <head>, so relative links still load.
    assert html.startswith(f"<html><head>{tags(base + '/page')}")
    assert "<h1>Books</h1>" in html
    assert str(path) in capsys.readouterr().out


def test_view_resolves_a_page_s_own_relative_base(base, opened):
    assert _cli.main(["view", f"{base}/shop/based"]) == 0
    html = saved(opened[0]).read_text(encoding="utf-8-sig")
    # Ours comes first and wins; it's the page's own base, resolved, where
    # the page's <base href="/"> would mean the root of the disk.
    assert html.startswith(f"<html><head>{tags(base + '/')}<base href='/'>")


def test_view_keeps_the_charset_the_header_gave(base, opened):
    for path in ("/shop/based", "/latin"):
        assert _cli.main(["view", f"{base}{path}"]) == 0
        assert "café" in saved(opened[-1]).read_text(encoding="utf-8-sig")


def test_view_adds_tags_to_a_page_without_head(base, opened):
    assert _cli.main(["view", f"{base}/headless"]) == 0
    assert (
        saved(opened[0])
        .read_text(encoding="utf-8-sig")
        .startswith(tags(f"{base}/headless") + "<p>")
    )


def test_view_keeps_the_doctype_first(base, opened):
    assert _cli.main(["view", f"{base}/doctype"]) == 0
    html = saved(opened[0]).read_text(encoding="utf-8-sig")
    assert html.startswith(f"<!DOCTYPE html><html lang=en>{tags(base + '/doctype')}<title>")


def test_view_ignores_a_head_tag_in_a_script(base, opened):
    assert _cli.main(["view", f"{base}/script-head"]) == 0
    assert saved(opened[0]).read_text(encoding="utf-8-sig").startswith(tags(f"{base}/script-head"))


def test_view_writes_a_byte_order_mark(base, opened):
    # It outranks any charset signal, a <meta> past the first 1024 bytes too.
    assert _cli.main(["view", f"{base}/latin"]) == 0
    assert saved(opened[0]).read_bytes().startswith(BOM + b"<meta charset")


def test_view_of_a_block_fails(base, opened):
    assert _cli.main(["view", f"{base}/blocked"]) == 1
    assert opened  # shown all the same


# --- shell -----------------------------------------------------------------


def test_shell_c_prints_the_expression(base, capsys):
    assert _cli.main(["shell", f"{base}/page", "-c", "page.css('h1::text').get()"]) == 0
    assert capsys.readouterr().out == "Books\n"


def test_shell_fetch_replaces_page(base, capsys):
    code = f"(fetch('{base}/b'), page.css('h1::text').get())[1]"
    assert _cli.main(["shell", f"{base}/page", "-c", code]) == 0
    assert capsys.readouterr().out == "Second\n"


def test_shell_fetch_keeps_cookies(base, capsys):
    code = f"fetch('{base}/echo').text"
    assert _cli.main(["shell", f"{base}/set-cookie", "-c", code]) == 0
    assert "session=abc" in capsys.readouterr().out


def test_shell_fetch_posts(base, capsys):
    code = f"fetch('{base}/form', method='POST', form={{'q': 'maps'}}).text"
    assert _cli.main(["shell", "-c", code]) == 0
    assert capsys.readouterr().out == "q=maps\n"


def test_shell_without_url_starts_with_no_page(capsys):
    assert _cli.main(["shell", "-c", "page"]) == 0
    assert capsys.readouterr().out == "None\n"


def test_shell_reads_a_local_file(tmp_path, capsys):
    path = tmp_path / "saved.html"
    path.write_bytes(PAGE)
    assert _cli.main(["shell", str(path), "-c", "page.css('title::text').get()"]) == 0
    assert capsys.readouterr().out == "Shop\n"
    assert _cli.main(["shell", str(path), "-c", "(page.version, page.url)"]) == 0
    assert capsys.readouterr().out == f"('file', {path.resolve().as_uri()!r})\n"


def test_shell_view_opens_the_page(base, opened, capsys):
    assert _cli.main(["shell", f"{base}/page", "-c", "view()"]) == 0
    assert "<h1>Books</h1>" in saved(opened[0]).read_text(encoding="utf-8-sig")


def test_shell_c_error_is_a_traceback_and_status_1(base, capsys):
    assert _cli.main(["shell", f"{base}/page", "-c", "page.nothing_here()"]) == 1
    assert "AttributeError" in capsys.readouterr().err


def test_shell_shows_a_block_and_says_so(base, capsys):
    assert _cli.main(["shell", f"{base}/blocked", "-c", "page.blocked"]) == 0
    out = capsys.readouterr()
    assert out.out == "cloudflare\n"
    assert "blocked" in out.err


def test_interactive_shell_gets_the_names(base, monkeypatch, capsys):
    seen = {}

    def interact(banner, local, exitmsg):
        seen.update(local)
        print(banner)

    monkeypatch.setitem(sys.modules, "IPython", None)  # as if not installed
    monkeypatch.setattr(_cli.code, "interact", interact)
    assert _cli.main(["shell", f"{base}/page"]) == 0
    assert isinstance(seen["page"], netweir.Page)
    assert seen["netweir"] is netweir
    assert callable(seen["fetch"]) and callable(seen["view"]) and callable(seen["submit"])
    assert "fetch(url)" in capsys.readouterr().out


def test_shell_submit_sends_a_form(base, capsys):
    code = "submit(page.form(data={'q': 'maps'})).text"
    assert _cli.main(["shell", f"{base}/search", "-c", code]) == 0
    assert capsys.readouterr().out == "q=maps&go=1\n"


def test_shell_bad_url_is_status_1(capsys):
    assert _cli.main(["shell", "http://127.0.0.1:1/", "--timeout", "2", "-c", "page"]) == 1
    assert capsys.readouterr().err.startswith("netweir: ")


def test_shell_reads_a_relative_path_and_a_file_url(tmp_path, monkeypatch, capsys):
    (tmp_path / "saved.html").write_bytes(PAGE)
    monkeypatch.chdir(tmp_path)
    assert _cli.main(["shell", "saved.html", "-c", "page.css('h1::text').get()"]) == 0
    uri = (tmp_path / "saved.html").resolve().as_uri()
    assert _cli.main(["shell", uri, "-c", "page.css('h1::text').get()"]) == 0
    assert capsys.readouterr().out == "Books\nBooks\n"
    assert _cli.main(["fetch", "saved.html"]) == 0  # fetch and view take files too


def test_a_host_without_a_scheme_gets_https(monkeypatch):
    sent = []
    monkeypatch.setattr(_cli._Session, "send", lambda self, url, *a, **k: sent.append(url))
    hosts = [
        "example.com/books?page=2",
        "localhost:8000",
        "127.0.0.1",
        "bücher.de",
        # A file ending after a port or a path is still a page on a host.
        "localhost:8000/index.html",
        "example.com/sitemap.xml",
        "shop.example.com:8443/feed.json",
    ]
    for host in hosts:
        assert _cli.main(["shell", host, "-c", "1"]) == 0
    assert sent == ["https://" + host for host in hosts]


@pytest.mark.parametrize(
    "target",
    [
        "ftp://example.com/",
        "nowhere",
        "file:///no/such/file.html",
        # Files that aren't there, not hosts.
        "page.html",
        "notes.txt",
        "data.json",
    ],
)
def test_targets_that_can_t_be_fetched_are_status_2(target, capsys):
    assert _cli.main(["fetch", target]) == 2
    assert capsys.readouterr().err.startswith("netweir: ")


def test_a_proxy_netweir_can_t_use_is_status_2(capsys):
    assert _cli.main(["fetch", "--proxy", "bogus://127.0.0.1:9", "http://example.com/"]) == 2
    assert "proxy" in capsys.readouterr().err


def test_a_header_that_can_t_be_sent_is_status_2(base, capsys):
    assert _cli.main(["fetch", "--header", "X: a\nb", f"{base}/page"]) == 2


def test_header_more_than_once(base, capsysbinary):
    argv = ["fetch", "--header", "X-A: 1", "--header", "X-B: 2", f"{base}/echo"]
    assert _cli.main(argv) == 0
    sent = {k.lower(): v for k, v in json.loads(capsysbinary.readouterr().out).items()}
    assert (sent["x-a"], sent["x-b"]) == ("1", "2")


def test_profile_and_proxy_are_used(base, capsysbinary):
    assert _cli.main(["fetch", "--profile", "firefox", f"{base}/echo"]) == 0
    sent = {k.lower(): v for k, v in json.loads(capsysbinary.readouterr().out).items()}
    assert "Firefox/" in sent["user-agent"]
    assert _cli.main(["fetch", "--proxy", base, "http://proxied.test/x"]) == 0
    assert capsysbinary.readouterr().out == b"via proxy"


def test_ipython_is_used_when_installed(base, monkeypatch, capsys):
    seen = {}
    fake = types.ModuleType("IPython")
    fake.start_ipython = lambda argv, user_ns: seen.update(argv=argv, ns=user_ns)
    monkeypatch.setitem(sys.modules, "IPython", fake)
    assert _cli.main(["shell", f"{base}/page"]) == 0
    assert seen["argv"] == ["--no-banner"]
    assert isinstance(seen["ns"]["page"], netweir.Page)
    assert "netweir" in capsys.readouterr().out  # the banner


def test_errors_at_the_prompt_aren_t_chained_to_ipython(monkeypatch):
    contexts = []

    def interact(banner, local, exitmsg):
        try:
            raise ValueError("typed at the prompt")
        except ValueError as e:
            contexts.append(e.__context__)

    monkeypatch.setitem(sys.modules, "IPython", None)
    monkeypatch.setattr(_cli.code, "interact", interact)
    assert _cli.main(["shell"]) == 0
    assert contexts == [None]


posix = pytest.mark.skipif(sys.platform == "win32", reason="POSIX signals and pipes")


def command(*args):
    return [sys.executable, "-m", "netweir", *args]


@posix
def test_ctrl_c_stops_a_fetch_at_once(base):
    Handler.slow.clear()
    proc = subprocess.Popen(command("fetch", f"{base}/slow"), stderr=subprocess.PIPE, text=True)
    # Mid-fetch, however long a loaded machine takes to start Python.
    assert Handler.slow.wait(60), "the fetch never reached the server"
    proc.send_signal(signal.SIGINT)
    started = time.monotonic()
    _, err = proc.communicate(timeout=10)
    assert time.monotonic() - started < 3  # not when the server answers, 18 s on
    assert proc.returncode == 130
    assert err == "netweir: interrupted\n"


@posix
@pytest.mark.parametrize(
    "args, reader",
    [
        (("fetch", "/big"), "head -c 10"),
        (("fetch", "--headers", "/big"), "true"),
        (("shell", "/page", "-c", "page.status"), "true"),
        (("view", "/page"), "true"),
    ],
)
def test_a_reader_that_stops_early_is_not_an_error(base, args, reader):
    args = [base + a if a.startswith("/") else a for a in args]
    # view's browser is told to open nothing.
    env = {**os.environ, "BROWSER": "true"}
    line = " ".join(f"'{a}'" for a in command(*args)) + f" | {reader}"
    out = subprocess.run(line, shell=True, capture_output=True, timeout=60, env=env)
    assert out.stderr == b""
