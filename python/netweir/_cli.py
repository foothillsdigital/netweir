"""The ``netweir`` command.

netweir crawl spider.py -o items.jsonl -s concurrency=32
netweir startproject books && cd books && netweir genspider shop shop.example
netweir crawl shop -o items.jsonl
netweir check
netweir parse https://shop.example/item/1 --spider shop -c item
netweir serve --project books=spiders/
netweir fetch https://example.com/ --headers
netweir view https://example.com/
netweir shell https://example.com/
netweir install chrome
netweir install firefox
netweir queue push redis://localhost:6379/0 books https://books.toscrape.com/
"""

from __future__ import annotations

import argparse
import code
import contextlib
import dataclasses
import html
import importlib.util
import inspect
import json
import logging
import os
import re
import signal
import sys
import tempfile
import threading
import time
import traceback
import urllib.parse
import urllib.request
import webbrowser
from pathlib import Path


class _Usage(Exception):
    """A mistake on the command line: reported in one line, exit status 2."""


def _load_spider(path: str, name: str | None):
    spiders = _spiders_in(path)
    if name:
        spiders = [s for s in spiders if s.__name__ == name or s.name == name]
    if len(spiders) != 1:
        found = ", ".join(s.__name__ for s in spiders) or "none"
        raise _Usage(
            f"{path}: expected one Spider{' named ' + name if name else ''}, found {found}"
        )
    return spiders[0]


def _spiders_in(path: str) -> list:
    """The Spider classes a file defines (not those it imports)."""
    from netweir import Spider

    if not Path(path).is_file():
        raise _Usage(f"no such file: {path}")
    # A name of its own, so a file called netweir.py can't shadow the
    # package.
    module_name = "netweir_spider_" + "".join(c if c.isalnum() else "_" for c in Path(path).stem)
    module = _exec_file(module_name, path)
    return [
        obj
        for obj in vars(module).values()
        if inspect.isclass(obj)
        and issubclass(obj, Spider)
        and obj is not Spider
        and obj.__module__ == module.__name__
    ]


#: The folder this process loads spider files from: one per process.
_loaded_folder: str | None = None
_load_lock = threading.RLock()


class _OtherFolder(_Usage):
    """A spider file from a second folder, in a process that has one."""


def _exec_file(module_name: str, path: str):
    """Runs a spider file as module ``module_name``, registered before it
    runs, as dataclasses and pickle look modules up by name.

    Its folder goes first on sys.path for the rest of the process, as when
    Python runs a script, so the files beside it (a project's settings.py,
    helpers) can be imported, at load time and later in callbacks. Their
    modules are cached by their bare names, so a process holds one
    project: a file from a second folder is refused. Commands given files
    from several folders run a process for each."""
    global _loaded_folder
    folder = str(Path(path).resolve().parent)
    with _load_lock:
        if _loaded_folder is None:
            sys.path.insert(0, folder)
            _loaded_folder = folder
        elif _loaded_folder != folder:
            raise _OtherFolder(
                f"{path} is in {folder}, but this process has loaded spiders from"
                f" {_loaded_folder}: one folder of spiders per process, as the modules"
                " beside them (settings.py, helpers) are shared by name"
            )
        spec = importlib.util.spec_from_file_location(module_name, path)
        if spec is None or spec.loader is None:
            raise _Usage(f"can't load {path}")
        module = importlib.util.module_from_spec(spec)
        sys.modules[module_name] = module
        try:
            spec.loader.exec_module(module)
        except BaseException:
            sys.modules.pop(module_name, None)
            raise
        return module


def _forget_loaded() -> None:
    """Undoes what loading spider files did to this process: for tests,
    which load spiders from many folders in one."""
    global _loaded_folder
    with _load_lock:
        if _loaded_folder is None:
            return
        folder = Path(_loaded_folder)
        for name, module in list(sys.modules.items()):
            file = getattr(module, "__file__", None)
            if name.startswith("netweir_spider_") or (file and Path(file).parent == folder):
                del sys.modules[name]
        while _loaded_folder in sys.path:
            sys.path.remove(_loaded_folder)
        _loaded_folder = None


