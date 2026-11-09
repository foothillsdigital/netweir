"""One crawl shared by several processes through a queue in Redis.

Each test runs against a stand-in for Redis in this process, and again
against a real redis-server when there's one on the PATH (or must be,
with NETWEIR_REQUIRE_REDIS set).
"""

import asyncio
import collections
import itertools
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import textwrap
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest
from fake_redis import FakeRedis

import netweir
from netweir import _cli

PAGES = 6
PER_PAGE = 5
EXPECTED = sorted(f"{n}-{i}" for n in range(1, PAGES + 1) for i in range(PER_PAGE))


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    hits: collections.Counter = collections.Counter()
    headers_seen: dict[str, str] = {}
    #: The first page waits for this, so a test can have every process
    #: ready before the crawl gets going.
    go = threading.Event()
    #: When set, the first two item pages each wait until the other is
    #: being fetched too: with one request at a time per process, that
    #: takes two processes.
    pair: threading.Barrier | None = None
    paired = 0
    lock = threading.Lock()

    def log_message(self, *args):
        pass

    def do_GET(self):
        p = self.path
        if p == "/page/1":
            assert Handler.go.wait(60)
        if p.startswith("/item/") and Handler.pair is not None:
            with Handler.lock:
                Handler.paired += 1
                waits = Handler.paired <= 2
            if waits:
                Handler.pair.wait()
        Handler.hits[p] += 1
        Handler.headers_seen[p] = self.headers.get("X-From", "")
        if p.startswith("/page/"):
            n = int(p.rsplit("/", 1)[1])
            links = "".join(f'<a class="item" href="/item/{n}-{i}">i</a>' for i in range(PER_PAGE))
            nxt = f'<a class="next" href="/page/{n + 1}">next</a>' if n < PAGES else ""
            body = f"<html><body>{links}{nxt}</body></html>"
        else:
            body = f"<html><body><h1>{p.rsplit('/', 1)[1]}</h1></body></html>"
        data = body.encode()
        # Slow enough that two processes both get a share.
        time.sleep(0.05)
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


@pytest.fixture
def base():
    Handler.hits = collections.Counter()
    Handler.headers_seen = {}
    Handler.go.set()
    Handler.pair, Handler.paired = None, 0
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_port}"
    server.shutdown()
    server.server_close()


@pytest.fixture(params=["fake", "redis"])
def redis(request, tmp_path):
    """A Redis URL: the stand-in, or a redis-server of the test's own."""
    if request.param == "fake":
        fake = FakeRedis()
        yield fake.url
        fake.close()
        return
    binary = shutil.which("redis-server")
    if binary is None:
        if os.environ.get("NETWEIR_REQUIRE_REDIS"):
            pytest.fail("NETWEIR_REQUIRE_REDIS is set, and there's no redis-server on the PATH")
        pytest.skip("no redis-server on the PATH")
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    server = subprocess.Popen(
        [binary, "--port", str(port), "--bind", "127.0.0.1", "--save", "", "--appendonly", "no"],
        cwd=tmp_path,
        stdout=subprocess.DEVNULL,
    )
    try:
        for _ in range(200):
            try:
                socket.create_connection(("127.0.0.1", port), timeout=0.1).close()
                break
            except OSError:
                time.sleep(0.02)
        yield f"redis://127.0.0.1:{port}/0"
    finally:
        server.kill()
        server.wait()


@pytest.fixture
def fake():
    """The stand-in alone, for the faults only it can make."""
    fake = FakeRedis()
    yield fake
    fake.close()


def raw(url, *args):
    """Sends one command to the Redis at ``url`` as it is, and reads the
    first line of the answer: for putting things in a queue that netweir
    wouldn't."""
    host, port = url.removeprefix("redis://").split("/")[0].split(":")
    with socket.create_connection((host, int(port))) as s:
        parts = [a if isinstance(a, bytes) else str(a).encode() for a in args]
        s.sendall(b"*%d\r\n" % len(parts) + b"".join(b"$%d\r\n%s\r\n" % (len(a), a) for a in parts))
        return s.makefile("rb").readline()


