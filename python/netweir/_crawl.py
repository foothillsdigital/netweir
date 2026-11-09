"""Spiders and the crawl loop.

The Rust engine owns the queue, deduplication, robots.txt, TDMRep and
throttling; this loop hands it requests, takes batches of results back and
runs your callbacks and pipelines on them.
"""

from __future__ import annotations

import asyncio
import collections
import contextlib
import dataclasses
import functools
import hashlib
import inspect
import itertools
import json
import logging
import math
import os
import re
import signal
import threading
import time
import weakref
from collections.abc import AsyncIterable, AsyncIterator, Callable, Iterable, Mapping
from concurrent.futures.process import BrokenProcessPool
from typing import Any
from urllib.parse import urlsplit

from netweir import _files, _hooks, _memory, _monitor, _stats, _track
from netweir._errors import QueueError
from netweir._fetch import Page, _pairs, certificates, checked_auth, redirect_limit, seconds
from netweir._native import (
    _RESOURCE_TYPES,
    Crawler,
    StopFlag,
    TrackStore,
    _auth_domain,
    _check_environment,
    _check_queue_url,
    _robots_sitemaps,
    fingerprint,
)
from netweir._native import _sitemap as _parse_sitemap
from netweir._request import Request, _steps, steps_json
from netweir._rules import Follow, _warn_invalid

log = logging.getLogger("netweir")


def _checked_saved_logins(logins: Any) -> tuple[tuple[str, str, str | None], ...]:
    """``saved_logins`` as (user, password, domain) tuples, or why it can't be.
    Two passwords for one user and domain leave nothing to choose between."""
    checked: dict[tuple[str, str | None], str] = {}
    for login in logins:
        if (
            not isinstance(login, (tuple, list))
            or len(login) not in (2, 3)
            or not all(isinstance(part, str) for part in login)
        ):
            raise ValueError(
                "each of saved_logins is (user, password) or (user, password, domain), in strings"
            )
        domain = _auth_domain(login[2]) if len(login) == 3 else None
        if (login[0], domain) in checked:
            raise ValueError(f"saved_logins has {login[0]!r} twice for the same domain")
        checked[login[0], domain] = login[1]
    return tuple((user, password, domain) for (user, domain), password in checked.items())


@dataclasses.dataclass(frozen=True)
class Settings:
    """How a crawl behaves. Every field has a polite default."""

    profile: str = "chrome"
    proxy: str | None = None
    #: Seconds for a whole request.
    timeout: float = 30.0
    #: Bytes of a response body, after decompression, beyond which the
    #: request fails with FetchError(kind="too_large"); None for no limit.
    max_response_size: int | None = 64 * 1024 * 1024
    #: Read pages parsed as XML in recover mode: what can be repaired is,
    #: with a warning saying what, instead of raising XMLError.
    xml_recover: bool = False
    #: Seconds one page may take to parse, wherever it's parsed: in the
    #: engine, or when a callback first reads it. A defence against
    #: markup made to be slow; ParseTimeout past it.
    parse_timeout: float | None = 10.0
    #: Requests in flight across all sites, and to one host.
    concurrency: int = 64
    per_domain: int = 8
    obey_robots: bool = True
    #: The name matched against robots.txt User-agent lines.
    robots_agent: str = "netweir"
    #: Skip pages whose site reserves text and data mining rights (TDMRep).
    obey_tdmrep: bool = True
    #: Adapt each host's delay to how fast it answers.
    throttle: bool = True
    start_delay: float = 1.0
    min_delay: float = 0.0
    max_delay: float = 60.0
    target_concurrency: float = 1.0
    #: Links followed from a start page beyond which requests are dropped.
    max_depth: int | None = None
    #: Requests accepted for one host beyond which more are dropped.
    max_pages_per_domain: int | None = None
    #: The crawl stops once it has delivered this many items, sent this
    #: many requests (each retry among them), had this many errors in
    #: callbacks or pipelines, or run for this many seconds. With a
    #: checkpoint, running it again carries on from where it stopped.
    max_items: int | None = None
    max_pages: int | None = None
    max_errors: int | None = None
    max_time: float | None = None
    #: The crawl stops once this process, with its worker processes, has
    #: used this many MiB of memory at its peak; at memory_warning MiB it
    #: logs a warning, once. Read at most once a second.
    max_memory: int | None = None
    memory_warning: int | None = None
    #: Further tries after a server error, a network error, throttling or
    #: a block. Retry n waits a random time up to backoff_base * 2**n
    #: seconds, capped at backoff_max.
    retries: int = 3
    backoff_base: float = 1.0
    backoff_max: float = 60.0
    #: The statuses retried, as Scrapy's RETRY_HTTP_CODES. A 429 left out
    #: still slows its host down, but goes to the callback. A block is
    #: retried whatever its status.
    retry_http_codes: tuple[int, ...] = (408, 429, 500, 502, 503, 504, 522, 524)
    #: Redirects a request follows before it fails (Scrapy's
    #: REDIRECT_MAX_TIMES); Chrome follows 20.
    max_redirects: int = 20
    #: True, False (no certificate check), or the path of a PEM file of CA
    #: certificates to trust in place of the bundled ones, for hosts an
    #: internal CA vouches for. Nothing on the wire changes.
    verify: bool | str | os.PathLike = True
    #: A (user, password) login sent as HTTP Basic to http_auth_domain and
    #: its subdomains, and nowhere else (Scrapy's HTTPAUTH_USER,
    #: HTTPAUTH_PASS and HTTPAUTH_DOMAIN). Each needs the other.
    http_auth: tuple[str, str] | None = dataclasses.field(default=None, repr=False)
    http_auth_domain: str | None = None
    #: Where a resumed crawl finds the passwords of saved requests' own
    #: logins, which checkpoints and shared queues don't keep: each a
    #: (user, password) for auth=, or (user, password, domain) for meta's
    #: http_auth_domain. Sends nothing by itself.
    saved_logins: tuple = dataclasses.field(default=(), repr=False)
    #: Proxies to send requests through, as proxy_rotation says:
    #: "on_block" (a site moves to the next each time it blocks a session,
    #: which also starts with an empty cookie jar), "per_request" (every
    #: request takes the next) or "per_host" (each site keeps one until a
    #: block moves it on).
    proxies: tuple[str, ...] = ()
    proxy_rotation: str = "on_block"
    #: A proxy that fails this many requests in a row (no connection to
    #: it, a 407, a tunnel it broke off, or a block; never a site that's
    #: down or slow) is left out for
    #: proxy_cooldown seconds, twice that the next time, up to 30 minutes.
    proxy_failures: int = 3
    proxy_cooldown: float = 60.0
    #: If more than breaker_ratio of a host's last breaker_window responses
    #: were blocks, it pauses for breaker_pause seconds.
    breaker_window: int = 50
    breaker_ratio: float = 0.3
    breaker_pause: float = 300.0
    #: A directory to keep the crawl's state in. Rerunning the same crawl
    #: with the same directory resumes it: what was finished isn't fetched
    #: again, and items carry an ``_id`` so none is written twice. Delete
    #: the directory to start over.
    checkpoint: str | None = None
    #: A Redis URL ("redis://host:6379/0") holding a queue this crawl
    #: shares with its other processes, on this machine or others: each
    #: request is fetched by one of them, once. ``queue_key`` names the
    #: crawl there; by default, the spider's ``name`` or class name. A
    #: process's requests go back to the queue ``queue_lease`` seconds
    #: after it stops sending heartbeats. A process ends once the queue
    #: has been empty for ``queue_idle`` seconds, or with None waits for
    #: more until stopped. See docs/distributed.md.
    queue: str | None = None
    queue_key: str | None = None
    queue_lease: float = 60.0
    queue_idle: float | None = 30.0
    #: A directory to keep responses in while you write a spider: run it
    #: again and pages it already has come from there, with no request to
    #: the site and no delay. Blocks, throttling, server errors and pages
    #: fetched in Chrome are never kept. Delete the directory to clear it.
    cache: str | None = None
    #: Seconds after which a kept response is fetched again; None for never.
    cache_expiry: float | None = None
    #: A directory to remember, across runs, which requests' pages gave
    #: items: this run and later ones with the same directory skip them,
    #: unless they have dont_filter=True. Pages that gave none, such as
    #: listings, are fetched every run. ``meta["deltafetch_key"]`` (a str
    #: or bytes) names a request in place of its method, URL and body, and
    #: ``meta["deltafetch_enabled"] = False`` leaves one out. Output files
    #: are added to, not started afresh, so they keep every run's items.
    deltafetch: str | None = None
    #: Forget every page deltafetch remembered, at the start of this run.
    deltafetch_reset: bool = False
    #: A file to keep every request the crawl sends and every response it
    #: gets in, as WARC: a name ending in .warc.gz (each record gzipped) or
    #: .warc. Pages served from the cache or fetched in Chrome aren't in it.
    #: Without a checkpoint each run starts it afresh; a resumed crawl adds
    #: to it.
    warc: str | None = None
    #: The same, as a HAR file to open in a browser's network panel.
    har: str | None = None
    #: How similar (0 to 1) an element must be to count as a tracked
    #: selector's element after the selector stops matching.
    track_threshold: float = 0.75
    #: Stop the crawl at the first exception in a callback or pipeline.
    fail_fast: bool = False
    #: Processes to run callbacks in. With more than one, the spider class
    #: must be importable (defined at module level, or in the file given to
    #: ``netweir crawl``), and callbacks run in worker processes while the
    #: engine, pipelines and checkpoint stay in this one.
    workers: int = 1
    #: Which requests are fetched in Chrome: "off" (only those that ask
    #: with ``browser=True``), "on_block" (also any still blocked after its
    #: retries, once) or "always".
    browser: str = "off"
    #: Which browser those requests are fetched in: "chrome", "firefox"
    #: (the one ``netweir install firefox`` installed), or "match", which
    #: is Firefox when ``profile`` is a Firefox one and Chrome otherwise,
    #: so a site sees the same browser the crawl's requests look like.
    #: Firefox can't go through the crawl's proxy yet, so it can't be
    #: chosen with one.
    browser_engine: str = "chrome"
    #: Chrome pages open at once.
    browser_pages: int = 4
    #: Resource types the crawl's Chrome pages never fetch, such as
    #: ``("image", "font", "media")``: Chrome fails them before they leave
    #: it.
    browser_block: tuple[str, ...] = ()
    #: What the crawl's Chrome pages are shown in and say about themselves,
    #: with the keys ``Browser.new_context()`` takes: ``{"device": "Galaxy
    #: S23"}``, ``{"locale": "de-DE", "timezone_id": "Europe/Berlin"}``
    #: and so on. Chrome is started with it, so even its service workers
    #: agree, and a site's cookies handed back to the HTTP client go with
    #: the same language and user agent.
    browser_environment: Mapping[str, Any] | None = None
    #: Send each worker script request of the crawl's Chrome pages with
    #: its headers in Chrome's own order, at the cost of pausing every
    #: request they make (see ``netweir.browser(worker_header_order=...)``).
    browser_worker_header_order: bool = False
    #: What every page the crawl fetches in Chrome does once it has loaded,
    #: before its callback sees it: a list of steps (``netweir.steps``).
    #: A request's own ``steps`` take their place. A call step here names
    #: a spider method by its name, as a string.
    browser_steps: tuple[dict[str, Any], ...] = ()
    #: The most one page's steps may take, all told, in seconds; None for
    #: no limit. Steps that ask outright for more (their waits, timeouts
    #: and typing) are refused where they're written, and a page's steps
    #: that reach it stop there, failing the request.
    browser_steps_max: float | None = 120.0
    #: The only folder a ``set_input_files`` step from data (the shared
    #: queue, or a checkpoint) may upload files from; None for none. With a
    #: shared queue, that's every request, the spider's own included;
    #: without one, steps the spider makes may upload from anywhere.
    browser_upload_dir: str | None = None
    #: Seconds between progress lines in the log ("crawled 120 pages (at
    #: 60 pages/min), ..."); None for none.
    stats_interval: float | None = 60.0
    #: A port to serve the crawl's stats on while it runs, as Prometheus
    #: metrics at /metrics and as JSON at /stats.json; 0 for any free one.
    #: None (the default) for no endpoint. It listens on metrics_host,
    #: this machine only by default, and has no password.
    metrics_port: int | None = None
    metrics_host: str = "127.0.0.1"
    #: Host names the endpoint may be asked for when metrics_host isn't a
    #: loopback address. Requests naming any other (besides an IP
    #: address) are refused, so a web page can't reach it by DNS
    #: rebinding.
    metrics_allowed_hosts: tuple[str, ...] = ()

    def __post_init__(self):
        if self.concurrency < 1 or self.per_domain < 1:
            raise ValueError("concurrency and per_domain must be at least 1")
        seconds(self.timeout)
        if self.parse_timeout is not None and not self.parse_timeout > 0:
            raise ValueError("parse_timeout must be a positive number of seconds, or None")
        if not 0 <= self.min_delay <= self.max_delay or self.start_delay < 0:
            raise ValueError("delays must be non-negative, with min_delay <= max_delay")
        if self.target_concurrency <= 0:
            raise ValueError("target_concurrency must be positive")
        if self.max_depth is not None and self.max_depth < 0:
            raise ValueError("max_depth must be 0 or more")
        if self.max_pages_per_domain is not None and self.max_pages_per_domain < 1:
            raise ValueError("max_pages_per_domain must be at least 1")
        for field in ("max_items", "max_pages", "max_errors"):
            if getattr(self, field) is not None and getattr(self, field) < 1:
                raise ValueError(f"{field} must be at least 1")
        if self.max_time is not None and self.max_time <= 0:
            raise ValueError("max_time must be positive")
        for name in ("max_memory", "memory_warning"):
            value = getattr(self, name)
            if value is not None and value <= 0:
                raise ValueError(f"{name} is in MiB and must be positive")
        if self.cache_expiry is not None and self.cache_expiry <= 0:
            raise ValueError("cache_expiry must be positive")
        if self.deltafetch is not None and not str(self.deltafetch):
            raise ValueError("deltafetch is a directory: give one, or None")
        if self.deltafetch_reset and self.deltafetch is None:
            raise ValueError("deltafetch_reset empties deltafetch's record: set deltafetch too")
        for name in ("warc", "har"):
            path = getattr(self, name)
            if path is None:
                continue
            # A pathlib.Path is fine to pass.
            path = os.fspath(path)
            object.__setattr__(self, name, path)
            # Checked now, rather than once the crawl has started.
            if not os.path.isdir(os.path.dirname(os.path.abspath(path))):
                raise ValueError(f"{name}={path!r}: the directory it goes in doesn't exist")
        # One file, by link or by letter case (one file on a case-insensitive
        # disk, so refused on any).
        if (
            self.warc is not None
            and self.har is not None
            and os.path.realpath(self.warc).casefold() == os.path.realpath(self.har).casefold()
        ):
            raise ValueError(f"warc and har are both {self.warc!r}: each needs a file of its own")
        if self.warc is not None and not self.warc.endswith((".warc", ".warc.gz")):
            raise ValueError(f"warc must be a file ending in .warc.gz or .warc, not {self.warc!r}")
        if self.retries < 0 or self.breaker_window < 0:
            raise ValueError("retries and breaker_window must be 0 or more")
        if isinstance(self.retry_http_codes, int):
            raise ValueError(
                f"retry_http_codes is a tuple of statuses, such as ({self.retry_http_codes},)"
            )
        object.__setattr__(self, "retry_http_codes", tuple(self.retry_http_codes))
        for code in self.retry_http_codes:
            if isinstance(code, bool) or not isinstance(code, int) or not 100 <= code <= 599:
                raise ValueError(f"retry_http_codes: {code!r} isn't an HTTP status")
        redirect_limit(self.max_redirects)
        if not isinstance(self.verify, (bool, str, os.PathLike)):
            raise ValueError(f"verify is True, False or a CA file's path, not {self.verify!r}")
        if isinstance(self.verify, os.PathLike):
            object.__setattr__(self, "verify", os.fspath(self.verify))
        object.__setattr__(self, "http_auth", checked_auth(self.http_auth))
        if (self.http_auth is None) != (self.http_auth_domain is None):
            raise ValueError(
                "http_auth and http_auth_domain go together: the login is sent only to that"
                " domain and its subdomains, so a link or redirect elsewhere never gets it"
            )
        if self.http_auth_domain is not None:
            # A bare host: a port, scheme or path would never match, and a
            # public suffix would send the login to every site under it.
            domain = _auth_domain(self.http_auth_domain)
            object.__setattr__(self, "http_auth_domain", domain)
        object.__setattr__(self, "saved_logins", _checked_saved_logins(self.saved_logins))
        if not 0 < self.backoff_base <= self.backoff_max or self.breaker_pause < 0:
            raise ValueError("backoffs must be positive, with backoff_base <= backoff_max")
        if not 0 <= self.breaker_ratio <= 1:
            raise ValueError("breaker_ratio must be between 0 and 1")
        if not 0 < self.track_threshold <= 1:
            raise ValueError("track_threshold must be above 0 and at most 1")
        if self.browser not in ("off", "on_block", "always"):
            raise ValueError(f'browser must be "off", "on_block" or "always", not {self.browser!r}')
        if self.browser_engine not in ("chrome", "firefox", "match"):
            raise ValueError(
                'browser_engine must be "chrome", "firefox" or "match", '
                f"not {self.browser_engine!r}"
            )
        if self.browser_pages < 1:
            raise ValueError("browser_pages must be at least 1")
        if isinstance(self.browser_block, str):
            raise ValueError(
                f"browser_block is a tuple of resource types, such as ({self.browser_block!r},)"
            )
        object.__setattr__(self, "browser_block", tuple(self.browser_block))
        if self.browser_steps_max is not None:
            if isinstance(self.browser_steps_max, bool) or not (
                isinstance(self.browser_steps_max, (int, float)) and self.browser_steps_max > 0
            ):
                raise ValueError("browser_steps_max must be a positive number of seconds, or None")
            object.__setattr__(self, "browser_steps_max", float(self.browser_steps_max))
        steps = _steps(self.browser_steps, "browser_steps", self.browser_steps_max)
        for step in steps:
            if step.get("action") == "call" and not isinstance(step.get("method"), str):
                raise TypeError(
                    "browser_steps: a call step here names a spider method, as a string"
                    " (Settings go to worker processes, and a function can't)"
                )
        object.__setattr__(self, "browser_steps", tuple(steps))
        if self.browser_environment is not None:
            if not isinstance(self.browser_environment, Mapping):
                raise ValueError(
                    "browser_environment is a dict of new_context()'s keyword arguments, such as "
                    '{"locale": "de-DE"}'
                )
            environment = dict(self.browser_environment)
            _check_environment(environment)
            object.__setattr__(self, "browser_environment", environment)
        for kind in self.browser_block:
            if kind == "document":
                raise ValueError(
                    'browser_block can\'t hold "document": the page itself is one, so it'
                    " would never load"
                )
            if kind not in _RESOURCE_TYPES:
                raise ValueError(
                    f"browser_block: unknown resource type {kind!r}; known ones are "
                    + ", ".join(t for t in _RESOURCE_TYPES if t != "document")
                )
        if self.workers < 1:
            raise ValueError("workers must be at least 1")
        if self.stats_interval is not None and not (
            math.isfinite(self.stats_interval) and self.stats_interval > 0
        ):
            raise ValueError("stats_interval must be positive, or None for no progress lines")
        if self.metrics_port is not None and not 0 <= self.metrics_port <= 65535:
            raise ValueError("metrics_port must be a port, 0 to 65535, or None for no endpoint")
        if not self.metrics_host:
            raise ValueError("metrics_host can't be empty")
        if isinstance(self.metrics_allowed_hosts, str):
            raise ValueError(
                "metrics_allowed_hosts is a tuple of host names, such as"
                f" ({self.metrics_allowed_hosts!r},)"
            )
        object.__setattr__(self, "metrics_allowed_hosts", tuple(self.metrics_allowed_hosts))
        # A list is fine to pass; stored as a tuple, as Settings is frozen.
        object.__setattr__(self, "proxies", tuple(self.proxies))
        rotation = self.proxy_rotation
        if rotation not in ("on_block", "per_request", "per_host"):
            raise ValueError(
                f'proxy_rotation must be "on_block", "per_request" or "per_host", not {rotation!r}'
            )
        if rotation != "on_block" and not self.proxies:
            raise ValueError(f'proxy_rotation="{rotation}" rotates through proxies; give some')
        if rotation != "on_block" and self.proxy is not None:
            raise ValueError(
                f'proxy_rotation="{rotation}" takes proxies, not proxy as well: it isn\'t'
                " clear which would win; put that proxy in proxies"
            )
        if self.proxy_failures < 1:
            raise ValueError("proxy_failures must be at least 1")
        if self.proxy_cooldown <= 0:
            raise ValueError("proxy_cooldown must be positive")
        self._check_queue()

    def _check_queue(self) -> None:
        if self.queue is None:
            return
        scheme = self.queue.partition("://")[0].lower()
        if scheme == "rediss":
            raise ValueError(
                "queue: TLS to Redis (rediss://) isn't supported; use redis:// on a private"
                " network, or a TLS tunnel to the server"
            )
        if scheme not in ("redis", "redis+unix", "unix"):
            raise ValueError(f"queue must be a redis:// URL, not a {scheme or 'plain'} one")
        # The whole URL, as the client reads it (a port that isn't a
        # number, say), without connecting.
        _check_queue_url(self.queue)
        if self.checkpoint is not None:
            raise ValueError(
                "a crawl with a queue can't have a checkpoint too: the queue in Redis is the"
                " crawl's saved state, shared by every process; drop the checkpoint"
            )
        if self.deltafetch is not None:
            raise ValueError(
                "a crawl with a queue can't have deltafetch too: its record is one machine's,"
                " and the queue's seen set already skips what an earlier crawl under the"
                " same queue_key queued; drop deltafetch"
            )
        if self.queue_key == "":
            raise ValueError("queue_key can't be empty")
        if self.queue_lease <= 0:
            raise ValueError("queue_lease must be positive")
        if self.queue_idle is not None and self.queue_idle <= 0:
            raise ValueError("queue_idle must be positive, or None to wait for more for good")

    def _engine(self, allowed_domains: Iterable[str] = (), queue_key: str = "") -> Crawler:
        verify, cas = certificates(self.verify)
        return Crawler(
            allowed_domains=list(allowed_domains),
            profile=self.profile,
            proxy=self.proxy,
            timeout=self.timeout,
            verify=verify,
            ca_certificates=cas,
            max_redirects=self.max_redirects,
            retry_http_codes=list(self.retry_http_codes),
            http_auth=None if self.http_auth is None else (*self.http_auth, self.http_auth_domain),
            saved_logins=list(self.saved_logins),
            max_response_size=self.max_response_size,
            concurrency=self.concurrency,
            per_domain=self.per_domain,
            obey_robots=self.obey_robots,
            robots_agent=self.robots_agent,
            obey_tdmrep=self.obey_tdmrep,
            throttle=self.throttle,
            start_delay=self.start_delay,
            min_delay=self.min_delay,
            max_delay=self.max_delay,
            target_concurrency=self.target_concurrency,
            max_depth=self.max_depth,
            max_pages_per_domain=self.max_pages_per_domain,
            max_pages=self.max_pages,
            retries=self.retries,
            backoff_base=self.backoff_base,
            backoff_max=self.backoff_max,
            proxies=list(self.proxies),
            proxy_rotation=self.proxy_rotation,
            proxy_failures=self.proxy_failures,
            proxy_cooldown=self.proxy_cooldown,
            breaker_window=self.breaker_window,
            breaker_ratio=self.breaker_ratio,
            breaker_pause=self.breaker_pause,
            checkpoint=self._checkpoint_file(),
            browser=self.browser,
            browser_pages=self.browser_pages,
            browser_block=list(self.browser_block),
            browser_environment=self.browser_environment,
            browser_worker_header_order=self.browser_worker_header_order,
            browser_steps=steps_json(list(self.browser_steps), str) if self.browser_steps else None,
            browser_steps_max=self.browser_steps_max,
            browser_engine=self.browser_engine,
            cache=None if self.cache is None else os.path.join(self.cache, "cache.sqlite3"),
            cache_expiry=self.cache_expiry,
            deltafetch=None
            if self.deltafetch is None
            else os.path.join(self.deltafetch, "deltafetch.sqlite3"),
            deltafetch_reset=self.deltafetch_reset,
            warc=self.warc,
            har=self.har,
            queue=self.queue,
            queue_key=self.queue_key or queue_key,
            queue_lease=self.queue_lease,
            queue_idle=self.queue_idle,
            xml_recover=self.xml_recover,
            parse_timeout=self.parse_timeout,
        )

    def _checkpoint_file(self) -> str | None:
        if self.checkpoint is None:
            return None
        return os.path.join(self.checkpoint, "crawl.sqlite3")


