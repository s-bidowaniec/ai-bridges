from __future__ import annotations

import json
import sys
import time

from .agy import AgyAdapter
from .base import AdapterRequest, AdapterResponse, BaseAdapter, ErrorCode
from .claude import ClaudeAdapter
from .codex import CodexAdapter
from .mock import MockAdapter
from .tmux_cli import TmuxCliAdapter
from .vibe import VibeAdapter

ADAPTER_REGISTRY: dict[str, type[BaseAdapter]] = {
    "mock": MockAdapter,
    "claude": ClaudeAdapter,
    "vibe": VibeAdapter,
    "codex": CodexAdapter,
    "agy": AgyAdapter,
}

# Adaptery, które potrafią pracować w trwałej sesji tmux (poza mockiem).
TMUX_CAPABLE = {"vibe", "claude", "codex", "agy"}


def dispatch(raw_payload: dict) -> AdapterResponse:
    req = AdapterRequest.from_dict(raw_payload)

    start = time.monotonic()
    if req.mode == "tmux" and req.adapter in TMUX_CAPABLE:
        adapter: BaseAdapter = TmuxCliAdapter(req.adapter)
    else:
        adapter_cls = ADAPTER_REGISTRY.get(req.adapter)
        if adapter_cls is None:
            return AdapterResponse(
                status=ErrorCode.EXEC_ERROR,
                error_message=f"Unknown adapter '{req.adapter}'. Registered: {sorted(ADAPTER_REGISTRY)}",
                peer=req.peer,
            )
        adapter = adapter_cls()
    response = adapter.execute(req)
    response.latency_ms = int((time.monotonic() - start) * 1000)
    if not response.peer:
        response.peer = req.peer
    return response


def main() -> None:
    try:
        raw_stdin = sys.stdin.read()
        payload = json.loads(raw_stdin)
    except (json.JSONDecodeError, OSError) as exc:
        err = AdapterResponse(
            status=ErrorCode.EXEC_ERROR,
            error_message=f"Failed to decode JSON request on stdin: {exc}",
        )
        sys.stdout.write(json.dumps(err.to_dict()) + "\n")
        sys.stdout.flush()
        return

    response = dispatch(payload)
    sys.stdout.write(json.dumps(response.to_dict()) + "\n")
    sys.stdout.flush()


if __name__ == "__main__":
    main()
