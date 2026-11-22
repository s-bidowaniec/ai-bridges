> ⚠️ **Stale od commitu `f79e628`:** ten raport dotyczy poprzedniej architektury "Faza 5" (crates `bridge-tui`/`bridge-voice`/`bridge-agents`/`bridge-control`, matrix-sdk), która nie istnieje już w HEAD — aktualny kod to przepisanie **v5 tmux** (3 crates, `bridges/vibe-matrix-rs/`). Niektóre ustalenia (root, brak rate limitingu, media) zostały już zaadresowane w v5 — patrz `bridges/vibe-matrix-rs/README.md`. Raport zostaje jako historia.

Wszystkie pliki przeczytane, `cargo build --workspace` i `cargo test --workspace` przechodzą (6 testów OK, tylko warningi o nieużywanym kodzie). Poniżej pełny raport.

---

# Code Review — `vibe-matrix-rs` (Faza 5, prototyp)

Zakres: 40 plików źródłowych/configu (~2100 linii), `cargo build --workspace` ✅, `cargo test --workspace` ✅ (6 testów: allowlist, tui, command). Weryfikacja API matrix-sdk 0.7.1 względem `~/.cargo/registry` (m.in. `Client::sync` w 0.7.1 faktycznie zapętla się wewnętrznie i zwraca `Result<()>` — tu OK).

## P1 — krytyczne / blokery działania lub bezpieczeństwa

1. **`systemd/vibe-matrix-headless.service:15` — `WatchdogSec=120` bez żadnego `sd_notify` w aplikacji.**
   W `Cargo.lock` nie ma `sd-notify`/`libsystemd`, a żaden kod nie wysyła `WATCHDOG=1`. Systemd po 120 s uzna proces za zawieszony i wyśle SIGABRT — serwis będzie się restartował w kółko mimo że działa. `bridge-core/src/watchdog.rs` też nie pinga systemd (tylko `bail!`uje).
   **Fix:** albo usuń `WatchdogSec` z obu unitów, albo dodaj zależność `sd-notify` i w pętli głównej co `WatchdogSec/3` wysyłaj `sd_notify(false, "WATCHDOG=1")`; `run_watchdog` podłącz jako task i przy `bail` faktycznie zakończ proces z kodem błędu (żeby `Restart=always` zadziałał).

2. **README.md:16-17 + oba unity — dwie usługi, to samo konto i ten sam pokój.**
   `vibe-matrix-headless` i `vibe-matrix-tui` logują się jako ten sam `bot_user_id` (ten sam `config.toml`) i dołączają do tego samego `room_id`. Oba procesy dostają wszystkie eventy, a dedup jest osobny per proces (`dedup.redb` vs `dedup-tui.redb` — `bridge-headless/src/main.rs:47`, `bridge-tui/src/main.rs:66`) → **każda wiadomość będzie przetworzona dwa razy, bot odpowie dwa razy**.
   **Fix:** uruchamiaj tylko jedną usługę na pokój (lub użyj różnych pokojów/kont); ewentualnie współdzielony plik dedup z blokadą międzyprocesową.

3. **`bridge-agents/src/runner.rs:17,30-45` — stderr dziecka jest pipowane, ale nigdy nie czytane.**
   Jeśli agent zapisze >64 KB na stderr (a CLI agentowe lubią), bufor pipe się zapycha, dziecko blokuje się na write i wisi aż do `timeout` — użytkownik dostaje "Timeout" zamiast realnego błędu/wyjścia.
   **Fix:** spawnuj osobny task, który czyta stderr do bufora ograniczonego (np. ostatnie 8 KB); dołącz ogon stderr do wyniku/błędu.

4. **`bridge-headless/src/main.rs:95` i `bridge-tui/src/main.rs:104` — obsługiwany jest wyłącznie `MessageType::Text`.**
   `m.image` / `m.file` / `m.audio` / `m.video` / `m.notice` są po cichu ignorowane (brak gałęzi `else`), a cały moduł `bridge-core/src/media.rs` (upload/download) **nie jest podpięty nigdzie** (grep: zero wywołań `upload_media`/`download_media`). Bot nie reaguje na załączniki wcale.
   **Fix:** dopnij `media.rs` do handlera: dla `m.file`/`m.image`/`m.audio` pobierz przez `download_media` do `uploads_dir` (koniecznie przez `sanitize_filename` + `is_safe_path` — oba istnieją ale są nieużywane), dla `m.audio` → STT, odpowiedź agenta → ewentualnie TTS → `m.audio` upload.

