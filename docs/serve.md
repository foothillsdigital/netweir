# Running spiders as a service

Once a spider runs every night, or every hour, or whenever someone clicks
a button, `netweir crawl` in a cron job starts to creak. Who's running
what right now? Where did last Tuesday's log go? How do you stop the one
that's been going for six hours without killing the other two?

`netweir serve` answers those. It's a small HTTP service that runs your
spiders as jobs: you schedule one with a POST, it waits in a queue until
there's room, runs in a process of its own, and its log, items and stats
stay there for you to fetch afterwards. If you've used scrapyd, it's the
same idea, and it speaks the same API where it can, so `curl` lines and
dashboards written for scrapyd mostly work unchanged.

```
netweir serve --project shop=spiders/
```

```
curl http://127.0.0.1:6800/schedule.json -d project=shop -d spider=books
{"node_name": "box", "status": "ok", "jobid": "6487ec79947edab326d6db28a2d86511"}
```

That job is `netweir crawl spiders/books.py --spider books` running in the
background, with its items going to a JSON Lines file the service keeps.

## Projects

A project is a spider file, or a directory of them, named on the command
line:

```
netweir serve --project shop=spiders/ --project news=news/feeds.py
```

`netweir startproject` makes a project in this shape: serve its
`spiders/` directory ([Projects](project.md)).

In a directory, every `.py` file directly in it is read; subdirectories
aren't, so helpers can live there. A spider's name is its `name`
attribute, or its class name when it has none, the same name
`netweir crawl --spider` takes. Files are read in a separate process each
time you list or schedule, so an edited spider is picked up at once, and
a file that crashes on import gives you an error naming it rather than
taking the service down. Jobs run with the project's directory as their
working directory, so spider files can import each other.

There's no deploying code over HTTP: scrapyd's `addversion.json` isn't
supported, and neither are versions or eggs. `listversions.json` is there,
and always answers with no versions, so a client that asks before it
schedules carries on. Put the files on the machine
the way you put the rest of your code there, and name them with
`--project`. For the same reason, `delproject.json` answers with an
error; take the `--project` off and restart instead.

## Options

