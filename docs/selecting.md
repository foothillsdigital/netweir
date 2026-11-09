# Selecting

A page, and every element in it, answers CSS, XPath and Beautiful Soup
style queries. They return the same kinds of things, so you can mix them.

```python
import netweir

doc = netweir.parse("""
<ul class="books">
  <li class="book"><a href="/a" title="Book A">A</a><span class="price">£1.00</span></li>
  <li class="book sold"><a href="/b" title="Book B">B</a><span class="price">£2.50</span></li>
</ul>
""")
```

`netweir.parse(html)` gives you a document without fetching anything;
`netweir.get(url)` gives you a page you query the same way.

## CSS

```python
doc.css("li.book a::attr(href)").getall()    # ['/a', '/b']
doc.css(".price::text").get()                # '£1.00'
doc.css("li.sold").get()                     # '<li class="book sold">...</li>'
doc.css("a::attr(title), .price::text").getall()
# ['Book A', '£1.00', 'Book B', '£2.50']: each selector keeps its ending,
# and results come back in document order
```

- `::text` gives an element's own text nodes; ` ::text` (with a space)
  gives every text node below it.
- `::attr(name)` gives an attribute's value.
- With neither, `get()` gives the element's HTML, the way Chrome's
  `outerHTML` writes it.
- Form pseudo-classes read the markup as it stands: `:valid` and `:invalid`
  check `required`, `type="email"` and `"url"`, `min`, `max` and `step`, but
  not `pattern`, nor the limits only typing can break (`maxlength`,
  `minlength`). An input with a `pattern` matches as if it had none.

## XPath

All of XPath 1.0, plus the functions Scrapy users rely on:

```python
doc.xpath("//li[span[contains(., '2.50')]]/a/@title").get()   # 'Book B'
doc.xpath("count(//li)").get()                                # '2.0'
doc.xpath("//li[has-class('sold')]/a/text()").get()           # 'B'
doc.xpath("//a[re:test(@href, '^/[ab]$')]/@href").getall()     # ['/a', '/b']
doc.xpath("//li[a/@title = $t]/span/text()", t="Book A").get() # '£1.00'
```

Keyword arguments to `xpath()` bind `$variables`. A query that can't be
parsed raises `netweir.XPathError`; a CSS selector that can't be parsed
raises `netweir.SelectorError`.

XML is queried the same way. `netweir.parse(text, type="xml")` reads it,
a page served as XML is read as XML, and `namespaces=`,
`register_namespace()` and `remove_namespaces()` work as in Scrapy; see
[XML, feeds and CSV](xml.md).

## Selections

`css()` and `xpath()` return a `Selection`, a list of results:

| | Gives |
|---|---|
| `get(default=None)` | the first result as a string, or `default` |
| `getall()` | every result as a string |
| `re(pattern)` | every match of a regular expression in the results (the groups if it has any) |
| `re_first(pattern, default=None)` | the first of those |
| `attrib` | the first element's attributes, as a dict |
| `sel[0]`, `sel[1:3]`, `len(sel)` | indexing and slicing |
| `sel.css(...)`, `sel.xpath(...)` | a query run below every element in it |

Indexing gives a `Node` for an element and a `str` for text or an
attribute's value. `extract()` and `extract_first()` are the older names
for `getall()` and `get()`.

### Queries inside a loop

Write them inline, the way you would with parsel:

```python
for book in doc.css("li.book"):
    yield {
        "title": book.css("a::attr(title)").get(),
        "price": book.css(".price::text").get(),
    }
```

There's nothing to hoist or precompile. Each thread keeps up to 64 CSS
and 64 XPath queries it has compiled, so from the second book on, a query
goes straight to matching. Building a fresh query string every time (an id
pasted into the selector, say) still works; each one is compiled, and once
a thread has 64 (or 8 KB of query text) the cache starts over. A full
cache is about 0.5 MB per thread, times the number of threads that query;
very long selector lists can take it to about 2 MB.

## Beautiful Soup style

The same document answers Beautiful Soup's methods:

```python
import re

doc.find("a")["href"]                           # '/a'
[a["title"] for a in doc.find_all("a")]         # ['Book A', 'Book B']
doc.find("li", class_="sold").find("a").string  # 'B'
doc.find_all(string=re.compile("£"))            # the two prices
doc.find("span").find_parent("li")["class"]     # 'book'
```

`find`, `find_all` and the directional families (`find_parent(s)`,
`find_next_sibling(s)`, `find_previous_sibling(s)`, `find_next`,
`find_all_next`, `find_previous`, `find_all_previous`) take the filters you
know: a string, a list, `True`/`False`, a compiled regex or a function, with
`class_=`, `string=` (or `text=`), `limit=` and `recursive=`. Navigation
(`parent`, `children`, `contents`, `descendants`, siblings,
`next_element`), `get_text()`, `.string`, `select()` and `select_one()`
work as in Beautiful Soup, and so do the editing methods in
[Changing the page](#changing-the-page).

Two differences, both on purpose:

- Attribute values are strings. `node["class"]` is `"book sold"`, not a
  list; filters still match one class out of several.
- `.text` and `get_text()` leave out the contents of `<script>`, `<style>`
  and `<template>`, as Beautiful Soup 4.10 and later do.

## Changing the page

Sometimes the page needs tidying before you read it. The usual case is
throwing away scripts, styles and cookie banners so `get_text()` gives you
only what a reader sees:

```python
for junk in page.find_all(["script", "style", "noscript"]):
    junk.decompose()
for banner in page.css(".cookie-banner"):
    banner.decompose()

article = page.find("article").get_text(" ", strip=True)
```

These work as in Beautiful Soup:

| | Does |
|---|---|
| `node.decompose()` | removes the node and everything in it |
| `node.extract()` | removes it and returns it, to keep or put back elsewhere |
| `node.replace_with(a, "b", ...)` | puts nodes or strings where the node was, and returns the node |
| `node.unwrap()` | replaces the node with its contents (`<b>big</b>` becomes `big`), and returns the empty node |
| `page.new_tag("a", href="/x", string="next")` | a new element, not on the page yet |
| `node.append(x)`, `node.insert(i, x)` | adds a node or a string as the last child, or at position `i` of `contents` |
| `node.string = "text"` | replaces everything in the element with that text |
| `node.prettify()` | the HTML indented a level per tag, one tag or piece of text to a line |

```python
price = page.find("span", class_="price")
price.replace_with(page.new_tag("data", value="12.99", string="£12.99"))

for b in page.find_all("b"):
    b.unwrap()

print(page.find("table").prettify())
```

A node you append or insert that's already on the page moves there. A
string becomes a text node. `new_tag()` takes attributes as a dict, as
keyword arguments, or both, and `string=` for its text. Values are
written as Beautiful Soup writes them: a list joined with spaces
(`class_=["card", "sale"]`), None as an attribute with no value
(`hidden=None`), and anything else as its `str()`, so `True` is `"True"`.
A keyword name loses one trailing `_`, so `class_` is `class` and `for_`
is `for`; names in the `attrs` dict are used as they are.

Every query after an edit sees the edited page: CSS, XPath, the Beautiful
Soup methods and `find_similar()`. A `Selection` or a list from before the
edit still holds the nodes it found, removed ones included.

Edit first, then query. XPath and `find_similar()` read an index of the
page that every edit throws away and the next of them rebuilds, and the
rebuild covers what you've removed as well as the page. A loop that edits
and runs XPath on every turn pays for a whole index each time.

### Removed nodes

A node you've removed, with `decompose()` or any of the others, is off the
page but still readable through any handle you kept. It and everything
in it read as they were when you removed them:

```python
p = page.find("p")
p.decompose()
page.find("p")         # the next <p>, or None
p.html                 # still '<p>...</p>'
p.parent               # None
p.css("a::attr(href)") # queries see only the removed subtree
```

In Beautiful Soup, a decomposed tag shouldn't be used again. Here it's
safe: nothing is freed until the whole page is, so a handle can never
point at memory that's been reused. The cost is that `decompose()` doesn't
give memory back early. It's returned when you drop the page.

A query on a removed node treats it as a small document of its own, as
lxml does, so XPath's `/` and `//` start at the top of what you removed.

### Edits and queries running together

You can query a page from several threads, and edit it from one of them.
An edit waits until queries already running on other threads finish, and
queries that start while it waits let it go first, so nothing ever reads
a half-edited tree. An edit itself takes microseconds.

On free-threaded Python the queries really do run side by side, on one
page or a page each. If you find a node on one thread and read it on
another, Python counts its references the slow way, though, so look nodes
up on the thread that reads them.

What you can't do is edit a page from inside a query: a function passed
to `find_all()` that removes nodes, from that page or any other, raises
`RuntimeError`. (Two threads each editing the other's page from inside a
query would otherwise wait for each other forever.) Collect what you want
first, then edit:

```python
for tag in page.find_all(is_advert):
    tag.decompose()
```

An edit that makes no sense raises `ValueError` and changes nothing:
putting a node inside itself, adding a node from another page (use
`new_tag()` on this one), or an invalid tag or attribute name.

Known limit: you can't set or delete one attribute (`node["href"] =
...`), and there's no `insert_before()`, `insert_after()`, `extend()`,
`wrap()` or `new_string()`. `insert()` and `replace_with()` cover what
they do.

Known limit: `new_tag()` makes HTML elements, with attribute names
lowercased. A `new_tag("circle", viewBox="0 0 10 10")` put inside an
`<svg>` serialises as `<circle viewbox="0 0 10 10">`, which a browser
won't draw as SVG. For scraping, where you read the page rather than
render it, that rarely matters.

## Elements like this one

You've found one product card. `find_similar()` gives you the others:

```python
first = page.css("article.product")[0]
for card in first.find_similar():
    print(card.css("h3 a::text").get(), card.css(".price::text").get())

titles = first.find_similar().css("h3 a::text").getall()
```

It returns a `Selection` of the other elements, in document order, that

- have the same tag;
- sit at the same depth, inside elements with the same tags all the way
  up (an `<li>` in `html > body > main > ul`, say); and
- share at least `threshold` (0.5 by default) of their attribute
  features with the one you started from.

The features are the attribute names and each class on its own. Values
other than classes don't count, because a link or a `data-id` differs from
card to card. The share is what both have over what either has:
`class="card"` and `class="card sale"` share 1 of 2, which is 0.5 and
counts; `class="card"` and `class="card sale new"` share 1 of 3 and don't.
Two elements without attributes match fully.

So a `<div class="card">` in the sidebar isn't counted, since its parents
differ, and neither is a `<div class="banner">` next to your cards. Lower
`threshold` to take in cards with extra classes; `find_similar(0)` gives
every element of the same tag on the same path. The element you started
from isn't in the results.

It runs in Rust over the page's index: about 0.15 ms on a 1 MB listing
and 1.6 ms on a 10 MB one, plus building the index the first time (the
first XPath query builds it too).

## JSON

JSON APIs, and pages that carry their data in a `<script
type="application/json">`, answer [JMESPath](https://jmespath.org)
queries, as in parsel:

```python
page = netweir.get("https://example.com/api/products")
page.jmespath("items[].name")                    # ['Lamp', 'Rug']
page.jmespath("items[?price > `20`].name")       # ['Rug']

page.find("script", id="__NEXT_DATA__").jmespath("props.pageProps.items[0]")
page.css("script[type='application/json']::text").jmespath("total")  # one result per script

netweir.jmespath("props.items[].id", page.js_data("__STATE__"))
```

- `page.jmespath(expr)` reads the body as JSON.
- `node.jmespath(expr)` reads the node's text as JSON.
- `selection.jmespath(expr)` reads each result as JSON (an element's text,
  or the string) and gives a list of what each gives, leaving out the ones
  that give None.
- `netweir.jmespath(expr, data)` searches data you already have, such as
  what `js_data()` returned.

They return plain Python values: dicts, lists, strings, numbers, None.
Text that isn't JSON raises `ValueError`; on a selection it names the result,
such as `result 1 isn't JSON ('var x=1;': ...)`. For JavaScript objects that
aren't strict JSON, read them with `js_data()` first and pass the result
to `netweir.jmespath()`.

JMESPath needs the `jmespath` package, which Netweir doesn't install by
default:

```
pip install 'netweir[jmespath]'
```

Without it, these raise `ImportError` saying so.

## Following an element through a redesign

Give a query a name with `track=`, and Netweir remembers what its element
looked like. When a redesign breaks the query, Netweir finds the most
similar element instead. See [Self-healing](self-healing.md#tracked-selectors).

## Structured data

Many pages describe themselves for search engines and social networks:
a product's name, price and rating in JSON-LD, an article's title and image
in Open Graph tags, reviews marked up as microdata. When it's there, it's
usually the cleanest copy of the data on the page, and `page.metadata()`
reads all of it at once:

```python
meta = page.metadata()

for thing in meta["json_ld"]:  # every <script type="application/ld+json">
    if thing.get("@type") == "Product":
        price = thing["offers"]["price"]

meta["microdata"]   # [{"type": "https://schema.org/Review", "properties": {...}}]
meta["opengraph"]   # {"og:title": "...", "og:image": [...], "product:price:amount": "..."}
meta["twitter"]     # {"twitter:card": "summary_large_image", ...}
meta["dublin_core"] # {"dc.title": "...", ...}
```

JSON-LD comes as written, with a block that's a list giving its items one
by one; a block that isn't valid JSON is skipped. Microdata follows the
HTML standard: nested items, `itemref`, several names in one `itemprop`,
and each element's value where the standard says to find it (`content`,
`href` and `src` made absolute against the page, `datetime`, `value`, or
the text). In the meta tag sections and in microdata, a key given more than
once has a list of its values. A parsed document has it too:
`netweir.parse(html).metadata(base_url)`.


## Reading an article, and Markdown

Most of a news page isn't the news. There's the menu, a cookie banner,
share buttons, an advert halfway down, a box of related stories, a
sidebar of the most read, and a few hundred comments. When what you want
is the story itself, ask for the main content:

```python
page = netweir.get("https://courier.example/news/local/harbour-wall")

page.main_content()              # <Node article>, the element holding the story
page.markdown(main_only=True)    # the story as Markdown
page.clean_text(main_only=True)  # the story as plain text
```

`main_content()` gives the element that holds the page's content: the
article, the blog post, the docs page, the thread, the product's
description. It weighs every block of text on the page: paragraphs of
prose count for the element they're in, links count against it, and
anything inside navigation, sidebars, footers, comments, share and
related boxes, ads or cookie banners counts against whatever holds it.
The element that comes out ahead is the content. The
[design note](design/content.md) has the details.

You get None when nothing stands out: a page with only a few sentences
on it, or a listing, where the text is spread across a grid of cards
(three or more, each with a linked heading or a linked image above a
linked headline). A category page, a search result or a news homepage
has no one main content, and Netweir won't pretend it does. A page that
builds itself in JavaScript sometimes keeps a plain copy for crawlers in
`<noscript>`, as Discourse forums do; when nothing else on the page
stands out, that copy is read, unless it's only a notice asking you to
turn JavaScript on. A shop's grid of products counts as a listing too,
and so does a homepage full of headline cards, even with a stray
paragraph of small print beside them.

With `main_only=True`, the boilerplate inside the content is left out
too: the share buttons under the headline, the related links between
paragraphs, the tags at the end. Without it, you get the whole page (or
element) as it is, minus what nobody sees. Either way, scripts, styles,
form controls, screen-reader-only labels and hidden widgets (a modal, a
cookie banner, a form that pops up) are skipped. What a page folds away
until you click, such as an accordion's panels, a tab or a FAQ's answers,
is still its content and is kept.

### Articles

`page.article()` puts the rest of what a reader wants next to the text:

```python
article = page.article()
article.title      # 'Harbour wall gives way after record tide'
article.byline     # 'Priya Shah'
article.published  # datetime.datetime(2026, 10, 11, 7, 45, tzinfo=...)
article.image      # 'https://courier.example/media/2026/10/harbour-wall.jpg'
article.language   # 'en-GB'
article.text       # the story, a blank line between paragraphs
article.html       # the story's HTML, with the boilerplate taken out
```

Most fields come from the page's structured data first (JSON-LD's
`author` and `datePublished`, Open Graph tags) and from its HTML after
(a `rel="author"` link or a byline, a `<time datetime>` in the content,
`<html lang>`). The title is the exception: the `<h1>` you'd see on the
page comes first, then JSON-LD's `headline`, then `og:title`. A byline
names a person where the page gives one, so an Organization in JSON-LD
(usually the publication) only fills in when nothing else does. The date is read by
`parse_date`, so it's a `datetime`, and None if the page wrote something
Netweir can't read. A field the page doesn't give is None, and so are
`text` and `html` when there's no main content. `html` has no scripts,
event handlers or `javascript:` links, so it's safe to show.