def finishes(spider, timeout=60):
    """Runs the spider, failing the test if it doesn't end on its own."""
    return asyncio.run(asyncio.wait_for(spider.crawl(), timeout))


def settings(redis, **kw):
    kw = {"queue_idle": 1.0, "queue_lease": 2.0, **kw}
    return netweir.Settings(
        throttle=False,
        start_delay=0,
        obey_robots=False,
        obey_tdmrep=False,
        queue=redis,
        queue_key="books",
        **kw,
    )


SPIDER = textwrap.dedent(
    """
    import os
    import time

    import netweir

    class Items(netweir.Spider):
        start_urls = ["{base}/page/1"]

        def parse(self, page):
            for href in page.css("a.item::attr(href)").getall():
                yield page.follow(href, callback="item", meta={{"from": page.url}})
            if nxt := page.css("a.next::attr(href)").get():
                yield page.follow(nxt)

        def item(self, page):
            name = page.css("h1::text").get()
            if name == os.environ.get("HANG_ON"):
                time.sleep(600)  # stuck, until it's killed
            yield {{"name": name, "from": page.meta["from"], "pid": os.getpid()}}
    """
)


def crawl_command(script, out, redis, *more):
    return [
        sys.executable, "-m", "netweir", "crawl", str(script), "-o", str(out),
        "--queue", redis, "--queue-key", "books",
        "-s", "throttle=false", "-s", "start_delay=0", "-s", "obey_robots=false",
        "-s", "obey_tdmrep=false", "-s", "concurrency=2", "-s", "queue_lease=2",
        "-s", "queue_idle=3", *more,
    ]  # fmt: skip


def rows(path):
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]


def test_two_processes_share_the_crawl_with_no_page_twice_and_none_lost(base, redis, tmp_path):
    script = tmp_path / "spider.py"
    script.write_text(SPIDER.format(base=base), encoding="utf-8")
    outs = [tmp_path / f"items-{n}.jsonl" for n in range(2)]
    # Both join before the first page answers, however slowly one starts,
    # and both have to fetch an item before any item is answered.
    Handler.go.clear()
    Handler.pair = threading.Barrier(2, timeout=60)
    procs = [
        subprocess.Popen(
            crawl_command(script, out, redis, "-s", "per_domain=1"),
            stderr=subprocess.PIPE,
            text=True,
        )
        for out in outs
    ]
    queue = netweir.Queue(redis, "books")
    deadline = time.monotonic() + 60
    while queue.status()["processes"] < 2:
        assert time.monotonic() < deadline, "the processes never joined"
        time.sleep(0.05)
    Handler.go.set()
    for proc in procs:
        _, err = proc.communicate(timeout=120)
        assert proc.returncode == 0, err
    items = rows(outs[0]) + rows(outs[1])
    assert sorted(i["name"] for i in items) == EXPECTED
    assert all(i["from"].endswith(f"/page/{i['name'].split('-')[0]}") for i in items)
    assert all(count == 1 for count in Handler.hits.values()), Handler.hits
    assert len({i["pid"] for i in items}) == 2, "both processes did some of it"
    status = netweir.Queue(redis, "books").status()
    assert (status["waiting"], status["held"], status["processes"]) == (0, 0, 0)


def test_a_killed_process_requests_go_to_another_and_nothing_is_lost(base, redis, tmp_path):
    script = tmp_path / "spider.py"
    script.write_text(SPIDER.format(base=base), encoding="utf-8")
    first, second = tmp_path / "items-1.jsonl", tmp_path / "items-2.jsonl"
    stuck = subprocess.Popen(
        crawl_command(script, first, redis),
        env={**os.environ, "HANG_ON": "3-2"},
        stderr=subprocess.DEVNULL,
    )
    try:
        deadline = time.monotonic() + 60
        while Handler.hits["/item/3-2"] == 0:
            assert time.monotonic() < deadline, "never reached the page it hangs on"
            assert stuck.poll() is None, "it ended on its own"
            time.sleep(0.05)
        time.sleep(0.3)
    finally:
        stuck.kill()  # SIGKILL: no giving back, no more heartbeats
        stuck.wait()
    proc = subprocess.run(
        crawl_command(script, second, redis), capture_output=True, text=True, timeout=120
    )
    assert proc.returncode == 0, proc.stderr
    items = rows(first) + rows(second)
    assert sorted(set(i["name"] for i in items)) == EXPECTED, "nothing lost"
    assert "3-2" in {i["name"] for i in rows(second)}
    assert Handler.hits["/item/3-2"] == 2
    assert max(Handler.hits.values()) <= 2
    assert "leases_reclaimed" in proc.stderr
    reclaimed = proc.stderr.split("leases_reclaimed ")[1].split(",")[0].split()[0]
    assert int(reclaimed) >= 1