class Spider:
    """Subclass this. Set ``start_urls`` (or override ``start``), write
    ``parse``, and run it with ``run()`` or ``await crawl()``.

    To crawl a site from its sitemaps, set ``sitemap_urls`` to sitemaps or
    to its robots.txt, which names them. ``sitemap_rules`` sends each page
    to a callback by the first pattern (a regex) its URL matches;
    ``sitemap_follow`` says which sitemaps of an index to read.

    A callback receives a Page and yields (or returns) items, which are
    dicts or dataclasses, and Requests to follow. It can be an async
    generator, a generator, a coroutine or a plain function.
    """

    name: str | None = None
    start_urls: list[str] = []
    #: Sitemaps (XML, gzipped or not, or text), sitemap indexes, or a
    #: robots.txt whose Sitemap lines name them.
    sitemap_urls: list[str] = []
    #: (pattern, callback) pairs: a page goes to the first whose regex its
    #: URL matches; pages none matches are skipped.
    sitemap_rules: list[tuple[str, Callable[..., Any] | str]] = [("", "parse")]
    #: Regexes for which sitemaps of an index to read: all, by default.
    sitemap_follow: list[str] = [""]
    #: Also crawl the other-language versions a sitemap lists for a page.
    sitemap_alternate_links: bool = False
    #: Link rules (``netweir.Follow``), run in Rust. A spider made only of
    #: rules and Items runs no Python per page.
    rules: list[Follow] = []
    settings: Settings = Settings()
    #: Stages applied to every item in order, sync or async: functions of
    #: the item, or objects with ``process_item(item, spider)``. Each returns
    #: the item (changed or not), or None to drop it. An object may also
    #: have ``open_spider(spider)`` and ``close_spider(spider)``.
    pipelines: list[Any] = []
    #: Objects with any of ``process_request(request, spider)``,
    #: ``process_response(request, page, spider)`` and
    #: ``process_exception(request, error, spider)``, sync or async; see
    #: docs/crawling.md. They, spider middlewares and pipeline objects
    #: also get the signals they have methods for (netweir.signals). A
    #: class here, or in spider_middlewares or pipelines, is made when the
    #: crawl starts, with ``from_crawler(crawler)`` if it has one.
    middlewares: list[Any] = []
    #: Scrapy's spider middlewares, in the order of their numbers there
    #: (the first nearest the engine): objects with any of
    #: ``process_spider_input``, ``process_spider_output``,
    #: ``process_spider_exception`` and ``process_start``.
    spider_middlewares: list[Any] = []
    #: Scrapy's per-spider settings, which ``crawler.settings.get()`` reads.
    custom_settings: dict[str, Any] = {}
    #: While a crawl runs, the crawler ``from_crawler`` was given: its
    #: ``signals``, ``stats`` and ``engine``, as on Scrapy's spiders.
    crawler: Any = None
    #: Domains the crawl may go to, each with its subdomains; a domain with
    #: a port allows only that port. Requests elsewhere are dropped (and
    #: counted as "offsite") unless they have dont_filter=True. Empty for
    #: anywhere.
    allowed_domains: list[str] = []
    #: Asks for replacement selectors when tracked ones break (see
    #: netweir.repair). Proposals end up in ``repairs`` after a run.
    repair: Any = None
    repairs: list[Any] = []
    #: Why the last run ended: "finished" when it ran out of requests, or
    #: the setting that stopped it ("max_items", "max_pages", "max_errors",
    #: "max_time").
    finish_reason: str | None = None
    #: Checks run once the crawl has ended: netweir.Checks, or functions of
    #: (stats, spider) returning None, a message or a list of messages.
    checks: list[Any] = []
    #: Contracts of your own for ``netweir check``, besides the built-in
    #: ones: subclasses of netweir.contracts.Contract.
    contracts: list[Any] = []
    #: After a run: what the checks found wrong (empty if they all passed),
    #: how many items each field was present and not empty in, and how many
    #: validation errors each field had.
    check_failures: list[str] = []
    coverage: dict[str, int] = {}
    invalid_fields: dict[str, int] = {}
    #: After a run: for each proxy of Settings.proxies, then each other
    #: proxy a request named, {"requests": n, "failures": n, "benched": n}.
    proxy_stats: dict[str, dict[str, int]] = {}
    #: The crawl's stats, as a netweir.Stats: read while the crawl runs,
    #: each read is the stats so far; after a run, the stats run()
    #: returns, or, for a run that was interrupted or raised, those it had
    #: got to. ``inc_value`` and the rest keep stats of the spider's own.
    stats: Mapping[str, Any] = {}

    async def start(self) -> AsyncIterator[Request]:
        """The first requests. An async generator by default; a plain
        generator or a list works too. By default, what
        ``start_requests()`` gives, as in Scrapy, so a spider written for
        Scrapy before 2.13, with only ``start_requests()``, works as it is."""
        for request in self.start_requests():
            yield request

    def start_requests(self) -> Iterable[Request]:
        """Scrapy's older way to give the first requests, which the default
        ``start()`` yields: a generator or a list. By default, a request
        for each of ``start_urls``, then each of ``sitemap_urls``."""
        for url in self.start_urls:
            yield Request(url)
        for url in self.sitemap_urls:
            yield Request(url, callback="_sitemap")

    def sitemap_filter(self, entries: Iterable[dict[str, Any]]) -> Iterable[dict[str, Any]]:
        """Chooses which sitemap entries to crawl; all of them by default.
        Each is a dict with ``loc``, ``lastmod``, ``changefreq``,
        ``priority`` (None where the sitemap doesn't say) and
        ``alternates``. Applies to an index's entries too."""
        return entries

    async def _sitemap(self, page: Page) -> AsyncIterator[Request]:
        """Reads a sitemap, an index or a robots.txt, and yields the pages
        and sitemaps it leads to. The sitemap is read on a thread of its
        own, so a large one doesn't hold up the crawl."""
        if urlsplit(page.url).path.endswith("/robots.txt"):
            for url in _robots_sitemaps(page.text):
                yield Request(page.urljoin(url), callback="_sitemap")
            return
        limit = self.settings.max_response_size or 1 << 30
        # Cancelled, the crawl stops waiting at once, and the reader gives
        # up soon after rather than reading on for no one.
        stop = StopFlag()
        try:
            kind, entries, repaired = await asyncio.to_thread(
                _parse_sitemap, page.body, limit, stop
            )
        except ValueError as e:
            log.warning("%s: not read as a sitemap: %s", page.url, e)
            return
        finally:
            stop.set()
        if repaired:
            log.warning("%s: sitemap read, broken: %s", page.url, repaired)
        entries = self.sitemap_filter(entries)
        if kind == "index":
            follow = [re.compile(p) for p in self.sitemap_follow]
            for entry in entries:
                if any(p.search(entry["loc"]) for p in follow):
                    yield Request(entry["loc"], callback="_sitemap")
            return
        rules = [(re.compile(p), cb) for p, cb in self.sitemap_rules]
        for entry in entries:
            urls = [entry["loc"]]
            if self.sitemap_alternate_links:
                urls += entry["alternates"]
            for url in urls:
                callback = next((cb for p, cb in rules if p.search(url)), None)
                if callback is not None:
                    yield Request(url, callback=callback)

    def parse(self, page: Page) -> Any:
        raise NotImplementedError(f"{type(self).__name__} needs a parse(self, page) method")

    @property
    def logger(self) -> logging.Logger:
        """A logger named after the spider, as Scrapy's ``spider.logger``."""
        return logging.getLogger(f"netweir.{self.name or type(self).__name__}")

    def opened(self) -> Any:
        """Called before the first request is queued, after the middlewares
        and pipelines have been told. Sync or async; an exception here stops
        the crawl before anything is fetched."""

    def closed(self, reason: str) -> Any:
        """Called once the crawl has ended, however it ended: ``reason`` is
        ``finish_reason``, ``"error"`` after an exception or
        ``"interrupted"`` after Ctrl-C. Sync or async."""

    def on_block(self, request: Request, page: Page) -> Any:
        """Called when bot protection still blocked ``request`` after every
        retry, each with a new session. ``page.blocked`` names the vendor.
        Like a callback, it may yield items and Requests. By default it
        logs a warning."""
        kind = page._classified()[2]
        log.warning("%s: blocked by %s (%s); giving up", request.url, page.blocked, kind)

    def run(self, output: Any = None) -> dict[str, int]:
        """Crawls to the end and returns the stats. ``output`` writes every
        item to a .jsonl, .json, .xml, .csv or .parquet file, local or a
        cloud URL (or is an exporter)."""
        return _run(self.crawl(output))

    async def crawl(self, output: Any = None) -> dict[str, int]:
        from netweir import export

        pipelines = list(self.pipelines)
        if output is not None:
            pipelines.append(export.to_path(output) if isinstance(output, str) else output)
        _check_outputs(self.settings, pipelines)
        # Nothing is opened or written until this has worked: a crawl that
        # can't start leaves its output files as they were.
        run = _Run(self, pipelines)
        started: list[export._Exporter] = []
        reason = "error"
        #: Set when the crawl itself is raising, so a failure closing an
        #: exporter doesn't replace that error (sys.exc_info() can't tell:
        #: it also sees an exception the caller is handling).
        raising = False
        stats: dict[str, int] | None = None
        try:
            # Before anything is opened: a metrics port that's taken stops
            # the crawl here.
            run.monitor.start()
            # None, or the state of the output files when items were last
            # recorded: a resumed run cuts each file back to its recorded
            # length, dropping anything half-written or not recorded (which
            # the run makes again), and adds to it.
            state = run.resume()
            files = run.recorded_files(state)
            # Every output is checked before any is touched.
            exporters = [s for s in pipelines if isinstance(s, export._Exporter)]
            for stage in exporters:
                run.check_output(stage, state, files)
            # Before any output file is touched: a pipeline that can't open
            # leaves them as they were.
            await run.open()
            # The WARC and HAR files too: a crawl that couldn't start leaves
            # them as they were.
            run.engine.start_archive()
            # Cut each file back to where the record last saw it: the pages
            # of anything after that weren't recorded, and are fetched
            # again. Then record, before any is opened, how long each will
            # be once it is (a JSON or XML file taken up again loses its
            # closing tag; one started afresh is empty), as not closed: a
            # crash from here on leaves at most a tail the next run cuts.
            ends = {}
            for stage in exporters:
                if isinstance(stage, export.parquet) or stage.batch is not None:
                    continue
                path = os.path.abspath(stage.path)
                size = files.get(path, (None,))[0]
                if size is None:
                    ends[path] = 0
                    continue
                if os.path.exists(stage.path):
                    with open(stage.path, "r+b") as f:
                        f.truncate(size)
                ends[path] = stage._resume_length()
            if ends and (run.delta or state is not None):
                run.record_lengths(ends)
            for stage in exporters:
                path = os.path.abspath(stage.path)
                if isinstance(stage, export.parquet):
                    # Written whole at the end; a resumed run adds a part.
                    run.tidy_parts(stage, files)
                    # Named only once its part is on record.
                    stage.hold = run.saving or (run.delta and stage.batch is None)
                    stage.start(append=state is not None or path in files)
                elif stage.batch is not None:
                    # With deltafetch, each run's batches go beside the last
                    # run's, never over them.
                    stage.start(append=run.delta)
                else:
                    stage.start(append=path in files)
                started.append(stage)
            run.outputs_ready = True
            stats = await run.go()
            # Stopped by a limit part way through a batch: the batch is
            # fetched again by a resumed run.
            run.completed = self.finish_reason == "finished"
            reason = self.finish_reason or "finished"
            return stats
        except (KeyboardInterrupt, asyncio.CancelledError):
            reason = "interrupted"
            raising = True
            raise
        except BaseException:
            raising = True
            raise
        finally:
            try:
                run.monitor.stop()
            except Exception:  # noqa: BLE001 - the crawl still winds up
                log.exception("stopping the progress line and metrics endpoint failed")
            # However the crawl ended, what each proxy did is known.
            self.proxy_stats = run.engine.proxy_stats()
            await run.engine.close_browser()
            # Each stage is closed even if one before it fails (an upload,
            # say); the first failure is raised once the crawl is wound up.
            failure: BaseException | None = None
            for stage in pipelines:
                if isinstance(stage, export._Exporter) and not any(stage is s for s in started):
                    continue
                close = getattr(stage, "close", None)
                if callable(close):
                    try:
                        close()
                    except Exception as e:  # noqa: BLE001 - raised below
                        log.error("closing %r failed: %s", stage, e)
                        failure = failure or e
            # Checks too come after the exporters, so one can read the output
            # files, and before closed() and spider_closed, which may want to
            # pass the results on. Not for a crawl that raised.
            if stats is not None and not raising:
                self.check_failures = await _monitor.run_checks(self.checks, stats, self)
                stats["checks_failed"] = len(self.check_failures)
            # However the crawl ended, what it had done.
            run.live._freeze(stats if stats is not None else run.stats())
            # After the exporters: closed() and close_spider find the output
            # files complete.
            await run.close(reason)
            run.finish(closed=failure is None)
            if failure is not None and not raising:
                raise failure


