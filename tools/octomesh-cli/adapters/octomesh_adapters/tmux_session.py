"""Trwała sesja tmux dla peera — agent żyje w tmuxie między wywołaniami `octo ask`.

Wzorzec jak w vibe-matrix-rs v5: `new-session -d`, `send-keys -l <tekst>` + Enter,
`capture-pane -p -S -<history>`, odpowiedź = suffix po poprzednim capture
(stabilność = 3 identyczne próbki pod rząd).
"""

from __future__ import annotations

import os
import subprocess
import time

DEFAULT_HISTORY = 300
DEFAULT_POLL_SECS = 2.0
DEFAULT_STABLE_SAMPLES = 3
STARTUP_WAIT_SECS = 3.0


class TmuxError(Exception):
    pass


def _run(args: list[str], timeout: float = 30.0) -> str:
    try:
        proc = subprocess.run(
            args,
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
        )
    except subprocess.TimeoutExpired as exc:
        raise TmuxError(f"{' '.join(args)} timed out: {exc}") from exc
    except OSError as exc:
        raise TmuxError(f"{' '.join(args)} failed to start: {exc}") from exc
    if proc.returncode != 0:
        raise TmuxError(f"{' '.join(args)} exit {proc.returncode}: {proc.stderr.strip()}")
    return proc.stdout


class TmuxSession:
    def __init__(
        self,
        name: str,
        tmux_bin: str = "tmux",
        history: int = DEFAULT_HISTORY,
    ) -> None:
        self.name = name
        self.tmux_bin = tmux_bin
        self.history = history

    def _cmd(self, *args: str) -> list[str]:
        return [self.tmux_bin, *args]

    def exists(self) -> bool:
        proc = subprocess.run(
            self._cmd("has-session", "-t", self.name),
            capture_output=True,
            timeout=10,
            check=False,
        )
        return proc.returncode == 0

    def create(self, cwd: str, launch_cmd: str, env: dict[str, str] | None = None) -> None:
        """Tworzy sesję detached z komendą startową agenta (np. `vibe --yolo`)."""
        if self.exists():
            return
        args = [
            "new-session",
            "-d",
            "-s",
            self.name,
            "-c",
            cwd or ".",
            "-x",
            "200",
            "-y",
            "50",
            launch_cmd,
        ]
        run_env = {**os.environ, **(env or {})}
        try:
            proc = subprocess.run(
                self._cmd(*args),
                capture_output=True,
                text=True,
                timeout=30,
                check=False,
                env=run_env,
            )
        except (subprocess.TimeoutExpired, OSError) as exc:
            raise TmuxError(f"tmux new-session failed: {exc}") from exc
        if proc.returncode != 0:
            raise TmuxError(f"tmux new-session exit {proc.returncode}: {proc.stderr.strip()}")
        time.sleep(STARTUP_WAIT_SECS)

    def ensure(self, cwd: str, launch_cmd: str, env: dict[str, str] | None = None) -> None:
        if not self.exists():
            self.create(cwd, launch_cmd, env)

    def send(self, text: str) -> None:
        """Wysyła tekst literalnie + Enter (jedno wywołanie = jedna wiadomość)."""
        _run(self._cmd("send-keys", "-t", self.name, "-l", text))
        _run(self._cmd("send-keys", "-t", self.name, "Enter"))

    def capture(self) -> str:
        return _run(self._cmd("capture-pane", "-p", "-t", self.name, "-S", f"-{self.history}"))

    def kill(self) -> None:
        subprocess.run(
            self._cmd("kill-session", "-t", self.name),
            capture_output=True,
            timeout=10,
            check=False,
        )

    def wait_stable(
        self,
        prev: str,
        timeout_secs: float,
        poll_secs: float | None = None,
        stable_needed: int | None = None,
    ) -> str:
        """Polluje capture aż treść się ustabilizuje (albo timeout). Zwraca finalny capture."""
        poll_secs = poll_secs if poll_secs is not None else DEFAULT_POLL_SECS
        stable_needed = stable_needed if stable_needed is not None else DEFAULT_STABLE_SAMPLES
        last = prev
        stable = 0
        deadline = time.monotonic() + timeout_secs
        while True:
            time.sleep(poll_secs)
            if not self.exists():
                break  # sesja padła (agent się wyłączył)
            cur = self.capture()
            if cur == last:
                stable += 1
                if stable >= stable_needed or time.monotonic() >= deadline:
                    return cur
            else:
                stable = 0
                last = cur
            if time.monotonic() >= deadline:
                return last


def extract_new_output(prev: str, cur: str, max_lines: int = 80) -> str:
    """Nowa część ekranu = suffix po poprzednim capture; przy scrollu fallback do ostatnich linii."""
    if cur.startswith(prev):
        return cur[len(prev) :].strip()
    lines = cur.splitlines()
    start = max(0, len(lines) - max_lines)
    return "\n".join(lines[start:]).strip()
