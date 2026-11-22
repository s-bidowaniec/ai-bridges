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


class VibeAdapter(BaseAdapter):
    """Headless subprocess wrapper for Mistral Vibe CLI (`vibe --prompt`)."""

    name = "vibe"

    def execute(self, req: AdapterRequest) -> AdapterResponse:
        binary = req.command or "vibe"
        if shutil.which(binary) is None:
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

        cmd = [binary, "--prompt", effective_prompt]
        if req.readonly:
            cmd.append("--plan")
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
                error_message=f"Mistral Vibe CLI timed out after {req.timeout_secs}s",
                peer=req.peer,
            )
        except OSError as exc:
            return AdapterResponse(
                status=ErrorCode.EXEC_ERROR,
                error_message=str(exc),
                peer=req.peer,
            )

        if proc.returncode != 0:
            code = classify_cli_failure(proc.returncode, proc.stderr, proc.stdout)
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
