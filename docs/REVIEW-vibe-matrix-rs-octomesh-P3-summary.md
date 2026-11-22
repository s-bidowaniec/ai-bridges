> ⚠️ **Stale od commitu `f79e628`:** raport dotyczy poprzedniej architektury "Faza 5" (crates `bridge-tui`/`bridge-voice`/`bridge-agents`/`bridge-control`), nieistniejącej w HEAD — aktualny kod to v5 tmux (3 crates). Część ustaleń została już zaadresowana (patrz `bridges/vibe-matrix-rs/README.md`). Raport zostaje jako historia.

# Code Review — vibe-matrix-rs (Faza 5): P3 + podsumowanie (część 2/2)

## P3 — usprawnienia / hardening

18. **Unwrappy w kodzie produkcyjnym:** `runner.rs:30` (`stdout.take().unwrap()`), `state.rs:34-40` (7× `RwLock` unwrap — poisoned lock = panic), `main.rs:150` / `tui/main.rs:129` (`EventId::parse("$unknown").unwrap()`), `rate_limit.rs:13` (`NonZeroU32::new().unwrap()` — `new(0)` panikuje). Regexy w `tui.rs:9-33` — `unwrap` w `OnceLock`, bezpieczne w praktyce, ale zamień na `expect("static regex")`.
    **Fix:** `unwrap_or_else` z sensownym fallbackiem / `parking_lot::RwLock` / wczesna walidacja `max_per_sec > 0`.

19. **systemd hardening nierówny między unitami:** TUI (`vibe-matrix-tui.service:19-21`) ma `PrivateTmp=no` i `ReadWritePaths=/home/vibebridge /tmp` — szeroki zapis do całego home (w tym `~/.ssh`, `~/.vibe` z tokenami agentów); brakuje `ProtectKernelTunables/Modules/ControlGroups`, `RestrictNamespaces`, `CapabilityBoundingSet=`, `ProtectProc=invisible`, `RestrictSUIDSGID`, `SystemCallErrorNumber`. Headless ma `ProtectHome=read-only` + tylko `work` i `.vibe/logs` — **agent-vibe piszący do `~/.vibe` (HOME=/home/vibebridge) dostanie EROFS** (`main.rs:162` ustawia HOME).
    **Fix:** dla headless dodaj `ReadWritePaths=... ~/.vibe` (lub osobny `HOME` dla agenta w `work_dir`); dla TUI zawęź `ReadWritePaths` i domknij brakujące dyrektywy; `EnvironmentFile` z tokenami — upewnij się 0600, rozważ `LoadCredential=`.

20. **Martwy kod / duplikacja:** `bridge-core/src/matrix_client.rs` (`MatrixBridge`, `sync_forever` — myląca nazwa, robi jeden sync) nieużywany — oba mainy budują klienta inline; `secrets.rs::redact`, `media.rs::{sanitize_filename, is_safe_path, upload_media, download_media}`, `rate_limit.rs`, `watchdog.rs`, `tmux_ctl::new_session`, `State::agent_env` — wszystko niepodpięte; `bridge-tui` nie ma `bridge-control` (brak `/komend` w trybie TUI). Warningi kompilatora to potwierdzają (`unused variable: registry`, `field agent_name is never read`).
    **Fix:** albo podepnij, albo usuń; jeden konstruktor klienta współdzielony przez oba binary.

21. **`bridge-voice/src/stt.rs:30`** — `resp.json()` przed sprawdzeniem statusu (przy 502 HTML dostaniesz błąd serde zamiast HTTP); `stt.rs:15` ładuje cały plik do RAM (do 50 MB na wiadomość) — streamuj multipart. `tts.rs:33` — „duration" z liczby słów to zgadywanka; policz z bajtów Opus lub z odpowiedzi API.

22. **Małe rzeczy:** `command.rs:67` — `/voice` bez argumentu = `off` (zaskakujące); `handler.rs:64` — odpowiedź `/list` używa markdown `**`, ale wysyłana jako `text_plain` (widać gwiazdki) — użyj `text_html`; edycje wiadomości (`m.replace`) nie są filtrowane → bot odpowiada na każdą edycję jako nową wiadomość; brak ficzera `e2e-encryption` w matrix-sdk — w pokoju E2EE bot będzie głuchy (dokumentuj lub dodaj); `.gitignore` nie zawiera `.octomesh/` (runtime'owy `state.db` w repo); nieużywane deps `zeroize`, `thiserror` (warningi).

23. **Walidacja configu:** `allowed_senders` może być pusta (fail-closed OK, ale warto `warn!`); `room_id` nie jest walidowany przy ładowaniu (błąd dopiero przy `RoomId::parse`); `AgentRegistry::get` (`registry.rs:33`) cicho fallbackuje na domyślnego agenta przy literówce w nazwie.

---

## Podsumowanie

**Co jest już dobrze:**
- Architektura workspace jest czysta, a fundamenty bezpieczeństwa są przemyślane: token poza serde (`config.rs:12,74-78`), `SecretString` (Debug = `<redacted>`), `env_clear()` + allowlist env w runnerze (`runner.rs:19-25`) — token Matrix **nie** może wyciec do procesu agenta, `kill_on_drop`, allowlist z blokadą własnego bota, dedup w redb (przetrwa restart), reakcje 👀/👍/✅/❌ faktycznie działają, kolejka z backpressure, streaming upload/download mediów z limitem, `sanitize_filename`/path-traversal check, sensowne regexy czyszczenia ANSI w `tui.rs` (z testami), `cargo build`/`test` zielone, hardening systemd w headless jest na dobrym poziomie.

**Co trzeba zrobić przed Fazą 6 (E2E + produkcja):**
1. Naprawić P1: `WatchdogSec` vs brak `sd_notify`, konflikt headless+TUI na jednym koncie/pokoju, niedrożny stderr w runnerze, podpięcie `media.rs` (m.image/m.file/m.audio), inbound/outbound rate limiting, wymuszenie `https://` dla homeservera.
2. Domknąć P2: podłączyć `State::agent_env()` do runów (żeby `/model|/provider|/think` działały), spiąć `bridge-voice` (STT dla `m.audio`, TTS dla odpowiedzi, prawdziwy waveform/duration), realne `/new` i `/attach img`, sprawdzanie exit statusu tmux, nie wysyłać nieznanych `/komend` do agenta, exit-code agenta, graceful shutdown (SIGTERM), atomowy dedup + TTL, redakcja sekretów w odpowiedziach/logach, typing indicator.
3. Hardening: ujednolicić i domknąć dyrektywy systemd (szczególnie TUI), `ProtectProc`, zawężenie `ReadWritePaths`, walidacja nazw sesji tmux, usunięcie martwego kodu, `.gitignore` dla `.octomesh/`.
4. Testy E2E: mock homeserver (np. `matrix-sdk` + lokalny Synapse w dockerze lub `wiremock`) — scenariusze: dedup po restarcie, allowlist odrzuca obcego, rate limit, załącznik → agent → odpowiedź, `/voice on` → `m.audio`, double-run headless+TUI (regresja), SIGTERM podczas jobu.

**Verdykt:** prototyp kompiluje się i przechodzi testy jednostkowe, ale jako całość nie jest jeszcze spójny — kluczowe moduły (`rate_limit`, `watchdog`, `media`, `bridge-voice`) są napisane, lecz niepodpięte do głównej pętli, a `WatchdogSec` bez `sd_notify` i podwójne uruchomienie obu serwisów to blokery, które uniemożliwią stabilną pracę produkcyjną.
