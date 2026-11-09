"""``netweir serve``: spiders run as jobs, for clients of an HTTP API
shaped like scrapyd's. See docs/design/serve.md.

Each job is a ``netweir crawl`` in a process of its own; the service
queues them, starts them, cancels them and remembers them, in SQLite.
"""

from __future__ import annotations

import base64
import binascii
import contextlib
import dataclasses
import datetime
import errno
import hmac
import ipaddress
import itertools
import json
import logging
import math
import os
import re
import shutil
import signal
import socket
import socketserver
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any
from urllib.parse import parse_qs, urlsplit

log = logging.getLogger("netweir.serve")

#: What a project, spider or job id may be: never "." or "..", never a
#: slash or a percent-escape, so a path built from them stays put.
NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,99}")
#: How long a cancelled job has to wind up before it's killed.
GRACE = 30.0
#: How long finding a project's spiders may take.
LISTING_TIMEOUT = 60.0
#: The most a POST body may be: parameters, never files.
MAX_BODY = 1 << 20
#: Settings that name a file or directory the crawl writes to. A job may
#: only name one inside its project's data directory, DIR/data/PROJECT.
PATH_SETTINGS = ("checkpoint", "cache", "deltafetch", "warc", "har")
#: A connection has REQUEST_DEADLINE seconds from being accepted to send
#: its whole request, and may pause at most REQUEST_TIMEOUT seconds in
#: it; one client address may hold PER_CLIENT connections at once, and
#: everyone together MAX_CONNECTIONS. What a slow client can hold is
#: PER_CLIENT threads for REQUEST_DEADLINE seconds at a time.
REQUEST_DEADLINE = 15.0
REQUEST_TIMEOUT = 15.0
PER_CLIENT = 8
MAX_CONNECTIONS = 256


def main(args) -> int:
    logging.basicConfig(level=logging.INFO, format="%(levelname)s %(message)s")
    try:
        projects: dict[str, Path] = {}
        for text in args.project:
            name, path = _project(text)
            # Ignoring case: on a filesystem that does, Shop and shop
            # would share files.
            if name.lower() in (p.lower() for p in projects):
                raise ValueError(f"--project {name} is given twice")
            projects[name] = path
        if args.max_jobs is not None and args.max_jobs < 1:
            raise ValueError("--max-jobs must be at least 1")
        if args.keep < 0:
            raise ValueError("--keep can't be negative")
        if bool(args.username) != bool(args.password):
            raise ValueError("--username and --password go together")
        credentials = (args.username, args.password) if args.username else None
        if credentials is None and not _loopback(args.bind) and not args.insecure_no_auth:
            raise ValueError(
                f"--bind {args.bind} without --username and --password would let anyone who"
                " can reach the port run your spiders, and any web page read their logs and"
                " items; set them, or give --insecure-no-auth"
            )
        service = Service(
            Path(args.dir),
            projects,
            max_jobs=args.max_jobs or os.cpu_count() or 1,
            keep=args.keep,
            credentials=credentials,
        )
    except ValueError as e:
        print(f"netweir: {e}", file=sys.stderr)
        return 2
    try:
        httpd = server(service, args.bind, args.port)
    except OSError as e:
        service.close()
        print(f"netweir: can't listen on {args.bind}:{args.port}: {e}", file=sys.stderr)
        return 1
    host, port = httpd.server_address[:2]
    if credentials is None and not _loopback(args.bind):
        log.warning(
            "--insecure-no-auth: listening on %s without a password; anyone who can reach"
            " port %d can run your spiders, and any web page can read their logs and items",
            args.bind,
            port,
        )
    if not projects:
        log.warning("no --project given: there are no spiders to run")
    service.start()
    log.info("serving on http://%s:%d", f"[{host}]" if ":" in host else host, port)
    from netweir._cli import STOPS

    # A service manager's stop, or Ctrl-Break on Windows, ends the
    # service as Ctrl-C does. Jobs have consoles of their own, so a
    # Ctrl-Break typed here reaches only the service.
    for stop in STOPS:
        signal.signal(stop, _interrupt)
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        log.info("stopping: cancelling running jobs")
    finally:
        httpd.server_close()
        service.stop()
    return 0


def _interrupt(signum, frame) -> None:
    raise KeyboardInterrupt


def _project(text: str) -> tuple[str, Path]:
    """NAME=PATH, or PATH, named after its file or directory."""
    name, sep, path = text.partition("=")
    if not sep:
        # A directory is named whole, after it's resolved, so "." is the
        # current directory; a file drops its extension.
        given = Path(text)
        name, path = (given.resolve().name if given.is_dir() else given.stem), text
    if not NAME.fullmatch(name):
        raise ValueError(
            f"--project {name!r}: a project's name is letters, digits, '.', '_' and '-'"
        )
    if not Path(path).exists():
        raise ValueError(f"--project {name}: no such file or directory: {path}")
    return name, Path(path).resolve()


def _loopback(host: str) -> bool:
    # A host name ignores case, and may end in a dot.
    host = host.lower().rstrip(".")
    if host == "localhost":
        return True
    try:
        return ipaddress.ip_address(host).is_loopback
    except ValueError:
        return False


def _ip(host: str) -> bool:
    try:
        ipaddress.ip_address(host)
    except ValueError:
        return False
    return True


def _python() -> tuple[str, dict[str, str]]:
    """The Python to run a job with, and what its environment needs.
    In a venv on Windows, sys.executable is a launcher that starts the
    real interpreter as its child, outside any job object the launcher
    is put in; as multiprocessing does (bpo-35797), the interpreter is
    started directly and told which venv it's in."""
    base = getattr(sys, "_base_executable", sys.executable)
    if sys.platform == "win32" and os.path.normcase(base) != os.path.normcase(sys.executable):
        return base, {"__PYVENV_LAUNCHER__": sys.executable}
    return sys.executable, {}