#: While crawls run on the main thread, innermost last: each stops its
#: crawl as Ctrl-C does. For a SIGTERM that comes while SIGINT is ignored
#: (a job started with `&`).
_interrupts: list[Callable[[], None]] = []


def _run(main: Any) -> Any:
    """asyncio.run(main), where Ctrl-C cancels ``main`` and then raises
    KeyboardInterrupt once it has wound up; a second Ctrl-C raises it at
    once. Python 3.11's asyncio.run does that, 3.10's doesn't: it raised
    KeyboardInterrupt wherever the loop happened to be. Raised as a done
    future was scheduling its task's wakeup, it left that task asleep for
    good, and asyncio.run waited on it for ever: the crawl hung, its
    output never closed."""
    state = {"interrupted": False}
    try:
        return asyncio.run(_interruptible(main, state))
    except asyncio.CancelledError:
        # Out here, as 3.11 does: raised in the main task, it would end the
        # loop before the other tasks had wound up.
        if state["interrupted"]:
            raise KeyboardInterrupt from None
        raise


async def _interruptible(main: Any, state: dict[str, bool]) -> Any:
    if threading.current_thread() is not threading.main_thread():
        return await main
    loop = asyncio.get_running_loop()
    task = asyncio.current_task()

    def interrupt(signum: int | None = None, frame: Any = None) -> None:
        if state["interrupted"]:
            raise KeyboardInterrupt
        state["interrupted"] = True
        # As a callback of its own: this runs between any two bytecodes of
        # the loop's, task.cancel() included.
        loop.call_soon_threadsafe(task.cancel)

    previous = signal.getsignal(signal.SIGINT)
    # Only where 3.11's would: not over a handler of the program's own, or
    # when SIGINT is ignored. On 3.11 and later, asyncio's is there already.
    catching = previous is signal.default_int_handler
    if catching:
        try:
            signal.signal(signal.SIGINT, interrupt)
        except ValueError:
            # Not the main interpreter (gh-91880): run as before.
            catching = False
    _interrupts.append(interrupt)
    try:
        return await main
    finally:
        _interrupts.remove(interrupt)
        # Unless something else has taken SIGINT since.
        if catching and signal.getsignal(signal.SIGINT) is interrupt:
            signal.signal(signal.SIGINT, previous)


def _check_outputs(settings: Settings, pipelines: list[Any]) -> None:
    """Refuses exporters a crawl with a checkpoint can't resume: resuming
    cuts the output file back and adds to it, which neither an object in a
    bucket nor a run of batches allows. And remote ones with a shared queue:
    they're uploaded only at the end, so a process killed before then loses
    items whose pages the queue has as done."""
    from netweir import export

    if settings.queue is not None:
        for stage in pipelines:
            if isinstance(stage, export._Exporter) and stage.remote:
                raise ValueError(
                    f"a crawl with a queue can't write to {stage.path}: a process killed"
                    " before the upload would lose items the queue has as done; write to a"
                    " local file, or send items to a shared store from a pipeline"
                )
    if settings.deltafetch is not None:
        for stage in pipelines:
            if not isinstance(stage, export._Exporter):
                continue
            # deltafetch keeps every run's items in the output; a run that
            # emptied it would lose the items of every page it skips.
            if stage.batch is None and stage.remote:
                raise ValueError(
                    f"with deltafetch, {stage.path} would be replaced each run, losing the"
                    " items of pages it skips: write to a local file, which each run adds"
                    " to, or use batch=, whose files each run adds to"
                )
    if settings.checkpoint is None:
        return
    for stage in pipelines:
        if not isinstance(stage, export._Exporter):
            continue
        if stage.remote:
            raise ValueError(
                f"a crawl with a checkpoint can't write to {stage.path}: write to"
                " a local file and upload it once the crawl is done"
            )
        if stage.batch is not None:
            raise ValueError(
                f"a crawl with a checkpoint can't write {stage.path} in batches:"
                " drop batch= or the checkpoint"
            )


def _tail(path: str, size: int) -> str:
    """A digest of a file's last 64 KiB before ``size``, and of ``size``:
    enough to tell the file a crawl wrote from another one that has grown
    past the same length."""
    digest = hashlib.sha256(str(size).encode())
    if size == 0:
        return digest.hexdigest()  # the file may not be there yet
    with open(path, "rb") as f:
        start = max(0, size - _TAIL)
        f.seek(start)
        digest.update(f.read(size - start))
    return digest.hexdigest()


#: How much of a file's end _tail reads.
_TAIL = 64 * 1024

#: An output file as last recorded: its length, a digest of its end
#: (_tail), and whether the run that wrote it closed it.
_Recorded = tuple[int, str | None, bool]


def _check_resumable(stage: Any, size: int, tail: str | None = None) -> None:
    """Before a resumed crawl cuts ``stage``'s file back to ``size``: the
    file must still be the one it wrote, at least that long, starting as
    the format starts and, when a digest of its end was recorded, the same
    up to ``size``. Otherwise the cut would lose someone else's file, cut
    an item in half, or pad it with zero bytes."""
    path = stage.path
    actual = os.path.getsize(path)
    if actual < size:
        raise ValueError(
            f"{path} is shorter ({actual} bytes) than this crawl recorded ({size}):"
            " it was changed since; move it away, or start the crawl afresh"
            " (a new checkpoint, or deltafetch_reset=True)"
        )
    if tail is not None and _tail(path, size) != tail:
        raise ValueError(
            f"{path} isn't the file this crawl wrote: its first {size} bytes changed"
            " since (another run wrote to it, or it was replaced); move it away, or"
            " start the crawl afresh (a new checkpoint, or deltafetch_reset=True)"
        )
    head = stage._head()
    if head:
        with open(path, "rb") as f:
            start = f.read(len(head))
        if start != head[: len(start)] or len(start) < min(len(head), size):
            raise ValueError(
                f"{path} doesn't start as this crawl's file did: it was replaced since;"
                " move it away, or start the crawl afresh (a new checkpoint, or"
                " deltafetch_reset=True)"
            )


class _Stop(BaseException):
    """A limit was reached, or something asked for the crawl to end
    (CloseSpider, crawler.engine.close_spider), which isn't a limit. A
    BaseException, so that the callback or pipeline it's raised under
    doesn't take it for its own error."""

    def __init__(self, reason: str, limit: bool = True):
        super().__init__(reason)
        self.reason = reason
        self.limit = limit


def _closed(error: BaseException) -> _Stop:
    """The _Stop for a CloseSpider, Netweir's or Scrapy's."""
    return _Stop(getattr(error, "reason", None) or "cancelled", limit=False)


def _warn_if_priced(page: Page) -> None:
    if page.outcome == "payment_required":
        price = page._classified()[4]
        log.warning("%s: payment required%s; not retried", page.url, f" ({price})" if price else "")


def _worker_died(name: str, url: str) -> RuntimeError:
    return RuntimeError(
        f"a worker process died while running {name} for {url}, or couldn't start;"
        " with a checkpoint, the crawl resumes where it stopped"
    )


