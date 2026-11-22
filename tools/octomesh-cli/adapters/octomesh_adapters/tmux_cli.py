"""Adapter tmux: peer jako żyjący agent w stałej sesji tmux (zamiast one-shot `-p`).

Sesja `<session_prefix>-<peer>` przetrwa między wywołaniami `octo ask` — agent pamięta
kontekst rozmowy. Bridge/adapter tylko: send-keys + capture-pane + wait_stable.
"""

from __future__ import annotations

import shutil

from .base import (
    AdapterRequest,
    AdapterResponse,
    BaseAdapter,
    ErrorCode,
    classify_cli_failure,
)
from .context import build_effective_prompt
from .tmux_session import TmuxError, TmuxSession, extract_new_output

# Domyślne argumenty startowe per adapter (launch w interaktywnym CLI w tmux).
DEFAULT_LAUNCH_ARGS: dict[str, list[str]] = {
    "vibe": ["--yolo"],
    "claude": [],
    "codex": [],
    "agy": ["--dangerously-skip-permissions"],
}


class TmuxCliAdapter(BaseAdapter):
    """Generyczny adapter tmux dla CLI agentów (vibe/codex/claude/agy)."""

    name = "tmux"

    def __init__(self, adapter_name: str = "tmux") -> None:
        self.name = adapter_name

    def execute(self, req: AdapterRequest) -> AdapterResponse:
        tmux_bin = req.tmux_bin or "tmux"
        if shutil.which(tmux_bin) is None:
            return AdapterResponse(
                status=ErrorCode.CLI_NOT_FOUND,
                error_message=f"tmux binary '{tmux_bin}' not found in PATH (tryb tmux wymaga tmux)",
                peer=req.peer,
            )

        launch_args = req.launch_args or DEFAULT_LAUNCH_ARGS.get(req.adapter, [])
        launch_cmd = " ".join([req.command or req.adapter, *launch_args]).strip()
        session_name = f"{req.session_prefix}-{req.peer}"
        sess = TmuxSession(session_name, tmux_bin=tmux_bin)

        effective_prompt = build_effective_prompt(
            prompt=req.prompt,
            cwd=req.cwd,
            include_diff=req.include_diff,
            readonly=req.readonly,
        )

        try:
            sess.ensure(req.cwd, launch_cmd, env=req.env)
            baseline = sess.capture()
            sess.send(effective_prompt)
            final = sess.wait_stable(baseline, timeout_secs=float(req.timeout_secs))
        except TmuxError as exc:
            code = classify_cli_failure(1, str(exc))
            return AdapterResponse(
                status=code,
                error_message=f"tmux error: {exc}",
                peer=req.peer,
            )

        if not sess.exists():
            return AdapterResponse(
                status=ErrorCode.EXEC_ERROR,
                error_message=f"sesja tmux '{session_name}' zakończyła się podczas pracy agenta",
                peer=req.peer,
            )

        output = extract_new_output(baseline, final)
        # timeout: agent mielił do końca — zwróć co mamy, ale oznacz
        timed_out = final == baseline or not output
        if timed_out:
            return AdapterResponse(
                status=ErrorCode.TIMEOUT,
                output=output,
                error_message=f"Agent '{req.peer}' nie odpowiedział w {req.timeout_secs}s (tmux session '{session_name}')",
                peer=req.peer,
            )

        code = classify_cli_failure(0, "", output)
        if code != ErrorCode.EXEC_ERROR and code != ErrorCode.OK:
            # wzorce rate-limit/auth w odpowiedzi agenta → failover w mesh
            return AdapterResponse(
                status=code,
                output=output,
                error_message=f"agent zgłosił {code.value}",
                peer=req.peer,
            )

        return AdapterResponse(
            status=ErrorCode.OK,
            output=output,
            peer=req.peer,
        )