def _end(process: subprocess.Popen, tree: Any) -> bool:
    """Kills a job and whatever it started, at once: its process group,
    which its worker processes share, or on Windows its job object
    `tree`, and any job nested in that (Chrome's, say). False when that
    couldn't be done, for _taskkill to try."""
    if sys.platform != "win32":
        with contextlib.suppress(ProcessLookupError):
            os.killpg(process.pid, signal.SIGKILL)
        return True
    if tree is not None and _kernel32.TerminateJobObject(tree, 1):
        return True
    if tree is not None:
        log.warning(
            "couldn't end the job object of pid %d: %s",
            process.pid,
            ctypes.WinError(ctypes.get_last_error()),
        )
    return False


def _taskkill(process: subprocess.Popen) -> None:
    """On Windows, without a job object: kills the process tree, going by
    parent, so a grandchild whose parent already ended is missed. Slow:
    never with the service's lock held."""
    try:
        done = subprocess.run(
            [str(Path(_system_directory(), "taskkill.exe")), "/F", "/T", "/PID", str(process.pid)],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            timeout=30,
            creationflags=subprocess.CREATE_NO_WINDOW,
        )
        if done.returncode:
            log.warning(
                "taskkill of pid %d: %s", process.pid, done.stderr.decode(errors="replace").strip()
            )
    except (OSError, subprocess.SubprocessError) as e:
        log.warning("taskkill of pid %d: %s", process.pid, e)
    # Whatever taskkill couldn't do, the job itself still goes.
    process.kill()


#: Run in a process of its own, with a job's pid: sends Ctrl-Break to
#: every process on that job's console, and to nothing else. Exits with
#: what went wrong, if anything: AttachConsole fails once the job is gone.
#: Started with no console, so it can attach to the job's.
_BREAK = """
import ctypes, signal, sys
kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
signal.signal(signal.SIGBREAK, signal.SIG_IGN)
if not kernel32.AttachConsole(int(sys.argv[1])):
    sys.exit(f"AttachConsole: {ctypes.WinError(ctypes.get_last_error())}")
if not kernel32.GenerateConsoleCtrlEvent(signal.CTRL_BREAK_EVENT, 0):
    sys.exit(f"GenerateConsoleCtrlEvent: {ctypes.WinError(ctypes.get_last_error())}")
"""


def _break(processes: list[subprocess.Popen]) -> list[str | None]:
    """Asks jobs on Windows to stop as Ctrl-C would, by sending Ctrl-Break
    to each one's console, all at once: for each, None once sent, or why
    it wasn't. Slow: never with the service's lock held. The Popen
    objects hold the jobs' handles, so no pid has been reused."""
    python, extra = _python()
    helpers: list[subprocess.Popen | str] = []
    for process in processes:
        try:
            helpers.append(
                subprocess.Popen(
                    [python, "-I", "-S", "-c", _BREAK, str(process.pid)],
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.PIPE,
                    env={**os.environ, **extra},
                    creationflags=subprocess.DETACHED_PROCESS,
                )
            )
        except OSError as e:
            helpers.append(str(e))
    whys: list[str | None] = []
    for helper in helpers:
        if isinstance(helper, str):
            whys.append(helper)
            continue
        try:
            _, err = helper.communicate(timeout=30)
        except subprocess.TimeoutExpired:
            helper.kill()
            helper.communicate()
            whys.append("the helper timed out")
            continue
        # Ended by the Ctrl-Break it sent (it can arrive as the helper
        # exits, past ignoring it): sent, then, since it's on no other
        # console.
        if helper.returncode in (0, STATUS_CONTROL_C_EXIT):
            whys.append(None)
        else:
            whys.append(err.decode(errors="replace").strip() or f"exit status {helper.returncode}")
    return whys


STATUS_CONTROL_C_EXIT = 0xC000013A


if sys.platform == "win32":
    import ctypes
    from ctypes import wintypes

    _kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    _kernel32.CreateJobObjectW.argtypes = (wintypes.LPVOID, wintypes.LPCWSTR)
    _kernel32.CreateJobObjectW.restype = wintypes.HANDLE
    _kernel32.AssignProcessToJobObject.argtypes = (wintypes.HANDLE, wintypes.HANDLE)
    _kernel32.AssignProcessToJobObject.restype = wintypes.BOOL
    _kernel32.TerminateJobObject.argtypes = (wintypes.HANDLE, wintypes.UINT)
    _kernel32.TerminateJobObject.restype = wintypes.BOOL
    _kernel32.CloseHandle.argtypes = (wintypes.HANDLE,)
    _kernel32.CloseHandle.restype = wintypes.BOOL
    _kernel32.GetSystemDirectoryW.argtypes = (wintypes.LPWSTR, wintypes.UINT)
    _kernel32.GetSystemDirectoryW.restype = wintypes.UINT


def _tree(process: subprocess.Popen) -> Any:
    """A job object holding the process and whatever it starts, or None
    when there isn't one: on Windows only, and if it could be made.
    Without KILL_ON_JOB_CLOSE: a job outlives the service."""
    if sys.platform != "win32":
        return None
    job = _kernel32.CreateJobObjectW(None, None)
    if not job:
        log.warning("couldn't make a job object: %s", ctypes.WinError(ctypes.get_last_error()))
        return None
    # Known limit: assigned just after the start, so a process the job
    # started in that instant would escape. The job is the interpreter
    # itself (see _python), whose start-up takes far longer than that.
    if not _kernel32.AssignProcessToJobObject(job, int(process._handle)):  # type: ignore[attr-defined]
        log.warning(
            "couldn't put pid %d in a job object: %s",
            process.pid,
            ctypes.WinError(ctypes.get_last_error()),
        )
        _kernel32.CloseHandle(job)
        return None
    return job


def _system_directory() -> str:
    buffer = ctypes.create_unicode_buffer(260)
    size = _kernel32.GetSystemDirectoryW(buffer, len(buffer))
    if not 0 < size < len(buffer):
        raise OSError("GetSystemDirectoryW failed")
    return buffer.value


