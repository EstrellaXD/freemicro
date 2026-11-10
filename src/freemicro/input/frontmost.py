"""Cheap, cached lookup of the frontmost macOS app, for per-app pad profiles.

Per-app profiles (see :mod:`freemicro.padconfig`) let one pad key mean different
things in different applications, which means resolving a press has to know what
is frontmost. The hard constraint is that this must add **no** perceptible
latency to a key press: the OS must never be queried synchronously on the key
path.

Two pieces keep that promise:

* :func:`detect_frontmost` is the actual, in-process lookup. It reads
  ``NSWorkspace.sharedWorkspace.frontmostApplication.localizedName`` straight
  through the Objective-C runtime with :mod:`ctypes` - no PyObjC, no subprocess,
  no dependency on anything outside the standard library, exactly like the rest
  of FreeMicro's core. It never raises: every failure path (not on macOS, the
  runtime not loadable, nothing frontmost) degrades to ``None``, which the
  resolver reads as "no profile, use the base bindings".

* :class:`FrontmostWatcher` is what the run loop actually calls. It caches the
  last name and only lets the OS be re-read once every ``interval`` seconds, so
  the run loop can poll it every tick without turning a human-paced app switch
  into a stream of lookups. The bridge reads the *cached* value on the key path;
  the watcher does the OS work on the tick. That split is the whole point.

The provider is injectable so the entire test suite runs without touching the
real OS - the same discipline the backends and clocks use everywhere else.
"""

from __future__ import annotations

import sys
import time
from typing import Callable, Optional, Tuple

#: Monotonic, never the wall clock: a throttle window must survive an NTP step.
_monotonic = time.monotonic


def _detect_via_appkit() -> Optional[Tuple[str, str]]:
    """Ask AppKit for the frontmost app's name and bundle id via the objc runtime.

    Returns ``(localised name, bundle id)`` - the bundle id is ``""`` for the
    rare app without one - or ``None`` if anything at all is not exactly
    right. Guarded to the last line: this runs inside a live key bridge and a
    background daemon, so a wrong ``ctypes`` signature must degrade to "no
    profile", never take the process down.
    """
    import ctypes
    import ctypes.util

    objc_path = ctypes.util.find_library("objc")
    if not objc_path:
        return None
    objc = ctypes.CDLL(objc_path)
    # Loading AppKit is what makes the NSWorkspace class exist in this process.
    appkit_path = ctypes.util.find_library("AppKit")
    if not appkit_path:
        return None
    ctypes.CDLL(appkit_path)

    objc.objc_getClass.restype = ctypes.c_void_p
    objc.objc_getClass.argtypes = [ctypes.c_char_p]
    objc.sel_registerName.restype = ctypes.c_void_p
    objc.sel_registerName.argtypes = [ctypes.c_char_p]

    def msg(receiver: Optional[int], selector: bytes, restype=ctypes.c_void_p):
        """One objc_msgSend call with an explicit result type.

        A fresh prototype per call is deliberate: ``objc_msgSend`` is variadic,
        so ctypes needs the exact signature every time, and reusing one function
        object with a mutated ``restype`` is how you get a silent misread.
        """
        if not receiver:
            return None
        send = ctypes.CDLL(objc_path)["objc_msgSend"]
        send.restype = restype
        send.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
        return send(ctypes.c_void_p(receiver), objc.sel_registerName(selector))

    workspace_cls = objc.objc_getClass(b"NSWorkspace")
    if not workspace_cls:
        return None
    shared = msg(workspace_cls, b"sharedWorkspace")
    app = msg(shared, b"frontmostApplication")

    def text(selector: bytes) -> str:
        utf8 = msg(msg(app, selector), b"UTF8String", restype=ctypes.c_char_p)
        try:
            return utf8.decode("utf-8") if utf8 else ""
        except (UnicodeDecodeError, AttributeError):
            return ""

    name = text(b"localizedName")
    return (name, text(b"bundleIdentifier")) if name else None


def detect_frontmost() -> Optional[str]:
    """The frontmost app's localised name, or ``None`` if it cannot be read.

    macOS only, and best-effort by design: on any other platform, or if the
    Objective-C runtime cannot be reached, this returns ``None`` and the caller
    treats that as "no profile is active". It never raises.
    """
    app = detect_frontmost_app()
    return app[0] if app else None


