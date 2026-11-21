# Roadmapa: przepisanie vibe-matrix bridge na Rust + hardening

**Data:** 2026-11-21
**Cel:** zastąpienie `bridges/vibe-matrix/` (Python, ~2570 linii) wersją Rust z pełnym hardeningiem

---

## 1. Stack technologiczny

| Warstwa | Crate | Uwagi |
|---|---|---|
| Async runtime | `tokio` (full) | multi-thread, process, fs, signal |
| Matrix protocol | `matrix-sdk` (v0.7+) | sync, media, reactions; alternatywnie `ruma` |
| HTTP client | `reqwest` (rustls) | media upload/download, typing fallback |
| Tmux | `std::process::Command` / `tmux-rs` | `send-keys -l`, `capture-pane` |
| Subprocess vibe | `tokio::process::Command` + `kill_on_drop` + `process_group` | zabija całą grupę procesów przy timeout |
| Secrets | `secrecy` + `zeroize` | token jako `SecretString`, wipe on Drop |
| Config | `figment` + `serde` | env + plik, walidacja przy starcie |
| Logging | `tracing` + `tracing-subscriber` | redaction layer maskuje sekrety |
| Sandbox | `landlock` (Linux 5.13+) | FS whitelist + network whitelist |
| Rate limiting | `governor` | max N msg/s do Matrix |
| Deduplikacja | `redb` (embedded KV) | event_id → processed, przetrwa restart |
| CLI | `clap` v4 (derive) | `--headless` / `--tui` / `--config` |
| Metryki | `metrics` + `metrics-exporter-prometheus` | tury, latency, błędy |

---

## 2. Architektura (Cargo workspace)

```
vibe-matrix-rs/
├── Cargo.toml              # workspace
├── crates/
│   ├── bridge-common/      # config, secrets, logging, hardening utils (~400 linii)
│   ├── bridge-core/        # Matrix client, media, reactions, allowlist, dedup (~1200)
│   ├── bridge-headless/    # bin: vibe -p subprocess, queue, worker (~600)
│   └── bridge-tui/         # bin: tmux, TUI cleaning, journal monitor (~1000)
├── tests/                  # integracyjne + fuzz (~800)
└── systemd/                # hardened unit files
```

Szacunkowy rozmiar: **~4000 linii Rust** (vs 2570 Python — więcej przez typy i error handling).
Binary release: ~15–25 MB.

---

## 3. Fazy

### Faza 0 — fundamenty (1–2 tyg.)
- [ ] Workspace + `bridge-common` (config, `SecretString`, tracing z redaction)
- [ ] `bridge-core` z `matrix-sdk`: sync loop, allowlist, dedup przez `redb`
- [ ] systemd unit z pełnym hardeningiem

### Faza 1 — headless MVP (2–3 tyg.)
- [ ] Reakcje 👀/✅/❌, typing indicator + keepalive
- [ ] `vibe -p` przez `tokio::process` z `kill_on_drop`, `env_clear()`, strumieniowy stdout z limitem
- [ ] Media upload: `canonicalize()` + `starts_with(work_dir)`, wykluczenie `uploads/`
- [ ] Media download: stream do pliku z `take(MAX_MEDIA_BYTES)` — nie do RAM

### Faza 2 — TUI bridge (2–3 tyg.)
- [ ] `tmux capture-pane` / `send-keys -l` przez `tokio::process`
- [ ] Port `clean_response_text` (regex ANSI, box-drawing, spinnery) — crate `vte` + `regex`
- [ ] `VibeSessionMonitor`: journal JSONL przez `tokio::fs` + `notify` (watch, nie polling)
- [ ] Kolejka: `tokio::sync::mpsc::channel(16)` — backpressure

### Faza 3 — hardening (1–2 tyg.)
- [ ] **Landlock**: FS tylko `/home/vibebridge/work` + `.vibe/logs`, sieć tylko `matrix.owl-post.eu` + `api.mistral.ai`
- [ ] **Seccomp** (opcjonalnie): blokada `ptrace`, `mount`
- [ ] Token tylko w nagłówku `Authorization: Bearer` (nigdy w URL)
- [ ] Subprocess: `process_group(0)`, `stdin(null)`, rlimit (nofile, nproc, as)
- [ ] Rate limiting przez `governor`
- [ ] `cargo audit` + `cargo deny` + `trivy` w CI

### Faza 4 — produkcja (1 tyg.)
- [ ] Testy integracyjne z mock Matrix serverem
- [ ] Fuzz `clean_response_text` przez `cargo-fuzz`
- [ ] Metryki Prometheus
- [ ] Graceful shutdown: drain kolejki, typing off, sync przed wyjściem

---

## 4. Hardening — fixy względem znanych bugów (FIX_NOTES4.md)

| Bug (Python) | Rozwiązanie w Rust |
|---|---|
| Path traversal `_resolve_media` | `canonicalize()?` + `starts_with(&work_dir)` |
| `MATRIX_BOT_TOKEN` w env podprocesu | `Command::env_clear()` + `secrecy` |
| Osierocone procesy przy timeout | `kill_on_drop(true)` + `process_group(0)` |
| Cały stdout do RAM | `BufReader::lines()` + `take(max_bytes)` |
| Token w query URL | reqwest + Bearer header |
| Brak backpressure | `mpsc::channel(16)` |
| `seen_set` tylko w RAM | `redb` — dedup przetrwa restart |
| Brak rate limit | `governor::RateLimiter` |
