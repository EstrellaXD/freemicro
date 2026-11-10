"""Read and write a Creator Micro 2's config files, the way Work Louder's Input does.

The pad keeps its whole configuration - layers, macros, per-app links - in two
files on its own filesystem: ``keymap.json`` and ``smart_actions.json``. One
complete write of ``keymap.json`` takes effect at once and persists.

The wire sequence, from the Input app (v1.1.0):

* **read**: ``fs.readbin {file, offset, len}`` in 3072-byte steps; each reply is
  ``{total_size, data: <base64>}``.
* **write**: ``fs.writebin {file, data: <base64>, append: true, offset,
  completed}`` in 3072-byte chunks, ``completed`` true only on the last.
* **verify**: ``fs.list {checksum: true}`` returns each file's SHA-1.

Every call uses the long JSON-RPC envelope ``{method, params, id}``, as Input
does: firmware 0.6.3 reads ``fs.writebin``'s data only from ``params``.

Writing is the one operation that can damage the pad: two hosts talking to it
during a write has bricked a unit (recovery took opening the case). So this
module does one request at a time, waits for *its own* reply (``match_id``),
leaves 50 ms between requests like Input, and never aborts halfway. The caller
is responsible for being the pad's only host - see ``freemicro keymap push``.
"""

from __future__ import annotations

import base64
import hashlib
import itertools
import time
from typing import Any, Dict, Optional

#: Bytes per chunk, both directions. 3072 raw bytes is 4096 base64 characters.
CHUNK = 3072
#: Input's spacing between requests.
GAP_SECONDS = 0.05
#: How long one chunk may take to answer.
TIMEOUT_SECONDS = 5.0

KEYMAP = "keymap.json"
SMART_ACTIONS = "smart_actions.json"


class PadFsError(RuntimeError):
    """The pad did not answer, or answered with an error."""


class PadFs:
    """Sequential, id-matched file access on one open :class:`Device`."""

    def __init__(self, device: Any) -> None:
        self.device = device
        # Request ids stay below 999, as Input's do.
        self._ids = itertools.cycle(range(500, 999))

    def call(self, method: str, params: Dict[str, Any]) -> Any:
        """One request; its ``result``, or :class:`PadFsError`."""
        reply = self.device.request(
            method, params, request_id=next(self._ids),
            timeout=TIMEOUT_SECONDS, match_id=True, standard=True,
        )
        time.sleep(GAP_SECONDS)
        if reply is None:
            raise PadFsError(f"{method}: the pad did not answer")
        if "error" in reply:
            raise PadFsError(f"{method}: {reply['error']}")
        return reply.get("result")

    def read(self, name: str) -> bytes:
        data = b""
        while True:
            result = self.call("fs.readbin", {"file": name, "offset": len(data), "len": CHUNK})
            if not isinstance(result, dict) or not isinstance(result.get("data"), str):
                raise PadFsError(f"fs.readbin {name}: unexpected reply {result!r}")
            chunk = base64.b64decode(result["data"])
            data += chunk
            if len(data) >= int(result.get("total_size", 0)) or not chunk:
                return data

    def write(self, name: str, data: bytes) -> None:
        """Write ``data`` as ``name`` in one uninterrupted chunk sequence."""
        offsets = range(0, max(len(data), 1), CHUNK)
        for offset in offsets:
            self.call("fs.writebin", {
                "file": name,
                "data": base64.b64encode(data[offset:offset + CHUNK]).decode("ascii"),
                "append": True,
                "offset": offset,
                "completed": offset == offsets[-1],
            })

    def checksum(self, name: str) -> Optional[str]:
        """The pad's SHA-1 of ``name``, or ``None`` if the file is absent."""
        entries = self.call("fs.list", {"checksum": True})
        for entry in entries if isinstance(entries, list) else []:
            if isinstance(entry, dict) and entry.get("name") == name:
                return entry.get("checksum")
        return None

    def write_verified(self, name: str, data: bytes) -> None:
        """Write, then prove the pad holds exactly ``data``."""
        self.write(name, data)
        expected = hashlib.sha1(data).hexdigest()
        actual = self.checksum(name)
        if (actual or "").lower() != expected:
            raise PadFsError(
                f"{name}: checksum after write is {actual}, expected {expected}"
            )


__all__ = ["CHUNK", "KEYMAP", "PadFs", "PadFsError", "SMART_ACTIONS"]
