import os
import re
from collections.abc import Awaitable, Callable, Iterator, Mapping, Sequence
from pathlib import Path
from typing import Any, Literal, TypedDict, TypeVar, overload

from netweir._errors import FetchError
from netweir._helpers import Article, Chunk, Records
from netweir._network import Expectation

_D = TypeVar("_D")

__version__: str

class SelectorError(ValueError): ...
class XPathError(ValueError): ...
class ParseTimeout(TimeoutError): ...

class XMLError(ValueError):
    line: int
    column: int
    reason: str

Filter = (
    str
    | list[str]
    | tuple[str, ...]
    | set[str]
    | bool
    | re.Pattern[str]
    | Callable[..., object]
    | None
)

class Node:
    # parsel-style queries
    def css(self, query: str, track: str | None = None) -> Selection: ...
    def xpath(
        self,
        query: str,
        track: str | None = None,
        namespaces: Mapping[str, str] | None = None,
        **variables: str | int | float | bool,
    ) -> Selection: ...
    def register_namespace(self, prefix: str, uri: str) -> None: ...
    def remove_namespaces(self) -> None: ...
    @property
    def type(self) -> Literal["html", "xml"]: ...
    def tracked(
        self,
        kind: Literal["css", "xpath"],
        query: str,
        name: str,
        store: TrackStore,
        site: str,
        threshold: float,
    ) -> Selection: ...
    # Beautiful Soup style search
    def find(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        recursive: bool = True,
        string: Filter = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> Node | None: ...
    def find_all(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        recursive: bool = True,
        string: Filter = None,
        limit: int | None = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> list[Node]: ...
    def find_parent(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> Node | None: ...
    def find_parents(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        limit: int | None = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> list[Node]: ...
    def find_next_sibling(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> Node | None: ...
    def find_next_siblings(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        limit: int | None = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> list[Node]: ...
    def find_previous_sibling(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> Node | None: ...
    def find_previous_siblings(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        limit: int | None = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> list[Node]: ...
    def find_next(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> Node | None: ...
    def find_all_next(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        limit: int | None = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> list[Node]: ...
    def find_previous(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> Node | None: ...
    def find_all_previous(
        self,
        name: Filter = None,
        attrs: dict[str, Filter] | Filter = None,
        string: Filter = None,
        limit: int | None = None,
        class_: Filter = None,
        **kwargs: Filter,
    ) -> list[Node]: ...
    def select(self, css: str) -> list[Node]: ...
    def select_one(self, css: str) -> Node | None: ...
    # what the node is
    @property
    def kind(
        self,
    ) -> Literal[
        "element", "text", "comment", "processing_instruction", "document", "doctype", "other"
    ]: ...
    @property
    def tag(self) -> str | None: ...
    @property
    def name(self) -> str | None: ...
    @property
    def text(self) -> str: ...
    @property
    def html(self) -> str: ...
    @property
    def string(self) -> str | None: ...
    @string.setter
    def string(self, text: str) -> None: ...
    def metadata(self, base_url: str | None = None) -> dict[str, Any]: ...
    def js_data(self, name: str | None = None) -> Any: ...
    def get_text(self, separator: str = "", strip: bool = False) -> str: ...
    def css_path(self) -> str | None: ...
    def main_content(self) -> Node | None: ...
    @overload
    def markdown(self, base_url: str | None = None, main_only: Literal[False] = False) -> str: ...
    @overload
    def markdown(self, base_url: str | None, main_only: bool) -> str | None: ...
    @overload
    def clean_text(self, main_only: Literal[False] = False) -> str: ...
    @overload
    def clean_text(self, main_only: bool) -> str | None: ...
    def chunks(
        self,
        base_url: str | None = None,
        max_chars: int = 2000,
        *,
        overlap: int = 0,
        main_only: bool = True,
    ) -> list[Chunk]: ...
    def article(self, base_url: str | None = None) -> Article: ...
    def records(self, url: str | None = None) -> Records | None: ...
    def page_type(
        self, url: str | None = None
    ) -> Literal[
        "product",
        "product-list",
        "job-posting",
        "job-posting-navigation",
        "article",
        "article-list",
        "forum-thread",
        "business-place",
        "real-estate",
        "social-media-post",
        "serp",
        "other",
    ]: ...
    # attributes
    @property
    def attrs(self) -> dict[str, str]: ...
    def attr(self, name: str) -> str | None: ...
    @overload
    def get(self, name: str) -> str | None: ...
    @overload
    def get(self, name: str, default: object) -> object: ...
    def __getitem__(self, name: str) -> str: ...
    # navigation
    @property
    def parent(self) -> Node | None: ...
    @property
    def parents(self) -> list[Node]: ...
    @property
    def children(self) -> list[Node]: ...
    @property
    def contents(self) -> list[Node]: ...
    @property
    def descendants(self) -> list[Node]: ...
    @property
    def next_sibling(self) -> Node | None: ...
    @property
    def previous_sibling(self) -> Node | None: ...
    @property
    def next_siblings(self) -> list[Node]: ...
    @property
    def previous_siblings(self) -> list[Node]: ...
    @property
    def next_element(self) -> Node | None: ...
    @property
    def previous_element(self) -> Node | None: ...
    # elements like this one, and JSON in it
    def find_similar(self, threshold: float = 0.5) -> Selection: ...
    def jmespath(self, expression: str) -> Any: ...
    # editing
    def decompose(self) -> None: ...
    def extract(self) -> Node: ...
    def replace_with(self, *items: Node | str) -> Node: ...
    def unwrap(self) -> Node: ...
    def append(self, child: Node | str) -> None: ...
    def insert(self, position: int, child: Node | str) -> None: ...
    def new_tag(
        self,
        name: str,
        attrs: dict[str, object] | None = None,
        string: str | None = None,
        **kwattrs: object,
    ) -> Node: ...
    def prettify(self) -> str: ...
    def __eq__(self, other: object) -> bool: ...
    def __hash__(self) -> int: ...

class Selection:
    def register_namespace(self, prefix: str, uri: str) -> None: ...
    def remove_namespaces(self) -> None: ...
    @property
    def relocated(self) -> bool: ...
    @property
    def score(self) -> float | None: ...
    @overload
    def get(self) -> str | None: ...
    @overload
    def get(self, default: _D) -> str | _D: ...
    def getall(self) -> list[str]: ...
    def extract(self) -> list[str]: ...
    @overload
    def extract_first(self) -> str | None: ...
    @overload
    def extract_first(self, default: _D) -> str | _D: ...
    def css(self, query: str) -> Selection: ...
    def xpath(
        self,
        query: str,
        namespaces: Mapping[str, str] | None = None,
        **variables: str | int | float | bool,
    ) -> Selection: ...
    def re(self, pattern: str | re.Pattern[str], replace_entities: bool = True) -> list[str]: ...
    @overload
    def re_first(
        self, pattern: str | re.Pattern[str], *, replace_entities: bool = True
    ) -> str | None: ...
    @overload
    def re_first(
        self, pattern: str | re.Pattern[str], default: _D, replace_entities: bool = True
    ) -> str | _D: ...
    @property
    def attrib(self) -> dict[str, str]: ...
    def jmespath(self, expression: str) -> list[Any]: ...
    def __len__(self) -> int: ...
    @overload
    def __getitem__(self, index: int) -> Node | str: ...
    @overload
    def __getitem__(self, index: slice) -> Selection: ...
    def __iter__(self) -> Iterator[Node | str]: ...

def _auth_domain(domain: str) -> str: ...
def _response(
    url: str,
    status: int,
    version: str,
    headers: list[tuple[str, str]],
    body: bytes,
    steps: str | None = None,
) -> Response: ...
def _check_steps(json: str, cap: float | None = None) -> None: ...

class Response:
    @property
    def url(self) -> str: ...
    @property
    def status(self) -> int: ...
    @property
    def version(self) -> str: ...
    @property
    def headers(self) -> list[tuple[str, str]]: ...
    @property
    def body(self) -> bytes: ...
    @property
    def steps(self) -> list[dict[str, Any]]: ...
    def text(self) -> str: ...
    def parse(
        self,
        timeout: float | None = None,
        type: Literal["html", "xml"] | None = None,
        recover: bool = False,
    ) -> Node: ...
    def _parse_quietly(
        self,
        timeout: float | None = None,
        type: Literal["html", "xml"] | None = None,
        recover: bool = False,
    ) -> tuple[Node, str | None]: ...
    @property
    def markup(self) -> Literal["html", "xml"]: ...
    @property
    def encoding(self) -> str: ...
    def classify(
        self,
    ) -> tuple[
        Literal["ok", "blocked", "throttled", "payment_required", "http_error"],
        str | None,
        Literal["challenge", "captcha", "block", "rate_limit"] | None,
        float | None,
        str | None,
    ]: ...

class Fetcher:
    def __init__(
        self,
        profile: str = "chrome",
        proxy: str | None = None,
        timeout: float = 30.0,
        verify: bool = True,
        max_size: int | None = None,
        cookies: bool = True,
        system_proxy: bool = True,
        ca_certificates: bytes | None = None,
        max_redirects: int = 20,
    ) -> None: ...
    def cookies(self) -> list[dict[str, Any]]: ...
    def find_cookies(
        self, name: str, domain: str | None = None, path: str | None = None
    ) -> list[dict[str, Any]]: ...
    def add_cookies(self, cookies: list[dict[str, Any]]) -> None: ...
    def remove_cookies(
        self, name: str, domain: str | None = None, path: str | None = None
    ) -> int: ...
    def clear_cookies(self, domain: str | None = None) -> int: ...
    def storage_state(self, path: str | os.PathLike[str] | None = None) -> dict[str, Any]: ...
    def load_storage_state(self, source: str | os.PathLike[str] | dict[str, Any]) -> list[str]: ...
    async def get(self, url: str, headers: list[tuple[str, str]] | None = None) -> Response: ...
    def get_blocking(self, url: str, headers: list[tuple[str, str]] | None = None) -> Response: ...
    async def send(
        self,
        url: str,
        method: str = "GET",
        kind: Literal["navigate", "form", "fetch"] = "navigate",
        body: bytes | None = None,
        content_type: str | None = None,
        referer: str | None = None,
        headers: list[tuple[str, str]] | None = None,
        follow: bool = True,
        chain: list[tuple[int, str]] | None = None,
        timeout: float | None = None,
        max_redirects: int | None = None,
        auth: tuple[str, str, str | None] | None = None,
    ) -> Response: ...
    def send_blocking(
        self,
        url: str,
        method: str = "GET",
        kind: Literal["navigate", "form", "fetch"] = "navigate",
        body: bytes | None = None,
        content_type: str | None = None,
        referer: str | None = None,
        headers: list[tuple[str, str]] | None = None,
        follow: bool = True,
        timeout: float | None = None,
        max_redirects: int | None = None,
        auth: tuple[str, str, str | None] | None = None,
    ) -> Response: ...

class Crawler:
    def __init__(
        self,
        profile: str = "chrome",
        proxy: str | None = None,
        timeout: float = 30.0,
        verify: bool = True,
        concurrency: int = 64,
        per_domain: int = 8,
        obey_robots: bool = True,
        robots_agent: str = "netweir",
        obey_tdmrep: bool = True,
        throttle: bool = True,
        start_delay: float = 1.0,
        min_delay: float = 0.0,
        max_delay: float = 60.0,
        target_concurrency: float = 1.0,
        max_depth: int | None = None,
        max_pages_per_domain: int | None = None,
        max_pages: int | None = None,
        retries: int = 3,
        backoff_base: float = 1.0,
        backoff_max: float = 60.0,
        proxies: list[str] | None = None,
        proxy_rotation: Literal["on_block", "per_request", "per_host"] = "on_block",
        proxy_failures: int = 3,
        proxy_cooldown: float = 60.0,
        breaker_window: int = 50,
        breaker_ratio: float = 0.3,
        breaker_pause: float = 300.0,
        checkpoint: str | None = None,
        browser: str = "off",
        browser_pages: int = 4,
        browser_block: list[str] = ...,
        max_response_size: int | None = None,
        allowed_domains: list[str] | None = None,
        cache: str | None = None,
        cache_expiry: float | None = None,
        deltafetch: str | None = None,
        deltafetch_reset: bool = False,
        warc: str | None = None,
        har: str | None = None,
        queue: str | None = None,
        queue_key: str | None = None,
        queue_lease: float = 60.0,
        queue_idle: float | None = 30.0,
        ca_certificates: bytes | None = None,
        max_redirects: int = 20,
        retry_http_codes: list[int] | None = None,
        http_auth: tuple[str, str, str] | None = None,
        saved_logins: list[tuple[str, str, str | None]] = ...,
        browser_environment: Environment | None = None,
        browser_worker_header_order: bool = False,
        browser_steps: str | None = None,
        browser_steps_max: float | None = 120.0,
    ) -> None: ...
    def add_rule(
        self,
        kind: Literal["css", "xpath", "navigation", "here"],
        query: str,
        item: ItemSpec | None = None,
        follow: bool = True,
        to_python: bool = False,
        priority: int = 0,
        allow: list[str] | None = None,
        deny: list[str] | None = None,
        allow_domains: list[str] | None = None,
        deny_domains: list[str] | None = None,
        browser: bool = False,
        steps: str | None = None,
        when: str | None = None,
    ) -> int: ...
    def submit(
        self,
        id: int,
        url: str,
        priority: int = 0,
        headers: list[tuple[str, str]] | None = None,
        dont_filter: bool = False,
        apply_rules: bool = False,
        to_python: bool = True,
        depth: int = 0,
        payload: str = "{}",
        row: int | None = None,
        browser: bool = False,
        method: str = "GET",
        kind: Literal["navigate", "form", "fetch", "image"] = "navigate",
        body: bytes | None = None,
        content_type: str | None = None,
        referer: str | None = None,
        retry_post: bool = False,
        proxy: str | None = None,
        delta_key: bytes | None = None,
        delta: bool = True,
        answered: bool = False,
        timeout: float | None = None,
        max_redirects: int | None = None,
        dont_redirect: bool = False,
        auth: tuple[str, str, str | None] | None = None,
        jar: str | None = None,
        cookies: list[dict[str, Any]] | None = None,
        dont_merge_cookies: bool = False,
        steps: str | None = None,
        steps_in_fingerprint: bool = True,
    ) -> Literal[
        "queued",
        "duplicate",
        "invalid",
        "too_deep",
        "trap",
        "domain_full",
        "offsite",
        "bad_proxy",
        "delta",
        "pushed",
        "answered",
        "capped",
    ]: ...
    def apply_rules(
        self, response: Response, url: str, depth: int
    ) -> tuple[str | None, str | None, list[tuple[Any, ...]]]: ...
    def download(
        self,
        id: int,
        url: str,
        kind: Literal["fetch", "image"] = "fetch",
        referer: str | None = None,
        headers: list[tuple[str, str]] | None = None,
        proxy: str | None = None,
        jar: str | None = None,
    ) -> Awaitable[tuple[Any, ...] | None]: ...
    def release(self, row: int) -> None: ...
    def capped(self) -> bool: ...
    def resume(self) -> dict[str, Any] | None: ...
    def ack(self) -> None: ...
    def set_tracks(self, store: TrackStore, threshold: float) -> None: ...
    def item_done(self, id: str) -> None: ...
    def settle(self, items: list[str], state: str) -> None: ...
    @staticmethod
    def check_cookies(url: str, cookies: list[dict[str, Any]]) -> None: ...
    @staticmethod
    def delta_key(
        method: str,
        url: str,
        body: bytes | None = None,
        delta_key: bytes | None = None,
        jar: str | None = None,
        steps: str | None = None,
    ) -> bytes | None: ...
    def produced(
        self, keys: list[bytes], files: list[tuple[str, int, str | None, bool]]
    ) -> None: ...
    def delta_files(self) -> dict[str, tuple[int, str | None, bool]] | None: ...
    def delta_was_empty(self) -> bool | None: ...
    def checkpoint_failing(self) -> tuple[str, bool] | None: ...
    def flush(self) -> None: ...
    def save_counters(self, json: str) -> None: ...
    def close(self) -> None: ...
    def close_browser(self) -> Awaitable[None]: ...
    def next_call(self) -> Awaitable[StepCall | None]: ...
    def close_calls(self) -> None: ...
    async def next(
        self, max: int = 256
    ) -> list[
        tuple[Literal["fetched"], int, Response, Node | None, BrowserPage | None]
        | tuple[Literal["failed"], int, FetchError]
        | tuple[Literal["dropped"], int, Literal["robots", "tdm"]]
        | tuple[Literal["handled"], int]
        | tuple[
            Literal["item"],
            int,
            dict[str, Any],
            list[tuple[str, str, str]],
            str,
            list[tuple[str, str, float | None]],
            str | None,
        ]
        | tuple[Literal["ruled"], int, str, Response, Node | None, int]
        | tuple[Literal["rule_failed"], int, str, FetchError]
        | tuple[Literal["rule_dropped"], int, str, Literal["robots", "tdm"]]
        | tuple[Literal["page_error"], str, str]
        | tuple[Literal["ignored"], str, str]
        | tuple[Literal["blocked"], int, int | None, str, str, str, Response]
        | tuple[Literal["paused"], str, float]
        | tuple[Literal["warning"], str]
        | tuple[
            Literal["leased"],
            tuple[int, str, int, list[tuple[str, str]], bool, int, str],
            tuple[str, str, bytes | None, str | None, str | None, bool, str | None],
        ]
        | tuple[Literal["fatal"], str]
    ]: ...
    def stats(self) -> dict[str, int]: ...
    def proxy_stats(self) -> dict[str, dict[str, int]]: ...
    def start_archive(self) -> None: ...

class StepCall:
    @property
    def url(self) -> str: ...
    @property
    def depth(self) -> int: ...
    @property
    def rule(self) -> int | None: ...
    @property
    def id(self) -> int: ...
    @property
    def index(self) -> int: ...
    @property
    def method(self) -> str | None: ...
    @property
    def timeout(self) -> float: ...
    @property
    def page(self) -> BrowserPage: ...
    def reply(self, json: str) -> None: ...
    def fail(self, message: str) -> None: ...

class Queue:
    """A crawl's shared queue in Redis. Raises ValueError for a URL or key
    that can't work, and netweir.QueueError when Redis can't be reached or
    fails."""

    def __init__(self, url: str, key: str) -> None: ...
    def push(self, urls: list[str], priority: int = 0, dont_filter: bool = False) -> int: ...
    def status(self) -> dict[str, int]: ...
    def clear(self) -> None: ...

class ItemSpec:
    def __init__(
        self,
        name: str,
        fields: list[
            tuple[
                str,
                Literal["css", "xpath"],
                str,
                str | None,
                bool,
                bool,
                # "text", "int", "float", "bool", "markdown", "markdown_main",
                # "clean_text", "clean_text_main", or "auto:<field>" for a
                # pinned field of an auto item.
                str,
                object,
                str | None,
            ]
        ],
        schema: str | None = None,
    ) -> None: ...
    def extract(
        self,
        node: Node,
        store: TrackStore | None = None,
        site: str = "",
        threshold: float = 0.75,
        base_url: str | None = None,
        page_url: str | None = None,
    ) -> tuple[dict[str, Any], list[tuple[str, str, str]], list[tuple[str, str, float | None]]]: ...

class TrackStore:
    def __init__(self, path: str) -> None: ...
    def get(self, site: str, name: str) -> str | None: ...
    def close(self) -> None: ...

def similarity(node: Node, kind: str, query: str, fingerprint: str) -> float | None: ...

class JsonlWriter:
    def __init__(self, path: str, append: bool = False) -> None: ...
    def write(self, item: Any) -> None: ...
    def flush(self) -> None: ...
    def close(self) -> None: ...

class JsonWriter:
    @staticmethod
    def resume_length(path: str) -> int: ...
    def __init__(self, path: str, append: bool = False) -> None: ...
    def write(self, item: Any) -> None: ...
    def flush(self) -> None: ...
    def close(self) -> None: ...

class XmlWriter:
    @staticmethod
    def resume_length(path: str, root: str = "items") -> int: ...
    def __init__(
        self, path: str, root: str = "items", item: str = "item", append: bool = False
    ) -> None: ...
    def write(self, item: Any) -> None: ...
    def flush(self) -> None: ...
    def close(self) -> None: ...

class CsvWriter:
    def __init__(
        self, path: str, fields: list[str] | None = None, append: bool = False
    ) -> None: ...
    def write(self, item: Any) -> list[str] | None: ...
    def flush(self) -> None: ...
    def close(self) -> None: ...

class ParquetWriter:
    def __init__(self, path: str) -> None: ...
    def write(self, item: Any) -> None: ...
    def close(self) -> list[tuple[str, int, int]]: ...

def parse(
    html: str,
    timeout: float | None = None,
    type: Literal["html", "xml"] | None = None,
    recover: bool = False,
) -> Node: ...
def _encode(text: str, encoding: str) -> bytes | None: ...
def _xml_nodes(
    text: str,
    tag: str,
    namespaces: list[tuple[str, str]],
    recover: bool = False,
    timeout: float | None = None,
    place: str | None = None,
) -> XmlNodeIter: ...

class XmlNodeIter:
    def __iter__(self) -> XmlNodeIter: ...
    def __next__(self) -> Node: ...

def _build_info() -> dict[str, Any]: ...
def fingerprint(url: str) -> str | None: ...
def launch(
    executable: str | os.PathLike[str] | None = None,
    headless: bool = True,
    args: list[str] = ...,
    timeout: float = 30.0,
    proxy: str | None = None,
    connect: str | None = None,
    user_data_dir: str | os.PathLike[str] | None = None,
    dialog_pause: tuple[float, float] = (0.5, 1.5),
    environment: Environment | None = None,
    worker_header_order: bool = False,
    engine: Literal["chrome", "firefox"] = "chrome",
    stealth: bool = True,
) -> Awaitable[Browser]: ...
def find_chrome() -> str: ...

class Size(TypedDict):
    width: int
    height: int

class Geolocation(TypedDict, total=False):
    latitude: float
    longitude: float
    accuracy: float

class Environment(TypedDict, total=False):
    device: str
    viewport: Size | None
    screen: Size
    device_scale_factor: float
    is_mobile: bool
    has_touch: bool
    locale: str
    timezone_id: str
    geolocation: Geolocation
    permissions: list[str]
    color_scheme: Literal["light", "dark", "no-preference"]
    reduced_motion: Literal["reduce", "no-preference"]
    forced_colors: Literal["active", "none"]
    contrast: Literal["more", "no-preference"]
    extra_http_headers: dict[str, str]
    offline: bool
    user_agent: str
    init_scripts: list[str]

def _devices() -> dict[str, Environment]: ...
def _check_environment(given: Environment) -> None: ...
def install_chrome(
    headless_shell: bool = False, index: str = ..., home: str | os.PathLike[str] | None = None
) -> tuple[str, str, bool]: ...
def install_firefox(
    base: str = ...,
    home: str | os.PathLike[str] | None = None,
    version: str | None = None,
    sums: Sequence[tuple[str, str]] | None = None,
) -> tuple[str, str, bool]: ...

class Browser:
    @property
    def version(self) -> str: ...
    @property
    def engine(self) -> Literal["chrome", "firefox"]: ...
    @property
    def stealth(self) -> bool: ...
    @property
    def closed(self) -> bool: ...
    def start_tracing(
        self, *, categories: Sequence[str] | None = None, screenshots: bool = False
    ) -> Awaitable[None]: ...
    def stop_tracing(self, path: str | os.PathLike[str] | None = None) -> Awaitable[bytes]: ...
    def new_page(
        self,
        *,
        device: str | None = None,
        viewport: Size | None = None,
        screen: Size | None = None,
        device_scale_factor: float | None = None,
        is_mobile: bool | None = None,
        has_touch: bool | None = None,
        locale: str | None = None,
        timezone_id: str | None = None,
        geolocation: Geolocation | None = None,
        permissions: list[str] | None = None,
        color_scheme: Literal["light", "dark", "no-preference"] | None = None,
        reduced_motion: Literal["reduce", "no-preference"] | None = None,
        forced_colors: Literal["active", "none"] | None = None,
        contrast: Literal["more", "no-preference"] | None = None,
        extra_http_headers: dict[str, str] | None = None,
        offline: bool | None = None,
        user_agent: str | None = None,
        init_scripts: list[str] | None = None,
    ) -> Awaitable[BrowserPage]: ...
    def new_context(
        self,
        storage_state: str | os.PathLike[str] | dict[str, Any] | None = None,
        *,
        device: str | None = None,
        viewport: Size | None = None,
        screen: Size | None = None,
        device_scale_factor: float | None = None,
        is_mobile: bool | None = None,
        has_touch: bool | None = None,
        locale: str | None = None,
        timezone_id: str | None = None,
        geolocation: Geolocation | None = None,
        permissions: list[str] | None = None,
        color_scheme: Literal["light", "dark", "no-preference"] | None = None,
        reduced_motion: Literal["reduce", "no-preference"] | None = None,
        forced_colors: Literal["active", "none"] | None = None,
        contrast: Literal["more", "no-preference"] | None = None,
        extra_http_headers: dict[str, str] | None = None,
        offline: bool | None = None,
        user_agent: str | None = None,
        init_scripts: list[str] | None = None,
    ) -> Awaitable[BrowserContext]: ...
    def methods_sent(self) -> list[str]: ...
    def close(self) -> Awaitable[None]: ...
    def __aenter__(self) -> Awaitable[Browser]: ...
    def __aexit__(self, *exc: object) -> Awaitable[None]: ...

class BrowserContext:
    def new_page(
        self,
        *,
        device: str | None = None,
        viewport: Size | None = None,
        screen: Size | None = None,
        device_scale_factor: float | None = None,
        is_mobile: bool | None = None,
        has_touch: bool | None = None,
        locale: str | None = None,
        timezone_id: str | None = None,
        geolocation: Geolocation | None = None,
        permissions: list[str] | None = None,
        color_scheme: Literal["light", "dark", "no-preference"] | None = None,
        reduced_motion: Literal["reduce", "no-preference"] | None = None,
        forced_colors: Literal["active", "none"] | None = None,
        contrast: Literal["more", "no-preference"] | None = None,
        extra_http_headers: dict[str, str] | None = None,
        offline: bool | None = None,
        user_agent: str | None = None,
        init_scripts: list[str] | None = None,
    ) -> Awaitable[BrowserPage]: ...
    def grant_permissions(
        self, permissions: list[str], origin: str | None = None
    ) -> Awaitable[None]: ...
    def clear_permissions(self) -> Awaitable[None]: ...
    def set_geolocation(self, geolocation: Geolocation | None) -> Awaitable[None]: ...
    def set_offline(self, offline: bool) -> Awaitable[None]: ...
    def set_extra_http_headers(self, headers: dict[str, str]) -> Awaitable[None]: ...
    def add_init_script(
        self, script: str | None = None, *, path: str | os.PathLike[str] | None = None
    ) -> Awaitable[None]: ...
    def route(
        self, pattern: str | re.Pattern[str] | Callable[[str], Any], handler: Callable[[Route], Any]
    ) -> Awaitable[None]: ...
    def unroute(
        self,
        pattern: str | re.Pattern[str] | Callable[[str], Any],
        handler: Callable[[Route], Any] | None = None,
    ) -> Awaitable[None]: ...
    def route_from_har(
        self,
        path: str | os.PathLike[str],
        *,
        url: str | re.Pattern[str] | Callable[[str], Any] = "**/*",
        not_found: Literal["abort", "fallback"] = "abort",
    ) -> Awaitable[None]: ...
    def cookies(self) -> Awaitable[list[dict[str, Any]]]: ...
    def set_cookies(self, cookies: list[dict[str, Any]]) -> Awaitable[None]: ...
    def storage_state(
        self, path: str | os.PathLike[str] | None = None
    ) -> Awaitable[dict[str, Any]]: ...
    def close(self) -> Awaitable[None]: ...
    def __aenter__(self) -> Awaitable[BrowserContext]: ...
    def __aexit__(self, *exc: object) -> Awaitable[None]: ...

class BrowserPage:
    @property
    def url(self) -> str: ...
    @property
    def closed(self) -> bool: ...
    @property
    def viewport_size(self) -> Size | None: ...
    def set_viewport_size(self, viewport: Size) -> Awaitable[None]: ...
    def set_extra_http_headers(self, headers: dict[str, str]) -> Awaitable[None]: ...
    def emulate_media(
        self,
        *,
        media: Literal["screen", "print", "null"] | None = None,
        color_scheme: Literal["light", "dark", "no-preference", "null"] | None = None,
        reduced_motion: Literal["reduce", "no-preference", "null"] | None = None,
        forced_colors: Literal["active", "none", "null"] | None = None,
        contrast: Literal["more", "no-preference", "null"] | None = None,
    ) -> Awaitable[None]: ...
    def add_init_script(
        self, script: str | None = None, *, path: str | os.PathLike[str] | None = None
    ) -> Awaitable[None]: ...
    def set_geolocation(self, geolocation: Geolocation | None) -> Awaitable[None]: ...
    def set_offline(self, offline: bool) -> Awaitable[None]: ...
    def grant_permissions(
        self, permissions: list[str], origin: str | None = None
    ) -> Awaitable[None]: ...
    def clear_permissions(self) -> Awaitable[None]: ...
    def goto(
        self, url: str, wait: str = "load", timeout: float | None = None
    ) -> Awaitable[BrowserResponse]: ...
    def wait_for_navigation(
        self, wait: str = "load", timeout: float | None = None
    ) -> Awaitable[BrowserResponse]: ...
    def wait_for_load(
        self, wait: str = "load", timeout: float | None = None
    ) -> Awaitable[BrowserResponse]: ...
    def go_back(
        self, wait: str = "load", timeout: float | None = None
    ) -> Awaitable[BrowserResponse | None]: ...
    def go_forward(
        self, wait: str = "load", timeout: float | None = None
    ) -> Awaitable[BrowserResponse | None]: ...
    def reload(
        self, wait: str = "load", timeout: float | None = None
    ) -> Awaitable[BrowserResponse]: ...
    def content(self) -> Awaitable[str]: ...
    def parse(self) -> Awaitable[Node]: ...
    def title(self) -> Awaitable[str]: ...
    def evaluate(self, js: str) -> Awaitable[Any]: ...
    def click(self, selector: str, timeout: float | None = None) -> Awaitable[None]: ...
    def dblclick(self, selector: str, timeout: float | None = None) -> Awaitable[None]: ...
    def fill(self, selector: str, text: str, timeout: float | None = None) -> Awaitable[None]: ...
    def type(
        self,
        selector: str,
        text: str,
        delay: float | tuple[float, float] | None = None,
        timeout: float | None = None,
    ) -> Awaitable[None]: ...
    def press(self, key: str) -> Awaitable[None]: ...
    def wait_for(
        self, selector: str, state: str = "visible", timeout: float | None = None
    ) -> Awaitable[None]: ...
    def hover(self, selector: str, timeout: float | None = None) -> Awaitable[None]: ...
    def drag(self, source: str, target: str, timeout: float | None = None) -> Awaitable[None]: ...
    def scroll(
        self,
        selector: str | None = None,
        *,
        x: float = 0.0,
        y: float | None = None,
        times: int | None = None,
        idle: float = 2.0,
        timeout: float | None = None,
    ) -> Awaitable[int]: ...
    def select_option(
        self,
        selector: str,
        value: str | Sequence[str] | None = None,
        *,
        label: str | Sequence[str] | None = None,
        index: int | Sequence[int] | None = None,
        timeout: float | None = None,
    ) -> Awaitable[list[str]]: ...
    def check(self, selector: str, timeout: float | None = None) -> Awaitable[None]: ...
    def uncheck(self, selector: str, timeout: float | None = None) -> Awaitable[None]: ...
    def set_input_files(
        self,
        selector: str,
        files: str | os.PathLike[str] | Sequence[str | os.PathLike[str]],
        timeout: float | None = None,
    ) -> Awaitable[None]: ...
    def frames(self) -> Awaitable[list[BrowserFrame]]: ...
    def frame(
        self, name: str | None = None, url: str | re.Pattern[str] | None = None
    ) -> Awaitable[BrowserFrame | None]: ...
    def screenshot(
        self,
        path: str | os.PathLike[str] | None = None,
        full_page: bool = False,
        quality: int | None = None,
    ) -> Awaitable[bytes]: ...
    def pdf(
        self,
        path: str | os.PathLike[str] | None = None,
        *,
        landscape: bool = False,
        print_background: bool = False,
        scale: float | None = None,
        format: str | None = None,
        width: float | None = None,
        height: float | None = None,
        margin: float | None = None,
        page_ranges: str | None = None,
    ) -> Awaitable[bytes]: ...
    def cookies(self) -> Awaitable[list[dict[str, Any]]]: ...
    def set_cookies(self, cookies: list[dict[str, Any]]) -> Awaitable[None]: ...
    def storage_state(
        self, path: str | os.PathLike[str] | None = None
    ) -> Awaitable[dict[str, Any]]: ...
    def on_response(self, callback: Callable[[BrowserResponse], Any]) -> None: ...
    def remove_response_listener(self, callback: Callable[[BrowserResponse], Any]) -> None: ...
    def on_request(self, callback: Callable[[BrowserRequest], Any]) -> None: ...
    def remove_request_listener(self, callback: Callable[[BrowserRequest], Any]) -> None: ...
    def on_dialog(self, callback: Callable[[BrowserDialog], Any]) -> None: ...
    def remove_dialog_listener(self, callback: Callable[[BrowserDialog], Any]) -> None: ...
    def on_popup(self, callback: Callable[[BrowserPage], Any]) -> None: ...
    def remove_popup_listener(self, callback: Callable[[BrowserPage], Any]) -> None: ...
    def wait_for_popup(self, timeout: float | None = None) -> Awaitable[BrowserPage]: ...
    def expect_popup(self, timeout: float | None = None) -> Expectation: ...
    def on_download(self, callback: Callable[[BrowserDownload], Any]) -> None: ...
    def remove_download_listener(self, callback: Callable[[BrowserDownload], Any]) -> None: ...
    def wait_for_download(
        self,
        pattern: str | re.Pattern[str] | Callable[[BrowserDownload], Any] | None = None,
        timeout: float | None = None,
    ) -> Awaitable[BrowserDownload]: ...
    def expect_download(
        self,
        pattern: str | re.Pattern[str] | Callable[[BrowserDownload], Any] | None = None,
        timeout: float | None = None,
    ) -> Expectation: ...
    def wait_for_response(
        self,
        pattern: str | re.Pattern[str] | Callable[[BrowserResponse], Any],
        timeout: float | None = None,
    ) -> Awaitable[BrowserResponse]: ...
    def wait_for_request(
        self,
        pattern: str | re.Pattern[str] | Callable[[BrowserRequest], Any],
        timeout: float | None = None,
    ) -> Awaitable[BrowserRequest]: ...
    def expect_response(
        self,
        pattern: str | re.Pattern[str] | Callable[[BrowserResponse], Any],
        timeout: float | None = None,
    ) -> Expectation: ...
    def expect_request(
        self,
        pattern: str | re.Pattern[str] | Callable[[BrowserRequest], Any],
        timeout: float | None = None,
    ) -> Expectation: ...
    def route(
        self, pattern: str | re.Pattern[str] | Callable[[str], Any], handler: Callable[[Route], Any]
    ) -> Awaitable[None]: ...
    def unroute(
        self,
        pattern: str | re.Pattern[str] | Callable[[str], Any],
        handler: Callable[[Route], Any] | None = None,
    ) -> Awaitable[None]: ...
    def route_from_har(
        self,
        path: str | os.PathLike[str],
        *,
        url: str | re.Pattern[str] | Callable[[str], Any] = "**/*",
        not_found: Literal["abort", "fallback"] = "abort",
    ) -> Awaitable[None]: ...
    def block(
        self, resource_types: Sequence[str] = ("image", "font", "media"), urls: Sequence[str] = ()
    ) -> Awaitable[None]: ...
    def close(self) -> Awaitable[None]: ...
    def __aenter__(self) -> Awaitable[BrowserPage]: ...
    def __aexit__(self, *exc: object) -> Awaitable[None]: ...

class BrowserFrame:
    @property
    def name(self) -> str: ...
    @property
    def url(self) -> str: ...
    def content(self) -> Awaitable[str]: ...
    def parse(self) -> Awaitable[Node]: ...
    def title(self) -> Awaitable[str]: ...
    def evaluate(self, js: str) -> Awaitable[Any]: ...
    def click(self, selector: str, timeout: float | None = None) -> Awaitable[None]: ...
    def dblclick(self, selector: str, timeout: float | None = None) -> Awaitable[None]: ...
    def fill(self, selector: str, text: str, timeout: float | None = None) -> Awaitable[None]: ...
    def type(
        self,
        selector: str,
        text: str,
        delay: float | tuple[float, float] | None = None,
        timeout: float | None = None,
    ) -> Awaitable[None]: ...
    def wait_for(
        self, selector: str, state: str = "visible", timeout: float | None = None
    ) -> Awaitable[None]: ...
    def hover(self, selector: str, timeout: float | None = None) -> Awaitable[None]: ...
    def drag(self, source: str, target: str, timeout: float | None = None) -> Awaitable[None]: ...
    def scroll(
        self,
        selector: str | None = None,
        *,
        x: float = 0.0,
        y: float | None = None,
        times: int | None = None,
        idle: float = 2.0,
        timeout: float | None = None,
    ) -> Awaitable[int]: ...
    def select_option(
        self,
        selector: str,
        value: str | Sequence[str] | None = None,
        *,
        label: str | Sequence[str] | None = None,
        index: int | Sequence[int] | None = None,
        timeout: float | None = None,
    ) -> Awaitable[list[str]]: ...
    def check(self, selector: str, timeout: float | None = None) -> Awaitable[None]: ...
    def uncheck(self, selector: str, timeout: float | None = None) -> Awaitable[None]: ...
    def set_input_files(
        self,
        selector: str,
        files: str | os.PathLike[str] | Sequence[str | os.PathLike[str]],
        timeout: float | None = None,
    ) -> Awaitable[None]: ...

class BrowserResponse:
    @property
    def url(self) -> str: ...
    @property
    def status(self) -> int: ...
    @property
    def headers(self) -> list[tuple[str, str]]: ...
    @property
    def method(self) -> str: ...
    @property
    def resource_type(self) -> str: ...
    def body(self) -> Awaitable[bytes]: ...
    def text(self) -> Awaitable[str]: ...
    def json(self) -> Awaitable[Any]: ...

class BrowserDialog:
    @property
    def type(self) -> str: ...
    @property
    def message(self) -> str: ...
    @property
    def default_value(self) -> str: ...
    @property
    def url(self) -> str: ...
    @property
    def handled(self) -> bool: ...
    def accept(self, prompt_text: str | None = None) -> Awaitable[None]: ...
    def dismiss(self) -> Awaitable[None]: ...

class BrowserDownload:
    @property
    def url(self) -> str: ...
    @property
    def suggested_filename(self) -> str: ...
    def path(self, timeout: float | None = None) -> Awaitable[Path]: ...
    def save_as(
        self, path: str | os.PathLike[str], timeout: float | None = None
    ) -> Awaitable[None]: ...
    def body(self, timeout: float | None = None) -> Awaitable[bytes]: ...
    def failure(self, timeout: float | None = None) -> Awaitable[str | None]: ...
    def cancel(self) -> Awaitable[None]: ...

class BrowserRequest:
    @property
    def url(self) -> str: ...
    @property
    def method(self) -> str: ...
    @property
    def headers(self) -> list[tuple[str, str]]: ...
    @property
    def resource_type(self) -> str: ...
    @property
    def post_data(self) -> bytes | None: ...

class Route:
    @property
    def request(self) -> BrowserRequest: ...
    @property
    def handled(self) -> bool: ...
    def abort(self) -> Awaitable[None]: ...
    def continue_(
        self,
        url: str | None = None,
        method: str | None = None,
        headers: dict[str, str] | Sequence[tuple[str, str]] | None = None,
        body: bytes | str | None = None,
    ) -> Awaitable[None]: ...
    def fulfill(
        self,
        status: int = 200,
        headers: dict[str, str] | Sequence[tuple[str, str]] | None = None,
        body: bytes | str | None = None,
        json: Any = None,
    ) -> Awaitable[None]: ...

_RESOURCE_TYPES: list[str]

def _glob_match(pattern: str, url: str) -> bool: ...

class StopFlag:
    def __init__(self) -> None: ...
    def set(self) -> None: ...

def _sitemap(
    body: bytes, max: int, stop: StopFlag | None = None
) -> tuple[Literal["urlset", "index"], list[dict[str, Any]], str | None]: ...
def _robots_sitemaps(body: str) -> list[str]: ...
def _offsite(url: str, allowed: list[str]) -> bool: ...
def js_data(text: str, all: bool = False) -> Any: ...
def _parse_price(text: str) -> tuple[str, str | None] | None: ...
def _currency_code(currency: str) -> str | None: ...
def _parse_date(
    text: str, day_first: bool | None, now: tuple[int, int, int, int, int, int, int]
) -> tuple[tuple[int, int, int, int, int, int, int], int | None, bool, int] | None: ...
def _check_queue_url(url: str) -> None: ...
def parse_gtin(text: str) -> dict[str, str] | None: ...
def parse_rating(text: str) -> dict[str, float | int | None] | None: ...
def parse_availability(text: str) -> str | None: ...
def parse_salary(text: str) -> dict[str, str | None] | None: ...
def _auto_fields(schema: str) -> list[str]: ...
