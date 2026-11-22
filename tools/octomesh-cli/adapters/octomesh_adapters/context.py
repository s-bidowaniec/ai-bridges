from __future__ import annotations

import subprocess
from pathlib import Path

MAX_DIFF_CHARS = 12_000


def gather_git_context(cwd: str | Path, max_chars: int = MAX_DIFF_CHARS) -> str:
    """Safely captures truncated `git status --short` and `git diff` for peer context."""
    repo_path = Path(cwd)
    if not repo_path.exists():
        return ""

    status_out = _run_git(["git", "status", "--short"], repo_path)
    diff_out = _run_git(["git", "diff", "HEAD"], repo_path)
    if not diff_out:
        diff_out = _run_git(["git", "diff"], repo_path)

    if not status_out and not diff_out:
        return ""

    sections: list[str] = ["--- GIT WORKSPACE CONTEXT ---"]
    if status_out:
        sections.append(f"Git Status:\n{status_out.strip()}")
    if diff_out:
        truncated = diff_out[:max_chars]
        if len(diff_out) > max_chars:
            truncated += f"\n... [diff truncated at {max_chars} chars]"
        sections.append(f"Git Diff:\n{truncated}")
    sections.append("--- END GIT CONTEXT ---")
    return "\n\n".join(sections)


def build_effective_prompt(prompt: str, cwd: str | Path, include_diff: bool, readonly: bool) -> str:
    parts: list[str] = []
    if readonly:
        parts.append(
            "[READONLY REVIEW MODE]: Analyze and provide feedback or patches in text only. "
            "Do NOT modify files on disk or run destructive commands."
        )
    parts.append(prompt)
    if include_diff:
        git_ctx = gather_git_context(cwd)
        if git_ctx:
            parts.append(git_ctx)
    return "\n\n".join(parts)


def _run_git(cmd: list[str], cwd: Path) -> str:
    try:
        proc = subprocess.run(
            cmd,
            cwd=cwd,
            capture_output=True,
            text=True,
            timeout=5,
            check=False,
        )
        if proc.returncode == 0:
            return proc.stdout
    except (OSError, subprocess.SubprocessError):
        pass
    return ""