class _Run:
    def __init__(self, spider: Spider, pipelines: list[Callable[[Any], Any]]):
        self.spider = spider
        self.pipelines = pipelines
        self.settings = spider.settings
        #: Whether a request's auth= has been logged as winning over its
        #: meta's http_user, once a crawl.
        self.warned_login = False
        #: Requests crawler.engine.crawl() was given, to queue.
        self.scheduled: collections.deque[Request] = collections.deque()
        #: Work a middleware handed back while a callback was running (a
        #: page to answer a request with, an errback to call), done once it
        #: has finished, so one callback never runs inside another.
        self.ready: collections.deque[Callable[[], Any]] = collections.deque()
        #: The reason crawler.engine.close_spider() gave, once it's called.
        self.closing: str | None = None
        #: Signal handlers connected by hand, by signal, and the handlers of
        #: each signal, found once.
        self.connected: dict[str, list[Callable[..., Any]]] = {}
        self.found: dict[str, list[Callable[..., Any]]] = {}
        #: Objects from_crawler made, which hear only what they connect.
        self.built: list[Any] = []
        #: Requests the engine has taken (or answered without a fetch): a
        #: spider_idle handler that adds to it keeps the crawl going.
        self.accepted = 0
        #: A served page was refused for max_pages: the crawl ends there once
        #: everything in hand is done, as it does for fetched pages.
        self.capped_out = False
        #: The exception a pipeline raised, so the callback it ends isn't
        #: reported as a spider error too.
        self.item_failure: BaseException | None = None
        self.engine_started = False
        #: Pipeline stages whose open_spider worked, for close_spider.
        self.opened: list[Any] = []
        #: Whether open() got as far as spider_opened, and the spider's
        #: opened(): who is told spider_closed and closed().
        self.signalled = False
        self.spider_opened = False
        #: Set once the output files are cut back and started; see finish().
        self.outputs_ready = False
        self.engine = self.settings._engine(
            spider.allowed_domains, spider.name or type(spider).__name__
        )
        #: Worker processes for callbacks, with Settings.workers > 1.
        self.pool: Any = None
        self.ids = itertools.count()
        self.waiting: dict[int, Request] = {}
        self.rules = list(spider.rules)
        #: Each rule's steps as the engine has them (JSON), for the
        #: deltafetch key of the pages it leads to.
        self.rule_steps: list[str | None] = []
        for rule in self.rules:
            self.rule_steps.append(
                None
                if rule.steps is None
                else _capped(
                    steps_json(rule.steps, functools.partial(_rule_method, spider)),
                    self.settings.browser_steps_max,
                    "Follow's steps",
                )
            )
            self.engine.add_rule(
                rule.kind,
                rule.query,
                rule.extract._spec if rule.extract is not None else None,
                follow=rule.follow,
                to_python=rule.callback is not None,
                priority=rule.priority,
                allow=rule.allow,
                deny=rule.deny,
                allow_domains=rule.allow_domains,
                deny_domains=rule.deny_domains,
                browser=rule.browser,
                steps=self.rule_steps[-1],
                when=rule.when,
            )
        # parse runs alongside the rules only if the spider writes its own.
        self.has_parse = type(spider).parse is not Spider.parse
        #: Items already delivered by an earlier run of this crawl, by _id.
        self.delivered: set[str] = set()
        self.saving = self.settings.checkpoint is not None
        #: Whether the crawl has warned that checkpoint commits are failing.
        self.checkpoint_warned = False
        #: Requests leave this process as data, callbacks by name: for a
        #: checkpoint, or for the shared queue.
        self.by_name = self.saving or self.settings.queue is not None
        #: Call steps' plain (not async) functions already warned about.
        self.warned_sync: set[Any] = set()
        #: Requests from the shared queue that couldn't be made Requests.
        self.queue_bad = 0
        #: Ids of items delivered but not yet recorded as such.
        self.unrecorded: list[str] = []
        self.delta = self.settings.deltafetch is not None
        #: With deltafetch: the page the running callback was given, as
        #: (method, url, body, deltafetch_key, cookie jar) or None, and
        #: whether it has given a delivered item yet. It's recorded only if
        #: the callback finishes: one that raises part way may have had more
        #: to give.
        self.from_page: tuple | None = None
        self.gave = False
        #: Keys of pages that gave items, until their items are on disk.
        self.produced: set[bytes] = set()
        #: Items delivered when output lengths were last recorded.
        self.delta_items = 0
        #: Output lengths to record in place of the files' own, while
        #: record_lengths runs.
        self.ends: dict[str, int] | None = None
        #: Parquet parts whose items are on record, by absolute path.
        self.parts: list[str] = []
        self.completed = False
        #: Where items being handled come from, for their _id.
        self.source = ""
        self.position = 0
        self.warned: set[str] = set()
        #: Depth of requests the running callback yields.
        self.depth = 0
        #: The page the running callback was given, for item signals.
        self.page: Page | None = None
        #: The URL of the page items being handled come from, when there is
        #: one: the Referer netweir.files sends.
        self.page_url: str | None = None
        #: The cookie jar of the request that page came from (as the engine
        #: names it): the one netweir.files downloads in.
        self.page_jar: str | None = None
        #: Whether the item being handled has met a validate stage yet, and
        #: failed one: each is counted once an item.
        self.validating = False
        self.invalid = False
        # Tracked selectors keep fingerprints in the checkpoint file, or in
        # the default store.
        state = self.settings._checkpoint_file()
        store = TrackStore(state) if state else _track.context()[0]
        self.tracks = (store, self.settings.track_threshold)
        self.engine.set_tracks(store, self.settings.track_threshold)
        spider.repairs = []
        spider.finish_reason = None
        spider.check_failures = []
        spider.coverage = {}
        spider.invalid_fields = {}
        spider.proxy_stats = {}
        #: When the crawl started, for max_time.
        self.started = time.monotonic()
        #: For max_memory: each worker's peak memory as it last reported
        #: it, by process id, and when to read memory use next.
        self.worker_peaks: dict[int, int] = {}
        self.next_memory_check = 0.0
        self.memory_warned = False
        self.memory_unknown = False
        #: Repairs to ask for once the crawl is done, one per site and name.
        self.repair_jobs: list[Any] = []
        self.repair_asked: set[tuple[str, str]] = set()
        self.counts = {
            "items": 0,
            "items_dropped": 0,
            "items_validated": 0,
            "items_invalid": 0,
            "requests_dropped": 0,
            "responses_dropped": 0,
            "callback_errors": 0,
            "invalid_urls": 0,
            "pages_ignored": 0,
            "items_already_written": 0,
            "relocated": 0,
            "lost": 0,
            "repair_proposals": 0,
        }
        #: spider.stats: live while the crawl runs, final once it has ended.
        self.live = _stats.Stats(self.live_stats)
        spider.stats = self.live
        self.monitor = _stats.Monitor(
            self.live, self.settings, spider.name or type(spider).__name__
        )
        # Last, so a from_crawler can read the stats.
        self.crawler = _hooks.Crawler(self)
        spider.crawler = self.crawler
        try:
            middlewares = self.build(spider.middlewares)
            spider_middlewares = self.build(spider.spider_middlewares)
            # In place: the crawl closes what's in this list.
            pipelines[:] = self.build(pipelines)
        except BaseException:
            # The crawl never starts, so nothing else closes these, and
            # spider.stats mustn't read the closed engine.
            self.live._freeze({})
            self.engine.close()
            if state:
                store.close()
            raise
        # process_request in order; process_response and process_exception
        # in reverse, the first middleware being nearest the engine.
        self.on_request = _methods(middlewares, "process_request")
        self.on_response = _methods(middlewares[::-1], "process_response")
        self.on_exception = _methods(middlewares[::-1], "process_exception")
        self.spider_mw = _hooks.SpiderMiddlewares(spider_middlewares, spider)
        #: Whether pages' callbacks go through spider middlewares.
        self.layered = bool(self.spider_mw)
        #: What gets signals: middlewares, spider middlewares, then
        #: pipeline stages.
        self.listeners = [*middlewares, *spider_middlewares, *pipelines]
        # Stages that download what items link to fetch through this crawl.
        for stage in pipelines:
            if isinstance(stage, _files._Media):
                stage._attach(self)

    def crawl_page(self, response: Any, request: Request, root: Any = None) -> Page:
        """A page of this crawl: parsed, if it's parsed when first read,
        within the crawl's budget and with its recover setting."""
        settings = self.spider.settings
        parse = (settings.parse_timeout, settings.xml_recover)
        return Page(response, request, root, parse=parse)

    def build(self, components: list[Any]) -> list[Any]:
        """The objects a list of components stands for: a class is made with
        ``from_crawler(crawler)`` if it has one and ``cls()`` if not, as
        Scrapy makes its components, and left out if it raises
        NotConfigured; anything else is itself."""
        made = []
        for component in components:
            if not isinstance(component, type):
                made.append(component)
                continue
            from_crawler = getattr(component, "from_crawler", None)
            try:
                if callable(from_crawler):
                    made.append(from_crawler(self.crawler))
                    self.built.append(made[-1])
                else:
                    made.append(component())
            except Exception as e:
                if not _named(e, "NotConfigured"):
                    raise
                log.info("%s left out: %s", component.__qualname__, e or "not configured")
        return made

    def connect(self, name: str, receiver: Callable[..., Any]) -> None:
        self.connected.setdefault(name, []).append(receiver)
        self.found.clear()

    def disconnect(self, name: str, receiver: Callable[..., Any]) -> None:
        with contextlib.suppress(ValueError):
            self.connected.get(name, []).remove(receiver)
        self.found.clear()

    def handlers(self, name: str) -> list[Callable[..., Any]]:
        """Who hears signal ``name``: the methods of that name of the
        middlewares and pipelines (but not of those from_crawler made,
        which hear only what they connect, as in Scrapy), then what was
        connected by hand, each once."""
        found = self.found.get(name)
        if found is None:
            found = []
            for listener in self.listeners:
                method = getattr(listener, name, None)
                if callable(method) and not any(listener is b for b in self.built):
                    found.append(method)
            for receiver in self.connected.get(name, []):
                # A bound method is a new object each time: == finds it.
                if receiver not in found:
                    found.append(receiver)
            self.found[name] = found
        return found

    async def download(
        self, url: str, kind: str, referer: str | None, jar: str | None = None
    ) -> tuple | None:
        """Fetches a file an item links to through the engine, ahead of the
        pages and outside them, in the cookie jar ``jar`` names (None for
        the crawl's own): for netweir.files and netweir.images, which the
        crawl waits for. See the engine's ``download``."""
        return await self.engine.download(next(self.ids), url, kind, referer, jar=jar)

    def resume(self) -> dict[str, int] | None:
        """Queues what an earlier run of this crawl left unfinished, and
        returns the output files' lengths as last recorded (None without a
        checkpoint)."""
        saved = self.engine.resume()
        if saved is None or not (saved["pending"] or saved["items"] or saved["counters"]):
            # No checkpoint, or one this crawl has just created.
            return None
        self.delivered = set(saved["items"])
        for head, sent, policy in saved["pending"]:
            request = _restored(head, sent, policy)[0]
            try:
                self.from_data(request, "checkpoint")
            except ValueError as e:
                log.error("%s from the checkpoint is skipped: %s", request.url, e)
                continue
            self.submit(request, row=head[0])
        if saved["pending"] or self.delivered:
            log.info(
                "resuming: %d requests left, %d items already written",
                len(saved["pending"]),
                len(self.delivered),
            )
        state = json.loads(saved["counters"]) if saved["counters"] else {}
        self.parts = list(state.get("parts", []))
        return state

    def payload(self, request: Request) -> str:
        """The request's own part, saved in the checkpoint or sent to the
        shared queue as JSON: callbacks by name, meta and cb_kwargs."""
        if not self.by_name:
            return "{}"
        try:
            meta = json.dumps(request.meta)
        except TypeError as e:
            raise TypeError(f"with a {self.keeper}, meta must be JSON: {e}") from None
        cb_kwargs = ""
        if request.cb_kwargs:
            try:
                cb_kwargs = f', "cb_kwargs": {json.dumps(request.cb_kwargs)}'
            except TypeError as e:
                raise TypeError(f"with a {self.keeper}, cb_kwargs must be JSON: {e}") from None
        callback = json.dumps(self.method_name(request.callback))
        errback = json.dumps(self.method_name(request.errback))
        browser = json.dumps(request.browser)
        # A request still waiting when the crawl stops keeps its cookies,
        # and its steps, call steps by name as callbacks are.
        cookies = f', "cookies": {json.dumps(request.cookies)}' if request.cookies else ""
        steps = ""
        if request.steps is not None:
            steps = f', "steps": {steps_json(request.steps, self.step_name)}'
        return (
            f'{{"callback": {callback}, "errback": {errback}, "meta": {meta}, '
            f'"browser": {browser}{cb_kwargs}{cookies}{steps}}}'
        )

    def ruled_request(self, rule: int, url: str, depth: int = 0) -> Request:
        """A request as rule ``rule`` made it, in Rust: its URL, depth,
        callback, and steps as the engine has them (call steps by name), so
        it pickles for a worker and keys deltafetch as the engine did."""
        steps = self.rule_steps[rule]
        return Request(
            url,
            callback=self.rules[rule].callback,
            depth=depth,
            steps=None if steps is None else json.loads(steps),
        )

    def from_data(self, request: Request, source: str) -> None:
        """Refuses a request read back from data (``source`` is "queue" or
        "checkpoint"; with a shared queue, every request is, the spider's
        own included) that names what it shouldn't: a callback, errback or
        call step that isn't a method of the spider's own classes, or one
        of netweir.Spider's that netweir itself calls as a callback; or a
        set_input_files step whose files aren't in ``browser_upload_dir``.
        A set_input_files step that passes is given the folder, which the
        engine checks again just before it uploads."""
        names = [("callback", request.callback), ("errback", request.errback)]
        for step in request.steps or ():
            if step.get("action") == "call":
                names.append(("call step", step.get("method")))
        for what, name in names:
            if name is None:
                continue
            if not isinstance(name, str):
                raise ValueError(f"its {what} is {name!r}, not a method's name")
            why = _not_a_callback(self.spider, name)
            if why is not None:
                raise ValueError(f"its {what} {name!r} {why}")
        uploads = [s for s in request.steps or () if s.get("action") == "set_input_files"]
        if not uploads:
            return
        folder = self.settings.browser_upload_dir
        if folder is None:
            raise ValueError(
                "it uploads files (set_input_files), and a request read back from the"
                f" {source} (with a shared queue, every request is) may upload only from"
                " Settings.browser_upload_dir, which isn't set"
            )
        root = os.path.normcase(os.path.realpath(folder))
        for step in uploads:
            files = step.get("files")
            files = [files] if isinstance(files, str) else list(files or ())
            for path in files:
                if not _inside(root, os.path.normcase(os.path.realpath(path))):
                    raise ValueError(
                        f"it uploads {path!r}, which isn't in Settings.browser_upload_dir"
                        f" ({folder!r})"
                    )
            # Whatever the data said, the engine holds it to this folder.
            step["within"] = os.path.realpath(folder)

    def step_name(self, fn: Callable[..., Any]) -> str:
        """The name a checkpoint or the shared queue keeps for a call
        step's function, which must be a spider method, as callbacks must."""
        try:
            name = self.method_name(fn)
        except TypeError:
            raise TypeError(
                f"with a {self.keeper}, call steps are saved by name, so {fn!r} must be a"
                " method of the spider"
            ) from None
        assert name is not None
        return name

    def engine_step_name(self, fn: Callable[..., Any]) -> str | None:
        """The name the engine is given for a call step's function: a
        spider method's, or None for any other, which is found again by
        its place in the request's steps."""
        name = getattr(fn, "__name__", None)
        if name is not None and getattr(self.spider, name, None) == fn:
            return name
        return None

    def method_name(self, fn: Callable[..., Any] | str | None) -> str | None:
        if fn is None or isinstance(fn, str):
            return fn
        name = getattr(fn, "__name__", None)
        if name is not None and getattr(self.spider, name, None) == fn:
            return name
        raise TypeError(
            f"with a {self.keeper}, callbacks are saved by name, so {fn!r} must be a"
            " method of the spider"
        )

    @property
    def keeper(self) -> str:
        """What keeps requests as data: for messages."""
        return "checkpoint" if self.saving else "queue"

    def submit(
        self, request: Request, row: int | None = None, answered: bool = False
    ) -> str | None:
        """Hands ``request`` to the engine; with ``answered``, only to be
        screened, as a middleware has its page. What the engine said, or
        None for a request it was never given."""
        rid = next(self.ids)
        # A request with no callback on a rules spider is the rules' to read.
        apply_rules = bool(self.rules) and request.callback is None
        to_python = not apply_rules or self.has_parse
        # Request.proxy wins; then meta's, where None or "" means no proxy
        # at all, as in Scrapy, which the engine reads as "".
        proxy = request.proxy
        meta = request.meta or {}
        if proxy is None and "proxy" in meta:
            proxy = meta["proxy"] or ""
        delta = _delta_meta(meta) if self.delta else (True, None)
        message = None
        # What fail_fast raises: the class of what went wrong.
        error: type[Exception] = TypeError
        if delta is None:
            message = (
                f'{request.url}: meta["deltafetch_key"] must be a str or bytes, not'
                f" {type(meta['deltafetch_key']).__name__}; skipped"
            )
        try:
            policy = _policy(request, meta, self.settings.max_redirects)
            jar = _jar_name(meta)
            if request.cookies:
                Crawler.check_cookies(request.url, request.cookies)
            steps = None
            if request.steps is not None:
                steps = _capped(
                    steps_json(request.steps, self.engine_step_name),
                    self.settings.browser_steps_max,
                    "steps",
                )
            steps_in_fingerprint = _steps_in_fingerprint(meta)
        except (TypeError, ValueError) as e:
            message = f"{request.url}: {e}; skipped"
            error = type(e)
        if message is not None:
            # As a callback's error would be, from a callback or start().
            self.counts["callback_errors"] += 1
            if self.settings.fail_fast:
                raise error(message)
            log.error("%s", message)
            return None
        payload = self.payload(request) if row is None else "{}"
        if request.auth is not None and _meta_login(meta) and not self.warned_login:
            self.warned_login = True
            log.warning(
                "%s: auth= wins over meta's http_user and http_pass, and goes to this"
                " request's own origin only, whatever meta's http_auth_domain says"
                " (said once a crawl)",
                request.url,
            )
        outcome = self.engine.submit(
            rid,
            request.url,
            request.priority,
            _pairs(request.headers),
            request.dont_filter,
            apply_rules=apply_rules,
            to_python=to_python,
            depth=request.depth,
            payload=payload,
            row=row,
            browser=request.browser,
            method=request.method,
            kind=request.kind,
            body=request.body.encode() if isinstance(request.body, str) else request.body,
            content_type=request.content_type,
            referer=request.referer,
            retry_post=request.retry_post,
            proxy=proxy,
            delta=delta[0],
            delta_key=delta[1],
            answered=answered,
            **policy,
            jar=jar,
            cookies=request.cookies,
            dont_merge_cookies=bool(meta.get("dont_merge_cookies")),
            steps=steps,
            steps_in_fingerprint=steps_in_fingerprint,
        )
        if outcome in ("queued", "pushed", "answered"):
            self.accepted += 1
        if outcome == "queued":
            self.waiting[rid] = request
        elif outcome == "delta":
            log.debug("gave items in an earlier run, skipped: %s", request.url)
        elif outcome == "invalid":
            self.counts["invalid_urls"] += 1
            log.warning("not an http(s) URL, skipped: %s", request.url)
        elif outcome == "bad_proxy":
            self.counts["invalid_urls"] += 1
            log.warning(
                "%s: its proxy can't be used (an http://, https://, socks5:// or socks5h:// URL,"
                " with any login percent-encoded); skipped",
                request.url,
            )
        return outcome

    def resolve(
        self, fn: Callable[..., Any] | str | None, default: str | None
    ) -> Callable[..., Any] | None:
        if fn is None:
            return getattr(self.spider, default) if default else None
        if isinstance(fn, str):
            return getattr(self.spider, fn)
        return fn

    async def go(self) -> dict[str, int]:
        # page.css(track=...) in callbacks uses this crawl's store.
        token = _track._crawl.set(self.tracks)
        asking = _track._repairs.set(self.ask_repair if self.spider.repair else None)
        reported = _track._crawl_reported.set(set())
        try:
            return await self.crawl()
        finally:
            _track._crawl_reported.reset(reported)
            _track._repairs.reset(asking)
            _track._crawl.reset(token)

    def ask_repair(self, site: str, name: str, kind: str, query: str, html: Any, url: str):
        """Queues a repair for after the crawl. ``html`` is the page, or a
        function that gives it, called only for a repair not yet queued."""
        if self.spider.repair is None or (site, name) in self.repair_asked:
            return
        from netweir.repair import Job

        self.repair_asked.add((site, name))
        fingerprint = self.tracks[0].get(site, name)
        page = html() if callable(html) else html
        self.repair_jobs.append(Job(site, name, kind, query, fingerprint, page, url))

    async def run_repairs(self) -> None:
        """Asks for the repairs the crawl ran into, off the event loop."""
        for job in self.repair_jobs:
            try:
                proposal = await asyncio.to_thread(self.spider.repair.propose, job)
            except Exception:  # noqa: BLE001 - a failed repair never fails the crawl
                log.exception("asking for a repair of %s on %s failed", job.name, job.site)
                continue
            if proposal is not None:
                self.spider.repairs.append(proposal)
                self.counts["repair_proposals"] += 1

    def check_limits(self) -> None:
        """Raises _Stop if the crawl has reached one of its limits."""
        if self.closing is not None:
            raise _Stop(self.closing, limit=False)
        settings = self.settings
        if settings.max_items is not None and self.counts["items"] >= settings.max_items:
            raise _Stop("max_items")
        if (
            settings.max_errors is not None
            and self.counts["callback_errors"] >= settings.max_errors
        ):
            raise _Stop("max_errors")
        if settings.max_time is not None and self.time_left() <= 0:
            raise _Stop("max_time")
        if settings.max_memory is not None or settings.memory_warning is not None:
            self.check_memory()

    def check_memory(self) -> None:
        """The memory limits, read at most once a second: the peak resident
        memory of this process plus each worker's, as each last said."""
        now = time.monotonic()
        if now < self.next_memory_check:
            return
        self.next_memory_check = now + 1.0
        own = _memory.peak()
        if own is None:
            if not self.memory_unknown:
                self.memory_unknown = True
                log.warning("can't read memory use here: max_memory and memory_warning do nothing")
            return
        used = (own + sum(self.worker_peaks.values())) // (1024 * 1024)
        settings = self.settings
        if (
            settings.memory_warning is not None
            and used >= settings.memory_warning
            and not self.memory_warned
        ):
            self.memory_warned = True
            log.warning(
                "memory use has reached %d MiB (memory_warning is %d)",
                used,
                settings.memory_warning,
            )
        if settings.max_memory is not None and used >= settings.max_memory:
            log.warning(
                "memory use has reached %d MiB (max_memory is %d)", used, settings.max_memory
            )
            raise _Stop("max_memory")

    def time_left(self) -> float | None:
        if self.settings.max_time is None:
            return None
        return self.settings.max_time - (time.monotonic() - self.started)

    async def next_events(self) -> list[tuple]:
        """The next batch, waiting no longer than max_time allows, and
        reading memory use every second while it waits, so slow pages don't
        keep max_memory from being seen."""
        watching = self.settings.max_memory is not None or self.settings.memory_warning is not None
        if self.time_left() is None and not watching:
            return await self.engine.next(256)
        task = asyncio.ensure_future(self.engine.next(256))
        try:
            while True:
                left = self.time_left()
                timeout = 1.0 if watching else left
                if left is not None:
                    timeout = min(timeout, max(left, 0))
                done, _ = await asyncio.wait({task}, timeout=timeout)
                if done:
                    return task.result()
                if left is not None and self.time_left() <= 0:
                    raise _Stop("max_time")
                self.check_memory()
        except BaseException:
            task.cancel()
            raise

    async def crawl(self) -> dict[str, int]:
        started = self.started
        if not self.settings.obey_robots:
            log.warning("obey_robots is off: robots.txt is not being checked")
        abandoned = True
        calls: asyncio.Future | None = None
        try:
            if self.settings.workers > 1:
                from netweir import _workers

                state = self.settings._checkpoint_file()
                self.pool = _workers.Pool(
                    self.spider, self.settings, state or _track.default_path()
                )
            try:
                # A spider_opened handler may have raised CloseSpider, in
                # which case the engine never starts, as in Scrapy.
                self.check_limits()
                # As Scrapy's engine starts once the spider is open.
                await self.signal("engine_started")
                self.engine_started = True
                calls = asyncio.ensure_future(self.serve_calls())
                await self.queue_start()
                while True:
                    self.check_limits()
                    await self.drain()
                    events = await self.next_events()
                    if not events:
                        # The engine stops sending at max_pages, and ends
                        # the crawl with requests still queued (here, or
                        # in the shared queue, for all this process knows).
                        if (
                            self.engine.stats()["queued"]
                            or self.capped_out
                            or (self.settings.queue is not None and self.engine.capped())
                        ):
                            raise _Stop("max_pages")
                        if await self.idle():
                            continue
                        break
                    if self.pool is None:
                        # Each page goes through the middlewares as its turn
                        # comes, so a limit reached part way through the
                        # batch leaves the rest untouched.
                        await self.dispatch_batch(events, [None] * len(events), respond=True)
                    else:
                        # The middlewares run here, before a page goes to a
                        # worker; then the batch's callbacks all start, and
                        # their results are dealt with in the batch's order.
                        events = [e for e in [await self.respond(e) for e in events] if e]
                        jobs = [self.send(event) for event in events]
                        await self.dispatch_batch(events, jobs)
                    self.settle()
                self.spider.finish_reason = "finished"
                abandoned = False
            except _Stop as stop:
                self.spider.finish_reason = stop.reason
                if stop.limit:
                    log.info("stopping the crawl: %s reached", stop.reason)
                else:
                    log.info("stopping the crawl: %s", stop.reason)
        finally:
            if calls is not None:
                calls.cancel()
                with contextlib.suppress(asyncio.CancelledError):
                    await calls
            if self.pool is not None:
                # Stopping early (fail_fast, an interrupt) doesn't wait for
                # callbacks the workers are still running.
                self.pool.close(abandon=abandoned)
                self.pool = None
        await self.run_repairs()
        stats = self.stats()
        log.info(
            "crawl finished in %.1fs: %s",
            time.monotonic() - started,
            ", ".join(f"{k} {v}" for k, v in stats.items()),
        )
        return stats

    async def serve_calls(self) -> None:
        """Runs the call steps of pages in Chrome as they come, each as a
        task of its own, alongside the callbacks: a slow callback never
        holds a page's steps up. Always here, in this process, as the live
        page is, whether or not there are workers."""
        running: set[asyncio.Future] = set()
        try:
            while True:
                call = await self.engine.next_call()
                if call is None:
                    return
                task = asyncio.ensure_future(self.run_call(call))
                running.add(task)
                task.add_done_callback(running.discard)
        finally:
            # Calls that come now have nobody to run them: they fail at
            # once rather than wait out their timeouts.
            self.engine.close_calls()
            for task in running:
                task.cancel()

    async def run_call(self, call: Any) -> None:
        """One call step: the spider method it names, or the function in
        its request's steps, called with the page and the request; what it
        returns, as JSON, is the step's result."""
        try:
            request = self.waiting.get(call.id)
            if call.method is not None:
                fn = getattr(self.spider, call.method, None)
                if not callable(fn):
                    raise AttributeError(f"the spider has no method {call.method!r}")
            elif request is not None and request.steps is not None:
                fn = request.steps[call.index]["method"]
            else:
                raise LookupError("its function isn't known in this process")
            page = call.page
            if request is None:
                # A rule's request, which the rules made in Rust: the same
                # URL, depth and callback.
                if call.rule is not None:
                    request = self.ruled_request(call.rule, call.url, call.depth)
                else:
                    request = Request(call.url, depth=call.depth)
            plain = getattr(fn, "__func__", fn)
            if not inspect.iscoroutinefunction(plain) and plain not in self.warned_sync:
                self.warned_sync.add(plain)
                log.warning(
                    "the call step %s is a plain function: while it runs, nothing else in"
                    " the crawl does. An async def lets the crawl go on meanwhile (said once"
                    " a function)",
                    getattr(fn, "__qualname__", repr(fn)),
                )
            value = await asyncio.wait_for(_result(fn(page, request)), call.timeout)
            try:
                answer = json.dumps(value)
            except (TypeError, ValueError) as e:
                raise TypeError(f"what it returned isn't JSON: {e}") from None
            call.reply(answer)
        except asyncio.CancelledError:
            call.fail("the crawl stopped")
            raise
        except TimeoutError:
            # Before 3.11, asyncio's isn't the built-in one.
            call.fail(f"it took more than {call.timeout:g}s")
        except Exception as e:  # noqa: BLE001 - the step's on_error decides
            log.debug("a call step raised", exc_info=True)
            call.fail(f"{type(e).__name__}: {e}")

    async def queue_start(self) -> None:
        """The spider's start requests, through the spider middlewares'
        process_start."""
        start = self.spider.start()
        if self.spider_mw.middlewares:
            start = self.spider_mw.start(start)
        try:
            if isinstance(start, AsyncIterable):
                async for request in start:
                    await self.queue(request)
            else:
                for request in start:
                    await self.queue(request)
        except Exception as e:
            if _named(e, "CloseSpider"):
                raise _closed(e) from None
            raise

    async def idle(self) -> bool:
        """The engine has nothing left: spider_idle, whose handlers may give
        it more (crawler.engine.crawl), keep the crawl open with
        DontCloseSpider, or end it with CloseSpider, as in Scrapy. Whether
        the crawl goes on."""
        if not self.handlers("spider_idle"):
            return False
        before = self.accepted
        raised = await self.signal(
            "spider_idle", quiet=("DontCloseSpider", "CloseSpider"), spider=self.spider
        )
        await self.drain()
        # crawler.engine.close_spider() from a handler wins.
        self.check_limits()
        if any(_named(e, "DontCloseSpider") for e in raised):
            if self.accepted == before:
                # Asked again a while later, as Scrapy's heartbeat does.
                left = self.time_left()
                await asyncio.sleep(_IDLE_WAIT if left is None else max(0, min(_IDLE_WAIT, left)))
            return True
        if self.accepted != before:
            return True
        for e in raised:
            if _named(e, "CloseSpider"):
                raise _closed(e)
        return False

    async def drain(self) -> None:
        """Requests crawler.engine.crawl() was given, and work middlewares
        handed back while a callback was running."""
        while self.scheduled or self.ready:
            while self.scheduled:
                await self.queue(self.scheduled.popleft())
            while self.ready:
                await self.ready.popleft()()
                # A page answered without a fetch can lead to more such
                # pages, on and on, without the engine's batches between.
                self.check_limits()

    def stats(self) -> dict[str, Any]:
        stats = self.live_stats()
        for gauge in _stats.GAUGES:
            stats.pop(gauge, None)
        return stats

    def live_stats(self) -> dict[str, Any]:
        """The spider's own stats, then the engine's, gauges included, then
        the counts. Read from other threads too: the progress line and the
        metrics endpoint."""
        stats = {**self.live._custom, **self.engine.stats(), **self.counts}
        if "queue_bad" in stats:
            # The engine counts the members it couldn't read; this, those
            # it could but that weren't requests.
            stats["queue_bad"] += self.queue_bad
        return stats

    def settle(self) -> None:
        """Every event in the batch has been dealt with. Once the files its
        items went to are on disk, record the items as delivered and the
        requests as done, in that order, so a crash never loses an item:
        at worst, a resume writes one again. deltafetch's pages are
        recorded then too, so a page is never skipped for an item that
        didn't reach the file. With a shared queue, the requests are
        released once their items are in the files; Parquet's stay in
        memory until the end."""
        from netweir import export

        if self.saving:
            self.watch_checkpoint()
        exporters = [s for s in self.pipelines if isinstance(s, export._Exporter)]
        delta = self.delta and (self.produced or self.counts["items"] != self.delta_items)
        if not (self.saving or delta):
            if self.settings.queue is not None:
                for stage in exporters:
                    stage.flush()
            self.engine.ack()
            return
        # Not final until closed: Parquet is readable only then, and a file
        # bound for cloud storage (never with a checkpoint) is uploaded then.
        if not all(stage.flush() for stage in exporters) or any(s.remote for s in exporters):
            if not self.saving:
                self.engine.ack()
            return
        if self.saving:
            self.record()
        if self.checkpoint_committed():
            self.record_pages()
        self.engine.ack()

    def watch_checkpoint(self) -> None:
        """Warns once when checkpoint commits start failing, and once when
        they work again; raises OSError when the checkpoint has stopped,
        having held back more failed writes than it keeps. Reads the
        commits made so far, without waiting on one."""
        failing = self.engine.checkpoint_failing()
        if failing is None:
            if self.checkpoint_warned:
                self.checkpoint_warned = False
                log.warning("checkpoint commits work again: it's up to date")
            return
        why, stopped = failing
        if stopped:
            raise OSError(why)
        if not self.checkpoint_warned:
            self.checkpoint_warned = True
            log.warning(
                "%s; the crawl goes on, and each commit tries the failed writes"
                " again (a resume meanwhile starts from the last that worked)",
                why,
            )

    def checkpoint_committed(self) -> bool:
        """With a checkpoint and deltafetch, waits for the checkpoint's
        commit: the deltafetch record must never be ahead of it, or a resume
        would cut the output back to the checkpoint's lengths and lose items
        whose pages deltafetch skips. False if the commit failed (its writes
        go again with the next); the pages wait for the next batch."""
        if not (self.saving and self.delta):
            return True
        try:
            self.engine.flush()
        except OSError:
            return False
        return True

    def recorded_files(self, state: dict[str, Any] | None) -> dict[str, _Recorded]:
        """The output files as last recorded, by absolute path: a resuming
        checkpoint's, else deltafetch's (Parquet files and parts with a
        length of 0), else none."""
        if state is not None:
            tails, clean = state.get("tails", {}), state.get("clean", False)
            return {p: (n, tails.get(p), clean) for p, n in state.get("files", {}).items()}
        if self.delta:
            return dict(self.engine.delta_files())
        return {}

    def check_output(
        self, stage: Any, state: dict[str, Any] | None, files: dict[str, _Recorded]
    ) -> None:
        """Refuses an output file the crawl can't carry on: one changed
        since it was recorded, one that has grown since the run that wrote
        it closed it, or, with deltafetch, one that is there but that a
        record with pages in it doesn't know (its pages' items may be in
        another file, and starting it afresh would lose what it holds)."""
        from netweir import export

        path = os.path.abspath(stage.path)
        if stage.batch is not None or stage.remote or not os.path.exists(stage.path):
            return
        if path in files:
            size, tail, clean = files[path]
            if isinstance(stage, export.parquet):
                return
            _check_resumable(stage, size, tail)
            if isinstance(stage, export.csv):
                stage._check_columns()
            # A run that closed its file left it ending at the length on
            # record. Only after a crash can there be more, a tail it tore;
            # past a clean close, the bytes are someone else's.
            if clean and os.path.getsize(stage.path) > size:
                raise ValueError(
                    f"{stage.path} has grown since the crawl that wrote it finished"
                    f" ({os.path.getsize(stage.path)} bytes; it left {size}): something else"
                    " added to it, which carrying on would cut off; move it away, or start"
                    " the crawl afresh (a new checkpoint, or deltafetch_reset=True)"
                )
            return
        if self.delta and state is None and not self.engine.delta_was_empty():
            raise ValueError(
                f"{stage.path} is already there, and deltafetch's record in"
                f" {self.settings.deltafetch} doesn't know it: move it away (the run then"
                " writes only what's new), write to another path, or start afresh with"
                " deltafetch_reset=True"
            )

    def record_lengths(self, ends: dict[str, int]) -> None:
        """Records ``ends`` as the output files' lengths, not closed, and
        waits for that to be on disk."""
        self.ends = ends
        try:
            if self.saving:
                self.record()
                # A failed write goes again with the next commit, and the
                # first settle warns of it.
                with contextlib.suppress(OSError):
                    self.engine.flush()
            self.record_pages()
        finally:
            self.ends = None

    def output_lengths(self) -> dict[str, int]:
        """Each single output file's length (not a Parquet file's), by
        absolute path: as it is, or as given to record_lengths."""
        from netweir import export

        if self.ends is not None:
            return self.ends
        return {
            os.path.abspath(stage.path): os.path.getsize(stage.path)
            for stage in self.pipelines
            if isinstance(stage, export._Exporter)
            and not isinstance(stage, export.parquet)
            and stage.batch is None
            and os.path.exists(stage.path)
        }

    def record_pages(self, closed: bool = False) -> bool:
        """Tells deltafetch which pages gave delivered items, and how long
        each output file is with them in it (a Parquet file only once
        ``closed``, when it's written), and whether its writer has closed
        it. Whether that was recorded. A failed write is a warning, once,
        and its pages wait for the next write: until one lands, the files'
        lengths on record don't cover their items, and a crash meanwhile
        only means fetching those pages again."""
        if not self.delta:
            return False
        from netweir import export

        files = [
            (path, size, _tail(path, size), closed) for path, size in self.output_lengths().items()
        ]
        if self.ends is None and closed:
            for stage in self.pipelines:
                if isinstance(stage, export.parquet) and stage.batch is None:
                    # The output, and this run's part (closed, not yet
                    # named), in the same commit as its pages: a crash before
                    # it's named leaves a part the next run names.
                    files.append((os.path.abspath(stage.path), 0, None, True))
                    if stage.pending is not None:
                        files.append((os.path.abspath(stage.pending), 0, None, True))
        produced, self.produced = list(self.produced), set()
        try:
            self.engine.produced(produced, files)
        except OSError as e:
            # Kept for the next write, which records them with the lengths
            # that cover their items.
            self.produced |= set(produced)
            if "deltafetch" not in self.warned:
                self.warned.add("deltafetch")
                log.warning("%s; those pages will be fetched again next run", e)
            return False
        self.delta_items = self.counts["items"]
        return True

    def record(self, closed: bool = False) -> None:
        """Records the delivered items, and how long each output file is
        with them in it and whether the exporters have ``closed`` them, in
        one commit."""
        from netweir import export

        files = self.output_lengths()
        tails = {path: _tail(path, size) for path, size in files.items()}
        parts = [
            os.path.abspath(stage.pending)
            for stage in self.pipelines
            if isinstance(stage, export.parquet) and stage.pending is not None
        ]
        self.parts.extend(p for p in parts if p not in self.parts)
        counters = {"files": files, "tails": tails, "clean": closed, "parts": self.parts}
        self.engine.settle(self.unrecorded, json.dumps(counters))
        self.unrecorded = []

    def tidy_parts(self, stage: Any, files: dict[str, Any]) -> None:
        """Before a Parquet exporter starts: a part whose items were recorded
        (as delivered by the checkpoint, or with its pages by deltafetch)
        but which the process died before naming gets its name; any other
        .partial file of this output, never recorded, is deleted (the run
        makes its items again). A part the checkpoint has is one whose run
        closed its pipelines, as finish leaves it out otherwise."""
        import glob

        known = set(self.parts) | set(files)
        if self.delta:
            known |= set(self.engine.delta_files())
        stem = stage.path[: -len(".parquet")] if stage.path.endswith(".parquet") else stage.path
        for partial in glob.glob(glob.escape(stem) + "*.parquet.partial"):
            final = partial[: -len(".partial")]
            recorded = os.path.abspath(final) in known
            if recorded and not os.path.exists(final):
                os.replace(partial, final)
            else:
                os.remove(partial)

    def finish(self, closed: bool = True) -> None:
        """After the exporters are closed: the items they hold are final.
        The requests are done only if the crawl got to the end; one cut
        short leaves its last batch to be fetched again. ``closed`` is
        False when an exporter failed to close (an upload, say): its items
        may not be there, so deltafetch doesn't record their pages."""
        from netweir import export

        # With deltafetch, a run whose pipelines didn't all close leaves
        # the checkpoint as its last batch did: a Parquet part it won't name
        # mustn't have its items on record as delivered, nor its requests
        # as done, or a run taking the checkpoint up again would lose them.
        # That run fetches them again instead.
        keep = closed or not self.delta
        recorded = committed = False
        try:
            # Only once the output files were cut back to their recorded
            # lengths: a crawl that failed before that (a pipeline that
            # couldn't open) mustn't record a half-written tail as kept.
            if self.saving and self.outputs_ready and keep:
                self.record(closed=closed)
            if self.outputs_ready and closed:
                committed = self.checkpoint_committed()
                if committed:
                    recorded = self.record_pages(closed=True)
            if self.completed and (keep and (committed or not self.delta)):
                self.engine.ack()
        finally:
            # Not left to the garbage collector: a traceback can keep this
            # run alive, and Windows won't delete an open file.
            self.engine.close()
            if self.settings._checkpoint_file():
                self.tracks[0].close()
        # Only now, with their items on record, do Parquet parts get their
        # real names.
        for stage in self.pipelines:
            if not isinstance(stage, export.parquet):
                continue
            # A part is named once its items are on record: its pages in
            # deltafetch's record, or its items as delivered in the
            # checkpoint (which a run taking it up again won't write again,
            # so the part must stay).
            on_record = recorded or (self.saving and committed)
            if self.delta and stage.pending is not None and not on_record:
                # Named, it would be read along with the items the next run
                # fetches for its pages again.
                log.warning(
                    "%s: its pages couldn't be recorded, so it's left as %s.partial,"
                    " which the next run deletes; that run fetches its pages again",
                    stage.pending,
                    stage.pending,
                )
                continue
            stage.publish()

    async def dispatch_batch(
        self, events: list[tuple], jobs: list[Any], respond: bool = False
    ) -> None:
        """Deals with a batch's events in order. With fail_fast, a worker's
        error stops the crawl when it happens, not when its page's turn
        comes behind slower callbacks."""
        dispatching = asyncio.ensure_future(self._dispatch_all(events, jobs, respond))
        pending = [job for job in jobs if job is not None]
        if not (self.settings.fail_fast and pending):
            await dispatching
            return
        failed: asyncio.Future = asyncio.get_running_loop().create_future()

        def check(job: asyncio.Future) -> None:
            if failed.done() or job.cancelled() or job.exception() is not None:
                return
            error = job.result()[3]
            if error is not None:
                failed.set_result(error[0])

        for job in pending:
            job.add_done_callback(check)
        await asyncio.wait({dispatching, failed}, return_when=asyncio.FIRST_COMPLETED)
        if dispatching.done():
            failed.cancel()
            dispatching.result()
            return
        dispatching.cancel()
        self.counts["callback_errors"] += 1
        raise failed.result()

    async def _dispatch_all(self, events: list[tuple], jobs: list[Any], respond: bool) -> None:
        # What responding to the batch handed back (an errback for a
        # dropped page, with workers), before the batch is settled.
        await self.drain()
        for event, job in zip(events, jobs, strict=True):
            if respond:
                event = await self.respond(event)
            if event is not None:
                await self.dispatch(event, job)
            await self.drain()
            self.check_limits()

    def send(self, event: tuple) -> Any:
        """Starts the event's callback in a worker, if there are workers
        and it's a page's callback that can run in one. The pending result,
        or None."""
        if self.pool is None or event[0] not in ("fetched", "ruled"):
            return None
        if event[0] == "fetched":
            _, rid, response, _root, live = event[:5]
            if live is not None:
                return None  # a live Chrome page can't leave this process
            request = self.waiting.get(rid)
            if request is None:
                return None
            callback, default = request.callback, "parse"
        else:
            _, rule, url, response, _root, depth = event[:6]
            callback, default = self.rules[rule].callback, None
            request = self.ruled_request(rule, url, depth)
        try:
            name = self.method_name(callback) or default
            sent = dataclasses.replace(
                request, callback=name, errback=self.method_name(request.errback)
            )
        except TypeError:
            return None  # run here, where the error is reported as usual
        if name is None:
            return None
        try:
            return self.pool.submit(name, response, sent)
        except BrokenProcessPool:
            raise _worker_died(name, request.url) from None

    async def dispatch(self, event: tuple, job: Any = None) -> None:
        kind = event[0]
        if kind == "item":
            _, rule, item, invalid, url, notes, html = event
            item_class = self.rules[rule].extract
            _warn_invalid(invalid, self.warned)
            site = _track.site_of(url)
            for field, what, score in notes:
                self.counts["relocated" if what == "relocated" else "lost"] += 1
                _track.report(field, site, what, score)
                spec = item_class._fields[field.rsplit(".", 1)[1]]
                self.ask_repair(site, spec.track, spec.kind, spec.query, html or "", url)
            # Its own namespace: the page's callback numbers its items from 0
            # too.
            self.source, self.position = f"rule{rule}:{url}", 0
            self.page = None
            self.page_url = url
            self.page_jar = None
            # The page a rule's link led to, fetched as a plain GET. Its
            # one item is all it gives.
            self.from_page = (
                ("GET", url, None, None, None, self.rule_steps[rule]) if self.delta else None
            )
            self.gave = False
            try:
                await self.handle(item_class._finish(item, self.warned))
            finally:
                self.page_done()
        elif kind == "ruled":
            _, rule, url, response, root, depth = event[:6]
            callback = self.rules[rule].callback
            request = self.ruled_request(rule, url, depth)
            page = event[6] if len(event) > 6 else self.crawl_page(response, request, root)
            if job is not None:
                await self.collect(job, callback, request, page)
                return
            await self.run_callback(callback, None, (page,), url)
        elif kind == "rule_failed":
            _, _, url, error = event
            log.warning("%s: %s", url, error)
        elif kind == "rule_dropped":
            _, _, url, why = event
            log.info("skipped %s: %s", url, _why(why))
        elif kind == "ignored":
            _, url, reason = event
            self.counts["pages_ignored"] += 1
            log.info("rules skipped %s: %s", url, reason)
        elif kind == "page_error":
            _, url, message = event
            log.warning("%s: %s", url, message)
        elif kind == "handled":
            self.waiting.pop(event[1])
        elif kind == "blocked":
            _, rid, rule, url, _vendor, _kind, response = event
            request = self.waiting.pop(rid) if rule is None else self.ruled_request(rule, url)
            page = self.crawl_page(response, request)
            if rule is None:
                await self.left_downloader(request, page)
            await self.run_callback(self.spider.on_block, None, (request, page), url)
        elif kind == "warning":
            log.warning("%s", event[1])
        elif kind == "leased":
            _, head, sent, policy = event
            try:
                request, data = _restored(head, sent, policy)
                self.from_data(request, "queue")
                if data.get("pushed"):
                    # Pushed from outside: a start request, which the
                    # middlewares haven't seen yet.
                    await self.queue(request, row=head[0])
                else:
                    self.submit(request, row=head[0])
            except Exception as e:  # noqa: BLE001 - anyone can write to the queue
                # Whatever wrote it, it can't be crawled: it goes (neither
                # submit nor a middleware had queued it), so it can't stop
                # this process, or the next to take it.
                self.queue_bad += 1
                self.engine.release(head[0])
                log.warning("%r from the shared queue can't be made a request: %s", head[1], e)
                if self.settings.fail_fast:
                    raise
        elif kind == "fatal":
            raise QueueError(event[1])
        elif kind == "input_failed":
            # A process_spider_input raised, in respond().
            _, request, page, error, default = event
            try:
                await self.run_callback(request.callback, default, (page,), page.url, error)
            finally:
                if page.browser is not None:
                    await page.browser.close()
        elif kind == "paused":
            _, host, seconds = event
            log.warning(
                "%s keeps blocking: pausing it for %g s (the circuit breaker)", host, seconds
            )
        else:
            _, rid, detail, *rest = event
            request = self.waiting.pop(rid)
            if kind == "fetched" and job is not None:
                page = rest[2] if len(rest) > 2 else self.crawl_page(detail, request)
                _warn_if_priced(page)
                await self.collect(job, request.callback or "parse", request, page)
            elif kind == "fetched":
                if len(rest) > 2:
                    page = rest[2]  # as the middlewares returned it
                else:
                    page = self.crawl_page(detail, request, rest[0])
                    page.browser = rest[1]
                _warn_if_priced(page)
                try:
                    await self.run_callback(request.callback, "parse", (page,), request.url)
                finally:
                    # Its place in browser_pages goes to the next request.
                    if page.browser is not None:
                        await page.browser.close()
            elif kind == "failed":
                await self.left_downloader(request, None)
                await self.failed(request, detail)
            else:
                log.info("skipped %s: %s", request.url, _why(detail))

    def validated(self, stage: Any, item: Any) -> Any:
        """``item`` after a validate stage: as it was, None when it failed
        and is dropped, or with drop=False a dict item with its errors in
        ``_errors``. Errors are counted per field, and each field's first is
        logged with the page it came from."""
        if not self.validating:
            # Counted once an item, however many validate stages it meets.
            self.validating = True
            self.counts["items_validated"] += 1
        checked = item
        if isinstance(item, dict) and ("_id" in item or "_errors" in item):
            # Netweir's own fields: a schema that allows no others mustn't
            # fail an item for the _id a checkpoint gives it.
            checked = {k: v for k, v in item.items() if k not in ("_id", "_errors")}
        problems = stage.problems(checked)
        if not problems:
            return item
        if not self.invalid:
            self.invalid = True
            self.counts["items_invalid"] += 1
        fields = self.spider.invalid_fields
        for field, message in problems:
            # offers.0.price and offers.1.price are one field.
            key = ".".join("*" if part.isdigit() else part for part in field.split("."))
            if key not in fields:
                log.warning("invalid item from %s: %s: %s", self.source, field or "item", message)
            fields[key] = fields.get(key, 0) + 1
        if stage.drop:
            return None
        if isinstance(item, dict):
            item.setdefault("_errors", []).extend(f"{f}: {m}" if f else m for f, m in problems)
        return item

    def cover(self, item: Any) -> None:
        """Counts the item's fields that are there and not empty."""
        if isinstance(item, dict):
            values = item.items()
        elif dataclasses.is_dataclass(item):
            values = ((f.name, getattr(item, f.name)) for f in dataclasses.fields(item))
        else:
            return
        coverage = self.spider.coverage
        for field, value in values:
            try:
                there = field not in ("_id", "_errors") and not _monitor.empty(value)
            except Exception:  # noqa: BLE001 - counting must never cost an item
                there = True
            if there:
                coverage[field] = coverage.get(field, 0) + 1

    async def open(self) -> None:
        """As Scrapy opens a crawl: every pipeline's open_spider, then the
        spider_opened signal, then the spider's opened(). An exception stops
        the crawl: nothing has been fetched or written yet."""
        for stage in self.pipelines:
            open_spider = self._pipeline_hook(stage, "open_spider")
            if open_spider is not None:
                await _result(open_spider(self.spider))
            self.opened.append(stage)
        self.signalled = True
        for spider_opened in self.handlers("spider_opened"):
            try:
                await _result(_by_name(spider_opened, spider=self.spider))
            except Exception as e:
                # Ends the crawl before anything is fetched, as Scrapy 2.18
                # does; the rest are still told.
                if not _named(e, "CloseSpider"):
                    raise
                self.closing = self.closing or _closed(e).reason
        self.spider_opened = True
        await _result(self.spider.opened())

    async def close(self, reason: str) -> None:
        """The other end of open(), for whatever it reached, in Scrapy's
        order: close_spider, spider_closed, closed(), engine_stopped. An
        exception here is logged: the crawl is over either way."""
        calls: list[tuple[Any, Callable[[], Any]]] = []
        for stage in self.opened:
            close_spider = self._pipeline_hook(stage, "close_spider")
            if close_spider is not None:
                calls.append((stage, functools.partial(close_spider, self.spider)))
        if self.signalled:
            for closed in self.handlers("spider_closed"):
                call = functools.partial(_by_name, closed, spider=self.spider, reason=reason)
                calls.append((getattr(closed, "__self__", closed), call))
        if self.spider_opened:
            calls.append((self.spider, functools.partial(self.spider.closed, reason)))
        if self.engine_started:
            for stopped in self.handlers("engine_stopped"):
                calls.append((getattr(stopped, "__self__", stopped), stopped))
        for listener, call in calls:
            try:
                await _result(call())
            except Exception:  # noqa: BLE001 - closing never hides the crawl's outcome
                log.exception("error closing %r", listener)
        # The crawl is over: the spider no longer holds on to it.
        self.spider.crawler = None

    def _pipeline_hook(self, listener: Any, name: str) -> Callable[..., Any] | None:
        """open_spider or close_spider, which only pipeline stages have."""
        if not any(listener is stage for stage in self.pipelines):
            return None
        method = getattr(listener, name, None)
        return method if callable(method) else None

    async def signal(self, name: str, quiet: tuple[str, ...] = (), **arguments: Any) -> list:
        """Tells every handler of signal ``name`` (see handlers()), giving
        each the arguments it names, as Scrapy does: a handler written as
        ``item_scraped(self, item, response, spider)`` and one written as
        ``item_scraped(self, item)`` both work. An exception there is
        logged, not raised; those of the classes named in ``quiet``, which
        say something to the caller (IgnoreRequest, DontCloseSpider), are
        returned instead."""
        raised = []
        for method in self.handlers(name):
            try:
                await _result(_by_name(method, **arguments))
            except Exception as e:  # noqa: BLE001 - a signal handler never stops the crawl
                if any(_named(e, q) for q in quiet):
                    raised.append(e)
                else:
                    log.exception("error in %s of %r", name, getattr(method, "__self__", method))
        return raised

    async def left_downloader(self, request: Request, page: Page | None) -> None:
        """response_downloaded, for a fetch that gave a page, then
        request_left_downloader, as Scrapy's downloader sends them."""
        if page is not None:
            await self.signal(
                "response_downloaded", response=page, request=request, spider=self.spider
            )
        await self.signal("request_left_downloader", request=request, spider=self.spider)

    async def guarded(self, method: Callable[..., Any], *args: Any, allowed: tuple) -> Any:
        """A middleware method's result: None, or an instance of ``allowed``.
        IgnoreRequest (Netweir's or Scrapy's) gives it in an _Ignored. Any
        other exception, or a result of another type, counts as a callback
        error (fail_fast stops the crawl) and gives _FAILED."""
        try:
            result = await _result(method(*args))
            if result is None or isinstance(result, allowed):
                return result
            names = " or ".join(t.__name__ for t in allowed)
            raise TypeError(f"returned a {type(result).__name__}, not {names} or None")
        except Exception as e:
            if _named(e, "IgnoreRequest"):
                return _Ignored(e)
            self.counts["callback_errors"] += 1
            if self.settings.fail_fast:
                raise
            log.exception("error in %s", getattr(method, "__qualname__", method))
            return _FAILED

    async def queue(self, request: Request, row: int | None = None) -> None:
        """Schedules a request the spider's code made: request_scheduled,
        then the middlewares' process_request, then the engine. None goes
        on with the request as it is; a Page answers it without a fetch; a
        new Request is scheduled in its place, from the start, as Scrapy
        reschedules it; IgnoreRequest goes to process_exception and the
        errback; an error drops it. ``row`` is a URL pushed to the shared
        queue from outside, leased under it."""
        for _ in range(_RESCHEDULES):
            if self.handlers("request_scheduled"):
                ignored = await self.signal(
                    "request_scheduled",
                    quiet=("IgnoreRequest",),
                    request=request,
                    spider=self.spider,
                )
                if ignored:
                    # As in Scrapy: dropped, with no request_dropped.
                    self.counts["requests_dropped"] += 1
                    self.release(row)
                    return
            if self.on_request and request.headers is None:
                # So `request.headers["User-Agent"] = ...` works, as in Scrapy.
                request.headers = {}
            for process in self.on_request:
                result = await self.guarded(process, request, self.spider, allowed=(Request, Page))
                if result is None or result is request:
                    continue
                if result is _FAILED:
                    self.counts["requests_dropped"] += 1
                    self.release(row)
                    await self.signal("request_dropped", request=request, spider=self.spider)
                    return
                if isinstance(result, _Ignored):
                    self.release(row)
                    # After the running callback: an errback may follow.
                    self.ready.append(
                        functools.partial(self.failed, request, result.error, admitted=False)
                    )
                    return
                if isinstance(result, Page):
                    self.release(row)
                    await self.answer(request, result, admitted=False)
                    return
                result.depth = request.depth
                request = result
                break
            else:
                outcome = self.submit(request, row=row)
                if outcome in _REFUSED and self.handlers("request_dropped"):
                    await self.signal("request_dropped", request=request, spider=self.spider)
                return
        self.counts["callback_errors"] += 1
        self.release(row)
        message = (
            f"{request.url}: process_request gave a new request in place of the last"
            f" {_RESCHEDULES} times running; dropped"
        )
        if self.settings.fail_fast:
            raise RuntimeError(message)
        log.error("%s", message)

    def release(self, row: int | None) -> None:
        """Gives a request leased from the shared queue up."""
        if row is not None:
            self.engine.release(row)

    async def answer(self, request: Request, page: Page, admitted: bool = True) -> None:
        """A middleware answered ``request`` with ``page``: it goes through
        process_response to the callback, as a fetched page would, once the
        callback running now has finished. A request the engine hasn't seen
        yet is screened as one to fetch would be (a duplicate, offsite, too
        deep), so pages from a store can't lead the spider round in
        circles."""
        if not admitted:
            outcome = self.submit(request, answered=True)
            if outcome == "capped":
                # Served pages count toward max_pages as fetched ones do:
                # this one is dropped, as a request past the limit is never
                # sent, and the crawl ends once what's in hand is done.
                self.capped_out = True
                return
            if outcome != "answered":
                if outcome in _REFUSED:
                    await self.signal("request_dropped", request=request, spider=self.spider)
                return
        # The crawl's request, so depth and meta carry on; a copy, so a page
        # a middleware hands out twice keeps its own.
        mine = self.crawl_page(page._response, request, page._root)
        mine.browser = page.browser
        self.ready.append(functools.partial(self.answered, request, mine))

    async def answered(self, request: Request, page: Page) -> None:
        if self.rules and request.callback is None:
            # The rules read it, as they read a fetched page: the links
            # they find are queued in Rust.
            error, ignored, items = self.engine.apply_rules(
                page._response, request.url, request.depth
            )
            if error is not None:
                log.warning("%s: %s", request.url, error)
            if ignored is not None:
                self.counts["pages_ignored"] += 1
                log.info("rules skipped %s: %s", request.url, ignored)
            for event in items:
                await self.dispatch(event)
            if not self.has_parse:
                return
        rid = next(self.ids)
        self.waiting[rid] = request
        event = ("fetched", rid, page._response, page._root, page.browser)
        responded = await self.respond(event, page)
        if responded is not None:
            await self.dispatch(responded)

    async def failed(self, request: Request, error: BaseException, admitted: bool = True) -> None:
        """A request failed, or a process_request raised IgnoreRequest:
        process_exception, then the errback, as in Scrapy. A Request from a
        middleware is scheduled instead, a Page answers the request, and
        IgnoreRequest skips the middlewares left and goes to the errback."""
        for process in self.on_exception:
            instead = await self.guarded(
                process, request, error, self.spider, allowed=(Request, Page)
            )
            if isinstance(instead, _Ignored):
                error = instead.error
                break
            if isinstance(instead, Request):
                instead.depth = request.depth
                await self.queue(instead)
                return
            if isinstance(instead, Page):
                await self.answer(request, instead, admitted)
                return
            # None, or an error (already logged): on to the next.
        if _named(error, "IgnoreRequest"):
            self.counts["requests_dropped"] += 1
            await self.signal("request_dropped", request=request, spider=self.spider)
        await self.errback(request, error)

    async def errback(self, request: Request, error: BaseException) -> None:
        """The request's errback, or a warning without one; IgnoreRequest
        without one is dropped quietly, as in Scrapy."""
        if request.errback is not None:
            await self.run_callback(request.errback, None, (request, error), request.url)
        elif not _named(error, "IgnoreRequest"):
            log.warning("%s: %s", request.url, error)

    async def respond(self, event: tuple, page: Page | None = None) -> tuple | None:
        """A page on its way to its callback: the download signals, the
        middlewares' process_response, response_received, then the spider
        middlewares' process_spider_input. The event as it was, the event
        with the page they gave instead, an input_failed event, or None
        when they dropped it or asked for a request in its place. ``page``
        is one a middleware answered the request with, never downloaded."""
        if event[0] not in ("fetched", "ruled"):
            return event
        if (
            page is None
            and not self.on_response
            and not self.spider_mw.inputs
            and not any(self.handlers(name) for name in _RESPONSE_SIGNALS)
        ):
            return event
        if event[0] == "fetched":
            _, rid, response, root, live = event[:5]
            request = self.waiting[rid]
            default: str | None = "parse"
        else:
            _, rule, url, response, root, depth = event
            request = self.ruled_request(rule, url, depth)
            live, default = None, None
        if page is None:
            page = self.crawl_page(response, request, root)
            page.browser = live
            await self.left_downloader(request, page)
        for process in self.on_response:
            result = await self.guarded(
                process, request, page, self.spider, allowed=(Page, Request)
            )
            if result is None:
                continue
            if isinstance(result, Page):
                if result is not page:
                    # Another page answers this request: its depth and meta
                    # are the request's, so max_depth still counts from here.
                    # A copy, so a page a middleware hands out twice keeps
                    # its own request.
                    replacement = self.crawl_page(result._response, request, result._root)
                    replacement.browser = result.browser
                    result = replacement
                page = result
                continue
            if event[0] == "fetched":
                self.waiting.pop(rid)
            if live is not None:
                await live.close()
            if isinstance(result, Request):
                result.depth = request.depth
                await self.queue(result)
                return None
            self.counts["responses_dropped"] += 1
            if isinstance(result, _Ignored):
                # Straight to the errback, as Scrapy does from here.
                self.ready.append(functools.partial(self.errback, request, result.error))
            return None
        if live is not None and page.browser is not live:
            await live.close()  # replaced: its place goes to the next request
        await self.signal("response_received", response=page, request=request, spider=self.spider)
        if self.spider_mw.inputs:
            error = await self.spider_mw.input(page)
            if error is not None:
                if event[0] == "fetched":
                    self.waiting.pop(rid)
                return ("input_failed", request, page, error, default)
        # The page itself rides along, so the callback gets the very object
        # the middlewares returned; a worker gets its response.
        if event[0] == "fetched":
            return ("fetched", rid, page._response, page._root, page.browser, page)
        return ("ruled", rule, url, page._response, page._root, depth, page)

    async def run_callback(
        self,
        fn: Callable[..., Any] | str | None,
        default: str | None,
        args: tuple,
        url: str,
        failed: BaseException | None = None,
    ) -> None:
        """Runs a callback, an errback or on_block, and deals with what it
        gives. A page's callback goes through the spider middlewares;
        ``failed`` is the exception a process_spider_input raised for it,
        which goes to the request's errback instead."""
        name = fn if isinstance(fn, str) else getattr(fn, "__name__", default)
        # What it yields is one link further from the start.
        source = args[0]
        self.depth = (source.depth if isinstance(source, (Page, Request)) else 0) + 1
        self.source = source.url if isinstance(source, (Page, Request)) else url
        self.page = source if isinstance(source, Page) else None
        self.page_url = self.page.url if self.page is not None else None
        self.page_jar = _jar_of(self.page.request) if self.page is not None else None
        # An errback's or on_block's items came with no page to skip.
        self.from_page = self.delta_of(self.page.request) if self.page is not None else None
        self.gave = False
        self.position = 0
        # A page's callback gets its request's cb_kwargs; an errback and
        # on_block, which have no page, get none, as in Scrapy.
        kwargs = self.page.cb_kwargs if self.page is not None else {}
        stream = None
        try:
            fn = self.resolve(fn, default)
            if self.page is not None and (self.layered or failed is not None):
                page = self.page
                stream = await self.spider_mw.scrape(
                    page,
                    functools.partial(fn, *args, **kwargs),
                    functools.partial(self.input_errback, page.request),
                    failed,
                )
                async for out in stream:
                    await self.handle(out)
                self.page_done()
                return
            result = fn(*args, **kwargs)
            if inspect.isasyncgen(result):
                async for out in result:
                    await self.handle(out)
            else:
                if inspect.isawaitable(result):
                    result = await result
                if result is not None:
                    many = isinstance(result, Iterable) and not isinstance(result, _NOT_MANY)
                    for out in result if many else [result]:
                        await self.handle(out)
            self.page_done()
        except Exception as e:
            await self.callback_failed(e)
            log.exception("error in %s for %s", name, url)
        finally:
            if stream is not None:
                await stream.aclose()

    def input_errback(self, request: Request, error: BaseException) -> Any:
        """What a request's errback gives for a process_spider_input's
        exception; without an errback, the exception goes on to
        process_spider_exception, as in Scrapy."""
        if request.errback is None or isinstance(error, _hooks.Invalid):
            raise error
        return self.resolve(request.errback, None)(request, error)

    async def callback_failed(self, error: BaseException) -> None:
        """A callback, an errback, or a pipeline under one, raised: the
        crawl ends for CloseSpider, and otherwise it's counted, told to
        spider_error (unless a pipeline raised it, which item_error was
        told of) and raised again with fail_fast. The caller logs it."""
        if _named(error, "CloseSpider"):
            raise _closed(error) from None
        self.counts["callback_errors"] += 1
        if error is not self.item_failure:
            await self.signal(
                "spider_error",
                failure=_hooks.Failure(error),
                exception=error,
                response=self.page,
                spider=self.spider,
            )
        self.item_failure = None
        if self.settings.fail_fast:
            raise error

    async def collect(self, job: Any, fn: Any, request: Request, page: Page) -> None:
        """A worker's results for one page, dealt with as run_callback deals
        with a callback's: same depth, same item ids, same counts, through
        the spider middlewares here."""
        page_url = page.url
        name = fn if isinstance(fn, str) else getattr(fn, "__name__", "parse")
        self.depth = request.depth + 1
        self.source = page_url
        # The page stayed in the worker.
        self.page = None
        self.page_url = page_url
        self.page_jar = _jar_of(request)
        self.from_page = self.delta_of(request)
        self.gave = False
        self.position = 0
        try:
            outputs, reports, repairs, error, (pid, peak), ops = await job
        except BrokenProcessPool:
            # One page's callback killed its worker (os._exit, out of
            # memory): the pool is broken, and the crawl with it.
            raise _worker_died(name, request.url) from None
        if peak is not None:
            self.worker_peaks[pid] = peak
        self.live._replay(ops)
        for report in reports:
            _track.report(*report)
        for repair in repairs:
            self.ask_repair(*repair)

        async def replay() -> AsyncIterator[Any]:
            for out in outputs:
                yield out
            if error is not None:
                raise error[0]

        stream = None
        try:
            if self.layered:
                stream = await self.spider_mw.scrape(page, replay, self.input_errback)
            else:
                stream = replay()
            async for out in stream:
                await self.handle(out)
            self.page_done()
        except Exception as e:
            self.page = page  # for spider_error
            try:
                await self.callback_failed(e)
            finally:
                self.page = None
            if error is not None and e is error[0] and error[1]:
                log.error("error in %s for %s (in a worker)\n%s", name, request.url, error[1])
            else:
                log.exception("error in %s for %s", name, request.url)
        finally:
            if stream is not None:
                await stream.aclose()

    async def handle(self, out: Any) -> None:
        if out is None:
            return
        if isinstance(out, Request):
            out.depth = self.depth
            await self.queue(out)
            return
        if isinstance(out, _NOT_ITEMS):
            raise TypeError(
                f"a callback produced a {type(out).__name__}: yield dicts, dataclasses or Requests"
            )
        item = out
        item_id = None
        if self.saving:
            # Stable across runs: the page it came from and its place among
            # that page's items.
            key = f"{fingerprint(self.source) or self.source}:{self.position}"
            self.position += 1
            item_id = hashlib.sha256(key.encode()).hexdigest()[:24]
            if item_id in self.delivered:
                self.counts["items_already_written"] += 1
                # Delivered by the run before, whose record of its page may
                # be what was lost.
                self.page_gave_item()
                return
            if isinstance(item, dict):
                item.setdefault("_id", item_id)
        self.validating = self.invalid = False
        # An item an exporter has written is counted whatever cuts it off
        # after that (a second Ctrl-C, a cancel), so the stats agree with
        # the file. Known limit: an interrupt between the write and
        # written += 1 is a bytecode wide.
        counted, wrote = self.counts["items"], _written(self.pipelines)
        try:
            for stage in self.pipelines:
                if isinstance(stage, _monitor.validate):
                    given = item
                    item = self.validated(stage, item)
                    if item is None:
                        # Counted as items_invalid, not items_dropped; listeners
                        # hear of it as of any other drop.
                        await self.signal(
                            "item_dropped",
                            item=given,
                            response=self.page,
                            exception=None,
                            spider=self.spider,
                        )
                        return
                    continue
                given = item
                process = getattr(stage, "process_item", None)
                dropped = None
                try:
                    item = await _result(process(item, self.spider) if process else stage(item))
                except Exception as e:
                    # Netweir's DropItem, or Scrapy's, from a pipeline written
                    # for Scrapy (subclasses too): a drop, not an error.
                    if not _named(e, "DropItem"):
                        self.item_failure = e
                        await self.signal(
                            "item_error",
                            item=given,
                            response=self.page,
                            spider=self.spider,
                            failure=_hooks.Failure(e),
                            exception=e,
                        )
                        raise
                    log.debug("item dropped by %r: %s", stage, e)
                    item, dropped = None, e
                if item is None:
                    self.counts["items_dropped"] += 1
                    await self.signal(
                        "item_dropped",
                        item=given,
                        response=self.page,
                        exception=dropped,
                        spider=self.spider,
                    )
                    return
            self.counts["items"] += 1
        except BaseException as e:
            if (
                not isinstance(e, Exception)
                and item is not None
                and self.counts["items"] == counted
                and _written(self.pipelines) > wrote
            ):
                self.counts["items"] += 1
            raise
        self.page_gave_item()
        self.cover(item)
        await self.signal("item_scraped", item=item, response=self.page, spider=self.spider)
        if item_id is not None:
            self.delivered.add(item_id)
            self.unrecorded.append(item_id)
        if self.settings.max_items is not None and self.counts["items"] >= self.settings.max_items:
            raise _Stop("max_items")

    def delta_of(self, request: Request | None) -> tuple | None:
        """The request as the engine keys it for deltafetch: (method, url,
        body, deltafetch_key, cookie jar). None without deltafetch, or for
        a request that leaves it out."""
        if not self.delta or request is None:
            return None
        meta = request.meta or {}
        delta = _delta_meta(meta)
        if delta is None or not delta[0]:
            return None
        body = request.body.encode() if isinstance(request.body, str) else request.body
        steps = None
        if request.steps and _steps_in_fingerprint(meta):
            steps = steps_json(request.steps, self.engine_step_name)
        return (request.method, request.url, body, delta[1], _jar_name(meta), steps)

    def page_gave_item(self) -> None:
        """The running callback's page has given a delivered item."""
        self.gave = True

    def page_done(self) -> None:
        """The running callback has finished: if its page gave an item,
        deltafetch records the page once the items are on disk."""
        if self.gave and self.from_page is not None:
            key = Crawler.delta_key(*self.from_page)
            if key is not None:
                self.produced.add(key)
        self.from_page = None
        self.gave = False


