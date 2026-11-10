"""``freemicro keymap pull|push`` - edit a Creator Micro 2's layers without Input.

``pull`` saves the pad's ``keymap.json`` as readable JSON. ``push`` writes an
edited copy back. A push is the one FreeMicro operation that can damage the pad
(two hosts talking to it during a write has bricked a unit), so it runs behind
these gates, in order:

1. Refuse while Work Louder's Input app or the ChatGPT app runs - both talk to
   the pad on the same channel.
2. Pause the FreeMicro daemon (launchd ``bootout``) and take the pad lock, so
   this process is the pad's only host. The daemon is resumed afterwards,
   whatever happens.
3. Back up the pad's current file to ``~/.freemicro/keymap-backups/``.
4. Write in one uninterrupted sequence, then verify the pad's SHA-1, then ask
   the pad for its status.

``push --test`` writes the pad's own current bytes back unchanged: the safe
first run that proves the whole path on this pad before any real change.
"""

from __future__ import annotations

import json
import subprocess
import time
from pathlib import Path
from typing import Any, Callable, List, Optional

from freemicro.config import config_home
from freemicro.device.padfs import KEYMAP, PadFs, PadFsError

#: Allow the firmware to load a new keymap before the status check.
SETTLE_SECONDS = 2.5


def backup_dir() -> Path:
    return config_home() / "keymap-backups"


def _running(pattern: str) -> bool:
    return subprocess.run(["pgrep", "-f", pattern], capture_output=True).returncode == 0


def other_hosts() -> List[str]:
    """Apps that talk to the pad on its vendor channel and are running now."""
    found = []
    if _running("input.app/Contents/MacOS/input"):
        found.append("Work Louder Input")
    if _running("ChatGPT.app/Contents/MacOS/ChatGPT"):
        found.append("ChatGPT")
    return found


def encode(keymap: Any) -> bytes:
    """The bytes Input would write: compact JSON, non-ASCII escaped."""
    return json.dumps(keymap, separators=(",", ":"), ensure_ascii=True).encode("ascii")


def load_edit(path: Path) -> bytes:
    """Read an edited keymap and refuse anything that is not plausibly one."""
    keymap = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(keymap, dict) or not isinstance(keymap.get("profiles"), list) \
            or not keymap["profiles"]:
        raise ValueError(f"{path} is not a keymap: it needs a non-empty \"profiles\" list")
    return encode(keymap)


def with_pad(work: Callable[[PadFs], int], say: Callable[[str], None] = print) -> int:
    """Run ``work`` as the pad's only host: daemon paused, lock held, pad open."""
    from freemicro import daemon
    from freemicro.device import open_device

    paused = daemon.pause()
    if paused:
        say("Paused the FreeMicro daemon.")
    lock = daemon.PadLock(role="keymap")
    try:
        deadline = time.monotonic() + 10
        while not lock.acquire():
            if time.monotonic() > deadline:
                say(f"{daemon.describe_holder(daemon.lock_holder() or {})} has the pad. "
                    "Stop it and try again.")
                return 1
            time.sleep(0.25)
        device = open_device()
        if device is None:
            say("The pad is not connected (or Input Monitoring is not granted).")
            return 1
        try:
            if not getattr(device, "is_creator_micro_2", False):
                say("This pad is not a Creator Micro 2; its keymap is not editable here.")
                return 1
            return work(PadFs(device))
        finally:
            device.close()
    finally:
        lock.release()
        if paused:
            daemon.resume()
            say("Resumed the FreeMicro daemon.")


def pull(out: Path, say: Callable[[str], None] = print) -> int:
    def work(fs: PadFs) -> int:
        keymap = json.loads(fs.read(KEYMAP))
        out.write_text(json.dumps(keymap, indent=2, ensure_ascii=False) + "\n",
                       encoding="utf-8")
        say(f"Saved the pad's keymap to {out}")
        return 0
    return with_pad(work, say)


def push(source: Optional[Path], test: bool, say: Callable[[str], None] = print) -> int:
    hosts = other_hosts()
    if hosts:
        say(f"Refusing to write: {' and '.join(hosts)} is running and talks to the pad.\n"
            "  Quit it first. Two hosts during a write can brick the pad.")
        return 1
    new = None if test else load_edit(source)

    def work(fs: PadFs) -> int:
        current = fs.read(KEYMAP)
        backup_dir().mkdir(parents=True, exist_ok=True)
        backup = backup_dir() / f"{time.strftime('%Y%m%d-%H%M%S')}-{KEYMAP}"
        backup.write_bytes(current)
        say(f"Backed up the pad's keymap to {backup}")
        data = current if test else new
        if not test and data == current:
            say("The pad already has this keymap. Nothing written.")
            return 0
        say(f"Writing {len(data)} bytes ({'unchanged - test write' if test else 'new keymap'})…")
        try:
            fs.write_verified(KEYMAP, data)
        except PadFsError as exc:
            say(f"Write FAILED: {exc}\n  Restore with:  freemicro keymap push {backup}")
            return 1
        time.sleep(SETTLE_SECONDS)
        status = fs.device.self_test(timeout=3.0)
        if not status:
            say("Written and verified, but the pad did not answer a status check.\n"
                f"  If keys misbehave, restore with:  freemicro keymap push {backup}")
            return 1
        say(f"Written and verified (SHA-1 matches). Pad on layer "
            f"{status.get('layer_index')}, firmware {status.get('version')}.")
        return 0
    return with_pad(work, say)


__all__ = ["backup_dir", "encode", "load_edit", "other_hosts", "pull", "push"]