### Markdown

`markdown()` works on a page, a parsed document or any element in one:

```python
netweir.parse(html).markdown("https://example.com/")  # links made absolute
page.css("table.specs")[0].markdown()                 # one table
```

What you get is CommonMark, with GitHub's tables and strikethrough:

- Headings, paragraphs, `*emphasis*`, `**strong**`, `~~struck~~`,
  blockquotes, `---` for `<hr>`, and a backslash at the end of a line for
  `<br>`. Two `<br>`s in a row end the paragraph, as old pages use them.
- Links and images with their titles, made absolute against the page
  (`page.markdown()` does this for you; on a parsed document, pass the
  URL). A `javascript:` link becomes its text, and a link with no text
  is dropped. Lazy-loaded images are found in `data-src` or `srcset`.
- Lists nest (ten deep at most; deeper ones are written at ten), and an
  ordered list keeps its `start`. Two lists in a row get different
  markers (`-` then `*`), or Markdown would merge them.
- `<pre>` becomes a fenced code block, with the language from a
  `language-python` or `lang-py` class, and a fence longer than any run
  of backticks inside. Inline `<code>` is a code span.
- Tables become pipe tables. A cell spanning columns or rows is written
  once and the cells it covers are left empty, since Markdown can't
  merge them. A table inside another, or of a single column, is laid out
  as blocks instead, because that's a page layout rather than data.