def _delta_meta(meta: dict[str, Any]) -> tuple[bool, bytes | None] | None:
    """What a request's meta says to deltafetch, as scrapy-deltafetch reads
    it: whether the record is asked at all (``deltafetch_enabled``), and
    the request's own key, if it has one (an empty one is none). None for a
    key that's neither a str nor bytes."""
    if not meta.get("deltafetch_enabled", True):
        return (False, None)
    key = meta.get("deltafetch_key")
    if not key:
        return (True, None)
    if isinstance(key, str):
        return (True, key.encode())
    if isinstance(key, bytes):
        return (True, key)
    return None


class _Ignored:
    """What guarded() gives for a middleware that raised IgnoreRequest."""

    def __init__(self, error: BaseException):
        self.error = error


#: What guarded() gives for a middleware that failed.
_FAILED = object()

#: What the engine says of a request it refused, which request_dropped
#: hears of, as Scrapy's does of what its scheduler refuses.
_REFUSED = frozenset({"duplicate", "offsite", "too_deep", "trap", "domain_full", "delta"})

#: Signals of a response on its way to a callback.
_RESPONSE_SIGNALS = ("response_downloaded", "request_left_downloader", "response_received")

#: How many times running process_request may give a new request in place
#: of the last before the request is given up: a middleware that always
#: does would otherwise never end.
_RESCHEDULES = 100

