"""Focus a Claude Code session that runs inside herdr, from an Agent Key.

herdr (https://herdr.dev) is a terminal workspace manager: every agent is a
pane on a herdr server, so the session's tty belongs to herdr's pty and no
terminal tab can be raised for it. herdr instead exposes a local socket API, and
pressing an Agent Key is ``agent.focus {target: <pane id>}`` - the same request
the house-of-herdr Codex Micro plugin (MIT, alasano/house-of-herdr) sends.

The pane is found by the Claude session id, which herdr records for every
agent it detects (``agent_session.value``), so two agents in one project
directory are never confused. The wire format: one connection per request, one
newline-terminated JSON line each way.
"""

from __future__ import annotations

import json
import os
import socket
import threading
import time
from pathlib import Path
from typing import TYPE_CHECKING, Any, Callable, Dict, List, Optional, Sequence

if TYPE_CHECKING:
    from freemicro.state.engine import AgentState, SessionState

TIMEOUT_SECONDS = 2.0


def socket_path() -> str:
    return os.environ.get("HERDR_SOCKET_PATH") or str(
        Path.home() / ".config" / "herdr" / "herdr.sock"
    )


def request(method: str, params: Optional[Dict[str, Any]] = None) -> Dict[str, Any]:
    """One herdr API call; its ``result``. Raises ``OSError`` or ``ValueError``."""
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.settimeout(TIMEOUT_SECONDS)
        sock.connect(socket_path())
        line = json.dumps({"id": "freemicro", "method": method, "params": params or {}})
        sock.sendall(line.encode("utf-8") + b"\n")
        reply = b""
        while not reply.endswith(b"\n"):
            chunk = sock.recv(65536)
            if not chunk:
                break
            reply += chunk
    parsed = json.loads(reply)
    if parsed.get("error"):
        raise ValueError(f"herdr {method}: {parsed['error']}")
    return parsed.get("result") or {}


def pane_for_session(session_id: str) -> Optional[str]:
    """The herdr pane running Claude session ``session_id``, if any."""
    for agent in request("agent.list").get("agents", []):
        if session_id in ((agent.get("agent_session") or {}).get("value"),
                          agent.get("pane_id")):
            return agent.get("pane_id")
    return None


def focus_session(session_id: str) -> bool:
    """Focus the herdr pane of ``session_id``. ``False`` if it cannot be found."""
    try:
        pane = pane_for_session(session_id)
        if not pane:
            return False
        request("agent.focus", {"target": pane})
        return True
    except (OSError, ValueError):
        return False


# ---------------------------------------------------------------------------
# Agent Keys: one key per herdr agent, as the house-of-herdr plugin does
# ---------------------------------------------------------------------------

def _herdr_state(status: Any) -> "AgentState":
    from freemicro.state.engine import AgentState

    return {"working": AgentState.WORKING, "blocked": AgentState.WAITING,
            "done": AgentState.DONE}.get(status, AgentState.IDLE)


def agent_sessions(sessions: Sequence["SessionState"]) -> List["SessionState"]:
    """The sessions the Agent Keys show: herdr's agents while herdr runs.

    One session per herdr agent, so a key stands for an agent rather than a
    project directory - two agents in one folder get two keys, and a folder an
    agent merely passed through gets none. Every key is lit from herdr's own
    status, so the pad matches herdr; an agent FreeMicro's hooks track keeps
    the rest of that record. Agents sharing a
    folder are told apart as ``<dir>#2``, ``<dir>#3`` in pane order, so the
    names - and the sticky keys built on them - stay put.

    When herdr is not running, ``sessions`` is returned unchanged.
    """
    from dataclasses import replace

    from freemicro.state.engine import SessionState

    try:
        agents = request("agent.list").get("agents", [])
    except (OSError, ValueError):
        return list(sessions)
    by_id = {s.session_id: s for s in sessions}
    seen: Dict[str, int] = {}
    out: List[SessionState] = []
    for agent in sorted(agents, key=lambda a: str(a.get("pane_id"))):
        cwd = str(agent.get("foreground_cwd") or agent.get("cwd") or "")
        if not cwd:
            continue
        seen[cwd] = seen.get(cwd, 0) + 1
        path = cwd if seen[cwd] == 1 else f"{cwd}#{seen[cwd]}"
        sid = (agent.get("agent_session") or {}).get("value") or str(agent.get("pane_id"))
        known = by_id.get(sid)
        # herdr's status, not the hook's: the pad must show what herdr shows.
        state = _herdr_state(agent.get("agent_status"))
        if known is not None:
            out.append(replace(known, state=state, cwd=path, term_program="herdr"))
        else:
            out.append(SessionState(session_id=sid, state=state,
                                    updated_at=time.time(), cwd=path, term_program="herdr"))
    return out


