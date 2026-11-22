# vibe-matrix-rs

Rustowy mostek headless: **pokój Matrix ↔ lokalny agent CLI** (`vibe`, `agy`, `claude`, `codex`).

Wiadomość w pokoju → reakcja 👀 → `agent -p <tekst> ...` (non-interactive, czysty env, timeout) → odpowiedź w pokoju → ✅ (lub ❌).

> **Repo jest PUBLICZNE.** Sekrety (tokeny Matrix, klucze) żyją wyłącznie w plikach env
> poza repo (`chmod 600`), ładowanych przez systemd (`EnvironmentFile`). Zobacz `.env.example`.
> Nigdy nie commituj plików `.env`, tokenów ani stanu (`seen.txt`).

## Architektura

```
Matrix room ──sync──▶ bridge-headless ──spawn──▶ agent CLI (preset) ──▶ stdout ──▶ send_text ──▶ room
                ▲                                                    │
                └──────────── seen.txt (dedup eventów) ◀────────────┘
```

Workspace:

| crate | rola |
|---|---|
| `bridge-common` | `Config::from_env`, presety agentów, `clean_ansi`, seen-set |
| `bridge-core` | `MatrixClient` (sync / send_text / react) przez reqwest + Bearer |
| `bridge-headless` | pętla sync, allowlista, 👀/✅/❌, limit 3800 znaków odpowiedzi |

## Presety agentów (`AGENT`)

| `AGENT` | komenda |
|---|---|
| `vibe` | `vibe -p {prompt} --yolo` |
| `agy` | `agy -p {prompt} --dangerously-skip-permissions` |
| `claude` | `claude -p {prompt} --dangerously-skip-permissions` |
| `codex` | `codex exec --dangerously-bypass-approvals-and-sandbox {prompt}` |

Własna komenda: `AGENT_CMD="moj-agent --prompt {prompt}"` ( `{prompt}` = treść wiadomości, spacje = separator).

## Konfiguracja (env)

Skopiuj `.env.example` i wypełnij. Zmienne: `MATRIX_HOMESERVER`, `MATRIX_ACCESS_TOKEN` (token **bota**),
`MATRIX_ROOM_ID`, `MATRIX_BOT_USER_ID`, `MATRIX_ALLOWED_USERS`, `AGENT` / `AGENT_CMD`,
`AGENT_TIMEOUT_SECS`, `SYNC_TIMEOUT_MS`, `STATE_FILE`, `AGENT_HOME`, `AGENT_PATH`, `AGENT_ENV_KEYS`, `RUST_LOG`.

Token bota: `curl -X POST $HS/_matrix/client/v3/login -d '{"type":"m.login.password","user":"BOT","password":"..."}'`

## Instalacja lokalna (komputer użytkownika, systemd --user)

```bash
git clone git@github.com:s-bidowaniec/ai-bridges.git ~/ai-bridges
cd ~/ai-bridges/bridges/vibe-matrix-rs
cargo build --release
mkdir -p ~/.local/bin && cp target/release/bridge-headless ~/.local/bin/

# unit użytkownika (szablon: jedna instancja na agenta)
mkdir -p ~/.config/systemd/user
cp systemd/vibe-matrix-rs@.user.service ~/.config/systemd/user/vibe-matrix-rs@.service

# env per agent (sekrety lokalnie, 600!)
mkdir -p ~/.config/vibe-matrix-rs
install -m 600 .env.example ~/.config/vibe-matrix-rs/vibe.env   # i wyedytuj
# ... analogicznie agy.env, claude.env, codex.env (inne AGENT, pokój, bot, token)

systemctl --user daemon-reload
systemctl --user enable --now vibe-matrix-rs@vibe vibe-matrix-rs@agy vibe-matrix-rs@claude vibe-matrix-rs@codex
journalctl --user -u vibe-matrix-rs@vibe -f
```

## Instalacja na VPS (system, root)

```bash
cd /opt/ai-bridges/bridges/vibe-matrix-rs
cargo build --release
cp systemd/vibe-matrix-rs@.service /etc/systemd/system/
install -m 600 .env.example /etc/vibe-matrix-rs/vibe.env   # wyedytuj
systemctl daemon-reload
systemctl enable --now vibe-matrix-rs@vibe
journalctl -u vibe-matrix-rs@vibe -f
```

Legacy (pojedyncza instancja): `systemd/vibe-matrix-rs.service` + `/etc/vibe-matrix-rs.env`.

## Tworzenie bota i pokoju

```bash
# na hoście Synapse (zamknięta rejestracja):
register_new_matrix_user -c /etc/matrix-synapse/homeserver.yaml http://localhost:8008 -u vibe -p HASLO --no-admin
# pokój (private) przez API jako admin + invite bota; bot: POST /rooms/{id}/join
```

## Narzędzia

`tools/mx.py` — pomocniczy klient (send/read/rooms) bez sekretów w repo; token z `MATRIX_ADMIN_TOKEN`
lub `~/.config/vibe-matrix-rs/admin.env` (600).

```bash
python3 tools/mx.py send '!POKOJ:matrix.owl-post.eu' "cześć"
python3 tools/mx.py read '!POKOJ:matrix.owl-post.eu' 10
```

## Bezpieczeństwo

- Token tylko w nagłówku `Authorization: Bearer`, nigdy w URL/logach.
- `MATRIX_ALLOWED_USERS` — allowlista nadawców (puste = wszyscy w pokoju; pokój powinien być private).
- Proces agenta dostaje czysty env: `PATH`/`HOME`/`TERM` + klucze z `AGENT_ENV_KEYS`.
- Unit systemowy: `ProtectSystem=strict`, `ProtectHome=read-only`, `ReadWritePaths` tylko tam gdzie trzeba.
- Uwaga: ten build Synapse nie ma endpointu `/events/{id}/reactions/...` — reakcje idą przez
  `send/m.reaction` z `key` **wewnątrz** `m.relates_to`.
