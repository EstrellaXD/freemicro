"""Tests for chunked config-file access on a Creator Micro 2 (no hardware)."""

from __future__ import annotations

import base64
import hashlib

import pytest

from freemicro.device import padfs
from freemicro.device.padfs import CHUNK, PadFs, PadFsError


class _FakePad:
    """Answers fs.* like firmware 0.6.3 and records every write."""

    def __init__(self, files: dict, corrupt: bool = False) -> None:
        self.files = dict(files)
        self.corrupt = corrupt
        self.writes: list = []

    def request(self, method, params, request_id, timeout, match_id, standard):
        assert match_id and standard
        if method == "fs.readbin":
            data = self.files[params["file"]]
            chunk = data[params["offset"]:params["offset"] + params["len"]]
            result = {"total_size": len(data), "data": base64.b64encode(chunk).decode()}
        elif method == "fs.writebin":
            self.writes.append((params["offset"], params["completed"]))
            if params["offset"] == 0:
                self.files[params["file"]] = b""
            self.files[params["file"]] += base64.b64decode(params["data"])
            result = {"data_written": len(params["data"])}
        else:
            result = [{"name": n, "checksum": hashlib.sha1(d + b"x" * self.corrupt).hexdigest()}
                      for n, d in self.files.items()]
        return {"id": request_id, "result": result}


@pytest.fixture(autouse=True)
def _no_gap(monkeypatch):
    monkeypatch.setattr(padfs, "GAP_SECONDS", 0)


def test_write_verified_chunks_in_order_and_round_trips():
    data = bytes(range(256)) * 30  # 7680 bytes: three chunks
    pad = _FakePad({})
    fs = PadFs(pad)
    fs.write_verified("keymap.json", data)
    assert pad.writes == [(0, False), (CHUNK, False), (2 * CHUNK, True)]
    assert fs.read("keymap.json") == data


def test_write_verified_raises_on_checksum_mismatch():
    with pytest.raises(PadFsError):
        PadFs(_FakePad({}, corrupt=True)).write_verified("keymap.json", b"{}")
