"""JMESPath queries over JSON, with the ``jmespath`` package (the optional
``netweir[jmespath]`` extra, as parsel uses)."""

from __future__ import annotations

import json
from typing import Any


def _module():
    try:
        import jmespath
    except ImportError:
        raise ImportError(
            "JMESPath queries need the jmespath package: pip install 'netweir[jmespath]'"
        ) from None
    return jmespath


def _loads(text: str, what: str, show: bool = False) -> Any:
    try:
        return json.loads(text)
    except json.JSONDecodeError as e:
        if show:
            # The start of the text, so one result among many can be found.
            start = " ".join(text.split())
            start = start if len(start) <= 30 else start[:30] + "..."
            raise ValueError(f"{what} isn't JSON ({start!r}: {e})") from None
        raise ValueError(f"{what} isn't JSON ({e})") from None


def jmespath(expression: str, data: Any) -> Any:
    """``data`` (dicts, lists and plain values, as ``json.loads`` or
    ``js_data`` give them) searched with a JMESPath expression."""
    return _module().search(expression, data)


def search_text(expression: str, text: str, what: str = "the node's text") -> Any:
    # The extra is checked first, so a missing one is reported even when
    # the text isn't JSON either.
    module = _module()
    return module.search(expression, _loads(text, what))


def search_texts(expression: str, texts: list[str]) -> list[Any]:
    module = _module()
    found = (
        module.search(expression, _loads(t, f"result {i}", show=True)) for i, t in enumerate(texts)
    )
    return [value for value in found if value is not None]