def test_a_process_ends_once_the_queue_stays_empty_for_queue_idle(redis):
    class Nothing(netweir.Spider):
        pass

    spider = Nothing()
    spider.settings = settings(redis, queue_idle=1.0)
    started = time.monotonic()
    stats = spider.run()
    took = time.monotonic() - started
    assert 1.0 <= took < 10, took
    assert spider.finish_reason == "finished"
    assert stats["leased"] == 0


@pytest.mark.skipif(sys.platform == "win32", reason="stopped with SIGINT")
def test_with_queue_idle_none_a_process_waits_and_takes_what_is_pushed(base, redis, tmp_path):
    script = tmp_path / "spider.py"
    script.write_text(
        textwrap.dedent(
            """
            import netweir

            class Stamp:
                def process_request(self, request, spider):
                    request.headers["X-From"] = "middleware"

            class Fed(netweir.Spider):
                middlewares = [Stamp()]

                def parse(self, page):
                    yield {"url": page.url}
            """
        ),
        encoding="utf-8",
    )
    out = tmp_path / "items.jsonl"
    command = crawl_command(script, out, redis, "-s", "queue_idle=none")
    proc = subprocess.Popen(command, stderr=subprocess.PIPE, text=True)
    try:
        time.sleep(2.5)
        assert proc.poll() is None, "it waited"
        queue = netweir.Queue(redis, "books")
        assert queue.push([f"{base}/item/9-9", f"{base}/item/9-9"]) == 1
        deadline = time.monotonic() + 30
        while not rows(out):
            assert time.monotonic() < deadline, "the pushed URL wasn't crawled"
            assert proc.poll() is None, proc.stderr.read()
            time.sleep(0.05)
        assert rows(out) == [{"url": f"{base}/item/9-9"}]
        # A pushed URL is a start request: the middlewares see it.
        assert Handler.headers_seen["/item/9-9"] == "middleware"
        time.sleep(1.5)
        assert proc.poll() is None, "and it's still waiting"
    finally:
        proc.send_signal(signal.SIGINT)
        try:
            proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
    assert netweir.Queue(redis, "books").status()["processes"] == 0, "it left the crawl"


def test_rules_made_in_rust_go_through_the_queue_too(base, redis):
    class Name(netweir.Item):
        name = netweir.css("h1::text")

    class Ruled(netweir.Spider):
        start_urls = [f"{base}/page/1"]
        rules = [netweir.Follow("a.item", extract=Name), netweir.Follow("a.next")]

    spider = Ruled()
    spider.settings = settings(redis)
    items = []
    spider.pipelines = [lambda item: items.append(item) or item]
    stats = spider.run()
    assert sorted(i["name"] for i in items) == EXPECTED
    assert stats["pushed"] == stats["leased"] == PAGES + PAGES * PER_PAGE


def test_a_process_stopped_by_a_limit_gives_back_what_it_held(base, redis):
    class Items(netweir.Spider):
        start_urls = [f"{base}/page/1"]

        def parse(self, page):
            for href in page.css("a.item::attr(href)").getall():
                yield page.follow(href, callback="item")

        def item(self, page):
            yield {"name": page.css("h1::text").get()}

    spider = Items()
    spider.settings = settings(redis, max_pages=3, concurrency=1)
    spider.run()
    assert spider.finish_reason == "max_pages"
    status = netweir.Queue(redis, "books").status()
    assert status["held"] == 0 and status["processes"] == 0
    assert status["waiting"] == PER_PAGE + 1 - 3
    # Another run under the key takes up the rest, and only the rest.
    spider = Items()
    spider.settings = settings(redis)
    stats = spider.run()
    assert stats["fetched"] == PER_PAGE + 1 - 3
    assert all(count == 1 for count in Handler.hits.values()), Handler.hits


