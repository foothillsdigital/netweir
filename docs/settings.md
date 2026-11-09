# Settings

`netweir.Settings` holds every option a crawl has, each with a default that
is safe to start from. Set them on a spider:

```python
class Books(netweir.Spider):
    settings = netweir.Settings(concurrency=16, max_depth=3)
```

or from the shell with `-s name=value` (`-s max_depth=none` for `None`,
commas between the proxies of `-s proxies=...` and the statuses of
`-s retry_http_codes=...`, and `-s http_auth=user:password`). A misspelt
name is an error, not ignored. Times are in seconds.

A password on the command line can be read by anyone on the machine in
the process list, and stays in your shell's history. Put the login in
`NETWEIR_HTTP_AUTH` (`user:password`) instead, and give the crawl its
domain with `-s http_auth_domain=example.com`. `netweir crawl` reads the
variable only for a crawl that has a domain and no login of its own, so
one you export for one project doesn't break, or log in, the others, and
a spider's own `http_auth` always wins.

## Fetching

| Setting | Default | Does |
|---|---|---|
| `profile` | `"chrome"` | the browser every request looks like: `"chrome"`, `"firefox"` or `"safari"` (the newest of each), or a version such as `"safari-27-macos"` |
| `proxy` | `None` | an `http://`, `https://` or `socks5://` proxy for every request (`socks5h://` to have the proxy look names up); any other scheme is an error |
| `proxies` | `()` | proxies to send requests through, as `proxy_rotation` says |
| `proxy_rotation` | `"on_block"` | how `proxies` are used: `"on_block"` (a site takes the next proxy each time it blocks a session), `"per_request"` (every request takes the next) or `"per_host"` (each site keeps one until a block moves it on); the last two need `proxies` and refuse `proxy`. See [Proxies](self-healing.md#proxies) |
| `proxy_failures` | `3` | failures in a row after which a proxy is benched: no connection to the proxy, a 407 (or a tunnel refused with one), a tunnel it broke off, or a block page. A site that's down, slow or has a bad certificate doesn't count. See [Proxies](self-healing.md#when-a-proxy-stops-working) |
| `proxy_cooldown` | `60.0` | how long a benched proxy sits out; twice that each time after, up to 30 minutes |
| `timeout` | `30.0` | the limit for one whole request; a request's own `timeout` (or Scrapy's `download_timeout` meta key) replaces it |
| `max_redirects` | `20` | redirects a request follows before it fails with `FetchError(kind="too_many_redirects")`, as Scrapy's `REDIRECT_MAX_TIMES`; Chrome's limit is 20 too |
| `verify` | `True` | check each site's certificate. `False` skips the check; the path of a PEM file of CA certificates trusts those in place of the bundled ones, for hosts your own CA vouches for. Neither changes anything a site can see |
| `http_auth` | `None` | a `(user, password)` login sent as HTTP Basic, up front, to `http_auth_domain` and nowhere else, as Scrapy's `HTTPAUTH_USER` and `HTTPAUTH_PASS`. robots.txt is fetched with it too |
| `http_auth_domain` | `None` | the domain `http_auth` goes to, with its subdomains: every request and redirect there gets it, a request anywhere else doesn't, and nor does a redirect from HTTPS to plain HTTP. A new `http://` request there does get it, as in Scrapy, so keep those on HTTPS. A bare host name or IP address, with no scheme, port or path, and not a public suffix such as `com`. Required with `http_auth`; there's no "send it everywhere" |
| `saved_logins` | `()` | where a resumed crawl finds the passwords of its saved requests' own logins, which [checkpoints](self-healing.md#checkpoints) and [shared queues](distributed.md) don't keep: `(user, password)` for a request's `auth=`, `(user, password, domain)` for meta's `http_auth_domain`. It sends nothing to anyone by itself. A saved login found in neither this nor `http_auth` goes without, and the crawl warns once |
| `parse_timeout` | `10.0` | seconds one page may take to parse, wherever it's parsed: in the engine, or when a callback first reads it (a page read as XML is parsed then); past it, `ParseTimeout`. A defence against markup made to be slow. `None` for no limit |
| `xml_recover` | `False` | reads pages parsed as XML in recover mode: undeclared entities, a stray `&`, characters XML doesn't allow and the like are repaired, with a warning saying what, rather than raising `XMLError` where the page is read. See [Broken and hostile XML](xml.md#broken-and-hostile-xml) |
| `max_response_size` | `67108864` | the most bytes of a response body (64 MiB), counted after decompression, so a small compressed answer that unpacks to gigabytes is stopped too; a larger one fails with `FetchError(kind="too_large")`, and `None` reads any size |

## Pace and politeness

| Setting | Default | Does |
|---|---|---|
| `concurrency` | `64` | requests in flight across all sites |
| `per_domain` | `8` | requests in flight to one site |
| `obey_robots` | `True` | read robots.txt and stay out of what it disallows |
| `robots_agent` | `"netweir"` | the name matched against robots.txt's User-agent lines |
| `obey_tdmrep` | `True` | skip pages whose owners reserve text and data mining rights |
| `throttle` | `True` | adapt each site's delay to how fast it answers |
| `start_delay` | `1.0` | each site's delay before its first answer is timed |
| `min_delay` | `0.0` | the shortest delay the throttle sets |
| `max_delay` | `60.0` | the longest delay, from the throttle, blocks or Crawl-delay |
| `target_concurrency` | `1.0` | requests the throttle aims to have in flight to one site |

## Limits

| Setting | Default | Does |
|---|---|---|
| `max_depth` | `None` | links from a start page beyond which requests are dropped |
| `max_pages_per_domain` | `None` | requests accepted for one site beyond which more are dropped |
| `max_items` | `None` | stop the crawl once it has delivered this many items |
| `max_pages` | `None` | send no more than this many requests, a retry counting as one, then stop |
| `max_errors` | `None` | stop the crawl after this many errors in callbacks and pipelines |
| `max_time` | `None` | stop the crawl after this many seconds |
| `max_memory` | `None` | stop the crawl once it has used this many MiB of memory at its peak, worker processes included |
| `memory_warning` | `None` | log a warning, once, when the crawl has used this many MiB of memory |

## Recovery

| Setting | Default | Does |
|---|---|---|
| `retries` | `3` | further tries after a status in `retry_http_codes`, a network error or a block |
| `retry_http_codes` | `(408, 429, 500, 502, 503, 504, 522, 524)` | the statuses tried again, as Scrapy's `RETRY_HTTP_CODES` (and the same list). A 429 left out still slows its site down, but goes to your callback. A POST is only tried again after one that says it wasn't processed, unless the request has `retry_post=True` |
| `backoff_base` | `1.0` | retry n waits a random time up to `backoff_base` × 2ⁿ |
| `backoff_max` | `60.0` | the most any retry waits |
| `breaker_window` | `50` | how many of a site's last responses the circuit breaker looks at |
| `breaker_ratio` | `0.3` | the share of those that were blocks above which the site pauses |
| `breaker_pause` | `300.0` | how long the site pauses |

## Browser

| Setting | Default | Does |
|---|---|---|
| `browser` | `"off"` | which requests go through Chrome: `"off"` (those with `browser=True`), `"on_block"` (also any still blocked after its retries) or `"always"` |
| `browser_engine` | `"chrome"` | which browser those requests are fetched in: `"chrome"`, `"firefox"` (the one `netweir install firefox` installed), or `"match"`, which is Firefox when `profile` is a Firefox one and Chrome otherwise, so a site sees the browser the crawl's requests look like. Firefox can't go through the crawl's proxy yet, so a crawl with `proxy` or `proxies` refuses it. See [Browser](browser.md#in-a-crawl) |
| `browser_pages` | `4` | Chrome pages open at once |
| `browser_block` | `()` | resource types the crawl's Chrome pages never fetch, such as `("image", "font", "media")`; Chrome fails them before they leave it. Any of `stylesheet`, `image`, `media`, `font`, `script`, `texttrack`, `xhr`, `fetch` (which blocks `xhr` too, and the other way round), `prefetch`, `eventsource`, `websocket`, `manifest`, `signedexchange`, `ping`, `cspviolationreport`, `preflight`, `other`; see [Browser](browser.md#blocking-and-changing-requests) |
| `browser_worker_header_order` | `False` | sends each worker script request of the crawl's Chrome pages with its headers in Chrome's own order, which DevTools' user agent override otherwise changes; it pauses every request those pages make, which made a page with 300 subresources load in 118 ms where it took 93, and one with frames and workers in 292 where it took 200. See [A page's environment](environment.md#what-it-cant-hide) |
| `browser_steps` | `()` | what every page the crawl fetches in Chrome does once it has loaded, before its callback sees it: a list of steps such as `[steps.click("#accept", timeout=3, on_error="continue")]`. A request's own `steps` replace them. Call steps here name spider methods, as strings. See [Steps](steps.md#steps-for-every-page) |
| `browser_steps_max` | `120.0` | the most all of one page's steps may take, in seconds, or `None` for no limit; steps that ask outright for more are refused where they're written, and steps that reach it while running fail the request. See [Steps](steps.md#time-limits) |
| `browser_upload_dir` | `None` | the only folder a `set_input_files` step in a request read back from data (the shared queue, or a checkpoint) may upload from; `None` refuses such uploads. With a shared queue every request is read back from it, so any upload needs this. See [Steps](steps.md#steps-from-data) |
| `browser_environment` | `None` | what the crawl's Chrome pages are shown in and say about themselves, as a dict of `new_context()`'s keyword arguments: `{"device": "Galaxy S23"}`, `{"locale": "de-DE", "timezone_id": "Europe/Berlin"}`. Cookies Chrome hands back go with the same language, user agent and client hints; see [A page's environment](environment.md#in-a-crawl) |

## State

| Setting | Default | Does |
|---|---|---|
| `checkpoint` | `None` | a directory to keep the crawl's state in, so it resumes after a crash. It holds the logins requests were given; on Linux and macOS its files are readable by their owner alone |
| `cache` | `None` | a directory to keep responses in, so a rerun gets its pages from there rather than the site; see [Crawling](crawling.md#caching-while-you-develop) |
| `cache_expiry` | `None` | seconds after which a cached response is fetched again; `None` keeps it for good |
| `deltafetch` | `None` | a directory to remember, across runs, which requests' pages gave items, so later runs skip them; output files are added to, run after run. See [Crawling](crawling.md#skipping-what-you-already-have) |
| `deltafetch_reset` | `False` | forget every page `deltafetch` remembered, at the start of this run, and start the output files afresh |
| `warc` | `None` | a file to keep every request the crawl sent and every response it got in, as WARC: a name ending in `.warc.gz` or `.warc`; see [Crawling](crawling.md#keeping-what-you-fetched) |
| `har` | `None` | the same, as a HAR file to open in a browser's network panel |
| `track_threshold` | `0.75` | how similar an element must be to count as a tracked selector's element |
| `fail_fast` | `False` | stop the crawl at the first exception in a callback or pipeline |
| `workers` | `1` | processes to run callbacks in; see [Crawling](crawling.md#callbacks-in-worker-processes) |

## Watching

| Setting | Default | Does |
|---|---|---|
| `stats_interval` | `60.0` | seconds between progress lines in the log (pages and items so far and per minute, errors, the queue, bytes); `None` for none. See [Crawling](crawling.md#watching-a-crawl-as-it-runs) |
| `metrics_port` | `None` | a port to serve the crawl's stats on while it runs, for Prometheus at `/metrics` and as JSON at `/stats.json`; `0` takes a free one and logs it. `None` for no endpoint. A port that's taken stops the crawl before it starts |
| `metrics_host` | `"127.0.0.1"` | the address the endpoint listens on; anything other than a loopback address lets other machines read the stats, with no password, and logs a warning saying so |
| `metrics_allowed_hosts` | `()` | with `metrics_host` other than a loopback address, the host names the endpoint may be asked for; a request naming any other (an IP address is always fine) is refused, so a web page can't reach it through DNS rebinding |

## Sharing a crawl

| Setting | Default | Does |
|---|---|---|
| `queue` | `None` | a Redis URL (`redis://host:6379/0`) whose queue and duplicate filter the crawl shares with its other processes; see [One crawl, many machines](distributed.md). Refuses `checkpoint` and `deltafetch` |
| `queue_key` | `None` | the crawl's name in Redis; `None` uses the spider's `name`, or its class name |
| `queue_lease` | `60.0` | seconds after a process's last heartbeat before the requests it held go back to the queue |
| `queue_idle` | `30.0` | seconds the queue must stay empty, every process idle, before a process ends; `None` waits for more until stopped |