def _lock(path: Path) -> Any:
    """The open lock file, held until it's closed, or None when another
    process holds it."""
    f = open(path, "a+b")  # noqa: SIM115 - closed by Service.close()
    try:
        if sys.platform == "win32":
            import msvcrt

            f.seek(0)
            msvcrt.locking(f.fileno(), msvcrt.LK_NBLCK, 1)
        else:
            import fcntl

            fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError as e:
        f.close()
        if e.errno in (errno.EWOULDBLOCK, errno.EAGAIN, errno.EACCES, errno.EDEADLK):
            return None
        raise
    return f


def _now() -> str:
    """The time as scrapyd writes it: 2026-10-12 14:03:59.123456."""
    return datetime.datetime.now().isoformat(" ", "microseconds")


class Service:
    """The jobs: their table, their processes and the thread that starts
    and reaps them. Every method may be called from any thread."""

    def __init__(
        self,
        directory: Path,
        projects: dict[str, Path],
        max_jobs: int = os.cpu_count() or 1,
        keep: int = 100,
        credentials: tuple[str, str] | None = None,
    ):
        self.dir = Path(directory).resolve()
        # On a filesystem that ignores case, Shop and shop would share files.
        lowered = [name.lower() for name in projects]
        if len(set(lowered)) != len(lowered):
            raise ValueError("two projects have the same name but for case")
        self.projects = projects
        self.max_jobs = max_jobs
        self.keep = keep
        self.credentials = credentials
        self.node_name = socket.gethostname()
        try:
            self.dir.mkdir(parents=True, exist_ok=True)
            # Before the table is touched: a second service on this
            # directory would mark the first one's running jobs lost.
            self.held = _lock(self.dir / "lock")
        except OSError as e:
            raise ValueError(f"--dir {directory}: {e}") from None
        if self.held is None:
            raise ValueError(f"--dir {directory} is in use by another netweir serve")
        try:
            self.db = sqlite3.connect(
                self.dir / "jobs.db", check_same_thread=False, isolation_level=None
            )
        except (OSError, sqlite3.Error) as e:
            self.held.close()
            raise ValueError(f"--dir {directory}: {e}") from None
        self.db.row_factory = sqlite3.Row
        self.db.execute(
            """CREATE TABLE IF NOT EXISTS jobs (
                seq INTEGER PRIMARY KEY AUTOINCREMENT,
                id TEXT NOT NULL UNIQUE COLLATE NOCASE,
                project TEXT NOT NULL,
                spider TEXT NOT NULL,
                file TEXT NOT NULL,
                settings TEXT NOT NULL,
                arguments TEXT NOT NULL,
                priority REAL NOT NULL,
                state TEXT NOT NULL,
                outcome TEXT,
                pid INTEGER,
                exit_code INTEGER,
                start_time TEXT,
                end_time TEXT,
                stats TEXT
            )"""
        )
        #: One at a time: the table, the processes and the flags below.
        self.lock = threading.Lock()
        self.wake = threading.Event()
        #: Running jobs' processes and log files, by job id.
        self.running: dict[str, tuple[subprocess.Popen, Any]] = {}
        #: Jobs told to stop, and when they're killed if they haven't.
        self.cancelling: dict[str, float] = {}
        #: On Windows, running jobs' job objects, by job id.
        self.trees: dict[str, Any] = {}
        #: On Windows, jobs to kill by taskkill, once the lock is let go.
        self.unkilled: list[subprocess.Popen] = []
        self.stopping = False
        self.thread = threading.Thread(target=self._loop, name="netweir-serve", daemon=True)
        # Their process was the last service's child, and nobody is
        # watching it now.
        lost = self.db.execute(
            "UPDATE jobs SET state = 'finished', outcome = 'lost', end_time = ?"
            " WHERE state = 'running'",
            (_now(),),
        ).rowcount
        if lost:
            log.warning("%d job(s) were running when the service last stopped: marked lost", lost)
        self._forget_old()

    def start(self) -> None:
        self.thread.start()

    def stop(self) -> None:
        """Cancels the running jobs and waits for them; pending jobs stay
        pending, for the next service."""
        with self.lock:
            self.stopping = True
            asking = {jobid: self._signal(jobid) for jobid in list(self.running)}
        self._ask(asking)
        self.wake.set()
        if self.thread.is_alive():
            self.thread.join()
        self.close()

    def close(self) -> None:
        self.db.close()
        self.held.close()

    # --- the API's operations, each raising ValueError for a 400 ----------

    def schedule(
        self,
        project: str,
        spider: str,
        settings: list[str],
        arguments: dict[str, str],
        jobid: str | None,
        priority: float,
    ) -> str:
        import netweir
        from netweir import Settings
        from netweir._cli import _parse_setting

        fields = {f.name: f for f in dataclasses.fields(Settings)}
        parsed = dict(_parse_setting(text, fields) for text in settings)
        for key in PATH_SETTINGS:
            if parsed.get(key) is not None:
                self.data_path(project, key, parsed[key])
        for name in arguments:
            if not name.isidentifier() or name.startswith("_"):
                raise ValueError(
                    f"{name!r} isn't a spider argument: its name must be an identifier"
                    " not starting with '_'"
                )
            if hasattr(netweir.Spider, name):
                raise ValueError(
                    f"{name!r} isn't a spider argument: netweir.Spider has its own {name}"
                )
        found = self._list(project, (spider, settings))
        if found["settings_error"]:
            raise ValueError(f"setting: {found['settings_error']}")
        files = found["spiders"].get(spider)
        if not files:
            raise ValueError(f"project {project} has no spider {spider!r}")
        if len(files) > 1:
            raise ValueError(
                f"spider {spider} is in more than one file: {', '.join(Path(f).name for f in files)}"
            )
        jobid = jobid or uuid.uuid1().hex
        with self.lock:
            try:
                self.db.execute(
                    "INSERT INTO jobs (id, project, spider, file, settings, arguments, priority,"
                    " state) VALUES (?, ?, ?, ?, ?, ?, ?, 'pending')",
                    (
                        jobid,
                        project,
                        spider,
                        files[0],
                        json.dumps(settings),
                        json.dumps(arguments),
                        priority,
                    ),
                )
            except sqlite3.IntegrityError:
                raise ValueError(f"there is already a job {jobid}") from None
        self.wake.set()
        return jobid

    def cancel(self, project: str, jobid: str) -> str | None:
        """The job's state before: "pending", "running", or None when
        there was nothing to cancel."""
        asking: dict[str, Any] = {}
        with self.lock:
            row = self.db.execute(
                "SELECT id, state FROM jobs WHERE id = ? AND project = ?", (jobid, project)
            ).fetchone()
            if row is None or row["state"] == "finished":
                return None
            # As stored: ids ignore case, the processes' keys don't.
            jobid = row["id"]
            # Running, even if writing that to the table failed.
            state = "running" if jobid in self.running else row["state"]
            if state == "pending":
                self.db.execute(
                    "UPDATE jobs SET state = 'finished', outcome = 'cancelled', end_time = ?"
                    " WHERE id = ?",
                    (_now(), jobid),
                )
                self._forget_old()
            elif jobid in self.cancelling:
                # Cancelled again: no more waiting.
                self._kill(jobid)
            else:
                asking[jobid] = self._signal(jobid)
        self._ask(asking)
        return state

    def spiders(self, project: str) -> dict[str, list[str]]:
        """{spider name: [files defining it]}."""
        return self._list(project)["spiders"]

    def _list(self, project: str, check: tuple[str, list[str]] | None = None) -> dict[str, Any]:
        """The project's spiders, found by importing its files in a process
        of their own: what they run can't touch the service. With `check`,
        (spider, settings), also whether those settings, over the spider's
        own, are ones its crawl would start with."""
        if project not in self.projects:
            raise ValueError(f"no project {project!r}")
        path = self.projects[project]
        files = [path] if path.is_file() else sorted(path.glob("*.py"))
        try:
            done = subprocess.run(
                [sys.executable, "-m", "netweir._serve"],
                input=json.dumps({"paths": [str(f) for f in files], "check": check}),
                capture_output=True,
                text=True,
                timeout=LISTING_TIMEOUT,
                cwd=path if path.is_dir() else path.parent,
            )
        except subprocess.TimeoutExpired:
            raise ValueError(
                f"project {project}: its files took more than {LISTING_TIMEOUT:.0f} s to load"
            ) from None
        try:
            found = json.loads(done.stdout)
        except ValueError:
            raise ValueError(
                f"project {project}: its files couldn't be loaded: {done.stderr}"
            ) from None
        if found["errors"]:
            raise ValueError(f"project {project}: {'; '.join(found['errors'])}")
        return found

    def jobs(self, project: str | None = None) -> dict[str, list[dict[str, Any]]]:
        query = "SELECT * FROM jobs" + (" WHERE project = ?" if project else "")
        with self.lock:
            rows = self.db.execute(query, (project,) if project else ()).fetchall()
        pending = sorted(
            (r for r in rows if r["state"] == "pending"), key=lambda r: (-r["priority"], r["seq"])
        )
        running = [r for r in rows if r["state"] == "running"]
        finished = sorted(
            (r for r in rows if r["state"] == "finished"), key=lambda r: (r["end_time"], r["seq"])
        )
        return {
            "pending": [
                {
                    "id": r["id"],
                    "project": r["project"],
                    "spider": r["spider"],
                    "settings": json.loads(r["settings"]),
                    "args": json.loads(r["arguments"]),
                    "priority": r["priority"],
                }
                for r in pending
            ],
            "running": [{**_entry(r), "pid": r["pid"]} for r in running],
            "finished": [
                {**_entry(r), "end_time": r["end_time"], "outcome": r["outcome"]} for r in finished
            ],
        }

    def job(self, jobid: str) -> dict[str, Any]:
        with self.lock:
            row = self.db.execute("SELECT * FROM jobs WHERE id = ?", (jobid,)).fetchone()
        if row is None:
            raise ValueError(f"no job {jobid}")
        stats = json.loads(row["stats"]) if row["stats"] else None
        if row["state"] == "running":
            # The job rewrites its stats file every few seconds, each time
            # whole: the stats so far, with finish_reason None.
            path = self.file("stats", row["project"], row["spider"], row["id"])
            try:
                stats = json.loads(path.read_text(encoding="utf-8"))
            except (OSError, ValueError):
                stats = None  # not written yet
        return {
            "id": row["id"],
            "project": row["project"],
            "spider": row["spider"],
            "state": row["state"],
            "outcome": row["outcome"],
            "exit_code": row["exit_code"],
            "stats": stats,
        }

    def counts(self) -> dict[str, int]:
        with self.lock:
            rows = self.db.execute("SELECT state, count(*) FROM jobs GROUP BY state").fetchall()
        return {"pending": 0, "running": 0, "finished": 0, **dict(rows)}  # type: ignore[arg-type]

    def data_path(self, project: str, key: str, value: str) -> Path:
        """Where a checkpoint, cache, deltafetch, warc or har of `project` goes: `value`
        is a relative path inside DIR/data/PROJECT, so the same name in two
        jobs is the same directory, and a later job resumes from it."""
        base = (self.dir / "data" / project).resolve()
        given = Path(value)
        if (
            not value
            or ":" in value  # a URL (s3://, file:) or a Windows drive
            or "\\" in value
            or given.is_absolute()
            or value.startswith("/")  # rooted, though not absolute on Windows
            or ".." in given.parts
        ):
            raise ValueError(
                f"{key} must be a relative path inside the project's data directory,"
                f" without '..' or a URL, not {value!r}"
            )
        # A symlink inside it may point anywhere: where it ends up counts.
        path = (base / given).resolve()
        if not path.is_relative_to(base):
            raise ValueError(f"{key} {value!r} leads outside the project's data directory")
        return path

    def file(self, kind: str, project: str, spider: str, jobid: str) -> Path:
        """Where a job's log, items or stats are: built only from names
        NAME has passed."""
        suffix = {"logs": ".log", "items": ".jsonl", "stats": ".json"}[kind]
        return self.dir / kind / project / spider / (jobid + suffix)

    # --- the scheduler thread ---------------------------------------------

    def _loop(self) -> None:
        while True:
            self.wake.wait(0.1 if self.stopping else 0.5)
            self.wake.clear()
            with self.lock:
                try:
                    if self._step():
                        return
                except Exception:
                    # A full disk, say: the service carries on, and tries
                    # again next time round.
                    log.exception("the scheduler failed")
            self._taskkills()

    def _step(self) -> bool:
        """Reaps the jobs that ended and starts those there's room for;
        True once the service is stopping and every job has ended."""
        self._reap()
        if self.stopping:
            return not self.running
        if len(self.running) >= self.max_jobs:
            return False
        pending = self.db.execute(
            "SELECT * FROM jobs WHERE state = 'pending' ORDER BY priority DESC, seq"
        )
        for row in pending.fetchall():
            if len(self.running) >= self.max_jobs:
                break
            # Started already, if recording that it runs failed.
            if row["id"] not in self.running:
                self._run(row)
        return False

    def _run(self, row: sqlite3.Row) -> None:
        jobid, project, spider = row["id"], row["project"], row["spider"]
        paths = {
            kind: self.file(kind, project, spider, jobid) for kind in ("logs", "items", "stats")
        }
        python, extra = _python()
        command = [
            python, "-m", "netweir", "crawl", row["file"], "--spider", spider,
            "-o", str(paths["items"]), "--stats", str(paths["stats"]),
        ]  # fmt: skip
        output = None
        try:
            for path in paths.values():
                path.parent.mkdir(parents=True, exist_ok=True)
            output = open(paths["logs"], "ab")  # noqa: SIM115 - closed when the job is reaped
            if project not in self.projects:
                raise OSError(f"project {project} isn't served any more")
            for setting in json.loads(row["settings"]):
                key, _, value = setting.partition("=")
                if key in PATH_SETTINGS and value.lower() != "none":
                    # Checked again: a symlink may have changed since.
                    path = self.data_path(project, key, value)
                    path.parent.mkdir(parents=True, exist_ok=True)
                    setting = f"{key}={path}"
                command += ["-s", setting]
            for name, value in json.loads(row["arguments"]).items():
                command += ["-a", f"{name}={value}"]
            process = subprocess.Popen(
                command,
                cwd=Path(row["file"]).parent,
                stdin=subprocess.DEVNULL,
                stdout=output,
                stderr=subprocess.STDOUT,
                env={**os.environ, **extra, "PYTHONUNBUFFERED": "1"},
                # So a Ctrl-C at the service's terminal reaches only the
                # service, which cancels each job once. On Windows, the
                # same by a console of its own, hidden, which a cancel
                # sends Ctrl-Break to.
                start_new_session=True,
                creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
            )
        except (OSError, ValueError) as e:
            log.error("job %s: couldn't start: %s", jobid, e)
            if output is not None:
                output.write(f"netweir: the job couldn't start: {e}\n".encode())
                output.close()
            self.db.execute(
                "UPDATE jobs SET state = 'finished', outcome = 'failed', start_time = ?,"
                " end_time = ? WHERE id = ?",
                (_now(), _now(), jobid),
            )
            self._forget_old()
            return
        # Known before the table says so, so a failure writing it can't
        # get the job started twice. Known limit: if that write fails and
        # the service then dies, the next one finds it pending and runs it
        # again.
        self.running[jobid] = (process, output)
        if (tree := _tree(process)) is not None:
            self.trees[jobid] = tree
        log.info("job %s: %s %s started, pid %d", jobid, project, spider, process.pid)
        self.db.execute(
            "UPDATE jobs SET state = 'running', pid = ?, start_time = ? WHERE id = ?",
            (process.pid, _now(), jobid),
        )

    def _signal(self, jobid: str) -> Any:
        """Asks a running job to stop as Ctrl-C would, and gives it GRACE
        seconds. On Windows that takes a helper process, too slow to start
        with the lock held: the job's process, for _ask, then."""
        process = self.running[jobid][0]
        if process.poll() is not None:
            # It ended on its own; the reaper records how.
            return None
        self.cancelling[jobid] = time.monotonic() + GRACE
        if sys.platform != "win32":
            process.terminate()
            return None
        return process

    def _ask(self, asking: dict[str, Any]) -> None:
        """Without the lock: sends the jobs _signal left on Windows their
        Ctrl-Break, and kills those it couldn't be sent to."""
        asking = {jobid: process for jobid, process in asking.items() if process is not None}
        if asking:
            for (jobid, process), why in zip(
                asking.items(), _break(list(asking.values())), strict=True
            ):
                if why is None:
                    continue
                with self.lock:
                    # Windows has no SIGTERM (terminate() ends a process at
                    # once): without the Ctrl-Break, there's no asking.
                    if jobid in self.running and process.poll() is None:
                        log.warning("job %s: couldn't be asked to stop (%s); killed", jobid, why)
                        self._kill(jobid)
        self._taskkills()

    def _kill(self, jobid: str) -> None:
        """With the lock held: kills a running job and whatever it started,
        or leaves it to _taskkills."""
        process = self.running[jobid][0]
        if process.poll() is None and not _end(process, self.trees.get(jobid)):
            self.unkilled.append(process)

    def _taskkills(self) -> None:
        """Without the lock: kills the jobs _kill couldn't."""
        with self.lock:
            unkilled, self.unkilled = self.unkilled, []
        for process in unkilled:
            _taskkill(process)

    def _reap(self) -> None:
        for jobid, (process, output) in list(self.running.items()):
            code = process.poll()
            if code is None:
                if jobid in self.cancelling and time.monotonic() > self.cancelling[jobid]:
                    log.warning(
                        "job %s: still running %.0f s after cancelling; killed", jobid, GRACE
                    )
                    self._kill(jobid)
                continue
            if jobid in self.cancelling:
                outcome = "cancelled"
            else:
                outcome = "finished" if code == 0 else "failed"
            row = self.db.execute(
                "SELECT project, spider FROM jobs WHERE id = ?", (jobid,)
            ).fetchone()
            try:
                stats = self.file("stats", row["project"], row["spider"], jobid).read_text(
                    encoding="utf-8"
                )
                json.loads(stats)
            except (OSError, ValueError):
                # Killed before it could write them.
                stats = None
            self.db.execute(
                "UPDATE jobs SET state = 'finished', outcome = ?, exit_code = ?, end_time = ?,"
                " stats = ? WHERE id = ?",
                (outcome, code, _now(), stats, jobid),
            )
            # Only once the table says so: if writing it failed, the job
            # is still here to be reaped next time round.
            output.close()
            if (tree := self.trees.pop(jobid, None)) is not None:
                if outcome == "cancelled":
                    # What it started and left behind, ignoring the
                    # Ctrl-Break, goes with it. Known limit: on POSIX, a
                    # process group outlives its reaped leader unswept.
                    _end(process, tree)
                _kernel32.CloseHandle(tree)
            del self.running[jobid]
            self.cancelling.pop(jobid, None)
            log.info("job %s: %s, exit status %d", jobid, outcome, code)
            self._forget_old()

    def _forget_old(self) -> None:
        """Drops all but the newest `keep` finished jobs, and their files."""
        old = self.db.execute(
            "SELECT id, project, spider FROM jobs WHERE state = 'finished'"
            " ORDER BY end_time DESC, seq DESC LIMIT -1 OFFSET ?",
            (self.keep,),
        ).fetchall()
        for row in old:
            for kind in ("logs", "items", "stats"):
                self.file(kind, row["project"], row["spider"], row["id"]).unlink(missing_ok=True)
            self.db.execute("DELETE FROM jobs WHERE id = ?", (row["id"],))