@pytest.mark.parametrize(
    ("change", "message"),
    [
        ({"checkpoint": "state"}, "can't have a checkpoint too"),
        ({"deltafetch": "seen"}, "can't have deltafetch too"),
        ({"queue": "rediss://h:6380/0"}, "TLS"),
        ({"queue": "http://h/"}, "redis:// URL"),
        ({"queue_lease": 0}, "queue_lease must be positive"),
        ({"queue_idle": 0}, "queue_idle must be positive"),
        ({"queue_key": ""}, "queue_key can't be empty"),
    ],
)
def test_settings_that_cannot_work_are_refused(change, message):
    with pytest.raises(ValueError, match=message):
        netweir.Settings(**{"queue": "redis://h:6379/0", **change})


def test_callbacks_and_meta_must_travel_as_data(base, redis):
    class Meta(netweir.Spider):
        start_urls = [f"{base}/page/1"]

        def start(self):
            yield netweir.Request(f"{base}/page/1", meta={"when": object()})

    spider = Meta()
    spider.settings = settings(redis)
    with pytest.raises(TypeError, match="with a queue, meta must be JSON"):
        spider.run()


def test_cb_kwargs_travel_through_the_queue(base, redis):
    class Shelved(netweir.Spider):
        def start(self):
            yield netweir.Request(f"{base}/page/{PAGES}", cb_kwargs={"shelf": "last"})

        def parse(self, page, shelf):
            for href in page.css("a.item::attr(href)").getall():
                yield page.follow(href, callback="item", cb_kwargs={"shelf": shelf})

        def item(self, page, shelf):
            yield {"name": page.css("h1::text").get(), "shelf": shelf}

    items = []
    spider = Shelved()
    spider.settings = settings(redis)
    spider.pipelines = [lambda item: items.append(item) or item]
    stats = finishes(spider)
    assert stats["callback_errors"] == 0
    assert sorted(i["name"] for i in items) == [f"{PAGES}-{i}" for i in range(PER_PAGE)]
    assert {i["shelf"] for i in items} == {"last"}


def test_cb_kwargs_must_travel_as_data(base, redis):
    class Kwargs(netweir.Spider):
        def start(self):
            yield netweir.Request(f"{base}/page/1", cb_kwargs={"when": object()})

    spider = Kwargs()
    spider.settings = settings(redis)
    with pytest.raises(TypeError, match="with a queue, cb_kwargs must be JSON"):
        spider.run()


def test_a_queue_that_cannot_be_reached_stops_the_crawl_before_it_starts(tmp_path, capsys):
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]

    class Nothing(netweir.Spider):
        pass

    spider = Nothing()
    spider.settings = settings(f"redis://:secret@127.0.0.1:{port}/0")
    with pytest.raises(netweir.QueueError, match="can't reach the queue") as caught:
        spider.run(output=str(tmp_path / "items.jsonl"))
    assert "secret" not in str(caught.value)
    assert not (tmp_path / "items.jsonl").exists()
    # From the command line, it's one line.
    script = tmp_path / "spider.py"
    script.write_text("import netweir\n\nclass Nothing(netweir.Spider):\n    pass\n")
    argv = ["crawl", str(script), "--queue", f"redis://127.0.0.1:{port}/0", "-q"]
    assert _cli.main(argv) == 1
    assert capsys.readouterr().err.startswith("netweir: can't reach the queue")


def test_the_queue_command_pushes_shows_and_clears(redis, capsys):
    assert _cli.main(["queue", "push", redis, "books", "https://e.com/a", "https://e.com/a"]) == 0
    assert capsys.readouterr().out == "1 queued, 1 seen before\n"
    assert _cli.main(["queue", "status", redis, "books"]) == 0
    out = capsys.readouterr().out
    assert "waiting: 1\n" in out and "seen: 1\n" in out and "queued: 1\n" in out
    assert _cli.main(["queue", "clear", redis, "books"]) == 0
    assert netweir.Queue(redis, "books").status()["seen"] == 0
    assert _cli.main(["queue", "push", redis, "books", "not a url"]) == 2
    assert "not an http(s) URL" in capsys.readouterr().err
    assert _cli.main(["queue", "status", redis, "books", "https://e.com/"]) == 2


