# Trying a page: fetch, view, extract and shell

A selector that works in your browser's devtools can still find nothing in
a spider. The browser ran the page's JavaScript and Netweir didn't, or the
site sent your browser one page and Netweir another. These three commands
show you the page Netweir gets, so you can write selectors against that,
and `extract` shows what Netweir reads from it without any.

```
netweir fetch https://quotes.toscrape.com/           # the body, on stdout
netweir view https://quotes.toscrape.com/            # opened in your browser
netweir extract https://books.toscrape.com/          # its product, as JSON
netweir shell https://quotes.toscrape.com/           # a Python prompt with it
```

Each one fetches the way `netweir.get` does: the same browser profile on
the wire and the same redirect handling. Give any of them a file instead
of a URL (a path, or a `file:` URL) to look at a page you saved. A host
without a scheme, such as `example.com` or `localhost:8000`, gets
`https://`, even when its path ends like a file, as in
`localhost:8000/index.html`. A bare name that ends like a file, such as
`page.html`, is taken for a file and has to exist. They take
the same options:

| option | what it does |
|---|---|
| `--profile NAME` | the browser to look like, as the `profile` setting (default `chrome`) |
| `--proxy URL` | send the requests through a proxy |
| `--timeout SECONDS` | the most a request may take (default 30) |
| `--header 'Name: value'` | add a request header; give it more than once for more |

A block page isn't treated as an error. You see the page that came back,
and a line on stderr names the vendor that blocked it, so a block never
passes for the real page.

`fetch`, `view` and `extract` exit with status 0 when a page came back, a 404
included. They exit with 1 when the fetch failed or the page was a block,
and with 2 for a mistake on the command line, such as a URL or a header
that can't be sent. `shell` exits with 1 only when its first page can't be
fetched, or when the expression given with `-c` raises. Ctrl-C stops a
command at once with status 130; at the shell's prompt, it stops the
fetch that's running and leaves you at the prompt.

## fetch

`netweir fetch URL` writes the response body to stdout exactly as it
arrived, so this saves the page byte for byte:

```
netweir fetch https://quotes.toscrape.com/ > quotes.html
```

`--headers` writes the status line and the response headers instead, in
the order the server sent them.

## extract

`netweir extract URL` reads the page's product without selectors and
prints it as JSON, the way `page.extract(netweir.auto.Product)` would
(see [Auto-extraction](extraction.md)):

```
netweir extract https://shop.example/p/lamp
netweir extract --type job-posting https://jobs.example/j/1042
netweir extract --type article --field-names zyte https://paper.example/story
```

`--type` is `product`, `product-list`, `product-navigation`,
`job-posting`, `job-posting-navigation`, `article`, `article-list`,
`article-navigation`, `forum-thread`, `business-place`, `real-estate`,
`social-media-post`, `serp` or `page-content`; without it, the
command reads whichever the page looks like (see `page.page_type()`), and a page that looks like
none of them as a product, with a note saying so. The item ends with
`metadata`, whose `sources` say where each field came from: `json-ld`, `microdata:` and the element's
CSS path, `meta:` and the tag, `js:` and the path in the app's state, or
`dom:` and the element. That path is
what you'd pin a field with when one comes out wrong. `--field-names zyte`
writes the common schema's camelCase names (`regularPrice`) instead of
Netweir's snake_case ones. `--browser` reads the page Chrome rendered.

## view

`netweir view URL` saves the page to a temporary file, opens it in your
default browser, and prints the file's path. The file is the page as
text, in UTF-8 with a byte order mark, with two tags put first in its
`<head>`: a charset saying
so, and a `<base href>` naming where the page came from, so its
stylesheets and images still load from the site. The first of each wins,
so the page's own charset and base don't get in the way; the base is the
page's own resolved against its URL, so links point where they did on the
site. What you see is what Netweir got. If an element is missing here, a
selector for it won't find it either.

## Pages built by JavaScript

`fetch`, `view` and `extract` take `--browser`, which loads the page in Chrome and
uses the HTML Chrome ends up with once the page's scripts have run. This
is the page a crawl gets with `browser="always"`. If a selector works with
`--browser` and not without it, the spider needs Chrome for that page. See
[Browser](browser.md).

```
netweir view --browser https://quotes.toscrape.com/js/
```

Chrome sends its own headers, so `--browser` can't be combined with
`--profile` or `--header`. `--headers` gives the status as
`browser 200`, followed by the headers Chrome received.

`--engine firefox` loads the page in the Firefox `netweir install firefox`
installed instead, and needs no `--browser` of its own. Netweir can't send
Firefox through a proxy yet, so `--engine firefox` can't go with `--proxy`.

```
netweir fetch --engine firefox https://quotes.toscrape.com/js/
```

## shell

`netweir shell URL` fetches the page and starts Python with these names
set:

| name | what it is |
|---|---|
| `page` | the page, a `netweir.Page`: `page.css(...)`, `page.xpath(...)`, `page.metadata()` and the rest |
| `fetch(url)` | fetches another page and makes it `page`; takes `method=`, `form=`, `json=`, `body=`, `headers=` and `referer=` |
| `submit(form)` | submits a form read with `page.form(...)` and makes the answer `page` |
| `view(page=None)` | opens `page`, or the page you pass, in your browser |
| `netweir` | the module |

```
$ netweir shell https://quotes.toscrape.com/
>>> page.css("small.author::text").get()
'Albert Einstein'
>>> fetch(page.urljoin(page.css("li.next a::attr(href)").get()))
<Page 200 https://quotes.toscrape.com/page/2/>
>>> page.css("small.author::text").get()
'Marilyn Monroe'
```

All the pages in one shell share a session, so cookies a page sets are
sent with the next fetch. A login works the same way as in a script:

```
>>> fetch("https://quotes.toscrape.com/login")
<Page 200 https://quotes.toscrape.com/login>
>>> submit(page.form(data={"username": "ada", "password": "s3cret"}))
<Page 200 https://quotes.toscrape.com/>
```

IPython is used if it's installed, which gives you completion and history.
Otherwise you get Python's own prompt. Without a URL, the shell starts with
`page` set to `None`.

Given a saved page instead of a URL, as in `netweir shell quotes.html`,
`page` is the file, read as if it had been fetched from its `file:` URL.
Its `version` is `"file"` and it has no headers.

To try one expression and exit, pass it with `-c`. The shell prints the
result as `print()` would, so a string comes out without quotes:

```
netweir shell https://quotes.toscrape.com/ -c 'page.css("small.author::text").getall()'
```

The shell's requests look like a crawl's on the wire, but they skip the
rest of a crawl: no robots.txt, throttling, duplicate filter or cache.
`--browser` isn't available in the shell yet.