- Heading permalinks (`¶`, `#`) and wiki `[edit]` links are left out.
  Emphasis Markdown couldn't open or close where it sits (`a<em>(x)</em>b`)
  is written as `<em>` HTML.
- A non-breaking space stays one in Markdown, and is a plain space in
  `clean_text()`.
- Text that would read as Markdown is escaped (`2\*3`, `\[note\]`,
  `1986\. A good year` at the start of a line), whitespace is collapsed as
  a browser collapses it, and there's never more than one blank line in a
  row outside code.

The same page always gives the same Markdown, so you can diff it between
crawls.

### Clean text

`clean_text()` is what a reader sees, as plain text: a blank line between
paragraphs and headings, one line per list item, a line per table row
with tabs between the cells, and preformatted text left as it was. It's
`get_text()` that knows where the blocks are:

```python
doc = netweir.parse("<h1>Specs</h1><p>Oak,<br>hand finished</p><ul><li>46 cm</li><li>2 kg</li></ul>")
doc.get_text()    # 'SpecsOak,hand finished46 cm2 kg'
doc.clean_text()  # 'Specs\n\nOak,\nhand finished\n\n46 cm\n2 kg'
```

### Chunks for retrieval

A retrieval pipeline wants a page in pieces small enough to embed, each
one still making sense on its own. `page.chunks()` cuts the page's
Markdown along its headings, then packs the blocks under each heading into
chunks of at most `max_chars` characters:

