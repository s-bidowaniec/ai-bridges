# Plan wdrożenia obsługi wiadomości głosowych w moście Matrix ↔ Vibe (`VOICE_PLAN.md`)

**Data analizy:** 2026-11-21  
**Zakres:** Analiza techniczna i plan wdrożenia dwukierunkowego głosu (`m.audio` / Matrix voice message) w `/home/vibebridge/bridge/bridge.py` (obecnie `vibe 3.1.3`, headless `vibe --yolo -p`).  
**Środowisko docelowe:** VPS Linux x86_64 (Ubuntu 24.04 Noble), 6 vCPU, 11 GiB RAM (9 GiB wolne, 0 B swap), brak GPU, brak karty dźwiękowej (`/dev/snd` zawiera wyłącznie `seq` i `timer`). Usługa `vibe-bridge.service` działa jako użytkownik `vibebridge` z `/usr/bin/python3` (Python 3.12.3, `matrix-nio` + `aiohttp`), `ProtectSystem=strict`, `ReadWritePaths=/home/vibebridge`.

---

## 1. Możliwości głosowe `mistral-vibe 3.1.3` i Mistral API

### 1.1. Czy `vibe` ma głos w trybie headless (`vibe -p`)?
**Nie.** Przegląd flag CLI (`vibe --help`) oraz źródeł pakietu `/root/.local/share/uv/tools/mistral-vibe` potwierdza:
- `vibe` nie posiada żadnych flag CLI dla audio/STT/TTS (wszystkie flagi headless to m.in. `-p/--prompt`, `--prompt-file`, `--output text|json|streaming`, `--output-dir`, `-c/--continue`, `--resume`, `--yolo/--auto-approve`, `--max-turns`, `--max-price`, `--max-tokens`, `--time-limit`, `--model`, `--thinking`).
- `vibe` uruchamiane przez `/usr/local/bin/vibe` wywołuje `vibe/cli/launcher.py`, który robi `os.execvpe` na natywną binarkę Rust TUI **`vibe/_bin/vibe-rs`** (~180 MB) komunikującą się z `vibe-app-server`.
- W TUI komenda `/voice` służy do konfiguracji trybu głosowego, a skrót rozpoczęcia/zatrzymania nagrywania to **`Ctrl+T`** (uwaga: `Ctrl+R` to wyszukiwanie w historii `reverse-i-search` lub `redo` w trybie vi, a nie nagrywanie głosu).
- Zarówno TUI (`vibe-rs`), jak i adapter ACP (`vibe-acp` → `vibe/acp/voice.py`) są na sztywno spięte z fizycznym mikrofonem i głośnikiem systemu przez **`miniaudio`** (`CaptureDevice` / `PlaybackDevice`). Na headless VPS bez karty dźwiękowej kończy się to błędem `NoAudioInputDeviceError` / `NoAudioOutputDeviceError`.

### 1.2. Co dokładnie znajduje się w pakiecie `/root/.local/share/uv/tools/mistral-vibe`?
W zainstalowanym pakiecie (Python 3.14, `vibe 3.1.3` + `mistralai` SDK) istnieją gotowe komponenty głosowe oparte o rodzinę modeli **Mistral Voxtral** (brak `whisper`, brak `magistral` dla audio — Magistral to modele tekstowe reasoning):

