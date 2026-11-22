# Code Review — `bridges/vibe-matrix-rs` MVP (commit `c98a34db`, wrzucony z innego PC przez agy/vibe)

Zakres: 10 plików, ~502 linie, 3 crates (`bridge-common`, `bridge-core`, `bridge-headless`) + systemd unit. Wariant **headless-only MVP** (reqwest bezpośrednio, bez matrix-sdk) jako lekka alternatywa dla pełnego prototypu Faza 5. Lokalnie: `cargo build --release` ✅ (1 warning: `bridge-core` nie używa `bridge-common`).

## P1 — krytyczne / blokery

1. **`bridge-headless/src/main.rs:81` — `&clean[..3800]` panikuje przy wielobajtowym UTF-8.**
   Slicing po bajtach; jeśli bajt 3800 wpada w środek emoji/ąęź → `panic: byte index 3800 is not a char boundary` → cały proces pada (wątek główny), systemd restartuje. Polski/emoji tekst to norma na Matrix.
   **Fix:** `clean.char_indices().take_while(|(i,_)| *i < 3800).map(|(_,c)| c).collect::<String>()` albo `clean.get(..3800).unwrap_or(&clean)`.

2. **`systemd/vibe-matrix-rs.service: User=root` + token w `EnvironmentFile=/etc/vibe-matrix-rs.env`.**
   Bridge trzyma `MATRIX_ACCESS_TOKEN` i puszcza `vibe` jako root z `HOME=/root` i `ReadWritePaths=/root/.vibe`. Każdy błąd/prompt-injection w odpowiedzi agenta = root na VPS. Hardening (`ProtectSystem=strict` itp.) przy root daje połowę wartości.
   **Fix:** osobny user `vibe-rs` (DynamicUser lub statyczny), token przez `LoadCredential=` (0400, poza env), `HOME` w `/var/lib/vibe-matrix-rs`, `CapabilityBoundingSet=` (pusty), `MemoryDenyWriteExecute` wyłączone tylko dla procesu vibe potomnego (komentarz w unit już o tym wspomina — OK, ale root to nadal root).

3. **`bridge-core/src/lib.rs: sync` — brak obsługi `M_LIMITED` i brak `since` przy pierwszym sync + pełny `timeline` bez filtrowania.**
   Pierwszy sync bez `since` może zwrócić `limited: true` z pełną historią — kod bierze wszystkie eventy (ale dedup plikiem ratuje przed re-processem tylko jeśli eventy były już widziane). Przy dużej historii i restarcie bota bez state_file → flood odpowiedzi.
   **Fix:** respektuj `room.timeline.limited`, przy `limited` nie odpowiadaj na stare eventy (tylko oznacz jako seen); opcja `initial_sync_limit`.

4. **`main.rs: save_seen` po każdym syncu + `seen` rośnie bez limitu.**
   Plik `seen.txt` dopisywany w całości co każdą odpowiedź sync (long-poll co 30s) — I/O w pętli, plik rośnie wiecznie (brak TTL/kompakcji), brak blokady międzyprocesowej (dwa instancje = race).
   **Fix:** redb (jak w Fazie 5) albo zapisuj co N sekund / przy zamknięciu; TTL 30 dni; `flock` na plik.

5. **Brak rate limitingu inbound i outbound.**
   Jedyny dozwolony nadawca może zalać bota — każda wiadomość = osobny proces `vibe -p` (600s timeout każdy) → fork bomb własnymi ludźmi. Brak kolejki, brak limitu równoległych runów.
   **Fix:** semafor (max 1-2 równoczesne runy) + kolejka FIFO + governor na wysyłkę (jak w Fazie 5 `rate_limit.rs`).

6. **`main.rs: run_vibe` — stderr dziecka nie czytany równolegle.**
   `wait_with_output()` czyta stdout+stderr dopiero po zakończeniu; przy >64KB na stderr pipe się zapycha i dziecko wisi do timeoutu 600s — użytkownik czeka 10 min na błąd.
   **Fix:** spawn osobnych tasków do czytania stdout/stderr (albo `wait_with_output` zamienić na równoległe read_to_end).

## P2 — ważne braki funkcjonalne

