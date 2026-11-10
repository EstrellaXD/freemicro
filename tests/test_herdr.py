"""Tests for the herdr presets ported from the house-of-herdr plugin."""

from __future__ import annotations

import pytest

from freemicro import herdr

AGENTS = [
    {"pane_id": "idle", "agent_status": "idle", "state_change_seq": 9},
    {"pane_id": "blocked", "agent_status": "blocked", "state_change_seq": 1},
    {"pane_id": "done-old", "agent_status": "done", "state_change_seq": 2},
    {"pane_id": "done-new", "agent_status": "done", "state_change_seq": 5},
]


@pytest.fixture
def herdr_api(monkeypatch):
    calls, focused = [], {"pane": None}

    def fake(method, params=None):
        calls.append((method, params))
        if method == "agent.list":
            return {"agents": [dict(a, focused=a["pane_id"] == focused["pane"]) for a in AGENTS]}
        if method == "workspace.list":
            return {"workspaces": [{"workspace_id": "w1", "focused": True}]}
        if method == "tab.list":
            return {"tabs": [{"tab_id": "t1"}, {"tab_id": "t2", "focused": True}]}
        return {}

    monkeypatch.setattr(herdr, "request", fake)
    monkeypatch.setattr(herdr, "_DIAL_STATE", {"mode": "workspaces", "last": 0.0})
    return calls, focused


@pytest.mark.parametrize("focused, step, expected", [
    (None, 1, "blocked"),        # no agent focused: start at the neediest
    ("blocked", 1, "done-new"),  # then by attention, newest first
    ("done-new", 1, "done-old"),
    ("idle", 1, "blocked"),      # and wrap
    ("blocked", -1, "idle"),
])
def test_step_agent_follows_herdr_attention_order(herdr_api, focused, step, expected):
    calls, state = herdr_api
    state["pane"] = focused
    herdr.step_agent(step)
    assert calls[-1] == ("agent.focus", {"target": expected})


def test_tab_next_wraps_within_the_focused_workspace(herdr_api):
    calls, _ = herdr_api
    herdr.PRESETS["tab-next"]()
    assert calls[-1] == ("tab.focus", {"tab_id": "t1"})


def test_dial_mode_switches_the_dial_from_workspaces_to_agents(herdr_api):
    calls, _ = herdr_api
    herdr.PRESETS["dial-mode"]()
    herdr.PRESETS["dial-next"]()
    assert calls[-1] == ("agent.focus", {"target": "blocked"})


def test_agent_sessions_gives_one_key_per_herdr_agent(monkeypatch):
    from freemicro.state.engine import AgentState, SessionState

    hooked = SessionState(session_id="s1", state=AgentState.WAITING, updated_at=1.0,
                          cwd="/code/old-subdir")
    gone = SessionState(session_id="s9", state=AgentState.DONE, updated_at=1.0, cwd="/code/x")
    monkeypatch.setattr(herdr, "request", lambda method, params=None: {"agents": [
        {"pane_id": "w1:p2", "cwd": "/code/api", "agent_status": "working",
         "agent_session": {"value": "codex-1"}},
        {"pane_id": "w1:p1", "foreground_cwd": "/code/api", "agent_status": "idle",
         "agent_session": {"value": "s1"}},
    ]})
    out = herdr.agent_sessions([hooked, gone])
    assert [(s.session_id, s.cwd, s.state) for s in out] == [
        ("s1", "/code/api", AgentState.IDLE),            # herdr's status wins
        ("codex-1", "/code/api#2", AgentState.WORKING),  # no hook record
    ]
    assert all(s.term_program == "herdr" for s in out)


def test_agent_sessions_is_a_no_op_without_herdr():
    from freemicro.state.engine import AgentState, SessionState

    sessions = [SessionState(session_id="s1", state=AgentState.IDLE, updated_at=1.0, cwd="/a")]
    assert herdr.agent_sessions(sessions) == sessions