| Ścieżka w `site-packages/` | Główne klasy / symbole | Rola i używane API |
|---|---|---|
| `vibe/cli/transcribe/mistral_transcribe_client.py` | `MistralTranscribeClient` | Realtime STT po WebSocket (`wss://api.mistral.ai`) przez `client.audio.realtime.transcribe_stream(...)`, domyślny model `voxtral-mini-transcribe-realtime-2602` (`pcm_s16le`, 16 kHz). |
| `vibe/cli/tts/mistral_tts_client.py` | `MistralTTSClient` | REST TTS (`POST https://api.mistral.ai/v1/audio/speech`) przez `client.audio.speech.complete_async(...)`, domyślny model `voxtral-mini-tts-latest`, domyślny głos `gb_jane_neutral`, zwraca base64 `response.audio_data`. Obsługiwane `SpeechOutputFormat`: `'pcm'`, `'wav'`, `'mp3'`, `'flac'`, **`'opus'`**. |
| `vibe/cli/audio_recorder/audio_recorder.py` | `AudioRecorder`, `NoAudioInputDeviceError` | Nagrywanie z lokalnego mikrofonu przez `miniaudio.CaptureDevice` (blokuje użycie na VPS bez urządzenia capture). |
| `vibe/cli/audio_player/audio_player.py` | `AudioPlayer`, `NoAudioOutputDeviceError` | Odtwarzanie WAV na lokalnym głośniku przez `miniaudio.PlaybackDevice`. |
| `vibe/cli/voice_manager/voice_manager.py` | `VoiceManager`, `TranscribeState` | Orkiestracja `AudioRecorder` + `MistralTranscribeClient` (tryb dyktowania). |
| `vibe/cli/narrator_manager/narrator_manager.py` | `NarratorManager`, `NarratorState` | Orkiestracja podsumowania tury (`TurnSummaryTracker`) → `MistralTTSClient` → `AudioPlayer`. |
| `vibe/app_server/_narration.py` | `NarrationService` | Bardzo cenny wzorzec: przed wysłaniem odpowiedzi do TTS generuje zwięzłe, „mówione” streszczenie tury modelem `mistral-vibe-cli-fast` (`UtilityPrompt.TURN_SUMMARY`, `max_tokens=512`). |
| `vibe/acp/voice.py` + `vibe/acp/agent.py` | `VoiceController` | Rozszerzenia protokołu ACP (`voice/transcribeStart`, `voice/transcribeStop`, `voice/narrate`, `voice/narrateCancel`) — cienki wrapper nad `VoiceManager`/`NarratorManager`, więc też wymaga lokalnego sprzętu audio. |
| `vibe/core/config/vibe_schema.py`, `vibe/core/config/models.py` | `TranscribeModelConfig`, `TTSModelConfig` | Konfiguracja `voice_mode_enabled`, `narrator_enabled`, `active_transcribe_model = "voxtral-realtime"`, `active_tts_model = "voxtral-tts"`. |
| `mistralai/client/transcriptions.py` | `Transcriptions.complete` / `complete_async` | **Batch REST STT**: `POST /v1/audio/transcriptions` (multipart z plikiem audio, `model`, `language`, `diarize`, `context_bias`). Idealny dla plików głosowych z Matrixa. |
| `mistralai/client/speech.py` | `Speech.complete` / `complete_async` | **REST TTS**: `POST /v1/audio/speech` (`model`, `input`, `voice_id`, `response_format="opus"`). |
| `mistralai/client/voices.py` | `Voices` | `GET /v1/audio/voices` — lista dostępnych presetów i własnych głosów. |

**Kluczowy wniosek:** Sam `vibe` nie przyjmie audio w trybie `-p`, ale **Mistral API** (do którego mostek **już ma `MISTRAL_API_KEY`** w `/home/vibebridge/bridge/env`) udostępnia kompletne, oficjalne REST API zarówno do STT (`/v1/audio/transcriptions`), jak i TTS (`/v1/audio/speech` z natywnym wyjściem `opus`) z pełnym wsparciem języka polskiego.

---

## 2. Odbieranie i wysyłanie wiadomości głosowych w Matrix (Element)

### 2.1. Odbieranie (`Matrix → mostek`)
Większość infrastruktury **już działa w `/home/vibebridge/bridge/bridge.py`**:
- Callback `handle_attachment` jest już zarejestrowany na `RoomMessageAudio` (w `ATTACHMENT_EVENTS`).
- `download_media(mxc_url)` już korzysta z wymaganego przez nowoczesny Synapse (Matrix 1.11) autoryzowanego endpointu:
  `GET https://matrix.owl-post.eu/_matrix/client/v1/media/download/{server_name}/{media_id}`
  z nagłówkiem `Authorization: Bearer <MATRIX_BOT_TOKEN>` (i fallbackiem na `client_global.download()`).
