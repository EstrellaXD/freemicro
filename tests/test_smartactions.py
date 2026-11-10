"""Tests for the host side of a Creator Micro 2's smart actions."""

from __future__ import annotations

import pytest

from freemicro.input.smartactions import handle_smart_action


@pytest.mark.parametrize(
    "method, params, opened, typed",
    [
        ("kb.sa.openapp", {"path": "/Applications/Ghostty.app"},
         [["open", "/Applications/Ghostty.app"]], []),
        ("kb.sa.openurl", {"url": "example.org"},
         [["open", "https://example.org"]], []),
        ("kb.sa.openurl", {"url": "httpx://evil"}, [], []),
        ("kb.sa.inserttext", {"text": "hello"}, [], ["hello"]),
        ("kb.sa.exec", {"cmd": "rm -rf ~"}, [], []),
    ],
)
def test_smart_action_runs_only_what_is_allowed(method, params, opened, typed):
    calls, text = [], []
    line = handle_smart_action({"m": method, "p": params}, text.append, calls.append)
    assert line and (calls, text) == (opened, typed)


def test_non_smart_action_messages_pass_through():
    assert handle_smart_action({"m": "v.oai.hid", "p": {"k": "AG00", "act": 1}},
                               print, print) is None
