from __future__ import annotations

import os
import shutil
import subprocess

from .base import (
    AdapterRequest,
    AdapterResponse,
    BaseAdapter,
    ErrorCode,
    classify_cli_failure,
)
from .context import build_effective_prompt


class AgyAdapter(BaseAdapter):
    """Headless subprocess wrapper for Antigravity CLI (`agy -p --dangerously-skip-permissions`).

    Auth: agy needs either a Google OAuth login (interactive, stored in
    ~/.gemini/antigravity-cli/) or GEMINI_API_KEY / GOOGLE_API_KEY in env.
    Without auth it exits non-zero with "authentication required" -> AUTH_ERROR.
    """

    name = "agy"

    def execute(self, req: AdapterRequest) -> AdapterResponse:
        binary = req.command or "agy"
        if shutil.which(binary) is None and not (os.path.isfile(binary) and os.access(binary, os.X_OK)):
            return AdapterResponse(
                status=ErrorCode.CLI_NOT_FOUND,
                error_message=f"CLI binary '{binary}' not found in PATH",
                peer=req.peer,
            )

        effective_prompt = build_effective_prompt(
            prompt=req.prompt,
            cwd=req.cwd,
            include_diff=req.include_diff,
            readonly=req.readonly,
        )

        cmd = [binary, "-p", effective_prompt, "--dangerously-skip-permissions"]
        cmd.extend(req.extra_args)

        merged_env = {**os.environ, **req.env}

        try:
            proc = subprocess.run(
                cmd,
                cwd=req.cwd,
                env=merged_env,
                capture_output=True,
                text=True,
                timeout=req.timeout_secs,
                check=False,
            )
        except subprocess.TimeoutExpired:
            return AdapterResponse(
                status=ErrorCode.TIMEOUT,
                error_message=f"Antigravity CLI timed out after {req.timeout_secs}s",
                peer=req.peer,
            )
        except OSError as exc:
            return AdapterResponse(
                status=ErrorCode.EXEC_ERROR,
                error_message=str(exc),
                peer=req.peer,
            )

        if proc.returncode != 0:
            combined = f"{proc.stderr}\n{proc.stdout}"
            code = classify_cli_failure(proc.returncode, proc.stderr, proc.stdout)
            lowered = combined.lower()
            if "authentication required" in lowered or "not logged in" in lowered or "oauth" in lowered:
                code = ErrorCode.AUTH_ERROR
            return AdapterResponse(
                status=code,
                output=proc.stdout.strip(),
                error_message=(proc.stderr.strip() or f"Exited with status {proc.returncode}"),
                peer=req.peer,
            )

        return AdapterResponse(
            status=ErrorCode.OK,
            output=proc.stdout.strip(),
            peer=req.peer,
        )