- **Rozpoznanie wiadomości głosowej vs zwykłego pliku audio:**
  Element wysyła wiadomości głosowe jako `m.room.message` z `msgtype: "m.audio"`, kontenerem **Ogg/Opus** (`audio/ogg` lub `audio/ogg; codecs=opus`) oraz pustym znacznikiem **`"org.matrix.msc3245.voice": {}`** w `event.source["content"]`.
  - Jeśli w `content` jest klucz `"org.matrix.msc3245.voice"` (lub dowolny `RoomMessageAudio`, jeśli chcemy transkrybować też przesłane pliki audio): mostek kieruje plik do pipeline'u STT zamiast wysyłać do `vibe` surową ścieżkę `[załącznik: ... opisz ten plik]`.

### 2.2. Wysyłanie (`mostek → Matrix` jako natywna bańka Voice Message)
Aby Element (Web, Desktop, Element X / Mobile) wyświetlił wiadomość jako **interaktywną notatkę głosową z wykresem fali (waveform)**, a nie zwykły załącznik do pobrania:
1. **Kodek i kontener:** Wymagany jest **Ogg/Opus** (`mimetype: "audio/ogg"`, np. mono, 24–32 kbps, 48 kHz lub 24 kHz). Wysłanie MP3 z flagą `msc3245.voice` powoduje problemy z odtwarzaniem/renderowaniem w części klientów Element.
2. **Upload pliku:** `POST /_matrix/media/v3/upload?filename=reply.ogg` z `Content-Type: audio/ogg` i `Authorization: Bearer <token>` → zwraca `{"content_uri": "mxc://..."}` (funkcja `upload_media()` już jest w `bridge.py`).
3. **Metadane dźwięku:**
   - `duration`: czas trwania w **milisekundach** (np. z `ffprobe`).
   - `waveform` (MSC3246): tablica 64–100 liczb całkowitych z zakresu **`0..1024`** reprezentujących obwiednię głośności (RMS/peak) w równych przedziałach czasowych (wyliczana z dekodowanego PCM przez `ffmpeg` + prosty kod w Pythonie — ~15 linii, bez ciężkich bibliotek).
4. **Payload eventu `m.room.message`:**
```json
{
  "msgtype": "m.audio",
  "body": "Voice message",
  "filename": "reply.ogg",
  "url": "mxc://matrix.owl-post.eu/AbCdEf123456",
  "info": {
    "mimetype": "audio/ogg",
    "size": 24576,
    "duration": 6200
  },
  "org.matrix.msc1767.audio": {
    "duration": 6200,
    "waveform": [12, 48, 190, 420, 680, 510, 300, 120, 40, 15]
  },
  "org.matrix.msc3245.voice": {}
}
```
*(Opcjonalnie można dodać `"org.matrix.msc1767.file"` / `"org.matrix.msc1767.text"`, ale dla Elementa kluczowe są powyższe pola: `msgtype`, `url`, `info`, `org.matrix.msc1767.audio` oraz `org.matrix.msc3245.voice`.)*

---

## 3. Propozycja architektury: porównanie opcji (a) i (b)