5. **Brak jakiegokolwiek rate limitingu inbound; `SendRateLimiter` (`bridge-core/src/rate_limit.rs`) nie jest używany.**
   Jedyny dozwolony nadawca może zalać kolejkę — `tx.send(...).await` (`main.rs:132`) przy pełnej kolejce (16) **blokuje handler eventów**, czyli cały sync loop; serwer zerwie long-polling. Rate limiter wysyłki też nie istnieje w praktyce (tylko `lib.rs:13`).
   **Fix:** podłącz `SendRateLimiter` przed każdym `room.send`; dodaj per-nadawca inbound limiter (governor keyed) i przy limicie odpowiedz komunikatem „zwolnij" albo odrzuć z logiem.

6. **`bridge-common/src/config.rs:93-95` — homeserver może być `http://`.**
   Walidacja przyjmuje każdy string z prefixem `http` — token bota (`VIBE_BRIDGE_BOT_TOKEN`) leci wtedy plaintekstem.
   **Fix:** wymagaj `https://` (chyba że jawny `allow_insecure_http = true` w configu).

## P2 — ważne braki funkcjonalne

7. **`bridge-headless/src/main.rs:158-168` — stan z `/model`, `/provider`, `/think`, `/voice` nie ma żadnego wpływu na runy.**
   Worker buduje env „na sztywno" (tylko `PATH`, `HOME`, `MISTRAL_API_KEY`), a `State::agent_env()` (`bridge-control/src/state.rs:42-60`), które uwzględnia model/provider/think i klucz API per provider, **nie jest nigdzie wywoływane**. `/provider openai` odpowiada „✅", ale agent dalej dostaje `MISTRAL_API_KEY`.
   **Fix:** w workerze użyj `control_ctx.state.agent_env()` (dodaj też `TERM`, `LANG`, `XDG_*`) zamiast ręcznej listy.

8. **`bridge-voice` jest martwym cratem** — nie jest zależnością żadnego binarki (grep w `Cargo.toml`), `voice_reply`/`supports_voice` nie są nigdzie czytane, `/voice on` (`handler.rs:69-72`) tylko zmienia stan in-memory. STT/TTS/waveform nie są spięte z pętlą; `m.audio` nieobsłużone; `waveform.rs:5-10` to placeholder (sinusoida), `tts.rs:33` liczy „duration" z liczby słów.
   **Fix:** dodaj `bridge-voice` do `bridge-headless`, przy `m.audio` → `stt::transcribe` → prompt, przy `voice_enabled`/`voice_reply` → `tts::synthesize` → upload → `m.audio` z prawdziwym `duration_ms` i `waveform` (dekodowanie Opus, np. przez `opus`/`ffmpeg`). `VoiceConfig.mistral_api_key` (`bridge-voice/src/lib.rs:15`) trzyma klucz jako zwykły `String` — zmień na `SecretString`. Brak karty dźwiękowej na VPS jest OK (API Mistrala), ale lokalne audio trzeba oznaczyć jako „nieprzetestowane".

