"""Checks Firefox's dynamic-start remote agent on this runner.

Usage: probe.py FIREFOX_BINARY OUT_DIR
The Firefox install must already hold autoconfig.js and mozilla.cfg.
"""

import asyncio
import http.server
import json
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

import websockets

FIREFOX, OUT = sys.argv[1], Path(sys.argv[2])
OUT.mkdir(parents=True, exist_ok=True)
PAGE = (Path(__file__).parent / "fp.html").read_bytes()
reports = {}


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header("content-type", "text/html")
        self.end_headers()
        self.wfile.write(PAGE)

    def do_POST(self):
        who = self.path.split("who=")[-1]
        reports[who] = json.loads(self.rfile.read(int(self.headers["content-length"])))
        self.send_response(204)
        self.end_headers()

    def log_message(self, *a):
        pass


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
URL = f"http://127.0.0.1:{server.server_port}/"


def launch(args, log):
    profile = tempfile.mkdtemp()
    return subprocess.Popen(
        [FIREFOX, "--headless", "--no-remote", "--profile", profile, *args],
        stdout=log, stderr=subprocess.STDOUT,
    )


def wait_report(who, limit=60):
    end = time.time() + limit
    while time.time() < end and who not in reports:
        time.sleep(0.5)
    return reports.get(who)


async def dynamic(results):
    async with websockets.connect("ws://127.0.0.1:9222/session", max_size=None) as ws:
        n = 0

        async def call(method, params):
            nonlocal n
            n += 1
            await ws.send(json.dumps({"id": n, "method": method, "params": params}))
            while True:
                m = json.loads(await asyncio.wait_for(ws.recv(), 20))
                if m.get("id") == n:
                    return m

        async def check(name, method, params):
            try:
                r = await call(method, params)
                results[name] = "ok" if r.get("type") == "success" else f"{r.get('error')}: {r.get('message', '')[:120]}"
                return r
            except asyncio.TimeoutError:
                results[name] = "TIMEOUT"

        s = await call("session.new", {"capabilities": {}})
        uc = s["result"]["capabilities"].get("moz:userContext")
        results["moz:userContext"] = uc
        tree = (await call("browsingContext.getTree", {}))["result"]["contexts"]
        results["tabs"] = [c["userContext"] == uc for c in tree]
        ctx = [c for c in tree if c["userContext"] == uc][0]["context"]
        await check("navigate", "browsingContext.navigate", {"context": ctx, "url": "data:text/html,<input id=i autofocus>", "wait": "complete"})
        await check("keys", "input.performActions", {"context": ctx, "actions": [{"type": "key", "id": "k", "actions": [{"type": "keyDown", "value": "a"}, {"type": "keyUp", "value": "a"}]}]})
        r = await call("script.evaluate", {"expression": "JSON.stringify({vis: document.visibilityState, typed: document.getElementById('i').value, wd: navigator.webdriver})", "target": {"context": ctx}, "awaitPromise": False})
        results["page"] = json.loads(r["result"]["result"]["value"])
        c2 = await check("create foreground", "browsingContext.create", {"type": "tab", "userContext": uc})
        await check("activate", "browsingContext.activate", {"context": ctx})
        await check("screenshot", "browsingContext.captureScreenshot", {"context": ctx})
        await check("print", "browsingContext.print", {"context": ctx})
        if c2 and c2.get("type") == "success":
            await check("close", "browsingContext.close", {"context": c2["result"]["context"]})
        await check("fingerprint page", "browsingContext.navigate", {"context": ctx, "url": URL + "#dynamic", "wait": "complete"})


def main():
    results = {}
    with open(OUT / "plain.log", "w") as log:
        p = launch([URL + "#plain"], log)
        wait_report("plain")
        p.kill()
    with open(OUT / "dynamic.log", "w") as log:
        p = launch(["about:blank"], log)
        started = time.time()
        while time.time() - started < 90:
            if "listening" in (OUT / "dynamic.log").read_text():
                break
            time.sleep(0.5)
        results["listening_after_s"] = round(time.time() - started, 1)
        try:
            asyncio.run(dynamic(results))
        except Exception as e:
            results["error"] = repr(e)
        wait_report("dynamic", 20)
        p.kill()
    a, b = reports.get("plain", {}), reports.get("dynamic", {})
    results["fingerprint_differences"] = {k: [a.get(k), b.get(k)] for k in set(a) | set(b) if a.get(k) != b.get(k)}
    results["reports"] = sorted(reports)
    (OUT / "results.json").write_text(json.dumps(results, indent=2))
    print(json.dumps(results, indent=2))
    page = results.get("page", {})
    ok = page.get("wd") is False and page.get("vis") == "visible" and page.get("typed") == "a"
    sys.exit(0 if ok else 1)


main()
