#!/usr/bin/env python3
"""Podpina już otwarte okno terminala z agentem (konsole) do sesji tmux, którą czyta mostek.

Użycie:
  attach-window.py <PID> [nazwa_sesji] [--force]

  - PID          proces agenta w otwartym oknie (znajdź: pgrep -fa 'codex|vibe|claude|agy')
  - nazwa_sesji  domyślnie <TMUX_PREFIX>-<agent-z-PID> (np. vibe-matrix-rs-codex),
                 żeby mostek (pokój tego agenta) go użył; albo własna nazwa + TMUX_SESSION w env
  - --force      zabij istniejącą sesję o tej nazwie przed podpięciem

Wymaga: reptyr i tmux w PATH (np. ~/.local/bin), ptrace_scope=0.
Po podpięciu okno konsole przestaje pokazywać agenta — patrzysz przez `tmux attach -t <sesja>`
(TMUX_TMPDIR jak w env mostka, domyślnie ~/.local/state/vibe-matrix-rs/tmux).
"""
from __future__ import annotations

import os
import shutil
import subprocess
import sys
import time

HOME = os.path.expanduser("~")
TMUX_TMPDIR = os.environ.get("TMUX_TMPDIR", f"{HOME}/.local/state/vibe-matrix-rs/tmux")


def run(cmd: list[str], **kw) -> subprocess.CompletedProcess:
    env = {**os.environ, "TMUX_TMPDIR": TMUX_TMPDIR}
    return subprocess.run(cmd, capture_output=True, text=True, env=env, **kw)


def main() -> int:
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    force = "--force" in sys.argv
    if not args:
        print(__doc__)
        return 2
    pid = int(args[0])
    session = args[1] if len(args) > 1 else None

    reptyr = shutil.which("reptyr") or f"{HOME}/.local/bin/reptyr"
    tmux = shutil.which("tmux") or f"{HOME}/.local/bin/tmux"
    for b in (reptyr, tmux):
        if not os.path.exists(b):
            print(f"BRAK {b} — zainstaluj reptyr/tmux do ~/.local/bin", file=sys.stderr)
            return 1

    try:
        cwd = os.readlink(f"/proc/{pid}/cwd")
        cmdline = open(f"/proc/{pid}/cmdline").read().replace("\0", " ").strip()
    except OSError:
        print(f"PID {pid} nie istnieje", file=sys.stderr)
        return 1
    print(f"PID {pid}: {cmdline[:100]}  (cwd={cwd})")

    if session is None:
        prefix = os.environ.get("TMUX_PREFIX", "vibe-matrix-rs")
        base = os.path.basename(cmdline.split()[0]) if cmdline else "agent"
        session = f"{prefix}-{base}"
    print(f"sesja tmux: {session}  (TMUX_TMPDIR={TMUX_TMPDIR})")

    has = run([tmux, "has-session", "-t", session])
    if has.returncode == 0:
        if not force:
            print(f"sesja {session} już istnieje — użyj --force żeby ją zastąpić", file=sys.stderr)
            return 1
        run([tmux, "kill-session", "-t", session])

    # sesja, w której jako komenda leci reptyr — target dostaje pty tego panelu
    r = run([tmux, "new-session", "-d", "-s", session, "-c", cwd, "-x", "200", "-y", "50",
             f"{reptyr} -T {pid}"])
    if r.returncode != 0:
        print(f"tmux new-session FAIL: {r.stderr.strip()}", file=sys.stderr)
        return 1
    time.sleep(2)
    ls = run([tmux, "ls"])
    if session not in ls.stdout:
        print(f"sesja {session} zniknęła (reptyr mógł paść) — tmux ls:\n{ls.stdout}{ls.stderr}", file=sys.stderr)
        return 1
    cap = run([tmux, "capture-pane", "-p", "-t", session, "-S", "-50"])
    tail = "\n".join([ln for ln in cap.stdout.splitlines() if ln.strip()][-5:])
    print(f"OK — proces {pid} jest w sesji {session}.\n--- ekran:\n{tail}\n")
    print(f"Podłącz się:  TMUX_TMPDIR={TMUX_TMPDIR} tmux attach -t {session}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
