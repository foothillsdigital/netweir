"""Opens browsers and pages over and over; when any step takes longer than
STALL seconds, records what Chrome and the machine are doing while it's
still stuck."""

import asyncio
import os
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import netweir

STALL = 10
OUT = sys.argv[1]
WORKER = sys.argv[2]
UNTIL = time.monotonic() + float(sys.argv[3])
captured = 0


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        body = b"<!doctype html><title>t</title><p>hi</p><script>fetch('/x')</script>"
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def run(cmd, path):
    with open(path, "w") as f:
        subprocess.run(cmd, stdout=f, stderr=subprocess.STDOUT, shell=True, timeout=120)


last = 0.0


def capture(step):
    global captured, last
    # Six at most, a minute apart, so they spread over the run.
    if captured >= 6 or time.monotonic() - last < 60:
        return
    captured += 1
    last = time.monotonic()
    d = os.path.join(OUT, f"w{WORKER}-stall{captured}")
    os.makedirs(d, exist_ok=True)
    with open(os.path.join(d, "step.txt"), "w") as f:
        f.write(f"{step} still waiting after {STALL}s at {time.strftime('%H:%M:%S')}\n")
    run("ps -axo pid,ppid,stat,pcpu,pmem,etime,wchan,command", os.path.join(d, "ps.txt"))
    run("top -l 2 -n 25 -o cpu -s 1", os.path.join(d, "top.txt"))
    run("vm_stat; uptime; sysctl vm.swapusage", os.path.join(d, "vm.txt"))
    pids = subprocess.run(
        ["pgrep", "-f", "Chrome for Testing"], capture_output=True, text=True
    ).stdout.split()
    threads = [
        threading.Thread(target=run, args=(f"sample {p} 3", os.path.join(d, f"sample-{p}.txt")))
        for p in pids[:80]
    ]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    run(
        "/usr/bin/log show --last 3m --style compact --predicate "
        '\'process == "syspolicyd" OR process == "XprotectService" OR process == "amfid" '
        'OR process == "kernel" OR process CONTAINS "Chrome"\' | tail -2000',
        os.path.join(d, "log.txt"),
    )


async def timed(step, coro):
    timer = threading.Timer(STALL, capture, args=(step,))
    timer.daemon = True
    start = time.monotonic()
    timer.start()
    try:
        return await coro
    finally:
        timer.cancel()
        took = time.monotonic() - start
        if took > 5:
            print(f"worker {WORKER}: {step} took {took:.1f}s", flush=True)


async def main():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    url = f"http://127.0.0.1:{server.server_port}/"
    rounds = steps = errors = 0
    while time.monotonic() < UNTIL:
        rounds += 1
        try:
            b = await timed("launch", netweir.browser(timeout=30))
            for _ in range(5):
                page = await timed("new_page", b.new_page())
                await timed("goto", page.goto(url))
                await timed("evaluate", page.evaluate("document.title"))
                await timed("page.close", page.close())
                steps += 4
            await timed("close", b.close())
        except Exception as e:
            errors += 1
            print(
                f"worker {WORKER}: round {rounds}: {type(e).__name__}: {str(e)[:300]}", flush=True
            )
    print(f"worker {WORKER}: {rounds} rounds, {steps} page steps, {errors} errors", flush=True)


asyncio.run(main())
