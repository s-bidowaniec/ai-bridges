from __future__ import annotations

import os
import stat
import textwrap
from pathlib import Path

import pytest

from octomesh_adapters import tmux_session
from octomesh_adapters.base import ErrorCode
from octomesh_adapters.runner import dispatch

FAKE_TMUX = r"""#!/usr/bin/env bash
# Fake tmux do testów: stan w $FAKE_TMUX_STATE, "agent" dopisuje REPLY na Enter.
set -u
STATE="${FAKE_TMUX_STATE:?set FAKE_TMUX_STATE}"
cmd="$1"; shift
orig_last="${@: -1}"
# wyciągnij -t <name> / -s <name>
name=""
while [ $# -gt 0 ]; do
  case "$1" in
    -t|-s) name="$2"; shift 2;;
    *) shift;;
  esac
done
mkdir -p "$STATE"
pane="$STATE/$name.pane"
case "$cmd" in
  has-session) [ -f "$pane" ] && exit 0 || exit 1;;
  new-session) echo "[launch]" > "$pane"; exit 0;;
  send-keys)
    if [ "$orig_last" = "Enter" ]; then
      printf '\nREPLY: ok\n>\n' >> "$pane"
    else
      printf '%s' "$orig_last" >> "$pane"
    fi
    exit 0;;
  capture-pane) cat "$pane"; exit 0;;
  kill-session) rm -f "$pane"; exit 0;;
  *) exit 0;;
esac
"""


@pytest.fixture()
def fake_tmux(tmp_path: Path, monkeypatch) -> Path:
    bin_path = tmp_path / "tmux"
    bin_path.write_text(FAKE_TMUX)
    bin_path.chmod(bin_path.stat().st_mode | stat.S_IEXEC)
    state = tmp_path / "state"
    state.mkdir()
    monkeypatch.setenv("FAKE_TMUX_STATE", str(state))
    monkeypatch.setattr(tmux_session, "DEFAULT_POLL_SECS", 0.05)
    monkeypatch.setattr(tmux_session, "DEFAULT_STABLE_SAMPLES", 2)
    monkeypatch.setattr(tmux_session, "STARTUP_WAIT_SECS", 0)
    return bin_path


def _req(bin_path: Path, **over) -> dict:
    base = {
        "adapter": "vibe",
        "peer": "testpeer",
        "command": "fake-agent",
        "prompt": "hello",
        "mode": "tmux",
        "session_prefix": "octotest",
        "tmux_bin": str(bin_path),
        "timeout_secs": 10,
        "cwd": ".",
    }
    base.update(over)
    return base


def test_tmux_adapter_ok_and_session_reused(fake_tmux: Path):
    resp = dispatch(_req(fake_tmux))
    assert resp.status == ErrorCode.OK, resp.error_message
    assert "REPLY: ok" in resp.output
    assert resp.peer == "testpeer"

    # drugie wywołanie = ta sama sesja tmux (kontekst pamiętany)
    resp2 = dispatch(_req(fake_tmux, prompt="again"))
    assert resp2.status == ErrorCode.OK
    state = Path(os.environ["FAKE_TMUX_STATE"])
    panes = list(state.glob("*.pane"))
    assert len(panes) == 1  # jedna sesja: octotest-testpeer


def test_tmux_adapter_missing_tmux_reports_cli_not_found(tmp_path: Path):
    resp = dispatch(_req(tmp_path / "no-such-tmux"))
    assert resp.status == ErrorCode.CLI_NOT_FOUND
    assert "tmux" in (resp.error_message or "")


def test_tmux_adapter_timeout_when_agent_silent(fake_tmux: Path, monkeypatch):
    # fake tmux, który NIE dopisuje odpowiedzi (symulacja milczenia agenta)
    silent = fake_tmux.parent / "tmux-silent"
    silent_script = textwrap.dedent(FAKE_TMUX).replace(
        """    if [ "$orig_last" = "Enter" ]; then
      printf '\\nREPLY: ok\\n>\\n' >> "$pane"
    else
      printf '%s' "$orig_last" >> "$pane"
    fi""",
        "    : # agent milczy — pane się nie zmienia",
    )
    silent.write_text(silent_script)
    silent.chmod(0o755)
    resp = dispatch(_req(silent, timeout_secs=1))
    assert resp.status == ErrorCode.TIMEOUT


def test_oneshot_still_default_for_mock():
    resp = dispatch(
        {
            "adapter": "mock",
            "peer": "mock-safe",
            "command": "mock",
            "prompt": "hi",
            "timeout_secs": 5,
        }
    )
    assert resp.status == ErrorCode.OK
    assert "[mock:mock-safe]" in resp.output