```python
for chunk in page.chunks(1000):
    chunk.id        # 'bd0d75a5b00504ff2a46d322c479820e'
    chunk.headings  # ('Guide', 'Install')
    chunk.text      # '## Install\n\nRun the installer...\n\n```sh\npip install netweir\n```'
    chunk.index     # 1
    chunk.url       # 'https://docs.example/guide'
```

- Every heading starts a new chunk, and `headings` holds the path to it,
  outermost first. A heading with nothing under it but a deeper heading
  shares that heading's chunk.
- A code block or table is never cut while it fits in one chunk. A
  bigger one is cut between lines, and each piece is fenced again or keeps
  the table's header row, so it still renders. A paragraph or list that's
  too long is cut at line breaks first, then at the end of a sentence, then
  between words.
- `max_chars` counts characters, not bytes and not tokens, and defaults
  to 2,000. `overlap=200` starts each chunk after the first in a section
  with up to 200 characters from the end of the one before, cut at a
  space. Overlap never crosses a heading.
- By default only the main content is chunked, so menus and footers stay
  out of your index. A page with no main content, such as a listing, is
  chunked whole. `main_only=False` always takes the whole page.
- `id` is a hash of the page's URL, the headings and the text. The same
  chunk of the same page gets the same id on the next crawl, so your
  store can skip what it already holds, and an edit to one section
  doesn't change the ids of the others. A chunk that appears twice under
  the same headings gets `-2` after its id the second time.

