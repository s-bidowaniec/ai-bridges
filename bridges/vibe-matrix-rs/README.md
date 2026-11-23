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

## Env (pełna lista w `.env.example`)
System: `/etc/vibe-matrix-rs.env` (lub `/etc/vibe-matrix-rs/<agent>.env` dla `@.service`),
owner `root:vibe-matrix-rs`, **chmod 640**. User: `~/.config/vibe-matrix-rs/<agent>.env`, chmod 600.
```
MATRIX_HOMESERVER=https://matrix.owl-post.eu   # https:// wymagane (ALLOW_INSECURE_HTTP=1 tylko lokalnie)
MATRIX_ACCESS_TOKEN=...
MATRIX_ROOM_ID=!....:matrix.owl-post.eu
MATRIX_BOT_USER_ID=@vibe-rs:matrix.owl-post.eu
MATRIX_ALLOWED_USERS=@stanislaw:matrix.owl-post.eu
INBOUND_MAX_PER_MIN=6      # rate limit inbound per nadawca (sliding window 60s)
MAX_SEEN_EVENTS=100000     # cap zbioru dedup
VIBE_TIMEOUT_SECS=600
SYNC_TIMEOUT_MS=30000
STATE_FILE=/var/lib/vibe-matrix-rs/seen.txt
SESSION_FILE=/var/lib/vibe-matrix-rs/session.txt
AGENT_FILE=/var/lib/vibe-matrix-rs/agent.txt
TMUX_PREFIX=vibe-matrix-rs
VIBE_WORKDIR=/var/lib/vibe-matrix-rs/vibe-work
AGENT_HOME=/var/lib/vibe-matrix-rs/agent-home
VIBE_ENV_KEYS=MISTRAL_API_KEY
# AGY_CMD=agy               # ścieżka do binarki agy (default: z PATH)
# AGENT_PATH=...            # opcjonalny PATH dla subprocessu agenta
```

## Build & run (na VPS, jako dedykowany user — NIE root)
```
useradd -r -d /var/lib/vibe-matrix-rs -s /usr/sbin/nologin vibe-matrix-rs
install -d -m 0750 -o vibe-matrix-rs -g vibe-matrix-rs /var/lib/vibe-matrix-rs
# env file: root:vibe-matrix-rs, chmod 640; konfiguracje agentów (vibe/claude/...)
# żyją w HOME=/var/lib/vibe-matrix-rs (ustawionym w unitcie)
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
- env: `AGENT_FILE`, `TMUX_PREFIX`, `VIBE_WORKDIR`, `AGY_CMD` (ścieżka do binarki agy), `AGENT_PATH`, `INBOUND_MAX_PER_MIN` (rate limit inbound), `MAX_SEEN_EVENTS` (reszta jak wcześniej)
- hardening: inbound rate limit per nadawca (sliding 60s, dla tekstu i `m.audio`), wymuszenie `https://` dla homeservera, token jako `SecretString`, zbiór `seen` z capem `MAX_SEEN_EVENTS` i zapisem tylko przy zmianie, walidacja agenta z `AGENT_FILE`
- uwaga: `agy` wymaga OAuth login na hoście (interaktywnie, w jego `$HOME`), inaczej sesja tmux od razu pada

## v6 — voice (m.audio ↔ STT/TTS przez Mistral API)
- wiadomość głosowa (`m.audio`) → download mxc → **STT** (`/v1/audio/transcriptions`, model `voxtral-mini-latest`, auto-language) → tekst → agent w tmux (`[voice message od ...]: ...`) → odpowiedź
- `/voice on|off` — gdy ON, wszystkie odpowiedzi (też na tekst) idą jako **voice message** (TTS `voxtral-mini-tts-latest` → opus → upload → `m.audio` + `org.matrix.msc3245.voice` + waveform)
- wymaga `MISTRAL_API_KEY` w env bota (`/etc/vibe-matrix-rs.env`)
- E2EE media (`content.file`) → komunikat "nieobsługiwane" (MVP)
- uwaga: modele transcribe nie znają `language=pl` — auto-detect; TTS odpowiada JSON `{"audio_data": base64}`
- dzwonienie `m.call` (MSC3245/WebRTC) = Faza 6 (wymaga SFU) — na razie tylko voice messages
- env: `VOICE_FILE`, `UPLOADS_DIR`, `STT_MODEL`, `TTS_MODEL`, `TTS_VOICE`

