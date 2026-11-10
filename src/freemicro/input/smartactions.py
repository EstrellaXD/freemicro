"""Run a Creator Micro 2's smart actions, the job Work Louder's Input app did.

A smart action (``smart_actions.json`` on the pad, bound to a key as
``SA_<n>``) is executed by the *host*: the firmware sends one notification and
the computer does the work. FreeMicro answers the same four Input does:

============================ =====================================================
``kb.sa.openapp {path}``     ``open <path>``
``kb.sa.openurl {url}``      ``open <url>``; ``https://`` is added when the scheme
                             is missing, and only http(s) is opened
``kb.sa.inserttext {text}``  types ``text`` into the frontmost app
``kb.sa.exec {cmd}``         **ignored** - a pad key that runs shell commands is
                             a security surface, so it is off
============================ =====================================================

The payloads come from the pad, which is the user's own device but still a
trust boundary, so every argument is passed to ``open`` as an argv element,
never through a shell.
"""

from __future__ import annotations

import subprocess
from typing import Any, Callable, List, Mapping, Optional
from urllib.parse import urlsplit

PREFIX = "kb.sa."


def _open(argv: List[str]) -> None:
    subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def handle_smart_action(
    message: Mapping[str, Any],
    type_text: Callable[[str], None],
    opener: Callable[[List[str]], None] = _open,
) -> Optional[str]:
    """Run ``message`` if it is a smart action; return a log line, else ``None``."""
    method = message.get("m") or message.get("method") or ""
    if not isinstance(method, str) or not method.startswith(PREFIX):
        return None
    params = message.get("p") or message.get("params") or {}
    if not isinstance(params, Mapping):
        return f"smart action {method}: ignored (no parameters)"
    action = method[len(PREFIX):]
    value = params.get({"openapp": "path", "openurl": "url",
                        "inserttext": "text", "exec": "cmd"}.get(action, ""))
    if not isinstance(value, str) or not value:
        return f"smart action {method}: ignored (unknown or empty)"
    if action == "openapp":
        opener(["open", value])
        return f"smart action: opened {value}"
    if action == "openurl":
        url = value if value.lower().startswith("http") else f"https://{value}"
        if urlsplit(url).scheme.lower() not in ("http", "https"):
            return f"smart action: refused non-http URL {value!r}"
        opener(["open", url])
        return f"smart action: opened {url}"
    if action == "inserttext":
        type_text(value)
        return f"smart action: typed {len(value)} characters"
    return f"smart action: ignored exec {value!r} (shell commands are off)"


__all__ = ["handle_smart_action"]