#: Seconds before spider_idle is sent again after DontCloseSpider, as
#: Scrapy's engine looks again every five.
_IDLE_WAIT = 5.0


def _policy(request: Request, meta: Mapping[str, Any], max_redirects: int) -> dict[str, Any]:
    """The engine's arguments for how ``request`` is handled: its own
    timeout, redirect limit and login, or else the Scrapy meta keys that
    say the same. ``max_redirects`` is the crawl's limit, which
    ``redirect_ttl`` can lower but not raise, as in Scrapy. A meta value
    that can't work is a ValueError."""
    # The fields again, as a middleware or callback may have changed them
    # since the request was made.
    timeout = request.timeout
    if timeout is not None:
        timeout = seconds(timeout)
    elif meta.get("download_timeout") is not None:
        timeout = seconds(meta["download_timeout"], 'meta["download_timeout"]')
    limit = redirect_limit(request.max_redirects)
    if limit is None and meta.get("redirect_ttl") is not None:
        limit = min(redirect_limit(meta["redirect_ttl"], "redirect_ttl"), max_redirects)
    auth = None
    if request.auth is not None:
        auth = (*checked_auth(request.auth), None)
    elif _meta_login(meta):
        domain = meta.get("http_auth_domain") or urlsplit(request.url).hostname or ""
        auth = (
            str(meta.get("http_user") or ""),
            str(meta.get("http_pass") or ""),
            _auth_domain(domain),
        )
    return {
        "timeout": timeout,
        "max_redirects": limit,
        "dont_redirect": not request.allow_redirects or bool(meta.get("dont_redirect")),
        "auth": auth,
    }