def test_queue_push_without_urls_is_a_usage_mistake(capsys):
    assert _cli.main(["queue", "push", "redis://localhost:6379/0", "books"]) == 2
    err = capsys.readouterr().err
    assert err.startswith("netweir: ") and len(err.splitlines()) == 1, err
    assert "-" in err and "stdin" in err, err


def test_queue_mistakes_are_value_errors_and_a_lost_server_a_queue_error(capsys):
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    for url in ["http://h/", "rediss://h:6380/0", "redis://h:notaport/0"]:
        with pytest.raises(ValueError):
            netweir.Queue(url, "books")
        assert _cli.main(["queue", "status", url, "books"]) == 2
    with pytest.raises(ValueError, match="queue_key"):
        netweir.Queue(f"redis://127.0.0.1:{port}/0", "")
    with pytest.raises(netweir.QueueError) as caught:
        netweir.Queue(f"redis://127.0.0.1:{port}/0", "books")
    assert isinstance(caught.value, ConnectionError)
    capsys.readouterr()
    assert _cli.main(["queue", "status", f"redis://127.0.0.1:{port}/0", "books"]) == 1
    assert len(capsys.readouterr().err.splitlines()) == 1


def test_a_malformed_queue_url_is_a_usage_mistake_found_without_connecting(tmp_path, capsys):
    with pytest.raises(ValueError, match="notaport"):
        netweir.Settings(queue="redis://h:notaport/0")
    script = tmp_path / "spider.py"
    script.write_text("import netweir\n\nclass Nothing(netweir.Spider):\n    pass\n")
    assert _cli.main(["crawl", str(script), "--queue", "redis://h:notaport/0", "-q"]) == 2
    err = capsys.readouterr().err
    assert err.startswith("netweir: ") and len(err.splitlines()) == 1, err


def test_a_long_backlog_of_pushes_doesnt_starve_the_heartbeat(fake):
    class Many(netweir.Spider):
        def start(self):
            for n in range(7000):
                yield netweir.Request(f"http://127.0.0.1:1/{n}")

    spider = Many()
    spider.settings = settings(fake.url, queue_lease=3.0, max_pages=1, retries=0)
    # Seven batches at 0.5 s each, well under the 1 s a call may take (a
    # third of a lease; with a 2 s lease, a busy macOS runner went past
    # the 0.67 s it left): 3.5 s of pushing, longer than a lease.
    fake.push_delay = 0.5
    finishes(spider)
    assert fake.scripts["-- netweir push"] >= 7
    gaps = [b - a for a, b in itertools.pairwise(fake.times)]
    # A heartbeat every lease/3 or so, between batches: never a lease
    # without one, so no other process would take this one for dead.
    assert max(gaps) < 3.0, gaps


def test_a_backlog_whose_every_batch_fails_once_still_gets_through(fake):
    class Many(netweir.Spider):
        def start(self):
            for n in range(5000):
                yield netweir.Request(f"http://127.0.0.1:1/{n}")

    spider = Many()
    spider.settings = settings(fake.url, queue_lease=2.0, max_pages=1, retries=0)
    # Each batch's first try takes longer than a call may (0.67 s), and
    # its second gets through: about a second a batch, five seconds of
    # failing now and then, and never one step failing for a lease.
    fake.push_delays = [0.8, 0.0] * 5
    finishes(spider)
    assert fake.scripts["-- netweir push"] >= 10


def test_a_step_that_keeps_failing_while_heartbeats_get_through_stops_the_process(fake):
    class One(netweir.Spider):
        def start(self):
            yield netweir.Request("http://127.0.0.1:1/")

    spider = One()
    spider.settings = settings(fake.url, queue_idle=None, queue_lease=2.0)
    # Every push takes longer than a call may (2 s / 3): it never gets
    # through, though every heartbeat does.
    fake.push_delay = 1.0
    started = time.monotonic()
    with pytest.raises(netweir.QueueError, match="longer than queue_lease"):
        finishes(spider, timeout=60)
    assert time.monotonic() - started < 10