9. **`bridge-control/src/handler.rs:55-59` — `/new` to placeholder**, mimo że `tmux_ctl::new_session` (`tmux_ctl.rs:18-24`) istnieje i jest nieużywane. `/attach img` (`AttachMode::Image`) zachowuje się identycznie jak `txt` — brak capture-pane → PNG → `m.image` (README przyznaje „docelowo").
   **Fix:** zaimplementuj `/new <agent>` przez `new_session` z nazwą sesji z rejestru agentów; `/attach img` jako pętla `capture-pane -p -e` → PNG co 1 s.

10. **`bridge-control/src/tmux_ctl.rs:6-9` — brak sprawdzania exit statusu tmux.**
    `tmux(...).output()` zwraca `Ok` nawet gdy tmux wychodzi z kodem 1 (stderr ignorowane). `/send` do nieistniejącej sesji odpowiada „✅ Wysłano", `/attach` pokazuje pusty tekst zamiast „(brak sesji)”. Nazwa sesji nie jest walidowana (tmux parsuje target `sesja:okno.pane` — można trafić w zły panel).
    **Fix:** sprawdzaj `out.status.success()` i stderr; waliduj nazwę sesji regexem `^[A-Za-z0-9_-]+$`.

11. **`bridge-control/src/command.rs:73` — nieznana komenda `/cokolwiek` leci do agenta jako prompt.**
    Literally `/foobar test` → `Command::Message("/foobar test")` → agent dostaje „/foobar test". Ryzyko prompt-injection przez literówki i mylenie komend.
    **Fix:** wiadomości zaczynające się od `/` i niebędące znaną komendą/agentem → odpowiedź „nieznana komenda, wpisz /help" (bez wysyłania do agenta).

12. **`bridge-agents/src/runner.rs:50-54` — ignorowany exit code dziecka.**
    Agent kończący się kodem 1 z pustym stdout → `Ok("")` → w ogóle nie wysyła odpowiedzi, ale daje reakcję ✅. Brak też `--` przed promptem — prompt zaczynający się od `-` może zostać zinterpretowany jako flaga CLI (`runner.rs:15`).
    **Fix:** po `child.wait()` sprawdź `status.success()`; przy failure zwróć `Err` z ogonem stderr; dodaj `--` przed promptem (gdzie CLI to wspiera).

13. **`bridge-tui/src/main.rs:152` — wykrywanie odpowiedzi po długości tekstu.**
    `if resp.len() != last_response_len` — jeśli nowa odpowiedź ma dokładnie taką samą długość jak poprzednia, nie zostanie wysłana i worker czeka pełne 10 min. `agent_name` z `/codex ...` jest sparsowane, ale nigdy nie użyte (ostrzeżenie kompilatora: „field agent_name is never read") — TUI zawsze pisze do jednej sesji tmux.
    **Fix:** porównuj event/offset panelu lub hash, nie samą długość; użyj `agent_name` (osobna sesja tmux per agent) albo udokumentuj ograniczenie.

14. **Brak graceful shutdown.** Żaden main nie obsługuje SIGTERM/SIGINT — proces ginie natychmiast, kolejka się nie drenuje, wysyłany jest tylko `kill_on_drop` na dzieci. `bridge-tui/src/main.rs:178-181` przy błędzie sync zwraca `Ok(())`.
    **Fix:** `tokio::signal::ctrl_c()` / `SIGTERM` → przerwij sync, dokończ bieżący job (lub wyślij „restartuję"), zamknij redb, exit 0.

15. **Dedup: błąd redb traktowany jak duplikat + wyścig TOCTOU + nieograniczony wzrost.**
    `main.rs:90-93`: `match ... { Ok(true) => {}, _ => return }` — błąd bazy = ciche porzucenie wiadomości; `tui/main.rs:102` podobnie (`unwrap_or(false)`). `dedup.rs:20-40`: check w read-txn, insert w write-txn — dwa współbieżne eventy mogą oba przejść. Tabela `events` rośnie wiecznie (brak TTL/kompakcji).
    **Fix:** pojedyncza write-txn z `insert` (wynik poprzedniej wartości = atomowy check-and-set); loguj błędy zamiast porzucać; dodaj okresowe czyszczenie starych wpisów (np. >30 dni).

16. **Sekrety: helper `redact()` (`bridge-common/src/secrets.rs:23-31`) nie jest nigdzie używany.**
    Odpowiedź agenta (`main.rs:177,182-192`) i błędy są wysyłane na Matrix bez redakcji — jeśli agent zwróci token/klucz (echo, błąd), wycieknie na Matrix. W logach też: `main.rs:117` loguje pierwsze 80 znaków promptu.
    **Fix:** перед wysyłką na Matrix i logowaniem przepuść tekst przez `redact(text, &[bot_token, MISTRAL_API_KEY, ...])`; rozważ `tracing` layer z redakcją. (Sam token dobrze izolowany: `serde(skip)` + osobny env + `env_clear()` w runnerze — to plus.)

17. **Typing indicator nie istnieje** (grep: zero wystąpień) — podczas 10-minutowej pracy agenta użytkownik nie widzi, że bot „pisze".
    **Fix:** `room.typing_notice(true)` na start jobu, `false` na koniec (matrix-sdk 0.7 ma `Room::typing_notice`).