### Opcja (a): Headless `vibe -p` + pipeline STT/TTS w `bridge.py`
Przepływ danych:
```text
Element (voice note m.audio + msc3245.voice)
  └─▶ bridge.py: download_media (Media v1) → /home/vibebridge/work/uploads/<evt>.ogg
        └─▶ STT (Mistral Voxtral API lub lokalny faster-whisper) → transkrypcja (tekst)
              ├─▶ (opcjonalnie) krótki m.notice z transkrypcją: "🎤 <tekst>"
              └─▶ queue → vibe_worker: vibe --yolo [-c] -p "<transkrypcja>"
                    └─▶ odpowiedź tekstowa + ewentualne obrazy/wideo (_extract_media)
                          ├─▶ wysłanie pełnego tekstu (m.notice) + mediów (m.image/m.video)
                          └─▶ przygotowanie tekstu mówionego (oczyszczenie markdown / krótkie streszczenie)
                                └─▶ TTS (Mistral Voxtral TTS lub Piper lokalnie) → Ogg/Opus + duration + waveform
                                      └─▶ upload_media (v3) → room_send (m.audio + msc3245.voice)
```
**Zalety:**
- Pełna deterministyczność: proces `vibe -p` kończy się kodem powrotu i kompletnym `stdout` (zero parsowania TUI).
- Brak zależności od karty dźwiękowej, PTY czy sesji terminalowych.
- Naturalna integracja z istniejącą kolejką `asyncio.Queue`, reakcjami (`👀` → `👍` → `✅` / `❌`) i ekstraktorem mediów w `bridge.py`.
- **Pamięć konwersacji bez TUI:** jeśli potrzebna jest persistentna sesja między wiadomościami, `vibe` w trybie headless **natywnie obsługuje `-c` / `--continue` oraz `--resume <SESSION_ID>`** (oraz `--output json` do odczytu metadanych sesji). Można więc mieć ciągłość rozmowy bez uruchamiania TUI.

### Opcja (b): Persistentna sesja `vibe` TUI w `tmux` z `/voice`
**Ocena techniczna: niezalecane i w praktyce niewykonalne w czystej formie dla `/voice` na tym VPS.**
1. **Brak sterowania przez `stdin`/`stdout` non-TTY:** `vibe-rs` to pełnoekranowe TUI w Ruście (alternate screen, sekwencje ANSI, paski stanu, kolejka promptów). Bez PTY nie uruchomi się interaktywnie.
2. **Kruchość automatyzacji `tmux` (`send-keys` + `capture-pane`):**
   - Brak niezawodnego sygnału końca generowania tury (trzeba heurystycznie parsować ekran/prompt/spinner).
   - Ucinanie długich odpowiedzi przez rozmiar bufora `tmux`, mieszanie ramek/kolorów ANSI z treścią, ryzyko zawieszenia na modalach lub podpowiedziach.
3. **Blokada `/voice` na headless VPS:** `/voice` w `vibe` otwiera systemowe urządzenie audio przez `miniaudio`/driver audio. Ten VPS nie ma karty dźwiękowej (`/dev/snd` ma tylko `seq` i `timer`). Aby zmusić TUI do „usłyszenia” pliku z Matrixa i „nagrania” odpowiedzi, trzeba by:
   - zbudować wirtualną pętlę audio (PipeWire/PulseAudio null-sink + virtual source lub moduł jądra `snd-aloop`),
   - wysyłać `Ctrl+T` do `tmux`, odtwarzać plik z Matrixa w **czasie rzeczywistym 1:1** (30 s wiadomości = 30 s czekania na „odegranie do mikrofonu”), ponownie wysyłać `Ctrl+T`,
   - przechwytywać strumień wyjściowy z wirtualnego głośnika (z detekcją ciszy, żeby wiedzieć, kiedy narrator skończył mówić).
   Jest to konstrukcja skrajnie krucha, wolna i trudna w debugowaniu.

*(Uwaga na przyszłość: jeśli kiedyś byłaby potrzeba długotrwałego, dwukierunkowego protokołu z `vibe` bez spawnowania procesu na każdą wiadomość, właściwą ścieżką nie jest `tmux` TUI, lecz `vibe-acp` — oficjalny interfejs JSON-RPC po stdio, wciąż jednak z obsługą STT/TTS po stronie mostka.)*

---

## 4. Rekomendacja i wybór narzędzi STT/TTS na Linux headless

### Rekomendacja architektury
Jednoznacznie **Opcja (a) — headless `bridge.py` + modułowy pipeline STT/TTS**. Jest o rząd wielkości prostsza, w pełni deterministyczna, odporna na restarty (`systemd`), działa bez karty dźwiękowej i wykorzystuje istniejący kod `download_media`/`upload_media`/`vibe_worker`.

