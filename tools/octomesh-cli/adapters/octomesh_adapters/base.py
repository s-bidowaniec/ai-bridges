from __future__ import annotations

from abc import ABC, abstractmethod
from dataclasses import dataclass, field
from enum import Enum
from typing import Any


class ErrorCode(str, Enum):
    OK = "OK"
    RATE_LIMITED = "RATE_LIMITED"
    TIMEOUT = "TIMEOUT"
    AUTH_ERROR = "AUTH_ERROR"
    CLI_NOT_FOUND = "CLI_NOT_FOUND"
    EXEC_ERROR = "EXEC_ERROR"


@dataclass(slots=True)
class AdapterRequest:
    adapter: str
    peer: str
    command: str
    prompt: str
    include_diff: bool = False
    readonly: bool = False
    timeout_secs: int = 120
    extra_args: list[str] = field(default_factory=list)
    env: dict[str, str] = field(default_factory=dict)
    cwd: str = "."
    # Tryb pracy peera: "oneshot" (subprocess `-p` per zadanie) lub "tmux" (trwała sesja).
    mode: str = "oneshot"
    session_prefix: str = "octo"
    launch_args: list[str] = field(default_factory=list)
    tmux_bin: str = "tmux"

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> AdapterRequest:
        return cls(
            adapter=str(data.get("adapter", "")),
            peer=str(data.get("peer", "")),
            command=str(data.get("command", "")),
            prompt=str(data.get("prompt", "")),
            include_diff=bool(data.get("include_diff", False)),
            readonly=bool(data.get("readonly", False)),
            timeout_secs=int(data.get("timeout_secs", 120)),
            extra_args=[str(x) for x in data.get("extra_args", [])],
            env={str(k): str(v) for k, v in data.get("env", {}).items()},
            cwd=str(data.get("cwd", ".")),
            mode=str(data.get("mode", "oneshot")),
            session_prefix=str(data.get("session_prefix", "octo")),
            launch_args=[str(x) for x in data.get("launch_args", [])],
            tmux_bin=str(data.get("tmux_bin", "tmux")),
        )


@dataclass(slots=True)
class AdapterResponse:
    status: ErrorCode
    output: str = ""
    error_message: str | None = None
    peer: str = ""
    latency_ms: int | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "status": self.status.value,
            "output": self.output,
            "error_message": self.error_message,
            "peer": self.peer,
            "latency_ms": self.latency_ms,
        }


RATE_LIMIT_PATTERNS = (
    "rate limit",
    "rate_limit",
    "429",
    "too many requests",
    "quota exceeded",
    "insufficient_quota",
    "overloaded_error",
)

AUTH_ERROR_PATTERNS = (
    "unauthorized",
    "invalid api key",
    "authentication failed",
    "401",
    "not logged in",
)


def classify_cli_failure(returncode: int, stderr: str, stdout: str = "") -> ErrorCode:
    combined = f"{stderr}\n{stdout}".lower()
    if any(pat in combined for pat in RATE_LIMIT_PATTERNS):
        return ErrorCode.RATE_LIMITED
    if any(pat in combined for pat in AUTH_ERROR_PATTERNS):
        return ErrorCode.AUTH_ERROR
    return ErrorCode.EXEC_ERROR


class BaseAdapter(ABC):
    name: str = "base"

    @abstractmethod
    def execute(self, req: AdapterRequest) -> AdapterResponse:
        raise NotImplementedError
