"""Driving Chrome or Firefox: ``netweir.browser()``."""

from __future__ import annotations

import os
from collections.abc import Generator, Sequence
from typing import TYPE_CHECKING, Any, Literal

from netweir._native import Browser, _devices, launch

if TYPE_CHECKING:
    from netweir._native import Environment

#: Every device preset by name, as the keyword arguments it stands for:
#: ``browser.new_context(**netweir.devices["Galaxy S23"])``, or just
#: ``device="Galaxy S23"``. Desktops keep the system Chrome runs on and
#: change its screen; phones are Android phones, in every client hint.
devices: dict[str, Environment] = _devices()


class browser:
    """Starts Chrome, or Firefox with ``engine="firefox"``. Use it either way:

    .. code-block:: python

        async with netweir.browser() as b:
            page = await b.new_page()

        b = await netweir.browser()
        ...
        await b.close()

    Without ``executable``, it uses ``$NETWEIR_CHROME``, then the newest
    Chrome ``netweir install chrome`` installed, then the usual install
    paths for Chrome, Chrome for Testing and Chromium. ``timeout``
    (seconds) is the default limit for navigations and actions. ``proxy``
    (``http://``, ``https://`` or ``socks5://``) carries everything Chrome
    fetches; an HTTP(S) proxy's URL may hold a login, a SOCKS proxy's
    can't.

    ``connect`` drives a browser that's already running instead of
    starting one: a ``ws://`` DevTools URL, or ``http://host:port``.
    Closing it closes the pages Netweir opened and leaves the browser
    running.

    ``user_data_dir`` is a profile directory to keep: cookies and storage
    survive to the next launch with the same directory, and it's never
    deleted. A leading ``~`` is your home directory. Pages from ``new_page()`` then share that profile.
    Chrome's "Preload pages" setting is turned off in it, and stays off.
    Google Chrome's own default profile is refused: Chrome won't be
    driven with it. Use a copy, or Chrome for Testing.

    ``dialog_pause`` is how long, in seconds, a person takes to react:
    no dialog is answered sooner after it opened, and a tab brought back
    to the front waits as long before it's used, a time picked between
    the two each time. Shorter is faster, and a page that times how long
    ``confirm()`` kept it waiting can tell.

    ``environment`` is every page's environment, with the keys
    ``new_context()`` takes (``device``, ``viewport``, ``locale``,
    ``timezone_id`` and the rest), unless a context or page says
    otherwise. Given here, a screen, pixel ratio, user agent and language
    are Chrome's own from the start, so even service workers agree.

    ``engine="firefox"`` starts the Firefox ``netweir install firefox``
    installed (or ``$NETWEIR_FIREFOX``) instead. Pages can't tell it's
    driven, as with Chrome. ``stealth=False`` drives any Firefox in its
    automation mode, which pages can tell (``navigator.webdriver`` is
    true); it's the only way to drive a Firefox Netweir didn't install.
    Firefox has no ``proxy``, ``connect`` or ``worker_header_order``, and
    a ``user_data_dir`` must be a profile Netweir made: those raise.

    ``worker_header_order=True`` sends each dedicated and shared worker's
    script request with its headers in Chrome's own order, which the user
    agent override every page has otherwise changes (User-Agent comes
    early), and a server reading header order can see. It's off by
    default because it pauses every request of every page: a page with
    300 subresources took 118 ms to load where it took 93, and one with
    frames and workers took 292 ms where it took 200.
    """

    def __init__(
        self,
        executable: str | os.PathLike[str] | None = None,
        *,
        headless: bool = True,
        args: Sequence[str] = (),
        timeout: float = 30.0,
        proxy: str | None = None,
        connect: str | None = None,
        user_data_dir: str | os.PathLike[str] | None = None,
        dialog_pause: tuple[float, float] = (0.5, 1.5),
        environment: Environment | None = None,
        worker_header_order: bool = False,
        engine: Literal["chrome", "firefox"] = "chrome",
        stealth: bool = True,
    ):
        self._options: dict[str, Any] = {
            "executable": None if executable is None else os.fspath(executable),
            "headless": headless,
            "args": list(args),
            "timeout": timeout,
            "proxy": proxy,
            "connect": connect,
            "user_data_dir": (
                None if user_data_dir is None else os.path.expanduser(os.fspath(user_data_dir))
            ),
            "dialog_pause": tuple(dialog_pause),
            "environment": None if environment is None else dict(environment),
            "worker_header_order": worker_header_order,
            "engine": engine,
            "stealth": stealth,
        }
        self._browser: Browser | None = None

    def __await__(self) -> Generator[Any, None, Browser]:
        return launch(**self._options).__await__()

    async def __aenter__(self) -> Browser:
        self._browser = await self
        return self._browser

    async def __aexit__(self, *exc: object) -> None:
        if self._browser is not None:
            await self._browser.close()
