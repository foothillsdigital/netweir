# Netweir for AI agents (MCP)

`netweir mcp` lets an AI agent read the web through Netweir. It's a
[Model Context Protocol](https://modelcontextprotocol.io) server: Claude
Code, Claude Desktop, Cursor and other MCP clients start it, and their
agent can then fetch pages as Chrome would, read them as Markdown, cut them
into chunks, and pull out products, job postings and articles as data.

## Setting it up

The server needs the MCP SDK, which comes with the `mcp` extra:

```
pip install 'netweir[mcp]'
```

On Windows, the SDK needs pywin32, which has no build for free-threaded
Python (3.14t) yet. Install it on the regular Python build there.

Then tell your client to start `netweir mcp`. In Claude Code:

```
claude mcp add netweir -- netweir mcp
```

Claude Desktop and Cursor take the same command in their MCP settings
file:

```json
{
  "mcpServers": {
    "netweir": {"command": "netweir", "args": ["mcp"]}
  }
}
```

If `netweir` isn't on the path the client uses, give the full path to it,
or run it as `python -m netweir mcp` with the right Python.

`--profile` picks the browser the requests look like (`chrome` by
default), `--proxy` sends them through a proxy, and `--timeout` sets how
long each one may take, in seconds (30 by default). `--engine firefox`
loads the pages a tool asks for with `browser=true` in Firefox instead of
Chrome; it can't go with `--proxy` yet.

## The tools

| Tool | Gives the agent |
|---|---|
| `read_page(url, main_only=true, max_chars=20000)` | the page as Markdown: its main content, or the whole page when it has none or `main_only` is false; its final URL, title and page type; cut at `max_chars`, with `truncated` saying so |
| `chunks(url, max_chars=2000, overlap=0)` | the main content cut at its headings, as [`page.chunks()`](selecting.md#chunks-for-retrieval) cuts it |
| `extract(url, type="auto")` | a product, product list, job posting, article, article list, forum thread, business, property, social post, search results page, page content or navigation page as data, with [auto-extraction](extraction.md); `auto` reads the page as whatever it looks like |
| `select(url, css, max_results=50)` | what a CSS selector finds: each element's text, or the strings `::text` and `::attr()` give |
| `links(url, max_results=200)` | the page's links, absolute, with their text, without repeats |

Every tool also takes `browser`. With `browser=true` the page is loaded
in Chrome and read once its scripts have run, for pages that build
themselves in JavaScript. The first such call starts Chrome (install it
with `netweir install chrome`), and later calls reuse it.

A page that can't be fetched, answers with an error status, is a bot
protection page, or isn't a web page (a PDF, an image) comes back as a
tool error saying why, so the agent can try another URL or
`browser=true`. So does `extract` when the page has nothing of the type
asked for. Only `http` and `https` URLs are fetched, with or without
Chrome, so an agent can't be talked into reading `file:` URLs on your
machine. The counts (`max_chars`, `max_results`) must be at least 1.

## What to know

- The server speaks over stdin and stdout only. It doesn't open a port,
  and it only fetches when the agent calls a tool.
- It fetches whatever web page the agent asks for, from your machine and
  your network, the way your browser would, addresses on your local
  network included. Robots.txt isn't checked, since each call is one
  page an agent was asked to read, not a crawl. For a crawl, write a
  spider.
- Each call fetches the page again. Cookies a site sets are kept for the
  rest of the session, as a browser keeps them. If Chrome closes or
  crashes, the next `browser=true` call starts another.