def _entry(row: sqlite3.Row) -> dict[str, Any]:
    project, spider, jobid = row["project"], row["spider"], row["id"]
    return {
        "id": jobid,
        "project": project,
        "spider": spider,
        "start_time": row["start_time"],
        "log_url": f"/logs/{project}/{spider}/{jobid}.log",
        # scrapyd's extension; .jsonl serves the same file.
        "items_url": f"/items/{project}/{spider}/{jobid}.jl",
    }


# --- HTTP ---------------------------------------------------------------------


class _Error(Exception):
    """An answer other than 200: its status and message."""

    def __init__(self, status: int, message: str):
        super().__init__(message)
        self.status = status


def _one(params: dict[str, list[str]], key: str, required: bool = True) -> str | None:
    values = params.get(key)
    if not values:
        if required:
            raise ValueError(f"{key} is required")
        return None
    return values[-1]


def _name(params: dict[str, list[str]], key: str, required: bool = True) -> str | None:
    value = _one(params, key, required)
    if value is not None and not NAME.fullmatch(value):
        raise ValueError(f"{key} {value!r} isn't a name: letters, digits, '.', '_' and '-'")
    return value


def _daemonstatus(service: Service, params) -> dict[str, Any]:
    return service.counts()


def _listprojects(service: Service, params) -> dict[str, Any]:
    return {"projects": sorted(service.projects)}