7. **Tylko `m.text`** — `m.image`/`m.file`/`m.audio`/`m.notice`, edycje (`m.replace`), reakcje innych, `m.room.topic` — wszystko ignorowane. Brak mediów w ogóle (w Fazie 5 jest `media.rs`).
8. **Brak typing indicator** — podczas 10-minutowego runu vibe użytkownik nie widzi że bot pracuje (w Pythonowym bridge i Fazie 5 jest).
9. **Brak graceful shutdown** — SIGTERM = natychmiastowa śmierć, `save_seen` nie zdąży, run vibe zabity przez kill_on_drop bez komunikatu.
10. **Brak komend** (`/new`, `/model`, `/status`, `/voice`) — każdy tekst leci do vibe, łącznie z `/cokolwiek` (literówka = prompt injection do agenta).
11. **`bridge-core` — `homeserver` bez walidacji `https://`** (token leci plaintekstem przy `http://`), `room_id` bez walidacji formatu, brak timeoutu na connect, jeden `reqwest::Client` OK, ale `enc()` ręczny percent-encoding — działa, ale `room_id` zawiera `!` i `:` — OK, testowane E2E.
12. **Brak redakcji sekretów w logach/odpowiedziach** — `tracing::info!(... body.len())` OK, ale odpowiedź vibe idzie na Matrix bez `redact()` (jeśli vibe echo token → wyciek). W Fazie 5 `secrets.rs` też niepodpięty, ale tu nie ma nawet helpera używalnego — `bridge-common` ma `init_logging` bez filtra redaction.
13. **Reakcja `m.reaction` jako zwykły event** — działa na tym Synapse (per commit msg), ale `key` powinien być też w `content` klasycznie; OK dopóki Synapse tak przyjmuje.
14. **`now_nanos()` txn id** — OK unikalne, ale `enc(&txn)` — txn jest alfanumeryczny, fine.

## P3 — usprawnienia / hardening

15. **`bridge-core` zależy od `bridge-common` tylko dla... nic** (warning kompilatora) — usuń deps.
16. **systemd:** brak `WatchdogSec` (OK — nie ma sd_notify), brak `CapabilityBoundingSet=`, `ProtectProc=invisible`, `RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX`, `SystemCallFilter=@system-service`, `EnvironmentFile` 0600 — dopilnować; rozważ `DynamicUser=yes` + `StateDirectory=vibe-matrix-rs`.
17. **`Config::from_env`** — `MATRIX_ALLOWED_USERS` puste = allow-all (fail-open!). W Fazie 5 było fail-closed. **Fix:** pusta allowlist = blokuj wszystko + warn.
18. **Brak metryk/healthchecka** (`/metrics`, `sd_notify READY=1`), brak wersjonowania odpowiedzi.
19. **`.gitignore` tylko `target/`** — dodać `Cargo.lock`? (dla binarki można commitować — OK zostawić), dodać `.octomesh/`.
20. **Testy:** zero testów jednostkowych (w Fazie 5 są 4). Dodać: `clean_ansi`, `enc`, parse `SyncResponse`, allowlist.

## Porównanie z pełnym prototypem Faza 5 (`~/work/vibe-matrix-rs`, 7 crates, matrix-sdk)

| Obszar | MVP (c98a34db) | Faza 5 |
|---|---|---|
| klient Matrix | ręczny reqwest (mało kodu, mniej deps, szybszy build) | matrix-sdk (więcej API, E2EE brak w obu) |
| dedup | plik seen.txt (bez TTL/locka) | redb (atomowy, ale TOCTOU — oba do poprawy) |
| TUI/tmux | brak | jest (bridge-tui) |
| media | brak | `media.rs` (ale niepodpięty — P1 tam) |
| voice | brak | `bridge-voice` (martwy crate) |
| komendy /new /model | brak | parser + handler (część placeholder) |
| rate limit | brak | moduł jest, niepodpięty |
| systemd user | **root** | vibebridge + hardening |
| testy | 0 | 6 |

## Werdykt

MVP jest **sprawny jako szybki zamiennik Pythona "na już"** (commit msg: działa E2E na VPS, reakcje/send OK), ale **NIE nadaje się do produkcji jako root bez poprawek**: P1 #1 (panic UTF-8), P1 #2 (root), P1 #5 (brak rate limit = fork bomb), P2 #7-9 (brak mediów/typing/graceful). Rekomendacja: zostawić MVP jako lekki wariant, ale produkcyjnie iść w **pełny prototyp Faza 5** po spięciu jego P1 (watchdog, media, rate limit) — albo w MVP od razu: naprawić char-boundary, przejść na usera `vibe-rs`, dodać semafor+kolejkę i typing.