# ---------------------------------------------------------------------------
# Presets: the house-of-herdr plugin's key logic, ported
# ---------------------------------------------------------------------------

#: herdr's tab attention priority (``slots.ts: attention``).
_ATTENTION = {"blocked": 4, "done": 3, "working": 2, "idle": 1, "unknown": 0}

#: The dial steps through these; ``dial-mode`` cycles them. The plugin's third
#: mode, ``scroll``, is not ported.
DIAL_MODES = ("workspaces", "agents")
_DIAL_STATE = {"mode": DIAL_MODES[0], "last": 0.0}
#: A fast spin must not queue a dozen navigation calls (``controls.ts``).
DIAL_MIN_INTERVAL = 0.12


def _cycle(items: List[Dict[str, Any]], step: int) -> Optional[Dict[str, Any]]:
    """The item ``step`` away from the focused one, wrapping."""
    current = next((i for i, item in enumerate(items) if item.get("focused")), -1)
    if current == -1 or len(items) < 2:
        return None
    return items[(current + step) % len(items)]


def step_workspace(step: int) -> None:
    target = _cycle(request("workspace.list").get("workspaces", []), step)
    if target:
        request("workspace.focus", {"workspace_id": target["workspace_id"]})


def step_tab(step: int) -> None:
    workspace = next((w for w in request("workspace.list").get("workspaces", [])
                      if w.get("focused")), None)
    if not workspace:
        return
    target = _cycle(request("tab.list", {"workspace_id": workspace["workspace_id"]})
                    .get("tabs", []), step)
    if target:
        request("tab.focus", {"tab_id": target["tab_id"]})


def step_agent(step: int) -> None:
    """Cycle every agent, neediest first then most recently changed."""
    agents = sorted(
        request("agent.list").get("agents", []),
        key=lambda a: (-_ATTENTION.get(a.get("agent_status"), 0),
                       -int(a.get("state_change_seq") or 0)),
    )
    if not agents:
        return
    # The focused pane is often not an agent: start at the neediest then.
    target = _cycle(agents, step) if any(a.get("focused") for a in agents) else agents[0]
    if target:
        request("agent.focus", {"target": target["pane_id"]})


def send_to_focused_pane(method: str, params: Dict[str, Any]) -> None:
    pane = (request("pane.current").get("pane") or {}).get("pane_id")
    if pane:
        request(method, {"pane_id": pane, **params})


def _dial(step: int) -> None:
    now = time.monotonic()
    if now - _DIAL_STATE["last"] < DIAL_MIN_INTERVAL:
        return
    _DIAL_STATE["last"] = now
    (step_workspace if _DIAL_STATE["mode"] == "workspaces" else step_agent)(step)


def _dial_mode() -> None:
    mode = DIAL_MODES.index(str(_DIAL_STATE["mode"]))
    _DIAL_STATE["mode"] = DIAL_MODES[(mode + 1) % len(DIAL_MODES)]


def _call(method: str, params: Optional[Dict[str, Any]] = None) -> Callable[[], None]:
    """A preset that is one fixed herdr request."""
    def run() -> None:
        request(method, params)
    return run


PRESETS: Dict[str, Callable[[], None]] = {
    "tab-next": lambda: step_tab(1),
    "tab-prev": lambda: step_tab(-1),
    "tab-new": _call("tab.create", {"focus": True}),
    "workspace-next": lambda: step_workspace(1),
    "workspace-prev": lambda: step_workspace(-1),
    "agent-next": lambda: step_agent(1),
    "agent-prev": lambda: step_agent(-1),
    "zoom": _call("pane.zoom"),
    "pane-split-right": _call("pane.split", {"direction": "right", "focus": True}),
    "pane-split-down": _call("pane.split", {"direction": "down", "focus": True}),
    **{f"pane-{d}": _call("pane.focus_direction", {"direction": d})
       for d in ("left", "right", "up", "down")},
    "dial-next": lambda: _dial(1),
    "dial-prev": lambda: _dial(-1),
    "dial-mode": _dial_mode,
}


def run_in_background(work: Callable[[], None]) -> None:
    """Run one herdr call off the key path; a herdr that is down is a no-op."""
    def guarded() -> None:
        try:
            work()
        except (OSError, ValueError):
            pass
    threading.Thread(target=guarded, daemon=True).start()


__all__ = [
    "DIAL_MODES",
    "PRESETS",
    "agent_sessions",
    "focus_session",
    "pane_for_session",
    "request",
    "run_in_background",
    "send_to_focused_pane",
    "socket_path",
    "step_agent",
    "step_tab",
    "step_workspace",
]