def _parse_setting(text: str, fields: dict) -> tuple[str, object]:
    key, sep, raw = text.partition("=")
    if not sep or key not in fields:
        known = ", ".join(sorted(fields))
        raise ValueError(f"unknown setting {key!r}; settings are: {known}")
    # Settings' annotations are strings, such as "int | None".
    kind = str(fields[key].type)
    if kind.endswith(" | None"):
        if raw.lower() == "none":
            return key, None
        kind = kind.removesuffix(" | None")
    if key == "http_auth":
        # As curl's -u: a password may hold commas and colons.
        user, sep, password = raw.partition(":")
        if not sep:
            raise ValueError("http_auth takes user:password")
        return key, (user, password)
    if kind.startswith("bool |"):
        # verify: true or false, or a CA file's path.
        if raw.lower() in ("true", "false"):
            return key, raw.lower() == "true"
        return key, raw
    if kind == "bool":
        if raw.lower() not in ("true", "false", "1", "0", "yes", "no"):
            raise ValueError(f"{key} takes true or false, not {raw!r}")
        return key, raw.lower() in ("true", "1", "yes")
    if kind == "int":
        try:
            return key, int(raw)
        except ValueError:
            raise ValueError(f"{key} takes a whole number, not {raw!r}") from None
    if kind == "float":
        try:
            return key, float(raw)
        except ValueError:
            raise ValueError(f"{key} takes a number, not {raw!r}") from None
    if kind.startswith("tuple"):
        parts = tuple(part.strip() for part in raw.split(",") if part.strip())
        if kind.startswith("tuple[int"):
            try:
                return key, tuple(int(p) for p in parts)
            except ValueError:
                raise ValueError(
                    f"{key} takes whole numbers, such as 500,503, not {raw!r}"
                ) from None
        return key, parts
    return key, raw