### Porównanie narzędzi STT i TTS na Linux headless (Ubuntu 24.04, 6 vCPU, 11 GiB RAM, brak GPU)

| Narzędzie | Typ | Instalacja (`apt` / `pip`) | Polski (PL) | Zasoby na VPS | Koszt | Uwagi dla headless VPS |
|---|---|---|---|---|---|---|
| **Mistral Voxtral Transcribe** (`voxtral-mini-latest` / `voxtral-mini-transcribe-3-0`) | STT (API) | Zero nowych pakietów (`aiohttp` już jest + `MISTRAL_API_KEY` w `env`) | Bardzo dobry | ~0 CPU / ~0 RAM | ~$0.0035 / min | **Najprostszy start.** Przyjmuje `.ogg`/`.mp3`/`.wav` bezpośrednio (multipart POST). Szybkość ~1–2 s. |
| **`faster-whisper`** (CTranslate2) | STT (lokalny) | `pip install faster-whisper` (brak w `apt`) | Bardzo dobry (`small`/`medium`/`turbo`) | `small`: ~0.5 GB RAM; `medium`: ~1.5 GB RAM; 6 vCPU int8 ≈ 0.3–0.8× RT | $0 | **Najlepsza opcja offline.** Wymaga konwersji lub dekodowania (PyAV/ffmpeg). Uwaga na brak swapu przy modelach `large`. |
| **`whisper.cpp`** | STT (lokalny) | **Brak w `apt` Noble** (kompilacja `cmake` z GitHub lub `pip install pywhispercpp`) | Bardzo dobry | Bardzo niski (~0.3–1 GB RAM) | $0 | Szybki na CPU, ale wymaga budowania/utrzymania binarki i konwersji wejścia do 16 kHz WAV (`ffmpeg`). |
| **Mistral Voxtral TTS** (`voxtral-mini-tts-latest` / `voxtral-mini-tts-3-0`) | TTS (API) | Zero nowych pakietów (`aiohttp` + `MISTRAL_API_KEY`) | Tak (wielojęzyczny, 35+ języków) | ~0 CPU / ~0 RAM | ~$13 / 1M znaków (~$0.0065 / 500 zn.) | **Najprostszy start.** Natywny `response_format="opus"` — brak dodatkowego kodowania do Opus (ew. remux do Ogg przez `ffmpeg`). |
| **`piper-tts`** (Rhasspy Piper) | TTS (lokalny) | `pip install piper-tts` + model `.onnx` (**UWAGA:** `apt install piper` w Ubuntu Noble to zupełnie inny program — GTK konfigurator myszy `libratbag`!) | Bardzo dobry (`pl_PL-gosia-medium`, `pl_PL-darkman-medium`) | ~150 MB RAM, ułamek sekundy na zdanie na CPU | $0 | **Najlepsza opcja offline/darmowa.** Generuje WAV, który `ffmpeg` pakuje w ~50 ms do Ogg/Opus. |
| **`edge-tts`** | TTS (API nieoficjalne) | `pip install edge-tts` (brak w `apt`) | Świetny (`pl-PL-ZofiaNeural`, `pl-PL-MarekNeural`) | ~0 CPU / ~0 RAM | $0 | Świetna jakość za darmo, ale nieoficjalne API MS (bywały blokady/zmiany tokenów — dobre jako opcja z fallbackiem na Piper). Zwraca MP3 → `ffmpeg` do Ogg/Opus. |
| **`kokoro` / `kokoro-onnx`** | TTS (lokalny) | `pip install kokoro-onnx` (brak w `apt`) | **Brak oficjalnego wsparcia PL** | ~400 MB RAM, szybki na CPU | $0 | Świetny po angielsku, nie nadaje się do polskiego. |
| **`ffmpeg` + `ffprobe`** | Narzędzia audio | `apt install ffmpeg` (kandydat `7:6.1.1-3ubuntu5`; uwaga: obecnie **nie jest** zainstalowany globalnie — `/root/.hermes/.../ffmpeg` jest niedostępny dla `vibebridge`!) | — | Minimalne | $0 | Niezbędne do wyliczenia `duration`, generowania `waveform` oraz transkodowania do `audio/ogg` (libopus). |