def _listspiders(service: Service, params) -> dict[str, Any]:
    return {"spiders": sorted(service.spiders(_name(params, "project")))}


# Parameters of schedule.json that aren't spider arguments. _version is
# scrapyd's, and there are no versions here.
_SCHEDULING = {"project", "spider", "setting", "jobid", "priority", "_version"}


def _schedule(service: Service, params) -> dict[str, Any]:
    priority = _one(params, "priority", required=False) or "0"
    try:
        number = float(priority)
    except ValueError:
        raise ValueError(f"priority is a number, not {priority!r}") from None
    if not math.isfinite(number):
        raise ValueError(f"priority is a finite number, not {priority!r}")
    jobid = service.schedule(
        _name(params, "project"),
        _name(params, "spider"),
        params.get("setting", []),
        {k: v[-1] for k, v in params.items() if k not in _SCHEDULING},
        _name(params, "jobid", required=False),
        number,
    )
    return {"jobid": jobid}


def _cancel(service: Service, params) -> dict[str, Any]:
    return {"prevstate": service.cancel(_name(params, "project"), _name(params, "job"))}


def _listjobs(service: Service, params) -> dict[str, Any]:
    return service.jobs(_name(params, "project", required=False))


def _jobstats(service: Service, params) -> dict[str, Any]:
    return service.job(_name(params, "job"))


