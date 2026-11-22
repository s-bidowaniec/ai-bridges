# vibe-matrix-rs (Rust MVP — Faza 0+1 z docs/ROADMAP-RUST.md)

Mostek Matrix room ↔ `vibe -p --yolo` (headless), przepisany z `bridges/vibe-matrix` (Python).

## Co działa w MVP
- sync long-poll `/_matrix/client/v3/sync` (reqwest + Bearer, bez matrix-sdk — szybszy build)
- allowlist nadawców (`MATRIX_ALLOWED_USERS`), własne wiadomości bota ignorowane
- dedup eventów przez plik stanu (`STATE_FILE`, nadpisywany set — docelowo `redb`)
- reakcje 👀 / ✅ / ❌ na wiadomość
- subprocess `vibe -p <msg> --yolo` z `env_clear()` + whitelist (`VIBE_ENV_KEYS`), `kill_on_drop`, timeout
- czyszczenie ANSI z odpowiedzi TUI (`clean_ansi`), limit 3800 znaków odpowiedzi
- systemd unit z podstawowym hardeningiem

## Env (/etc/vibe-matrix-rs.env, chmod 600)
```
MATRIX_HOMESERVER=https://matrix.owl-post.eu
MATRIX_ACCESS_TOKEN=...
MATRIX_ROOM_ID=!....:matrix.owl-post.eu
MATRIX_BOT_USER_ID=@vibe-rs:matrix.owl-post.eu
MATRIX_ALLOWED_USERS=@stanislaw:matrix.owl-post.eu
VIBE_CMD=vibe
VIBE_TIMEOUT_SECS=600
SYNC_TIMEOUT_MS=30000
STATE_FILE=/var/lib/vibe-matrix-rs/seen.txt
VIBE_ENV_KEYS=MISTRAL_API_KEY
```

## Build & run (na VPS)
```
cd /opt/ai-bridges/bridges/vibe-matrix-rs
cargo build --release
systemctl enable --now vibe-matrix-rs
journalctl -u vibe-matrix-rs -f
```

## Dalej (Faza 2+): tmux/TUI, media, redb dedup, landlock, governor rate-limit, metryki.

## Kontynuacja sesji (kontekst między wiadomościami) — v2
- każda wiadomość leci jako `vibe -p <msg> --yolo --output json [--resume <session_id>]`
- `session_id` brane z JSON journal i zapisywane w `SESSION_FILE` (default `/var/lib/vibe-matrix-rs/session.txt`)
- dzięki `--resume` vibe **pamięta poprzednie wiadomości** (tak jak tmux w wersji Python)
- workdir vibe: `VIBE_WORKDIR` (default `/var/lib/vibe-matrix-rs/vibe-work`)
- dodatkowo: bezpieczny `safe_truncate` (UTF-8 char boundary — nie panikuje na emoji), testy jednostkowe

## Komendy (v3)
- `/new` — reset sesji vibe (usuwa `SESSION_FILE`, następna wiadomość = nowy kontekst)
- `/model` / `/model <nazwa>` — pokaż/ustaw model (`--model`, zapis w `MODEL_FILE`)
- `/think` / `/think <off|low|medium|high|xhigh|max>` — pokaż/ustaw thinking (`--thinking`, zapis w `THINK_FILE`; vibe stosuje przy nowej sesji → użyj `/new`)
- `/status` — pokój, sesja, model, think, liczba seen, uptime
- `/help` — lista komend
- nieznane `/coś` → odpowiedź „nieznana komenda” (NIE leci do agenta)

## /provider (v4)
- `/provider` — pokaż aktualnego providera
- `/provider <mistral|openai|anthropic|google>` — przełącza providera: do subprocessa vibe leci tylko odpowiedni klucz (`MISTRAL_API_KEY` / `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `GEMINI_API_KEY`), zapis w `PROVIDER_FILE`
- brak klucza w env bota → komunikat ⚠️ (dodaj do `/etc/vibe-matrix-rs.env`, chmod 600) i błąd przy runie
- bez `/provider` = backward-compat: wszystkie klucze z `VIBE_ENV_KEYS`

## v5 — terminalowe "tuxy" (bez kluczy API, bez --resume)
Architektura: agent (vibe/codex/claude/agy) żyje w stałej sesji **tmux**, bridge robi tylko `send-keys` + `capture-pane`.
- sesja tmux: `<TMUX_PREFIX>-<agent>` (default `vibe-matrix-rs-vibe`), workdir `VIBE_WORKDIR`
- odpowiedź wykrywana przez poll `capture-pane` co 2s (koniec = 3 próbki bez zmian, max `VIBE_TIMEOUT_SECS`)
- komendy bridge'a: `/new [agent]`, `/agent <vibe|codex|claude|agy>`, `/attach` (capture ekranu), `/stop`, `/status`, `/help`
- wszystkie inne wiadomości (także `/model`, `/think` TUI agenta) lecą prosto do tmuxa — kontekst pamięta agent
- env: `AGENT_FILE`, `TMUX_PREFIX`, `VIBE_WORKDIR` (reszta jak wcześniej)
- uwaga: `agy` wymaga OAuth login na hoście (interaktywnie), inaczej sesja tmux od razu pada
