"""Tests for the gates in front of ``freemicro keymap push`` (no hardware)."""

from __future__ import annotations

import json

import pytest

from freemicro import padkeymap


def test_push_refuses_while_another_host_talks_to_the_pad(monkeypatch):
    monkeypatch.setattr(padkeymap, "other_hosts", lambda: ["ChatGPT"])
    monkeypatch.setattr(padkeymap, "with_pad", lambda *a, **k: pytest.fail("opened the pad"))
    lines = []
    assert padkeymap.push(None, test=True, say=lines.append) == 1
    assert "ChatGPT" in lines[0]


@pytest.mark.parametrize("content", ['[]', '{"profiles": []}', '{"version": 1}'])
def test_load_edit_rejects_files_that_are_not_keymaps(tmp_path, content):
    path = tmp_path / "keymap.json"
    path.write_text(content)
    with pytest.raises(ValueError):
        padkeymap.load_edit(path)


def test_load_edit_encodes_like_input(tmp_path):
    path = tmp_path / "keymap.json"
    path.write_text(json.dumps({"profiles": [{"name": "Café"}]}, indent=2))
    assert padkeymap.load_edit(path) == b'{"profiles":[{"name":"Caf\\u00e9"}]}'