def _listversions(service: Service, params) -> dict[str, Any]:
    # There are no versions; clients that ask before scheduling get none.
    project = _name(params, "project")
    if project not in service.projects:
        raise ValueError(f"no project {project!r}")
    return {"versions": []}


def _delproject(service: Service, params) -> dict[str, Any]:
    project = _name(params, "project")
    raise ValueError(
        f"project {project} comes from the command line: remove its --project and restart"
    )


_ENDPOINTS = {
    "/daemonstatus.json": ("GET", _daemonstatus),
    "/listprojects.json": ("GET", _listprojects),
    "/listspiders.json": ("GET", _listspiders),
    "/listversions.json": ("GET", _listversions),
    "/listjobs.json": ("GET", _listjobs),
    "/jobstats.json": ("GET", _jobstats),
    "/schedule.json": ("POST", _schedule),
    "/cancel.json": ("POST", _cancel),
    "/delproject.json": ("POST", _delproject),
}

# /logs/PROJECT/SPIDER/JOB.log and /items/PROJECT/SPIDER/JOB.jsonl (or
# .jl, as scrapyd names it), matched
# on the path as sent: a percent-escape fails NAME, so an encoded ".." or
# slash never gets as far as a file name.
_FILE = re.compile(r"/(logs|items)/([^/]+)/([^/]+)/([^/]+)\.(log|jsonl|jl)")