def detect_frontmost_app() -> Optional[Tuple[str, str]]:
    """``(name, bundle id)`` of the frontmost app, or ``None``. Never raises.

    The bundle id is what the pad's firmware matches ``linkedApps`` on.
    """
    if sys.platform != "darwin":
        return None
    try:
        return _detect_via_appkit()
    except Exception:  # noqa: BLE001 - a lookup must never break the key bridge
        return None


class FrontmostWatcher:
    """A throttled, cached view of the frontmost app for the run loop.

    :meth:`poll` is what the run loop calls on its tick. It re-reads the OS at
    most once every ``interval`` seconds and otherwise hands back the cached
    name, so polling every tick costs at most one lookup per window. The bridge
    never calls this on the key path - it reads the name the last poll cached.

    Both the clock and the provider are injectable, so tests drive the exact
    timing from data and never touch the real OS.
    """

    def __init__(
        self,
        provider: Optional[Callable[[], Optional[str]]] = None,
        *,
        clock: Optional[Callable[[], float]] = None,
        interval: float = 0.3,
    ) -> None:
        self.provider = provider or detect_frontmost
        self.clock = clock or _monotonic
        #: The throttle window in seconds. See
        #: :data:`freemicro.padconfig.DEFAULT_PROFILE_POLL_MS`.
        self.interval = max(0.0, interval)
        self._name: Optional[str] = None
        self._last_at: Optional[float] = None

    @property
    def current(self) -> Optional[str]:
        """The last cached name, without touching the OS."""
        return self._name

    def poll(self) -> Optional[str]:
        """Return the frontmost app name, re-reading the OS at most once/window.

        A provider that raises is swallowed and read as ``None`` - the same
        "no profile" degradation :func:`detect_frontmost` already promises, kept
        here too so an *injected* provider cannot break the loop either.
        """
        now = self.clock()
        if self._last_at is not None and now - self._last_at < self.interval:
            return self._name
        self._last_at = now
        try:
            name = self.provider()
        except Exception:  # noqa: BLE001 - see the class docstring
            name = None
        self._name = name or None
        return self._name



#: The vendor method that tells the pad which app is frontmost. On a Creator
#: Micro 2 the firmware then switches to the layer whose ``linkedAppId`` names
#: that bundle id in ``keymap.json``'s ``linkedApps``; an unlinked app keeps the
#: current layer. Verified on firmware 0.6.3.
METHOD_FOCUSED_APP = "host.focused_app"


class PadFocusForwarder:
    """Tell a Creator Micro 2 which app is frontmost, as Work Louder's Input does.

    This is what makes the pad's own per-app layers switch without Input
    running. Like Input it re-reads the OS once per ``interval`` and sends only
    on a change - plus once on every (re)connect, which Input forgets to do.
    """

    def __init__(
        self,
        provider: Optional[Callable[[], Optional[Tuple[str, str]]]] = None,
        *,
        clock: Optional[Callable[[], float]] = None,
        interval: float = 1.0,
    ) -> None:
        self.provider = provider or detect_frontmost_app
        self.clock = clock or _monotonic
        self.interval = interval
        self._device = None
        self._sent: Optional[Tuple[str, str]] = None
        self._last_at: Optional[float] = None

    def attach(self, device) -> None:
        """Forward to ``device`` from now on (``None`` on a drop)."""
        self._device = device if getattr(device, "is_creator_micro_2", False) else None
        self._sent = None
        self._last_at = None

    def poll(self) -> Optional[Tuple[str, str]]:
        """Send the frontmost app if it changed; return what was sent, if anything."""
        if self._device is None:
            return None
        now = self.clock()
        if self._last_at is not None and now - self._last_at < self.interval:
            return None
        self._last_at = now
        app = self.provider()
        if not app or not app[1] or app == self._sent:
            return None
        try:
            self._device.send({
                "m": METHOD_FOCUSED_APP,
                "p": {"appName": app[0], "process": app[1]},
                "id": 1,
            })
        except Exception:  # noqa: BLE001 - a dropped pad must not kill the loop
            return None
        self._sent = app
        return app


__all__ = [
    "METHOD_FOCUSED_APP",
    "FrontmostWatcher",
    "PadFocusForwarder",
    "detect_frontmost",
    "detect_frontmost_app",
]