@pytest.mark.skipif(sys.platform == "win32", reason="Windows resets a full backlog: no black hole")
def test_a_call_made_while_reconnecting_to_nowhere_still_ends_within_a_lease(fake):
    class Waiting(netweir.Spider):
        pass

    spider = Waiting()
    spider.settings = settings(fake.url, queue_idle=None, queue_lease=6.0)
    cut: list[float] = []

    def cut_off():
        # Just before a heartbeat is due (one every 2 s): the connection
        # drops, and every reconnect finds a port that takes the
        # connection and never answers.
        while not fake.last_time or time.time() - fake.last_time < 1.9:
            time.sleep(0.01)
        fake.black_hole()
        cut.append(time.monotonic())

    threading.Thread(target=cut_off, daemon=True).start()
    with pytest.raises(netweir.QueueError, match="longer than queue_lease"):
        finishes(spider, timeout=60)
    took = time.monotonic() - cut[0]
    # The first call fails at once (the connection drops); each after it
    # ends within a third of a lease (2 s), a second apart, so the lease,
    # run out 4.1 s after the cut, is found so by about 6 s. (That one call
    # is cut off at a third of a lease is the queue's own test.)
    assert took < 7.5, took


def test_a_password_in_a_socket_url_is_never_shown():
    # Where there are unix sockets the server isn't there (QueueError);
    # where there aren't (Windows), the URL is refused (ValueError).
    with pytest.raises((netweir.QueueError, ValueError)) as caught:
        netweir.Queue("redis+unix:///nowhere/redis.sock?db=0&pass=secret", "books")
    if sys.platform != "win32":
        assert isinstance(caught.value, netweir.QueueError)
    assert "secret" not in str(caught.value)
    assert "pass=***" in str(caught.value)


def test_remote_outputs_are_refused_with_a_queue():
    class Nothing(netweir.Spider):
        pass

    spider = Nothing()
    spider.settings = settings("redis://127.0.0.1:1/0")
    with pytest.raises(ValueError, match="with a queue can't write to memory://"):
        spider.run(output="memory://b/items.jsonl")


def items_spider(base):
    class Items(netweir.Spider):
        start_urls = [f"{base}/page/1"]

        def parse(self, page):
            for href in page.css("a.item::attr(href)").getall():
                yield page.follow(href, callback="item")
            if nxt := page.css("a.next::attr(href)").get():
                yield page.follow(nxt)

        def item(self, page):
            yield {"name": page.css("h1::text").get()}

    return Items()


def test_a_push_whose_reply_was_lost_is_sent_again_and_the_crawl_still_ends(base, fake):
    spider = items_spider(base)
    # Sent again after the lost reply, it must still be one request.
    spider.start = lambda: [netweir.Request(f"{base}/page/1", dont_filter=True)]
    spider.settings = settings(fake.url)
    items = []
    spider.pipelines = [lambda item: items.append(item) or item]
    fake.lose_reply(b"-- netweir push")
    # And the first page's links: sent again, they're still queued by
    # this process, not duplicates.
    fake.lose_reply(b"/item/1-0")
    stats = finishes(spider)
    assert not fake.losing, "a push's reply was lost"
    assert sorted(i["name"] for i in items) == EXPECTED
    assert Handler.hits["/page/1"] == 1
    assert (stats["pushed"], stats["duplicates"]) == (PAGES + PAGES * PER_PAGE, 0)
    status = netweir.Queue(fake.url, "books").status()
    assert (status["waiting"], status["held"]) == (0, 0)


def test_a_lease_whose_reply_was_lost_is_given_back_and_fetched(base, fake):
    spider = items_spider(base)
    spider.settings = settings(fake.url)
    items = []
    spider.pipelines = [lambda item: items.append(item) or item]
    # A lease that took something: one that took nothing loses nothing.
    fake.lose_reply(b"ZRANGESTORE", did_something=True)
    finishes(spider, timeout=30)
    assert not fake.losing, "a lease's reply was lost"
    assert sorted(i["name"] for i in items) == EXPECTED