class _Handler(BaseHTTPRequestHandler):
    server: _Server
    server_version = "netweir"
    timeout = REQUEST_TIMEOUT

    def setup(self) -> None:
        super().setup()
        # Cancelled once the request is read: sending an answer, a big
        # items file say, can take as long as it takes.
        self.deadline = self.server.watchdog.add(REQUEST_DEADLINE, self._cut)

    def finish(self) -> None:
        self.deadline.cancel()
        super().finish()

    def _cut(self) -> None:
        with contextlib.suppress(OSError):
            self.connection.shutdown(socket.SHUT_RDWR)

    def log_message(self, format: str, *args: Any) -> None:
        log.debug("%s %s", self.address_string(), format % args)

    def do_GET(self) -> None:
        self._answer("GET")

    def do_POST(self) -> None:
        self._answer("POST")

    def _answer(self, method: str) -> None:
        service = self.server.service
        try:
            self._check()
            split = urlsplit(self.path)
            if split.path.startswith(("/logs/", "/items/")) and method == "GET":
                self.deadline.cancel()
                return self._send_file(split.path)
            endpoint = _ENDPOINTS.get(split.path)
            if endpoint is None:
                raise _Error(404, f"no such endpoint: {split.path}")
            if method != endpoint[0]:
                raise _Error(405, f"{split.path} takes a {endpoint[0]}")
            params = parse_qs(split.query)
            if method == "POST":
                for key, values in parse_qs(self._body()).items():
                    params.setdefault(key, []).extend(values)
            self.deadline.cancel()
            try:
                answer = endpoint[1](service, params)
            except ValueError as e:
                raise _Error(400, str(e)) from None
            self._send_json(200, {"node_name": service.node_name, "status": "ok", **answer})
        except _Error as e:
            self._send_json(
                e.status, {"node_name": service.node_name, "status": "error", "message": str(e)}
            )

    def _check(self) -> None:
        """Refuses what a web page could send, and anyone without the
        credentials."""
        self._check_origin()
        credentials = self.server.service.credentials
        if credentials is None:
            return
        kind, _, token = self.headers.get("Authorization", "").partition(" ")
        try:
            given = base64.b64decode(token, validate=True)
        except (binascii.Error, ValueError):
            given = b""
        expected = f"{credentials[0]}:{credentials[1]}".encode()
        if kind.lower() != "basic" or not hmac.compare_digest(given, expected):
            raise _Error(401, "this service needs a username and password")

    def _check_origin(self, names: frozenset[str] | None = None) -> None:
        """Refuses what a web page could send. The crawl's metrics endpoint
        does too, and on an address other than a loopback one passes
        ``names``: the host names it may be asked for, besides IP
        addresses."""
        # Scrapyd clients don't send Origin; a page's script or form does.
        if "Origin" in self.headers:
            raise _Error(403, "requests from web pages are refused")
        host = self.headers.get("Host")
        if not host:
            return
        # A page on a name that resolves to this machine (DNS rebinding)
        # sends that name.
        name = host.rsplit(":", 1)[0] if not host.endswith("]") else host
        name = name.strip("[]").lower().rstrip(".")
        if _loopback(self.server.server_address[0]):
            if not _loopback(name):
                raise _Error(403, f"requests for {host} are refused")
        elif names is not None and not (_ip(name) or name in names):
            raise _Error(403, f"requests for {host} are refused: not an address or a known name")

    def _body(self) -> str:
        try:
            length = int(self.headers.get("Content-Length") or 0)
        except ValueError:
            raise _Error(400, "a Content-Length that isn't a number") from None
        if length < 0:
            raise _Error(400, "a negative Content-Length")
        if length > MAX_BODY:
            raise _Error(413, "the request is too big: parameters only")
        return self.rfile.read(length).decode("utf-8", "replace")

    def _send_file(self, path: str) -> None:
        match = _FILE.fullmatch(path)
        if (
            not match
            or match[5] not in {"logs": ("log",), "items": ("jsonl", "jl")}[match[1]]
            or not all(NAME.fullmatch(part) for part in match.groups()[1:4])
        ):
            raise _Error(404, f"no such file: {path}")
        file = self.server.service.file(match[1], match[2], match[3], match[4])
        try:
            f = open(file, "rb")  # noqa: SIM115 - closed below
        except OSError:
            raise _Error(404, f"no such file: {path}") from None
        with f:
            self.send_response(200)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.send_header("Content-Length", str(os.fstat(f.fileno()).st_size))
            self.end_headers()
            # Known limit: a file still being written is sent as long as it
            # was when the request came; the rest is cut off.
            shutil.copyfileobj(_Limited(f, os.fstat(f.fileno()).st_size), self.wfile)

    def _send_json(self, status: int, answer: dict[str, Any]) -> None:
        body = json.dumps(answer).encode()
        self.send_response(status)
        if status == 401:
            self.send_header("WWW-Authenticate", 'Basic realm="netweir"')
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class _Limited:
    """The first `size` bytes of a file, for a Content-Length that holds
    while the job goes on writing."""

    def __init__(self, f: Any, size: int):
        self.f, self.left = f, size

    def read(self, n: int = -1) -> bytes:
        n = self.left if n < 0 else min(n, self.left)
        data = self.f.read(n)
        self.left -= len(data)
        return data