def _meta_login(meta: Mapping[str, Any]) -> bool:
    """Whether ``meta`` holds a login, as Scrapy's http_user and http_pass."""
    return meta.get("http_user") is not None or meta.get("http_pass") is not None


def _jar_name(meta: dict[str, Any]) -> str | None:
    """The jar ``meta["cookiejar"]`` names, as the engine keys it: its JSON,
    so 1 and "1" are two jars as in Scrapy. None for the crawl's own."""
    key = meta.get("cookiejar")
    if key is None:
        return None
    try:
        return json.dumps(key, sort_keys=True)
    except TypeError:
        raise TypeError(
            f'meta["cookiejar"] must be a JSON value (a str, a number...), not {type(key).__name__}'
        ) from None


def _inside(root: str, path: str) -> bool:
    """Whether ``path`` is ``root`` or below it, both resolved. Paths on
    two Windows drives have no common path, and aren't."""
    try:
        # commonpath writes the result its own way (with backslashes, on
        # Windows): compare it with the root written the same way.
        return os.path.commonpath([root, path]) == os.path.commonpath([root])
    except ValueError:
        return False


def _capped(steps: str, cap: float | None, where: str) -> str:
    """``steps`` (JSON), once checked against ``browser_steps_max``."""
    from netweir._native import _check_steps

    try:
        _check_steps(steps, cap)
    except ValueError as e:
        raise ValueError(f"{where}: {e}") from None
    return steps


