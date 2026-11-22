from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

from octomesh_adapters.base import ErrorCode, classify_cli_failure
from octomesh_adapters.runner import dispatch

ADAPTERS_ROOT = Path(__file__).resolve().parent.parent


def test_mock_adapter_ok_and_readonly_context():
    resp = dispatch(
        {
            "adapter": "mock",
            "peer": "mock-safe",
            "command": "mock",
            "prompt": "Review architecture",
            "include_diff": False,
            "readonly": True,
            "timeout_secs": 5,
            "extra_args": [],
        }
    )
    assert resp.status == ErrorCode.OK
    assert resp.peer == "mock-safe"
    assert "[mock:mock-safe]" in resp.output
    assert resp.latency_ms is not None


def test_mock_adapter_rate_limit_detection():
    resp = dispatch(
        {
            "adapter": "mock",
            "peer": "mock-fast",
            "command": "mock",
            "prompt": "Please simulate rate_limit failure",
            "extra_args": ["--simulate-rate-limit-on-keyword"],
        }
    )
    assert resp.status == ErrorCode.RATE_LIMITED
    assert resp.error_message is not None
    assert "429" in resp.error_message


def test_classify_cli_failure_patterns():
    assert classify_cli_failure(1, "Error: HTTP 429 Too Many Requests") == ErrorCode.RATE_LIMITED
    assert classify_cli_failure(1, "APIError: insufficient_quota") == ErrorCode.RATE_LIMITED
    assert classify_cli_failure(1, "401 Unauthorized: invalid api key") == ErrorCode.AUTH_ERROR
    assert classify_cli_failure(2, "Segmentation fault") == ErrorCode.EXEC_ERROR


def test_runner_stdio_json_contract():
    payload = json.dumps(
        {
            "adapter": "mock",
            "peer": "stdio-peer",
            "command": "mock",
            "prompt": "Hello via stdin",
        }
    )
    proc = subprocess.run(
        [sys.executable, "-m", "octomesh_adapters.runner"],
        cwd=ADAPTERS_ROOT,
        input=payload,
        capture_output=True,
        text=True,
        check=True,
    )
    data = json.loads(proc.stdout.strip())
    assert data["status"] == "OK"
    assert data["peer"] == "stdio-peer"
    assert "Hello via stdin" in data["output"]