class _Server(ThreadingHTTPServer):
    daemon_threads = True
    service: Service

    def server_bind(self) -> None:
        # http.server's own looks up a name for the address, which nothing
        # reads. Where the resolver doesn't answer for the machine's name
        # (a Mac's .local one), that held up listening on 0.0.0.0 for 35 s.
        socketserver.TCPServer.server_bind(self)
        self.server_name, self.server_port = self.server_address[:2]

    def server_activate(self) -> None:
        super().server_activate()
        self.slots = _Slots(PER_CLIENT, MAX_CONNECTIONS)
        self.watchdog = _Watchdog()

    def server_close(self) -> None:
        super().server_close()
        if hasattr(self, "watchdog"):
            self.watchdog.close()

    def handle_error(self, request: Any, client_address: Any) -> None:
        # A client that went away mid-answer is no error of the server's.
        if isinstance(sys.exc_info()[1], ConnectionError):
            return
        log.exception("answering %s failed", client_address[0])

    def process_request(self, request: Any, client_address: Any) -> None:
        if not self.slots.take(client_address[0]):
            # Too many open already: this one is closed unanswered.
            self.shutdown_request(request)
            return
        super().process_request(request, client_address)

    def process_request_thread(self, request: Any, client_address: Any) -> None:
        try:
            super().process_request_thread(request, client_address)
        finally:
            self.slots.give_back(client_address[0])


class _Watchdog:
    """Runs each action that's still due once its deadline passes, on one
    thread for the whole server. A timer thread per connection would do
    the same, but each one leaves memory behind that the process doesn't
    get back, which a metrics endpoint scraped every 15 seconds for days
    can't afford."""

    def __init__(self) -> None:
        self.due: dict[int, tuple[float, Any]] = {}
        self.ids = itertools.count()
        self.changed = threading.Condition()
        self.closed = False
        threading.Thread(target=self._run, name="netweir-deadlines", daemon=True).start()

    def add(self, seconds: float, action: Any) -> _Due:
        key = next(self.ids)
        with self.changed:
            self.due[key] = (time.monotonic() + seconds, action)
            self.changed.notify()
        return _Due(self, key)

    def cancel(self, key: int) -> None:
        with self.changed:
            self.due.pop(key, None)

    def close(self) -> None:
        with self.changed:
            self.closed = True
            self.changed.notify()

    def _run(self) -> None:
        with self.changed:
            while not self.closed:
                now = time.monotonic()
                for key, (when, action) in list(self.due.items()):
                    if when <= now:
                        del self.due[key]
                        try:
                            action()
                        except Exception:  # noqa: BLE001 - the others still run
                            log.exception("a request deadline's action failed")
                soonest = min((when for when, _ in self.due.values()), default=None)
                self.changed.wait(None if soonest is None else soonest - now)


@dataclasses.dataclass
class _Due:
    watchdog: _Watchdog
    key: int

    def cancel(self) -> None:
        self.watchdog.cancel(self.key)


class _Slots:
    """How many connections each client address has open, capped per
    address and in all."""

    def __init__(self, per_client: int, total: int):
        self.per_client, self.total = per_client, total
        self.open: dict[str, int] = {}
        self.lock = threading.Lock()

    def take(self, client: str) -> bool:
        with self.lock:
            if self.open.get(client, 0) >= self.per_client:
                return False
            if sum(self.open.values()) >= self.total:
                return False
            self.open[client] = self.open.get(client, 0) + 1
            return True

    def give_back(self, client: str) -> None:
        with self.lock:
            self.open[client] -= 1
            if not self.open[client]:
                del self.open[client]


def server(service: Service, bind: str, port: int) -> _Server:
    """The HTTP server for `service`, listening; serve_forever() runs it."""
    cls = _Server
    if ":" in bind:
        cls = type("_Server6", (_Server,), {"address_family": socket.AF_INET6})
    httpd = cls((bind, port), _Handler)
    httpd.service = service
    return httpd


def _print_spiders(request: dict[str, Any]) -> None:
    """In a child process: the spiders each file of request["paths"]
    defines, as JSON on stdout, with what went wrong loading any of them,
    and, for request["check"], what's wrong with a spider's settings."""
    from netweir._cli import _parse_setting, _spiders_in
    from netweir._crawl import Settings

    out, sys.stdout = sys.stdout, sys.stderr  # what the files print isn't the answer
    found: dict[str, list[str]] = {}
    classes: dict[str, list[type]] = {}
    errors = []
    for path in request["paths"]:
        try:
            for cls in _spiders_in(path):
                name = cls.name or cls.__name__
                found.setdefault(name, []).append(path)
                classes.setdefault(name, []).append(cls)
        except BaseException as e:  # noqa: BLE001 - a file can raise anything, SystemExit too
            errors.append(f"{Path(path).name}: {type(e).__name__}: {e}")
    settings_error = None
    if request.get("check") and not errors:
        spider, settings = request["check"]
        if len(classes.get(spider, [])) == 1:
            fields = {f.name: f for f in dataclasses.fields(Settings)}
            try:
                parsed = dict(_parse_setting(text, fields) for text in settings)
                # Over the spider's own, as `netweir crawl -s` does. The
                # paths, the spider's own too, go in a directory thrown
                # away after: making the engine creates some of them. One
                # path named twice stays one, so that mistake is caught.
                own = classes[spider][0].settings
                with tempfile.TemporaryDirectory() as scratch:
                    stand_ins: dict[str, str] = {}
                    for key in PATH_SETTINGS:
                        value = parsed[key] if key in parsed else getattr(own, key)
                        if value is not None:
                            parsed[key] = stand_ins.setdefault(
                                os.path.realpath(value).casefold(),
                                os.path.join(scratch, str(len(stand_ins)), os.path.basename(value)),
                            )
                            os.makedirs(os.path.dirname(parsed[key]), exist_ok=True)
                    built = dataclasses.replace(own, **parsed)
                    # The queue's settings are checked by now; making the
                    # engine with it would connect to Redis.
                    dataclasses.replace(built, queue=None)._engine()
            except Exception as e:  # noqa: BLE001 - reported, whatever it is
                settings_error = str(e)
    json.dump({"spiders": found, "errors": errors, "settings_error": settings_error}, out)


if __name__ == "__main__":
    _print_spiders(json.load(sys.stdin))
