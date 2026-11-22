from __future__ import annotations

from .base import AdapterRequest, AdapterResponse, BaseAdapter, ErrorCode
from .context import build_effective_prompt


class MockAdapter(BaseAdapter):
    """Deterministic test adapter supporting offline failover simulation."""

    name = "mock"

    def execute(self, req: AdapterRequest) -> AdapterResponse:
        lower_prompt = req.prompt.lower()
        flags = set(req.extra_args)

        if "--always-rate-limit" in flags or (
            "--simulate-rate-limit-on-keyword" in flags and "rate_limit" in lower_prompt
        ):
            return AdapterResponse(
                status=ErrorCode.RATE_LIMITED,
                error_message=f"Simulated 429 rate limit on peer '{req.peer}'",
                peer=req.peer,
            )

        if "--always-timeout" in flags or "simulate_timeout" in lower_prompt:
            return AdapterResponse(
                status=ErrorCode.TIMEOUT,
                error_message=f"Simulated timeout after {req.timeout_secs}s on peer '{req.peer}'",
                peer=req.peer,
            )

        if "--always-auth-error" in flags or "simulate_auth_error" in lower_prompt:
            return AdapterResponse(
                status=ErrorCode.AUTH_ERROR,
                error_message=f"Simulated authentication failure on peer '{req.peer}'",
                peer=req.peer,
            )

        effective_prompt = build_effective_prompt(
            prompt=req.prompt,
            cwd=req.cwd,
            include_diff=req.include_diff,
            readonly=req.readonly,
        )

        return AdapterResponse(
            status=ErrorCode.OK,
            output=f"[mock:{req.peer}] Handled prompt ({len(effective_prompt)} chars): {req.prompt}",
            peer=req.peer,
        )
