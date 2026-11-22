# vibe-matrix (legacy, Python)

Mostek Matrix room ↔ Vibe CLI (`vibe -p` headless + TUI w tmux) — wersja Python 3.12,
działająca na VPS. **Zastępowana przez [`../vibe-matrix-rs`](../vibe-matrix-rs)** (Rust).

## Stan źródeł w repo

Pełny `bridge.py` (~2570 linii) **nie jest kompletny w tym repo** — został przeniesiony
z innego PC i zachował się tylko fragment `bridge.py.b64.001` (base64, pierwsza część pliku;
dekoduje się z błędami kodowania, niekompilowalny). Nie uruchamiaj fragmentu jako skryptu.

Działająca instalacja żyje na VPS; w repo trzymamy ten ślad historyczny oraz prototyp Fazy 6 (`voice_call.py`, `voice-call.service`, `VOICE_PLAN.md` — WebRTC/aiortc `m.call`).
Nowe wdrożenia: użyj `../vibe-matrix-rs` (Rust, hardening, tmux/subprocess + voice).
