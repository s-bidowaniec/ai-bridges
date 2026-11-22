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