## v7 — wątki + instalacja na PC (tryb "w aucie")
- odpowiedzi bota (tekst i voice) lecą jako **wątek** (`m.thread`) do wiadomości użytkownika
- `/voice new` — reset konwersacji głosowej (czyści sesję tmux)
- `pc-install/` — instalacja na PC (Linux, user systemd): `./pc-install/install.sh`
  - klonuje repo do ~/ai-bridges, buduje, konfiguruje `~/.config/vibe-matrix-rs/env`, enable'uje `systemd --user`
  - potem w pokoju: `/voice on` → seria voice messages = ciągła konwersacja z kontekstem (tmux)
- prawdziwy call `m.call` (WebRTC/MSC3245, answer bota) = Faza 6 (osobny klient WebRTC, np. aiortc w Pythonowym bridge)

## v8 — voice jako KONNEKTOR (passthrough do terminala)
Bridge tylko przekazuje audio między Matrix a terminalem agenta ( który ma własny tryb voice):
- `m.audio` od użytkownika → plik w `VOICE_DIR/in/<stem>.wav`
- agent w tmux (tryb voice) odpowiada plikiem w `VOICE_DIR/out/<stem>.opus` → watcher (1s) → upload → `m.audio` na Matrix
- fallback: jeśli w `VOICE_FALLBACK_SECS` (default 15s) terminal nie odpowie → lokalny STT→agent→TTS (v6 API)
- `/voice on|off|new` jak wcześniej; env: `VOICE_DIR`
- uwaga: prawdziwy m.call (WebRTC) NIE jest potrzebny — rozmowa głosowa idzie przez voice messages + terminal

## v10 — AGENT_MODE=tmux dla wszystkich agentów (okno podpięte do Matrix, attach)
- `AGENT_MODE=tmux` wymusza tryb tmux dla dowolnego agenta (vibe/codex/claude/agy); default bez zmiennej: agy=tmux, reszta=subprocess `--resume`
- sesja tmux: `<TMUX_PREFIX>-<agent>` (np. `vibe-matrix-rs-vibe`) — `tmux attach -t vibe-matrix-rs-vibe` podpina Twoje okno do pokoju Matrix; wiadomości z Matrixa lecą jako send-keys, odpowiedzi jako capture-pane
- **TMUX_TMPDIR pod $HOME** (np. `~/.local/state/vibe-matrix-rs/tmux`) — unit user ma `PrivateTmp=true`, więc bez tego socket tmux byłby w prywatnym /tmp i `tmux attach` z konsole by nie widział sesji; w shellu: `export TMUX_TMPDIR=$HOME/.local/state/vibe-matrix-rs/tmux`

## v9 — dispatch per agent + fixy E2E (Faza 6 test)
- vibe/codex/claude: subprocess `AGENT -p --resume <sid> --yolo --output json` (szybko, bez menu TUI; sesja per agent w `<SESSION_FILE>.<agent>`)
- agy: tmux TUI (send-keys/capture-pane) — tam działa tryb voice
- fix: media download v1 first (ten Synapse nie ma v3 download), HOME agenta = AGENT_HOME (nie /root)
- fix: m.audio NIE może mieć m.relates_to (Synapse 400) — voice odpowiedzi poza wątkiem, tekst w wątku
- E2E test OK: m.audio -> voice/in -> fallback 15s -> STT -> vibe -> TTS -> m.audio (MSC3245 voice) w pokoju