### Rekomendowany wybór silników (pragmatyczny)
1. **Wariant preferowany na start (najmniejsza złożoność, 0 obciążenia VPS):**
   - **STT:** Mistral API `POST /v1/audio/transcriptions` (`voxtral-mini-latest`, `language="pl"` lub auto) — wykorzystuje istniejący `MISTRAL_API_KEY` i `aiohttp`.
   - **TTS:**
     - **Opcja 1 (API, spójna z Mistralem):** Mistral API `POST /v1/audio/speech` (`voxtral-mini-tts-latest`, `response_format="opus"`) — przetestować jakość polskiego głosu przed zatwierdzeniem; **lub**
     - **Opcja 2 (lokalny, darmowy, 100% stabilny na PL):** **`piper-tts`** (`pl_PL-gosia-medium`, instalacja w venv dla `vibebridge`) + `ffmpeg` (WAV → Ogg/Opus).
   - **Architektura z interfejsem `STTProvider` / `TTSProvider`** pozwoli przełączać backend zmienną w `/home/vibebridge/bridge/env` (np. `VOICE_STT_BACKEND=mistral|faster_whisper`, `VOICE_TTS_BACKEND=mistral|piper|edge`) bez zmian w logice mostka.

---

## 5. Ryzyka i ograniczenia

1. **Rozmiary audio i formaty:**
   - Notatka głosowa Ogg/Opus (24–32 kbps) ma zaledwie ~180–240 KB/min, więc limit `MAX_MEDIA_BYTES = 50 MiB` jest bezpieczny. Jednak użytkownik może wysłać duży WAV/FLAC lub bardzo długie nagranie — warto dodać osobny limit dla głosu (np. `VOICE_MAX_AUDIO_BYTES = 15 MiB` i limit czasu nagrania np. 10 min przed wysłaniem do STT).
   - Pliki w `/home/vibebridge/work/uploads/` i tymczasowe pliki TTS rosną na dysku — przyda się okresowe czyszczenie tymczasowych plików TTS po wysłaniu.
2. **Limity czasu i blokowanie kolejki:**
   - Obecnie `bridge.py` ma jednego workera (`vibe_worker`) z `VIBE_TIMEOUT_SEC = 600`. Dodanie STT (2–10 s) i TTS (1–5 s) wydłuża czas jednej tury.
   - Timeout dla zapytań STT/TTS należy ustawić osobno (np. 60 s) z czytelnym komunikatem błędu i fallbackiem do samej odpowiedzi tekstowej (jeśli TTS zawiedzie, użytkownik i tak powinien dostać tekst z `vibe`!).
3. **Koszty API (przy wariancie Mistral API):**
   - STT jest bardzo tanie (~$0.0035/min → 100 minut nagrań ≈ $0.35).
   - TTS kosztuje ok. **$13 / 1M znaków**. Jeśli `vibe` zwróci 15 000 znaków logów lub kodu i wszystko trafi do TTS, jedno wywołanie kosztowałoby ~$0.20 i wygenerowałoby 15-minutowe nagranie nie do słuchania.
   - **Konieczne zabezpieczenie:** zawsze przycinać lub streszczać odpowiedź przed TTS (np. limit `VOICE_TTS_MAX_CHARS = 1500` znaków, usuwanie bloków kodu ```` ```...``` ```` i URL-i, albo krótki call do lekkiego modelu `mistral-small` w stylu `vibe/app_server/_narration.py` generujący 2–5 zdań podsumowania do odsłuchu, podczas gdy pełny tekst + kod idzie jako `m.notice`).