A parsed document or a single element chunks the same way. Pass the
page's URL first, since only a page knows its own; without it, links stay
as written and each chunk's `url` is None:

```python
netweir.parse(html).chunks("https://docs.example/guide", 1000)
page.css("article")[0].chunks(main_only=False)
```

`chunk.to_dict()` gives a dict ready for JSON. `chunk.to_langchain()`
gives a LangChain `Document` and `chunk.to_llamaindex()` a LlamaIndex
`TextNode`, each with the chunk's id, and its URL, headings and index as
metadata. Install `langchain-core` or `llama-index-core` yourself to use
them. Netweir doesn't embed chunks or store them: that's your vector
store's job. In an item, `netweir.chunks()` holds the same list of dicts
(see [declarative spiders](declarative.md)), and `netweir crawl docs
--url https://example.com/docs/ -o docs.jsonl` writes a whole docs site
that way (see [templates](extraction.md#a-whole-site-from-one-url)).

## Prices, dates and data in scripts

Three things turn up on nearly every job, and each is a regular
expression that works on the first site and breaks on the second. Netweir
reads all three in Rust, along with product codes, ratings, stock and
salaries.

### Prices

```python
netweir.parse_price("£1,234.50")       # Price(amount=Decimal('1234.50'), currency='£')
netweir.parse_price("1.234,50 €")      # Price(amount=Decimal('1234.50'), currency='€')
netweir.parse_price("From USD 1,099")  # Price(amount=Decimal('1099'), currency='USD')
netweir.parse_price("Sold out")        # None

netweir.parse_price("£5").code         # 'GBP'
netweir.parse_price("$5").code         # None: US, Canadian, Australian?
```

The amount is a `Decimal`, never a float, so `19.99` stays `19.99`.
`currency` is what the page wrote next to the number (`"£"`, `"zł"`,
`"US$"`, `"EUR"`), and `code` is its ISO 4217 code when it names one
currency only: `$`, `¥` and `kr` don't.

Which separator is the decimal point comes from the text. The last of
`.` and `,` is the decimal point when one or two digits follow it, or
when the other separator comes before it: `12,99` is 12.99 and
`1.234,56` is 1234.56. Three digits after a lone separator make it a
thousands separator, so `1,234` and `1.234` are both 1234, except where
that can't be right (`0.500` is a half). Spaces, thin spaces and
apostrophes (`1'234.50`) group thousands too, and so do Indian lakhs
and crores (`₹1,23,456`). The first number in the text is the price:
`From £10 to £20` is 10.

A few more shapes are read as you'd expect: `$.99` is 0.99; `299,- kr`
and `49,– €` are whole amounts with their currency after the dash; and
full-width digits (`￥１，２３４`) are ordinary digits. A minus sign
makes the amount negative when it's straight before the number or
before a currency straight before it (`-£12.00`, `−5 €`), or between
the currency and the number (`£-5`, `€ -12,50`). Refunds and discounts
export as `"-12.00"`. It has to stand on its own, at the start or after
a space or an opening bracket, so the hyphens in `2-for-$5` and the
`:-` in `MRP:-₹499`, which Indian shops use to label a price, aren't
signs.

### Dates

```python
netweir.parse_date("2026-10-12T09:30:00Z")           # 2026-10-12 09:30:00+00:00
netweir.parse_date("Mon, 12 Oct 2026 09:30:00 GMT")  # 2026-10-12 09:30:00+00:00
netweir.parse_date("October 12th, 2026 at 9:30 PM")  # 2026-10-12 21:30:00
netweir.parse_date("12 de octubre de 2026")          # 2026-10-12 00:00:00
netweir.parse_date("03/04/2026")                     # 4 March
netweir.parse_date("03/04/2026", day_first=True)     # 3 April
netweir.parse_date("3 days ago", now=page_fetched_at)
```

It reads ISO 8601 and RFC 3339, RFC 2822 and HTTP dates, numeric dates
(`12/10/2026`, `12-10-26`, `12.10.2026`, `2026/10/12`), and month names,
full or short, with or without weekdays and ordinals, in English, French,
German, Spanish, Italian, Portuguese and Dutch (`12. Oktober 2026, 09:30
Uhr`, `am 12. 10. 2026 um 9 Uhr`, `lundi 12 octobre 2026 à 9h30`).
Times can be `09:30`, `9:30:15.25`, `9.30pm`, `9 a.m.`, `21h30` or
`21 Uhr`.

A few rules decide the hard cases:

- Numeric dates with slashes or dashes are month first, as US English
  writes them, and ones with dots (`03.04.2026`) are day first, as every
  country that writes dots does. `day_first=True` or `False` decides for
  all of them. A number over 12 can only be the day, so `13/04/2026` is
  13 April either way.
- A two-digit year is 19xx or 20xx, whichever is nearer `now`.
- A zone makes the datetime aware: `Z`, offsets like `+01:00` or
  `-0500`, `UTC+1`, and the names UTC, UT, GMT, EST, EDT, CST, CDT, MST,
  MDT, PST, PDT, AKST, AKDT, HST, WET, WEST, BST, CET, CEST, MET, MEST,
  MEZ, MESZ, EET, EEST, MSK, JST, KST, HKT, SGT, PHT, AWST, ACST, ACDT,
  AEST, AEDT, NZST and NZDT. CST is US Central, as in RFC 2822, not China;
  IST is left out because it could be India, Ireland or Israel. What
  JavaScript's `Date.toString()` writes works too: once there's an offset,
  a trailing name in brackets (`GMT+0100 (Central European Standard
  Time)`) is skipped, in any language, though a single zone name in
  brackets that disagrees with the offset (`-05:00 (CST)`) makes the
  date None. An offset can also stand alone at the end, after a space
  (`12 Oct 2026 +0100`). Without a zone, the datetime is naive.
- Relative phrases are English: `now`, `today`, `yesterday at 9:30`,
  `3 days ago`, `an hour ago`, `in 2 weeks`, `2 hours, 5 minutes ago`.
  They count from `now`, the current time unless you pass one (a `date`
  counts as its midnight), and keep its zone. Pass the time you fetched
  the page and `3 days ago` means what the page meant. `today`,
  `yesterday` and `tomorrow` without a time are midnight, like any date
  without a time.
- In a zone with summer time, hours, minutes and seconds are elapsed
  time, so `1 hour ago` is always an hour before, even across the night
  the clocks change. Days and longer are on the calendar: `1 day ago` is
  the same clock time yesterday.

Anything Netweir doesn't fully understand is None, not a guess: a date
without a year (`12 Oct`), an impossible one (`31/04/2026`), or a date
with words around it (`Posted 12 Oct 2026`; take the date out with `re=`
first).

### Codes, ratings, stock and salaries

Four smaller readers do the same for what product and job pages repeat:

```python
netweir.parse_gtin("EAN: 4006381333931")
# {"type": "gtin13", "value": "4006381333931"}
netweir.parse_rating("4.5 out of 5 (1,204 reviews)")
# {"rating_value": 4.5, "best_rating": 5.0, "review_count": 1204}
netweir.parse_availability("Sold out")          # "OutOfStock"
netweir.parse_salary("£30k–£40k per annum")
# {"raw": "£30k–£40k per annum", "value_min": "30000", "value_max": "40000",
#  "rate_type": "annual", "currency": "GBP", "currency_raw": "£"}
```

`parse_gtin` knows GTIN-8, UPC, EAN, GTIN-14, ISBN-10 and -13 and ISSN,
and checks the check digit, so a mistyped code or a stray number is None
rather than a code. `parse_availability` gives `InStock` or `OutOfStock`
from schema.org's values or plain English, and None when the text doesn't
say. A salary's currency follows the same rule as a price's: `$` alone
names no currency, so `currency` is None and `currency_raw` is `$`.

### Data in scripts

Plenty of pages ship their data as JavaScript: `window.__INITIAL_STATE__
= {products: [...], 'total': 42,}`. `json.loads` gives up on the
unquoted keys, single quotes and trailing comma. `netweir.js_data` reads
it:

```python
netweir.js_data("var s = {name: 'Lamp', tags: ['a', `b`], sold: undefined,};")
# {'name': 'Lamp', 'tags': ['a', 'b'], 'sold': None}

page.js_data("__INITIAL_STATE__")  # the value assigned to that name
page.js_data("__NEXT_DATA__")      # Next.js: the JSON script with that id
page.js_data()                     # every object and array in every script
```

`js_data(text)` gives the first object or array literal in `text`, and
`js_data(text, all=True)` a list of every one. Empty `{}` and `[]` are
skipped: in code they're nearly always a function body or a default. So
is a `[` straight after a name or a bracket (`items[0]`, `f()[1]`),
which is indexing, not an array, unless the name is a keyword that
minified code writes against an array: `return[1,2]`, `of[`, `in[`,
`case[`, `typeof[`, `void[`, `else[`, `do[`, `yield[`, `await[`,
`delete[` and `throw[` all start arrays.
Beyond JSON it reads unquoted and single-quoted keys, single-quoted and
backtick strings (`${...}` kept as written), trailing commas, comments,
`undefined` (None), `NaN`, `Infinity`, hexadecimal numbers, minified
`!0`, `!1` and `void 0`, `new Date(...)` (its argument), and
`JSON.parse('...')` (the string decoded, then read).

It reads data, not code. Nuxt 2's `window.__NUXT__=(function(a,b){return
{...}}(...))`, object shorthand (`{a, b}`), spread (`...rest`),
functions, regular expressions, BigInt (`10n`) and numeric separators
(`1_000`) aren't read: a literal with any of them in it is skipped, though
an object or array nested inside it can still turn up on its own when
you search without a name.

`page.js_data(name)` looks through the page's scripts for `name` being
given a value: `window.name = ...`, `var name = ...`, `name: ...`,
`"name": ...` or `window["name"] = ...`, or a JSON script whose id is
`name`. It gives the first one it finds, or None. Without a name it lists
every object and array in every script, reading a `<script
type="application/json">` whole. On a page full of code that's noisier
than asking for a name: minified bundles hold plenty of small literals
that aren't the data you want. Templates (`<script type="text/template">`)
aren't scripts and are left alone. A parsed document or any element has
it too: `netweir.parse(html).js_data("__STATE__")`.

Script text can be built to hurt a parser, so nesting stops at 400 levels
and the search through a script stays proportional to its length.

### In items

All three are plain functions, so they work as `into=` in an `Item`:

```python
class Product(netweir.Item):
    name = netweir.css("h1::text")
    price = netweir.css(".price::text", into=netweir.parse_price)
    published = netweir.css("time::attr(datetime)", into=netweir.parse_date)
```

A `Price` exports as `{"amount": "1234.50", "currency": "£"}`, the amount
as a string so no digits are lost, and a datetime as ISO 8601.

Markdown and clean text have fields of their own, `netweir.markdown()`
and `netweir.clean_text()`; see [Declarative spiders](declarative.md#items).
For whole products, job postings and articles without selectors, see
[Auto-extraction](extraction.md).