| option | what it does |
|---|---|
| `--project NAME=PATH` | serve a spider file or directory as project NAME; give it once per project. A bare `PATH` is named after the file (without its extension) or the directory, so `.` takes the current directory's name |
| `--bind ADDRESS` | the address to listen on (default `127.0.0.1`, this machine only) |
| `--port N` | the port (default `6800`, scrapyd's) |
| `--dir DIR` | where the job table, logs, items and stats go (default `netweir-serve`) |
| `--max-jobs N` | how many jobs run at once (default: one per CPU) |
| `--keep N` | how many finished jobs to remember (default 100) |
| `--username U`, `--password P` | require HTTP basic auth; better read from `NETWEIR_SERVE_USERNAME` and `NETWEIR_SERVE_PASSWORD` (see below) |
| `--insecure-no-auth` | let `--bind` take an address other machines can reach without a username and password (see below) |

scrapyd runs four jobs per CPU by default. A Netweir crawl already fetches
and parses on several threads, so one per CPU keeps a machine busy
without making jobs fight for it.

Past `--keep` finished jobs, the oldest is forgotten, and its log, items
and stats files are deleted with it. Copy out anything you want to keep
longer, or raise the number.

One service owns a `--dir` at a time. Start a second on the same
directory and it stops with an error, before it touches anything. The
lock behind that is a file lock, which two machines sharing the
directory over NFS may not both see, so don't point two machines at one
`--dir`.

## Scheduling a job

```
curl http://127.0.0.1:6800/schedule.json \
    -d project=shop -d spider=books \
    -d setting=max_items=500 -d setting=concurrency=8 \
    -d category=fiction
```

- `setting=KEY=VALUE` is any Netweir [setting](settings.md), exactly as
  `-s KEY=VALUE` takes it. A setting that doesn't exist, or a value of the
  wrong type, is an error now, not a failed job in an hour. So is one
  the crawl would refuse as it started, such as `concurrency=0`,
  `min_delay` above `max_delay`, or a `profile` that doesn't exist.
  They're checked on top of the spider's own settings, as `-s` applies
  them, so `proxy_rotation=per_request` is fine for a spider that sets
  `proxies` and refused for one that doesn't. Scrapy's setting names
  (`DOWNLOAD_DELAY` and the rest) aren't translated.
- Any other parameter becomes an attribute of the spider, as a string:
  above, `self.category` is `"fiction"`. That's `netweir crawl -a
  category=fiction`, and it's what Scrapy does with spider arguments.
  A name has to be a Python identifier that doesn't start with `_`, and
  can't be anything `netweir.Spider` already has (`settings`,
  `pipelines`, `allowed_domains`, `parse` and the rest): an argument
  gives the spider data, not a new way to behave.
- `checkpoint`, `cache`, `deltafetch`, `warc` and `har` name a file or
  directory, so a job can't put one just anywhere. Each is a relative
  path inside the project's own data directory, `DIR/data/PROJECT`:
  `-d setting=checkpoint=books` keeps the checkpoint in
  `DIR/data/shop/books`, and every job of the project that names `books`
  shares it, so a cancelled crawl scheduled again carries on. The same
  goes for `deltafetch=seen`: each run skips what the last one found. An absolute path, `..`, a URL such as `s3://`, or a symlink in there
  that points outside is refused, with an error saying which.
- `jobid=NAME` names the job yourself; otherwise it gets a random hex id.
  Job ids, like project names, ignore case: `Job1` and `job1` are the
  same job, as they'd be the same file on macOS or Windows.
- `priority=N` puts the job ahead of waiting jobs with a lower number.

When a job ends, its outcome is `finished` (the crawl exited with status
0), `failed` (it didn't: an exception, or a [check](crawling.md#checks-at-the-end)
that failed), `cancelled`, or `lost`.

## Watching and fetching

```
curl http://127.0.0.1:6800/listjobs.json?project=shop
curl http://127.0.0.1:6800/jobstats.json?job=6487ec79947edab326d6db28a2d86511
curl http://127.0.0.1:6800/items/shop/books/6487ec79947edab326d6db28a2d86511.jl
curl http://127.0.0.1:6800/logs/shop/books/6487ec79947edab326d6db28a2d86511.log
```

`listjobs.json` gives `pending`, `running` and `finished` lists, each job
with its `log_url` and `items_url`, and its `outcome` once it's finished.
Items URLs end in `.jl`, as scrapyd's do; the same file is there as
`.jsonl` too.
`jobstats.json` is Netweir's own: the job's crawl stats, the same numbers
`netweir crawl` logs at the end, with `finish_reason` among them. While a
job runs you get its stats so far, at most 5 seconds old, with
`finish_reason` null; in its first 5 seconds, none yet. A job killed
before it could finish keeps the last stats it wrote.

Logs and items can be fetched while the job is still going; you get as
much as had been written when you asked.

## Cancelling

```
curl http://127.0.0.1:6800/cancel.json -d project=shop -d job=6487ec79947edab326d6db28a2d86511
```

A waiting job is taken out of the queue. A running one stops the way
Ctrl-C stops `netweir crawl`: callbacks stop, the items written so far
are closed off cleanly in the items file, and a
[checkpoint](self-healing.md#checkpoints), if the job has one, records
where it got to, so scheduling it again with the same `checkpoint`
setting carries on from there. A job that hasn't stopped 30 seconds later is killed,
along with any process it started, such as its workers; cancel it a
second time to kill it at once.

Stopping the service with Ctrl-C, or with a service manager's stop, cancels
its running jobs the same way and waits for them. Waiting jobs stay in the
queue for the next start.

## After a restart

Jobs are kept in `DIR/jobs.db`, so a restart forgets nothing: finished
jobs are still listed with their logs and items, and waiting jobs start
when there's room. A job that was running when the service died (killed
outright, or the machine went down) is marked `lost`, so it never just
vanishes from the list.

## Endpoints

| endpoint | method | parameters |
|---|---|---|
| `daemonstatus.json` | GET | |
| `listprojects.json` | GET | |
| `listspiders.json` | GET | `project` |
| `schedule.json` | POST | `project`, `spider`, `setting`, `jobid`, `priority`, spider arguments |
| `cancel.json` | POST | `project`, `job` |
| `listjobs.json` | GET | `project` (optional) |
| `jobstats.json` | GET | `job` |
| `listversions.json` | GET | `project` (always no versions) |
| `delproject.json` | POST | `project` (always an error) |
| `/logs/PROJECT/SPIDER/JOB.log` | GET | |
| `/items/PROJECT/SPIDER/JOB.jl` or `.jsonl` | GET | |

Answers are JSON with `node_name` and `status`, `"ok"` or `"error"` with
a `message`, as scrapyd's are. Parameters can go in the query string or a
form-encoded body. Project, spider and job names are letters, digits, `.`,
`_` and `-`, starting with a letter or digit.

## Keeping it private

Out of the box the service listens on 127.0.0.1, so only programs on the
same machine can reach it. Anyone who can reach it can run your spiders
with any settings they like, so before you `--bind 0.0.0.0` set a
username and password:

```
NETWEIR_SERVE_USERNAME=me NETWEIR_SERVE_PASSWORD=... netweir serve --bind 0.0.0.0 --project shop=spiders/
curl -u me:... http://box:6800/listjobs.json
```

Give them as environment variables, as above, rather than as
`--username` and `--password`: a command line can be read by every user
of the machine, in `ps` or `/proc`, for as long as the service runs.

Bind to another address without them and the service won't start
unless you also give `--insecure-no-auth`. Think hard before you do. It
isn't only people on your network: any web page open in a browser that
can reach the port can read `listjobs.json`, the logs and the items,
by pointing a host name of its own at your address (DNS rebinding), and
the service can't tell. It starts with a warning saying so. Basic auth
sends the password with every
request, readable by anyone on the path, so outside a network you trust
put the service behind a proxy that speaks HTTPS.

The service also refuses requests that only a web page would send: any
with an `Origin` header, and, while it listens on 127.0.0.1, any addressed
to a host name other than `localhost` or a loopback address. A page you
happen to have open can't schedule jobs on your machine.

A connection has 15 seconds from connecting to send its whole request,
or it's closed. One address can have 8 connections open at once and
everyone together 256; past either, new ones are closed unanswered. So a
client that opens connections and sends slowly holds at most 8 of the
service's threads, for 15 seconds at a time; one with hundreds of
addresses can still fill all 256, which is a reason to keep the port
private.

## What it doesn't do yet

- If the service is killed outright, with `kill -9` say, its running jobs
  aren't told. They carry on to their end unwatched, and are marked `lost`
  at the next start even if they finish.
- On Windows, a job cancelled just as one of its workers is starting can
  end with that worker dead rather than as `interrupted`. What it had
  written is kept either way.
- No `addversion.json` or `delversion.json`.