4. **Jakość i UX:**
   - Czytanie na głos surowego Markdownu (`**`, `` ` ``, tabel, diffów, długich ścieżek `/home/vibebridge/...`) brzmi źle — niezbędny jest prosty sanityzer tekstu dla TTS („wersja mówiona”) + równoległa wysyłka pełnej odpowiedzi tekstowej (`m.notice`), chyba że odpowiedź jest bardzo krótka.
   - Jeśli użytkownik wyśle zwykłą wiadomość tekstową, bot **nie powinien** domyślnie spamować głosem — odpowiedź głosowa powinna uruchamiać się albo automatycznie w odpowiedzi na `m.audio` (tryb `VOICE_REPLY_MODE=on_voice`), albo na jawne żądanie (np. prefiks `/voice` lub przełącznik `always`).
   - Lokalny `faster-whisper` potrafi „halucynować” napisy na nagraniach zawierających samą ciszę — warto użyć `vad_filter=True` lub odrzucać nagrania krótsze niż ~0.5 s / o zerowym poziomie sygnału.
   - `edge-tts` nie ma SLA (nieoficjalny endpoint Microsoftu), więc nie powinien być jedynym backendem produkcyjnym bez fallbacku.
5. **Uprawnienia i środowisko `systemd`:**
   - W `vibe-bridge.service` ustawione jest `ProtectSystem=strict` i `ReadWritePaths=/home/vibebridge` — wszelkie modele lokalne (Piper `.onnx`, cache Whispera) i pliki tymczasowe muszą lądować wewnątrz `/home/vibebridge/` (albo `/tmp`, bo `PrivateTmp=yes`).
   - Globalny `ffmpeg` musi być zainstalowany przez `apt` (w `/usr/bin/ffmpeg`), bo wersja w `/root/.hermes/` jest niedostępna dla użytkownika `vibebridge`.
   - Jeśli używamy `piper-tts` / `faster-whisper`, lepiej utworzyć dedykowany venv (np. `/home/vibebridge/bridge/.venv`) zamiast zaśmiecać systemowy `/usr/bin/python3` na Ubuntu 24.04 (PEP 668).

---

## 6. Krótki plan wdrożenia krok po kroku (dla wybranej opcji a)

1. **Przygotowanie zależności systemowych (root):**
   - Zainstalować `ffmpeg` (daje `ffmpeg` i `ffprobe` w `/usr/bin/`): `apt update && apt install -y ffmpeg`.
   - *(Opcjonalnie dla wariantu lokalnego TTS/STT)*: utworzyć `/home/vibebridge/bridge/.venv` (właściciel `vibebridge:vibebridge`) z `matrix-nio`, `aiohttp` oraz `piper-tts` (i pobrać model `pl_PL-gosia-medium.onnx` + `.json` do `/home/vibebridge/bridge/models/piper/`).
   - Zweryfikować działającym `MISTRAL_API_KEY` dostępność `/v1/audio/transcriptions` i `/v1/audio/speech` (oraz listę głosów z `/v1/audio/voices`, szczególnie dla języka polskiego).

2. **Wydzielenie modułu `/home/vibebridge/bridge/voice_io.py`:**
   - Funkcja `is_voice_message(event) -> bool`: sprawdza `isinstance(event, RoomMessageAudio)` oraz obecność `"org.matrix.msc3245.voice"` (z opcją obsługi dowolnego `RoomMessageAudio`).
   - Abstrakcja `transcribe_audio(path: Path) -> str`:
     - Domyślnie: async HTTP `POST https://api.mistral.ai/v1/audio/transcriptions` przez `aiohttp` (model `voxtral-mini-latest`, multipart `file` + opcjonalnie `language`), timeout 60 s.
     - Opcjonalny backend lokalny `faster-whisper`.
   - Funkcja `prepare_spoken_text(raw_text: str) -> str`: usuwa bloki kodu, formatowanie Markdown, długie ścieżki/URL-e i skraca do `VOICE_TTS_MAX_CHARS` (lub opcjonalnie wywołuje szybkie streszczenie jak w `vibe/app_server/_narration.py`).
   - Abstrakcja `synthesize_speech(text: str, out_ogg: Path) -> Path`:
     - Backend `mistral` (`POST /v1/audio/speech`, `model="voxtral-mini-tts-latest"`, `response_format="opus"` → remux/normalizacja do `audio/ogg` libopus przez `ffmpeg`) **lub** backend `piper` (WAV → `ffmpeg -c:a libopus -b:a 32k -ac 1`).
   - Funkcja `analyze_audio_ogg(path: Path) -> Tuple[int, List[int]]`: przez `ffprobe` pobiera `duration` (ms), a przez `ffmpeg` (dekodowanie do surowego PCM `s16le` mono 8 kHz na stdout) wylicza 64-elementową tablicę `waveform` w skali `0..1024`.

3. **Rozszerzenie `/home/vibebridge/bridge/bridge.py`:**
   - Dodać `send_voice(path: Path, body: str = "Voice message")`: wywołuje `analyze_audio_ogg()`, `upload_media(path, mime="audio/ogg")`, a następnie `room_send(..., "m.room.message", {msgtype: "m.audio", url, info, "org.matrix.msc1767.audio", "org.matrix.msc3245.voice": {}})`.
   - Rozszerzyć elementy kolejki z `(prompt, event_id)` na strukturę np. `(prompt, event_id, want_voice: bool)`.
   - W `handle_attachment()`: dla wiadomości głosowych (`RoomMessageAudio` z `msc3245.voice`) po pobraniu pliku uruchomić `transcribe_audio()`, opcjonalnie wysłać krótki `m.notice` z rozpoznaną transkrypcją (`🎤 ...`) i wrzucić do kolejki `(transcript, event_id, want_voice=True)`.
   - W `vibe_worker()` / `send_output()`: po uzyskaniu odpowiedzi od `vibe`, wysłać normalnie tekst + obrazy/wideo, a gdy `want_voice=True` (lub `VOICE_REPLY_MODE=always`), wygenerować `reply.ogg` przez `prepare_spoken_text()` + `synthesize_speech()` i wysłać `send_voice()` (z fallbackiem: błąd TTS loguje ostrzeżenie, nie psuje odpowiedzi tekstowej), po czym usunąć plik tymczasowy.
   - *(Opcjonalnie)* dodać obsługę komend czatowych w `on_event` (np. `!voice on/off/auto`) oraz konfigurację zmiennymi środowiskowymi w `/home/vibebridge/bridge/env`: `VOICE_ENABLED`, `VOICE_STT_BACKEND`, `VOICE_TTS_BACKEND`, `VOICE_TTS_VOICE`, `VOICE_REPLY_MODE`, `VOICE_TTS_MAX_CHARS`.

4. **Testy i wdrożenie:**
   - Napisać test jednostkowy/integracyjny (analogicznie do `/home/vibebridge/bridge/test_attach.py`): synteza krótkiego tekstu po polsku → sprawdzenie nagłówka Ogg/Opus, `duration > 0`, `len(waveform) == 64` → transkrypcja wygenerowanego pliku (round-trip TTS → STT) → test wysyłki `m.audio` z `msc3245.voice` do pokoju Matrix.
   - Zweryfikować renderowanie bańki głosowej i odtwarzanie w Element (Desktop/Web i Mobile).
   - Zrestartować `systemctl restart vibe-bridge` i sprawdzić logi (`journalctl -u vibe-bridge -f`).

## Faza 6 — PRAWDZIWE DZWONIENIE (m.call / MSC3245) — PROTOTYP GOTOWY
`voice_call.py` (obok bridge.py, ten sam env): bot odbiera `m.call.invite` z pokoju,
answeruje przez aiortc (WebRTC), audio z rozmowy -> STT (Mistral voxtral) co ~3.5s
-> `vibe -p --resume <sesja>` (kontekst rozmowy) -> TTS (voxtral-mini-tts) -> audio do call.
Serwis: `voice-call.service` (systemd user). Wymaga: pip install aiortc (zrobione),
publiczny IP VPS (host ICE candidates) — jak VPS za NAT: potrzebny TURN.
Test E2E: zadzwoń do bota z Element w pokoju -> bot odbiera i rozmawia głosem.
Docelowo: przepisać na Rust (webrtc crate) — Faza 7.