def member(seq, **change):
    request = {
        "url": "https://e.com/x", "priority": 0, "headers": [], "dont_filter": False,
        "depth": 0, "payload": "{}", "method": "GET", "kind": "navigate", "body": None,
        "content_type": None, "referer": None, "retry_post": False, "proxy": None,
        **change,
    }  # fmt: skip
    return f"{seq:016x}".encode() + json.dumps(request).encode()


def test_members_that_cant_be_read_are_counted_dropped_and_the_crawl_goes_on(base, redis, caplog):
    for bad in [
        b"not one of ours",
        member(1, body="aéb"),  # not hex, and its second pair cuts "é" in half
        member(2, payload="not json"),
        member(3, payload="[1]"),
        member(4, kind="bogus"),
        # Pushed from outside, so through the middlewares: still caught.
        member(5, kind="bogus", payload='{"pushed": true}'),
        # Not UTF-8 at all: it has to be released by its own bytes.
        b"0000000000000006" + b'{"url": "\xff\xfe"}',
    ]:
        assert raw(redis, "ZADD", "books:requests", 0, bad) == b":1\r\n"
    spider = items_spider(base)
    spider.settings = settings(redis)
    items = []
    spider.pipelines = [lambda item: items.append(item) or item]
    with caplog.at_level("WARNING", logger="netweir"):
        stats = finishes(spider)
    assert sorted(i["name"] for i in items) == EXPECTED
    assert stats["queue_bad"] == 7
    assert caplog.text.count("from the shared queue can't be made a request") == 4
    status = netweir.Queue(redis, "books").status()
    assert (status["waiting"], status["held"]) == (0, 0)


def test_a_pushed_url_whose_middleware_fails_is_released_not_given_back(base, redis):
    class Poison(Exception):
        pass

    class Refuse:
        def process_request(self, request, spider):
            if "poison" in request.url:
                raise Poison(request.url)

    class Fed(netweir.Spider):
        middlewares = [Refuse()]

        def parse(self, page):
            yield {"url": page.url}

    netweir.Queue(redis, "books").push([f"{base}/poison"])
    spider = Fed()
    # With fail_fast this process stops at it; the next one mustn't find it.
    spider.settings = settings(redis, fail_fast=True)
    with pytest.raises(Poison):
        finishes(spider)
    status = netweir.Queue(redis, "books").status()
    assert (status["waiting"], status["held"]) == (0, 0)


def test_a_process_cut_off_from_redis_stops_within_a_lease_of_its_last_heartbeat(fake):
    class Waiting(netweir.Spider):
        pass

    spider = Waiting()
    spider.settings = settings(fake.url, queue_idle=None, queue_lease=6.0)
    cut: list[float] = []

    def cut_off():
        # Just before a heartbeat is due (one every 2 s), so the lease has
        # less than 4 s left when Redis stops answering.
        while not fake.last_time or time.time() - fake.last_time < 1.9:
            time.sleep(0.01)
        fake.hang = True
        cut.append(time.monotonic())

    threading.Thread(target=cut_off, daemon=True).start()
    with pytest.raises(netweir.QueueError, match="longer than queue_lease"):
        finishes(spider, timeout=60)
    took = time.monotonic() - cut[0]
    # Counted from the failure (or with replies awaited for 30 s), it would
    # take 8 s or more.
    assert took < 7, took


def test_large_pushes_go_in_batches(fake, monkeypatch, capsys):
    urls = [f"https://e.com/{n}" for n in range(2500)]
    assert netweir.Queue(fake.url, "books").push(urls) == 2500
    assert fake.scripts["-- netweir push"] == 3

    def lines():
        for n, url in enumerate(urls):
            if n == 1500:
                # Read as it goes: the first thousand are in Redis already.
                assert fake.scripts["-- netweir push"] == 4
            yield f"{url}/b\n"

    monkeypatch.setattr(sys, "stdin", lines())
    assert _cli.main(["queue", "push", fake.url, "books", "-"]) == 0
    assert capsys.readouterr().out == "2500 queued, 0 seen before\n"
    assert fake.scripts["-- netweir push"] == 6