def _steps_in_fingerprint(meta: dict[str, Any]) -> bool:
    """Whether a request's steps are part of its fingerprint:
    ``meta["steps_in_fingerprint"]``, True unless it says otherwise."""
    flag = meta.get("steps_in_fingerprint", True)
    if not isinstance(flag, bool):
        raise TypeError(f'meta["steps_in_fingerprint"] must be True or False, not {flag!r}')
    return flag


#: The methods of netweir's own spider classes that a request may name as
#: its callback: netweir names parse and _sitemap itself, and on_block is
#: a callback in all but name. Any other (closed, crawl, run, start...)
#: is the spider's machinery, which a request from data mustn't call.
_NETWEIR_CALLBACKS = frozenset({"parse", "on_block", "_sitemap"})


def _not_a_callback(spider: Any, name: str) -> str | None:
    """Why ``name`` can't be a callback of ``spider`` for a request read
    back from data, or None if it can: a method of the spider's own
    classes, or one of netweir's spiders' in _NETWEIR_CALLBACKS. A name
    netweir's spider classes give their machinery (closed, crawl, start...)
    is refused even where the spider overrides it: it's still a hook
    netweir calls at its own time, not a callback."""
    if name.startswith("__"):
        return "starts with two underscores (a dunder, or private to its class), never a callback"
    mro = [k for k in type(spider).__mro__ if k is not object]
    ours = [k for k in mro if k.__module__.split(".")[0] == "netweir"]
    machinery = next((k for k in ours if name in vars(k)), None)
    if machinery is not None and name not in _NETWEIR_CALLBACKS:
        return (
            f"is netweir.{machinery.__name__}'s own {name}, which netweir calls itself, not a"
            " callback"
        )
    for klass in mro:
        if name not in vars(klass):
            continue
        attr = vars(klass)[name]
        if inspect.isfunction(attr) or isinstance(attr, (staticmethod, classmethod)):
            return None
        return f"is {type(attr).__name__} on {klass.__name__}, not a method"
    return f"isn't defined on {type(spider).__name__}"


def _rule_method(spider: Any, fn: Callable[..., Any]) -> str:
    """The name a rule's call step keeps for ``fn``, which must be a
    method of ``spider``: the rules run in Rust, which holds names."""
    name = getattr(fn, "__name__", None)
    if name is not None and getattr(spider, name, None) == fn:
        return name
    raise TypeError(
        f"Follow's call steps run spider methods, which the rules keep by name, so {fn!r}"
        " must be a method of the spider"
    )


def _jar_of(request: Request | None) -> str | None:
    """The jar ``request``'s ``meta["cookiejar"]`` names, or None for the
    crawl's own (or a name that isn't JSON, which never got as far as a
    page)."""
    if request is None:
        return None
    try:
        return _jar_name(request.meta or {})
    except TypeError:
        return None


def _restored(
    head: tuple, sent: tuple, policy: tuple = (None, None, False, None)
) -> tuple[Request, dict[str, Any]]:
    """A Request again from what the checkpoint or the shared queue kept of
    it, with its payload. A login kept for a domain came from Scrapy's meta
    keys, which come back with the meta and give it again."""
    _row, url, priority, headers, dont_filter, depth, payload = head
    method, kind, body, content_type, referer, retry_post, proxy = sent
    timeout, max_redirects, dont_redirect, auth = policy
    if kind not in ("navigate", "form", "fetch", "image"):
        raise ValueError(f"unknown kind of request {kind!r}")
    data = json.loads(payload)
    request = Request(
        url,
        callback=data.get("callback"),
        priority=priority,
        # None when there were none, so a middleware can set some as on a
        # new request.
        headers=headers or None,
        meta=data.get("meta", {}),
        dont_filter=dont_filter,
        errback=data.get("errback"),
        depth=depth,
        browser=data.get("browser", False),
        method=method,
        body=body,
        referer=referer,
        retry_post=retry_post,
        proxy=proxy,
        kind=kind,
        content_type=content_type,
        timeout=timeout,
        max_redirects=max_redirects,
        allow_redirects=not dont_redirect,
        auth=auth[:2] if auth is not None and auth[2] is None else None,
        cookies=data.get("cookies"),
        steps=data.get("steps"),
        cb_kwargs=data.get("cb_kwargs"),
    )
    return request, data


def _named(error: BaseException, name: str) -> bool:
    """``error`` is of a class called ``name``, or a subclass of one: Netweir's
    DropItem or IgnoreRequest, or Scrapy's, which a ported pipeline or
    middleware raises without importing Netweir's."""
    return any(cls.__name__ == name for cls in type(error).__mro__)


def _by_name(method: Callable[..., Any], **arguments: Any) -> Any:
    """Calls ``method`` with the arguments its parameters name, as Scrapy
    calls signal handlers; all of them if it takes ``**kwargs``."""
    parameters = _parameters(getattr(method, "__func__", method), method)
    if parameters is None:
        return method(**arguments)
    given = [(name, by_position) for name, by_position in parameters if name in arguments]
    positional = [arguments[name] for name, by_position in given if by_position]
    named = {name: arguments[name] for name, by_position in given if not by_position}
    return method(*positional, **named)


#: A handler's function, to the parameters _by_name gives it, read once.
#: Weak, so a handler that goes away doesn't stay here.
_PARAMETERS: weakref.WeakKeyDictionary = weakref.WeakKeyDictionary()


def _parameters(key: Any, method: Callable[..., Any]) -> list[tuple[str, bool]] | None:
    """(name, positional only) for each parameter a signal can fill, in
    order, or None for a handler that takes ``**kwargs`` or can't be read.
    A bound method is a new object each time, so its function is the key."""
    try:
        return _PARAMETERS[key]
    except (KeyError, TypeError):
        pass
    try:
        signature = inspect.signature(method)
    except (TypeError, ValueError):
        found = None
    else:
        values = signature.parameters.values()
        if any(p.kind is p.VAR_KEYWORD for p in values):
            found = None
        else:
            found = [
                (p.name, p.kind is p.POSITIONAL_ONLY) for p in values if p.name in _SIGNAL_ARGUMENTS
            ]
    # Not every callable can be held weakly: those are read each time.
    with contextlib.suppress(TypeError):
        _PARAMETERS[key] = found
    return found


#: Every argument a signal gives.
_SIGNAL_ARGUMENTS = frozenset(
    {"spider", "reason", "item", "response", "exception", "request", "failure"}
)


def _methods(objects: list[Any], name: str) -> list[Callable[..., Any]]:
    return [getattr(o, name) for o in objects if callable(getattr(o, name, None))]


def _written(pipelines: list[Any]) -> int:
    """Items the exporters among ``pipelines`` have written."""
    from netweir import export

    return sum(s.written for s in pipelines if isinstance(s, export._Exporter))


async def _result(value: Any) -> Any:
    """``value``, awaited if it's awaitable: for hooks that may be async."""
    return await value if inspect.isawaitable(value) else value


#: Iterable, but one result rather than many.
_NOT_MANY = _hooks._NOT_MANY
#: Never an item.
_NOT_ITEMS = (str, bytes, bytearray, int, float, bool, set, frozenset)


def _why(reason: str) -> str:
    return {"robots": "robots.txt", "tdm": "TDM reserved"}.get(
        reason, "redirected to a page already seen"
    )