def _login_from_environment(base, overrides: dict, fields: dict) -> None:
    """``http_auth`` from $NETWEIR_HTTP_AUTH (user:password), for a crawl
    with an ``http_auth_domain`` and no login of its own: a password on the
    command line is there for anyone to read in the process list and the
    shell's history. A variable exported for one project's crawls is left
    alone by the others, and never replaces a spider's login."""
    login = os.environ.get("NETWEIR_HTTP_AUTH")
    domain = overrides.get("http_auth_domain", base.http_auth_domain)
    if login and domain and "http_auth" not in overrides and base.http_auth is None:
        overrides["http_auth"] = _parse_setting(f"http_auth={login}", fields)[1]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="netweir")
    commands = parser.add_subparsers(dest="command", required=True)
    crawl = commands.add_parser("crawl", aliases=["runspider"], help="run a spider")
    crawl.add_argument(
        "file",
        help="a Python file defining a Spider subclass, a spider's name in a project, "
        "or a template: ecommerce, article, job or docs",
    )
    crawl.add_argument(
        "--url",
        action="append",
        default=[],
        help="where a template starts (give it more than once for several)",
    )
    crawl.add_argument(
        "--strategy",
        choices=["automatic", "full", "navigation", "pagination_only", "direct_item"],
        help="how far a template goes (default automatic)",
    )
    crawl.add_argument(
        "-o", "--output", help="write items to a .jsonl, .json, .xml, .csv or .parquet file"
    )
    crawl.add_argument(
        "--batch",
        type=int,
        metavar="N",
        help="start a new output file every N items; name them with {batch} in -o",
    )
    crawl.add_argument(
        "-s", "--set", action="append", default=[], metavar="KEY=VALUE", help="override a setting"
    )
    crawl.add_argument(
        "-a",
        dest="arguments",
        action="append",
        default=[],
        metavar="NAME=VALUE",
        help="set an attribute of the spider, as a string",
    )
    crawl.add_argument("--stats", metavar="FILE", help="write the stats to FILE as JSON at the end")
    crawl.add_argument("--spider", help="which Spider, when the file has several")
    crawl.add_argument(
        "--queue",
        metavar="REDIS_URL",
        help="share the crawl with other processes through this Redis (redis://host:6379/0)",
    )
    crawl.add_argument(
        "--queue-key", metavar="KEY", help="the crawl's name in Redis (default: the spider's)"
    )
    crawl.add_argument("-q", "--quiet", action="store_true", help="only warnings and errors")
    queue = commands.add_parser("queue", help="feed, look at or clear a crawl's shared queue")
    queue.add_argument("action", choices=["push", "status", "clear"])
    queue.add_argument("redis", metavar="REDIS_URL", help="redis://host:6379/0")
    queue.add_argument("key", help="the crawl's queue_key")
    queue.add_argument("urls", nargs="*", metavar="URL", help="for push: URLs, or - to read stdin")
    queue.add_argument("--priority", type=int, default=0, help="for push: higher goes first")
    queue.add_argument(
        "--dont-filter", action="store_true", help="for push: queue URLs even if seen before"
    )
    fetch = commands.add_parser("fetch", help="fetch a page and write its body to stdout")
    fetch.add_argument("url")
    fetch.add_argument(
        "--headers", action="store_true", help="the status line and headers instead of the body"
    )
    _fetch_options(fetch, browser=True)
    extract = commands.add_parser(
        "extract",
        help="read a page's product, job, article or other item without selectors, as JSON",
    )
    extract.add_argument("url")
    extract.add_argument(
        "--type",
        default=None,
        choices=[
            "product",
            "product-list",
            "product-navigation",
            "job-posting",
            "job-posting-navigation",
            "article",
            "article-list",
            "article-navigation",
            "forum-thread",
            "business-place",
            "real-estate",
            "social-media-post",
            "serp",
            "page-content",
        ],
        help="what to read (default: what the page looks like)",
    )
    extract.add_argument(
        "--field-names",
        default="snake",
        choices=["snake", "zyte"],
        help="zyte writes the common schema's camelCase names",
    )
    _fetch_options(extract, browser=True)
    view = commands.add_parser("view", help="open the page Netweir gets in your browser")
    view.add_argument("url")
    _fetch_options(view, browser=True)
    shell = commands.add_parser("shell", help="a Python prompt with a fetched page in `page`")
    shell.add_argument("url", nargs="?", help="a URL to fetch, or an HTML file to read")
    shell.add_argument("-c", dest="code", help="evaluate one expression, print it and exit")
    _fetch_options(shell, browser=False)
    install = commands.add_parser("install", help="download a browser for Netweir to drive")
    install.add_argument("browser", choices=["chrome", "firefox"])
    install.add_argument(
        "--headless-shell",
        action="store_true",
        help="chrome-headless-shell instead: lighter, but easier to tell from a real browser",
    )
    serve = commands.add_parser(
        "serve", help="run spiders as jobs over an HTTP API, as scrapyd does"
    )
    serve.add_argument(
        "--project",
        action="append",
        default=[],
        metavar="NAME=PATH",
        help="a spider file, or a directory of them, to serve as project NAME",
    )
    serve.add_argument("--bind", default="127.0.0.1", help="the address to listen on (127.0.0.1)")
    serve.add_argument("--port", type=int, default=6800, help="the port to listen on (6800)")
    serve.add_argument(
        "--dir", default="netweir-serve", help="where jobs, logs and items are kept (netweir-serve)"
    )
    serve.add_argument(
        "--max-jobs", type=int, metavar="N", help="jobs to run at once (one per CPU)"
    )
    serve.add_argument(
        "--keep", type=int, default=100, metavar="N", help="finished jobs to remember (100)"
    )
    serve.add_argument(
        "--username",
        default=os.environ.get("NETWEIR_SERVE_USERNAME"),
        help="require HTTP basic auth (or NETWEIR_SERVE_USERNAME)",
    )
    serve.add_argument(
        "--password",
        default=os.environ.get("NETWEIR_SERVE_PASSWORD"),
        help="the password for --username (or NETWEIR_SERVE_PASSWORD)",
    )
    serve.add_argument(
        "--insecure-no-auth",
        action="store_true",
        help="allow --bind to a non-loopback address without a username and password",
    )
    mcp = commands.add_parser(
        "mcp", help="serve Netweir's reading to an AI agent over MCP, on stdin and stdout"
    )
    mcp.add_argument("--profile", help="the browser to look like (default chrome)")
    mcp.add_argument("--proxy", help="send requests, and Chrome, through this proxy")
    mcp.add_argument("--timeout", type=float, default=30.0, help="seconds (default 30)")
    _engine_option(mcp, "the browser tools load pages in with browser=true (default chrome)")
    _project_commands(commands)
    args = parser.parse_args(argv)
    if args.command in ("startproject", "genspider", "list", "check", "parse", "version"):
        from netweir import _project

        return _project.main(args)
    if args.command == "serve":
        from netweir import _serve

        return _serve.main(args)
    if args.command == "install":
        return _install(args)
    if args.command == "mcp":
        from netweir import _mcp

        return _mcp.main(args)
    if args.command in ("fetch", "view", "shell", "extract"):
        return _page_command(args)
    if args.command == "queue":
        return _queue(args)

    logging.basicConfig(
        level=logging.WARNING if args.quiet else logging.INFO, format="%(levelname)s %(message)s"
    )
    try:
        spider, output = _prepare(args)
    except (_Usage, ValueError, OSError) as e:
        print(f"netweir: {e}", file=sys.stderr)
        return 2
    except Exception as e:
        # A project's spider file that raised as it was read.
        from netweir._project import _Failed

        if not isinstance(e, _Failed):
            raise
        print(f"netweir: {e}", file=sys.stderr)
        return 1
    from netweir import QueueError

    # A service manager's stop, or `kill`, ends the crawl as Ctrl-C does,
    # with the output files closed and the checkpoint saved. So does
    # Ctrl-Break on Windows, which is how `netweir serve` cancels a job
    # there: Windows has no SIGTERM to send.
    main_thread = threading.current_thread() is threading.main_thread()
    previous = {stop: signal.signal(stop, _interrupt) for stop in STOPS} if main_thread else {}
    reason = "error"
    from netweir._stats import PortTaken

    # The stats file is rewritten every few seconds while the crawl runs,
    # for whoever watches it (netweir serve's jobstats.json).
    stop_writing = threading.Event()
    writer = None
    if args.stats:
        writer = threading.Thread(
            target=_keep_writing, args=(args.stats, spider, stop_writing), daemon=True
        )
        writer.start()
    try:
        spider.run(output=output)
        reason = spider.finish_reason
    except PortTaken as e:
        print(f"netweir: {e}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        reason = "interrupted"
        print("netweir: interrupted", file=sys.stderr)
        return 130
    except QueueError as e:
        print(f"netweir: {e}", file=sys.stderr)
        return 1
    finally:
        for stop, handler in previous.items():
            # None for a handler that wasn't set from Python, which can't be put back.
            if handler is not None:
                signal.signal(stop, handler)
        if writer is not None:
            stop_writing.set()
            writer.join()
            _write_stats(args.stats, spider.stats, reason, tries=40)
    # A failed check is a failed crawl, for a scheduler or CI job.
    return 1 if spider.check_failures else 0


def _project_commands(commands) -> None:
    """Scrapy's project commands; they run in netweir._project."""
    start = commands.add_parser("startproject", help="make a project: spiders/ and a README")
    start.add_argument("name", help="the project's name")
    start.add_argument("dir", nargs="?", help="where to make it (./NAME)")
    gen = commands.add_parser("genspider", help="write a new spider from a template")
    gen.add_argument("name", nargs="?", help="the spider's name, and its file's")
    gen.add_argument("domain", nargs="?", help="the site's domain, or a URL to start from")
    gen.add_argument(
        "-t", "--template", default="basic", help="basic, crawl, sitemap, xmlfeed or csvfeed"
    )
    gen.add_argument("-l", "--list", action="store_true", help="list the templates")
    gen.add_argument("--force", action="store_true", help="write over a file that's there")
    commands.add_parser("list", help="the spiders in this project")
    check = commands.add_parser("check", help="run the contracts in callbacks' docstrings")
    check.add_argument("spiders", nargs="*", metavar="SPIDER", help="names or files (all)")
    check.add_argument("-l", "--list", action="store_true", help="list the contracted callbacks")
    check.add_argument("-v", "--verbose", action="store_true", help="a line for every contract")
    # A process checking one folder's spiders for another that started it.
    check.add_argument("--results", help=argparse.SUPPRESS)
    parse = commands.add_parser("parse", help="run a page through a spider's callback")
    parse.add_argument("url")
    parse.add_argument("--spider", help="a spider's name or file (the one whose domains cover URL)")
    parse.add_argument("-c", "--callback", help="the method to call (parse, or the rules)")
    parse.add_argument("-r", "--rules", action="store_true", help="let the rules pick the callback")
    parse.add_argument("-d", "--depth", type=int, default=1, help="levels of requests to follow")
    parse.add_argument("-m", "--meta", metavar="JSON", help="the request's meta")
    parse.add_argument("--cbkwargs", metavar="JSON", help="keyword arguments for the callback")
    parse.add_argument("--pipelines", action="store_true", help="send items through pipelines")
    parse.add_argument("--noitems", action="store_true", help="don't show items")
    parse.add_argument("--nolinks", action="store_true", help="don't show requests")
    parse.add_argument("-v", "--verbose", action="store_true", help="show every depth level")
    for command in (check, parse):
        command.add_argument(
            "-s", "--set", action="append", default=[], metavar="KEY=VALUE", help="a setting"
        )
        command.add_argument(
            "-a", dest="arguments", action="append", default=[], metavar="NAME=VALUE",
            help="set an attribute of the spider",
        )  # fmt: skip
    version = commands.add_parser("version", help="Netweir's version")
    version.add_argument(
        "-v", "--verbose", action="store_true", help="and Python's, Rust's, lexbor's, profiles"
    )


#: Seconds between rewrites of the --stats file while a crawl runs.
STATS_FILE_INTERVAL = 5.0


def _keep_writing(path: str, spider, stop: threading.Event) -> None:
    from netweir import Stats

    while not stop.wait(STATS_FILE_INTERVAL):
        # The class's empty dict until the crawl has started.
        if isinstance(spider.stats, Stats):
            # A write that fails is skipped; the next comes soon.
            with contextlib.suppress(OSError):
                _write_stats(path, spider.stats, None)


def _write_stats(path: str, stats, reason: str | None, tries: int = 1) -> None:
    """Writes the stats, with ``finish_reason`` (None while the crawl
    runs), in place of the file at ``path`` whole, so a reader never sees
    half of one."""
    # One snapshot, not one per key.
    stats = stats.get_stats() if hasattr(stats, "get_stats") else dict(stats)
    temporary = f"{path}.{os.getpid()}.tmp"
    try:
        with open(temporary, "w", encoding="utf-8") as f:
            # default=str: a Scrapy component's stats may hold a datetime.
            json.dump({**stats, "finish_reason": reason}, f, indent=2, default=str)
        for attempt in range(tries):
            try:
                os.replace(temporary, path)
                return
            except PermissionError:
                # Windows refuses while a reader has the file open.
                if attempt == tries - 1:
                    raise
                time.sleep(0.05)
    finally:
        with contextlib.suppress(OSError):
            os.remove(temporary)


def _queue(args) -> int:
    from netweir import Queue, QueueError

    if args.action == "push" and not args.urls:
        print(
            "netweir: queue push needs URLs, or - to read them from stdin",
            file=sys.stderr,
        )
        return 2
    if args.action != "push" and args.urls:
        print(f"netweir: queue {args.action} takes no URLs", file=sys.stderr)
        return 2
    try:
        queue = Queue(args.redis, args.key)
        if args.action == "push":
            sent = queued = 0
            # Read and sent a thousand at a time, so a long list from
            # standard input never sits in memory whole.
            lines = (line.strip() for line in sys.stdin) if args.urls == ["-"] else args.urls
            for chunk in _chunks((url for url in lines if url), 1000):
                queued += queue.push(chunk, priority=args.priority, dont_filter=args.dont_filter)
                sent += len(chunk)
            print(f"{queued} queued, {sent - queued} seen before")
        elif args.action == "status":
            for name, value in queue.status().items():
                print(f"{name}: {value}")
        else:
            queue.clear()
    except ValueError as e:
        print(f"netweir: {e}", file=sys.stderr)
        return 2
    except QueueError as e:
        print(f"netweir: {e}", file=sys.stderr)
        return 1
    return 0


def _chunks(items, size: int):
    chunk = []
    for item in items:
        chunk.append(item)
        if len(chunk) == size:
            yield chunk
            chunk = []
    if chunk:
        yield chunk


#: What asks a process to stop as Ctrl-C would.
STOPS = (signal.SIGTERM, signal.SIGBREAK) if sys.platform == "win32" else (signal.SIGTERM,)


def _interrupt(signum, frame) -> None:
    """Handles SIGTERM (and SIGBREAK) as whatever handles SIGINT now would:
    asyncio's handler, or the crawl's own on Python 3.10, while a crawl
    runs, which cancels it cleanly."""
    from netweir import _crawl

    handler = signal.getsignal(signal.SIGINT)
    if callable(handler):
        handler(signal.SIGINT, frame)
    elif _crawl._interrupts:
        # SIGINT ignored, as in a job started with `&` from a script. A
        # KeyboardInterrupt raised here could land inside asyncio's own
        # code and leave the crawl hanging.
        _crawl._interrupts[-1]()
    else:
        raise KeyboardInterrupt


def _install(args) -> int:
    from netweir import _native
    from netweir._errors import BrowserError

    if args.browser == "firefox" and args.headless_shell:
        print("netweir: --headless-shell is a Chrome build; Firefox has none", file=sys.stderr)
        return 2
    try:
        if args.browser == "firefox":
            what = "Firefox"
            print("netweir: installing Firefox...", file=sys.stderr)
            version, path, already = _native.install_firefox()
        else:
            what = "chrome-headless-shell" if args.headless_shell else "Chrome"
            print(f"netweir: installing {what} for Testing...", file=sys.stderr)
            version, path, already = _native.install_chrome(headless_shell=args.headless_shell)
    except BrowserError as e:
        print(f"netweir: {e}", file=sys.stderr)
        return 1
    print(f"{what} {version} {'was already' if already else 'is'} installed: {path}")
    return 0


def _prepare(args):
    """The spider and output the command line asks for, checked before the
    crawl starts, so a typo costs one line rather than a traceback."""
    from netweir import Settings, export
    from netweir._crawl import _check_outputs
    from netweir.templates import TEMPLATES

    template = None
    if Path(args.file).is_file() or args.file.endswith(".py"):
        spider_cls = _load_spider(args.file, args.spider)
    else:
        from netweir import _project

        if args.spider:
            raise _Usage(f"--spider picks from a file's spiders; {args.file} is a spider's name")
        if args.file in TEMPLATES and not _project.has_spider(args.file):
            spider_cls = template = TEMPLATES[args.file]
        else:
            spider_cls = _project.by_name(args.file)
    if template is not None and not args.url:
        raise _Usage(f"the {args.file} template needs --url: where to start")
    if template is None and (args.url or args.strategy):
        raise _Usage("--url and --strategy go with a template: ecommerce, article, job or docs")
    fields = {f.name: f for f in dataclasses.fields(Settings)}
    overrides = dict(_parse_setting(s, fields) for s in args.set)
    _login_from_environment(spider_cls.settings, overrides, fields)
    if args.queue is not None:
        overrides["queue"] = args.queue
    if args.queue_key is not None:
        overrides["queue_key"] = args.queue_key
    settings = dataclasses.replace(spider_cls.settings, **overrides)
    if args.batch is not None and not args.output:
        raise _Usage("--batch goes with -o")
    output = export.to_path(args.output, batch=args.batch) if args.output else None
    if args.stats and not Path(args.stats).resolve().parent.is_dir():
        raise _Usage(f"no such directory for --stats: {Path(args.stats).parent}")
    arguments = dict(_parse_argument(a) for a in args.arguments)
    if template is not None:
        arguments["url"] = " ".join(args.url)
        if args.strategy:
            arguments["strategy"] = args.strategy
    # Before the engine is made (it opens the checkpoint), so a checkpoint
    # with batches or a cloud path is one line and leaves nothing behind.
    _check_outputs(settings, [*spider_cls.pipelines, *([output] if output else [])])
    # The profile, proxy and limits are valid. Not the queue: an engine
    # with one would join the crawl, if only for a moment.
    dataclasses.replace(settings, queue=None)._engine()
    spider = spider_cls()
    spider.settings = settings
    for name, value in arguments.items():
        setattr(spider, name, value)
    # Worker processes build their own spider, and set these on it too.
    spider._arguments = arguments
    return spider, output


def _parse_argument(text: str) -> tuple[str, str]:
    name, sep, value = text.partition("=")
    if not sep:
        raise _Usage(f"-a takes NAME=VALUE, not {text!r}")
    if not name.isidentifier():
        raise _Usage(f"-a {name!r}: a spider argument's name must be a Python identifier")
    if name == "settings":
        raise _Usage("-a settings: use -s for settings")
    return name, value


# --- fetch, view and shell --------------------------------------------------


def _fetch_options(parser: argparse.ArgumentParser, browser: bool) -> None:
    parser.add_argument("--profile", help="the browser to look like (default chrome)")
    parser.add_argument("--proxy", help="send the request through this proxy")
    parser.add_argument("--timeout", type=float, default=30.0, help="seconds (default 30)")
    parser.add_argument(
        "--header",
        action="append",
        default=[],
        metavar="'NAME: VALUE'",
        help="add a request header",
    )
    if browser:
        parser.add_argument(
            "--browser", action="store_true", help="load the page in Chrome and take its HTML"
        )
        _engine_option(
            parser, "load the page in this browser: chrome or firefox (implies --browser)"
        )


def _engine_option(parser: argparse.ArgumentParser, help: str) -> None:
    parser.add_argument("--engine", choices=["chrome", "firefox"], help=help)


def _no_proxy_for_firefox(args) -> None:
    """Refuses --proxy with Firefox, which Netweir can't send through one yet."""
    if args.engine == "firefox" and args.proxy:
        raise _Usage("--proxy can't go with --engine firefox yet: Firefox takes no proxy")


def _page_command(args) -> int:
    from netweir import BrowserError, FetchError

    try:
        session = _Session(args)
        if args.command == "shell":
            status = _shell(args, session)
        else:
            page = session.first(args.url)
            if args.command == "fetch":
                _write(page, args.headers)
            elif args.command == "extract":
                _extract(page, args.type, args.field_names)
            else:
                _view(page)
            status = 1 if page.blocked else 0
        # Flushed here, where a closed pipe is caught, rather than as
        # Python exits, where it isn't.
        sys.stdout.flush()
        return status
    except KeyboardInterrupt:
        print("netweir: interrupted", file=sys.stderr)
        return 130
    except BrokenPipeError:
        # Whatever read stdout stopped early (`| head`): not an error, but
        # Python would complain on exit unless stdout goes somewhere.
        os.dup2(os.open(os.devnull, os.O_WRONLY), sys.stdout.fileno())
        return 141
    except FetchError as e:
        # A URL or header that can't be sent is a command-line mistake.
        print(f"netweir: {e}", file=sys.stderr)
        return 2 if e.kind == "invalid" else 1
    except (_Usage, ValueError) as e:
        print(f"netweir: {e}", file=sys.stderr)
        return 2
    except (BrowserError, OSError) as e:
        print(f"netweir: {e}", file=sys.stderr)
        return 1


def _extract(page, kind: str | None, field_names: str) -> None:
    import json

    from netweir import auto
    from netweir.export import _camel

    if kind is None:
        kind = page.page_type()
        if kind == "other":
            print(
                "netweir: this page doesn't look like any type netweir.auto reads; "
                "reading it as a product (--type picks another)",
                file=sys.stderr,
            )
            kind = "product"

    item = {
        "product": auto.Product,
        "product-list": auto.ProductList,
        "product-navigation": auto.ProductNavigation,
        "job-posting": auto.JobPosting,
        "article": auto.Article,
        "article-list": auto.ArticleList,
        "article-navigation": auto.ArticleNavigation,
        "job-posting-navigation": auto.JobPostingNavigation,
        "forum-thread": auto.ForumThread,
        "business-place": auto.BusinessPlace,
        "real-estate": auto.RealEstate,
        "social-media-post": auto.SocialMediaPost,
        "serp": auto.Serp,
        "page-content": auto.PageContent,
    }[kind].extract(page)
    if field_names == "zyte":
        item = _camel(item)
    print(json.dumps(item, indent=2, ensure_ascii=False))


def _write(page, headers: bool) -> None:
    if headers:
        print(f"{page.version} {page.status}")
        for name, value in page.raw_headers:
            print(f"{name}: {value}")
        sys.stdout.flush()
    else:
        sys.stdout.buffer.write(page.body)
        sys.stdout.buffer.flush()


# "example.com", "localhost:8000/path" or "127.0.0.1": a host without a
# scheme. A name ending in a file's extension ("page.html" that isn't
# there) is a missing file, not a host.
_HOST = re.compile(r"(localhost|[\w-]+(\.[\w-]+)+)(:\d+)?([/?#].*)?")
_FILE_ENDING = re.compile(r"\.(html?|xhtml|xml|json|txt|csv|md|py|js|css)$", re.I)


class _Session:
    """What fetch, view and shell fetch with: one Fetcher, so the shell's
    pages share cookies, plus the headers given on the command line."""

    def __init__(self, args):
        from netweir._native import Fetcher

        self.args = args
        self.engine = getattr(args, "engine", None)
        self.browser = getattr(args, "browser", False) or self.engine is not None
        self.headers = []
        for header in args.header:
            name, sep, value = header.partition(":")
            if not sep or not name.strip():
                raise _Usage(f"a header is 'Name: value', not {header!r}")
            self.headers.append((name.strip(), value.strip()))
        if self.browser and (args.profile or self.headers):
            raise _Usage(
                "--browser fetches as the browser itself: --profile and --header don't apply"
            )
        if self.browser:
            _no_proxy_for_firefox(args)
        # Made now, so a bad profile or timeout is a command-line mistake.
        self.fetcher = Fetcher(args.profile or "chrome", args.proxy, args.timeout)

    def first(self, target: str):
        """The page the command was given: a URL, a file (a path or a
        file: URL), or a host without a scheme, which gets https://."""
        from netweir import Page
        from netweir._native import _response

        if target.lower().startswith("file:"):
            path = Path(urllib.request.url2pathname(urllib.parse.urlparse(target).path))
            if not path.is_file():
                raise _Usage(f"no such file: {path}")
        elif "://" in target:
            path = None
        elif Path(target).is_file():
            path = Path(target)
        elif (host := _HOST.fullmatch(target)) and (
            # A port or a path after the host means a host, whatever the
            # ending; only a bare name like page.html could be a file.
            host[3] or (host[4] or "").startswith("/") or not _FILE_ENDING.search(target)
        ):
            path, target = None, "https://" + target
        elif _FILE_ENDING.search(target):
            raise _Usage(f"no such file: {target}")
        else:
            raise _Usage(f"not a URL or a file: {target}")
        if self.browser:
            url = path.resolve().as_uri() if path else target
            from netweir._crawl import _run as run

            return _say_if_blocked(Page(run(self._in_chrome(url))))
        if path:
            # As if fetched from its file: URL, so it decodes and resolves
            # links as a fetched page would.
            response = _response(path.resolve().as_uri(), 200, "file", [], path.read_bytes())
            return Page(response)
        return self.send(target)

    def send(
        self, url, method="GET", *, form=None, json=None, body=None, headers=None, referer=None
    ):
        from netweir import Page
        from netweir._fetch import _pairs, payload

        how = payload(method, form, json, body)
        response = self.fetcher.send_blocking(
            url, headers=self.headers + _pairs(headers), referer=referer, **how
        )
        return _say_if_blocked(Page(response))

    def submit(self, form, headers=None):
        from netweir import Page
        from netweir._fetch import _from_form, _pairs

        url, how = _from_form(form)
        response = self.fetcher.send_blocking(
            url, headers=self.headers + _pairs(headers), referer=form.referer, **how
        )
        return _say_if_blocked(Page(response))

    async def _in_chrome(self, url):
        import netweir
        from netweir._native import _response

        async with netweir.browser(
            proxy=self.args.proxy, timeout=self.args.timeout, engine=self.engine or "chrome"
        ) as browser:
            tab = await browser.new_page()
            response = await tab.goto(url)
            content = (await tab.content()).encode()
            headers = _utf8(response.headers)
            return _response(tab.url, response.status, "browser", headers, content)


def _utf8(headers: list[tuple[str, str]]) -> list[tuple[str, str]]:
    """Chrome's response headers for the HTML it gives back, which is a
    str whatever the page's charset was: Content-Type says UTF-8, which
    the HTML is encoded in."""
    kept = [(k, v) for k, v in headers if k.lower() != "content-type"]
    return [*kept, ("content-type", "text/html; charset=utf-8")]


def _say_if_blocked(page):
    if page.blocked:
        kind = page._classified()[2] or "block"
        print(
            f"netweir: blocked by {page.blocked} ({kind}): this is its page, not the site's",
            file=sys.stderr,
        )
    return page


# What may come before <html> and <head>: a byte order mark, whitespace, a
# doctype, an XML declaration and comments, then <html> and <head>
# themselves. Tags go after these, so a page without <head> keeps its
# doctype first and isn't shown in quirks mode, and a "<head>" further on,
# in a script say, isn't mistaken for the real one.
_PROLOGUE = re.compile(
    r"\A(?:\ufeff|\s+|<!--.*?-->|<!doctype[^>]*>|<\?xml[^>]*\?>)*"
    r"(?:<html\b[^>]*>)?(?:\s*<head\b[^>]*>)?",
    re.I | re.S,
)


def _view(page) -> Path:
    """Saves the page, as text in UTF-8, with a charset and a <base> naming
    where it came from at the top of <head>, and opens it in the browser.

    Both go first because the first of each wins: a <base href="/"> of the
    page's own would point at the root of the disk, and the charset the
    server sent in its Content-Type header doesn't travel with the file.
    The <base> is the page's own base resolved against its URL, so links
    resolve as they did on the site."""
    # Chrome's HTML was a string, kept as UTF-8 whatever the headers say.
    text = page.body.decode("utf-8", "replace") if page.version == "browser" else page.text
    tags = f'<meta charset="utf-8"><base href="{html.escape(page.urljoin(""))}">'
    # The file gets a byte order mark of its own, below.
    text = text.removeprefix("\ufeff")
    at = _PROLOGUE.match(text).end()
    with tempfile.NamedTemporaryFile(prefix="netweir-", suffix=".html", delete=False) as f:
        # A byte order mark outranks every other charset signal, including
        # a <meta charset> a browser would miss past its first 1024 bytes.
        f.write(b"\xef\xbb\xbf" + (text[:at] + tags + text[at:]).encode("utf-8"))
    path = Path(f.name)
    webbrowser.open(path.as_uri())
    print(path)
    return path


_BANNER = """Netweir {version} shell
  page              the page{what}
  fetch(url)        fetch another page into `page` (method=, form=, json=, body=, headers=, referer=)
  submit(form)      submit page.form(...) and make the answer `page`
  view(page=None)   open `page`, or the one given, in your browser
  netweir           the module"""


def _shell(args, session: _Session) -> int:
    import netweir

    ns: dict = {"netweir": netweir, "page": None}

    def fetch(url, method="GET", **kwargs):
        ns["page"] = session.send(url, method, **kwargs)
        return ns["page"]

    def submit(form, headers=None):
        ns["page"] = session.submit(form, headers)
        return ns["page"]

    def view(page=None):
        page = page if page is not None else ns["page"]
        if page is None:
            raise ValueError("no page to view: fetch(url) first")
        _view(page)

    ns.update(fetch=fetch, submit=submit, view=view)
    if args.url:
        ns["page"] = session.first(args.url)
    if args.code is not None:
        try:
            result = eval(compile(args.code, "<-c>", "eval"), ns)
        except Exception:
            traceback.print_exc()
            return 1
        # As Scrapy's shell -c prints it: a string without quotes.
        print(result)
        return 0
    what = f": {ns['page']!r}" if ns["page"] is not None else ", none yet"
    banner = _BANNER.format(version=netweir.__version__, what=what)
    try:
        from IPython import start_ipython
    except ImportError:
        start_ipython = None
    # Started outside the except, or every error typed at the prompt would
    # be shown chained to the missing IPython.
    if start_ipython is None:
        code.interact(banner=banner, local=ns, exitmsg="")
    else:
        print(banner)
        start_ipython(argv=["--no-banner"], user_ns=ns)
    return 0
