# Terminal projektu przez Matrix

Bramka udostępnia żywe sesje tmux. W sesji działa natywny Vibe, Codex,
Claude Code albo Antigravity. Program startuje w `VIBE_WORKDIR` i używa
narzędzi oraz uprawnień swojego konta na hoście. W tej samej sesji można
pracować w lokalnym oknie albo z telefonu przez Matrix.

## Szybki start

1. Na hoście zainstaluj tmux i wybrane CLI. Budowanie wymaga Rust 1.96 lub nowszego. Zaloguj natywne CLI jako konto,
   które uruchamia tmux. Wybierz **prywatny pokój z włączonym E2EE**, z kontem
   mostka i zaufanymi uczestnikami.
2. Skonfiguruj konto i urządzenie mostka zgodnie z sekcją E2EE poniżej. Ustaw
   `VIBE_WORKDIR` na katalog projektu. Na PC instalator
   [pc-install/install.sh](pc-install/install.sh) przygotowuje usługi użytkownika
   i oddzielny serwer tmux, dzięki czemu restart bramki pozostawia terminale.
3. Na telefonie wpisz `/new mapy codex`. Lokalnie otwórz ten sam terminal:

   ```bash
   export TMUX_TMPDIR="$HOME/.local/share/vibe-matrix-rs/tmux"
   tmux attach -t vibe-matrix-rs-mapy
   ```

   Użyj tego samego katalogu socketu, z którego korzysta bramka. Odłącz lokalne
   okno przez `Ctrl-b d` albo zamknij emulator terminala. Proces w tmux działa
   dalej. Jeśli proces agenta lub serwer tmux zakończy się, stan ekranu znika.
   Zapisane pliki projektu pozostają na hoście.

## Praca z telefonu

| Komenda | Działanie |
|---|---|
| `/new mapy codex` | Tworzy nowy terminal w katalogu projektu. Istniejące sesje zostają otwarte. |
| `/sessions` | Zwraca nazwy działających terminali tej instancji. |
| `/open mapy` | Wraca do działającej sesji i pokazuje ostatnie linie ekranu. |
| `/attach` | Pokazuje ostatnie linie wybranego panelu i włącza dalsze przekazywanie zmian. |
| `/detach` | Odpina widok Matrix. `/stop` ma takie samo, niedestrukcyjne działanie. |
| `/key Escape`, `/key C-c` | Wysyła klawisz do agenta. Dostępne klawisze pokazuje `/help`. |
| `/agent codex` | Wybiera domyślną sesję tego agenta. |

Tekst z Matrixa jest wklejany do przypiętego panelu i zatwierdzany klawiszem
Enter. Bramkę można odłączyć, a po ponownym podłączeniu albo następnym wejściu
przez Matrix praca odbywa się w tej samej sesji. Bramce nadal znana jest ostatnia
wybrana sesja po restarcie. Nie wpisuje ponownie wiadomości z początkowego
snapshotu pokoju.

Z Matrixa widać tekstowe zmiany ekranu, także po wejściu lokalnym. Jest to
próbkowany widok terminala, więc interfejsy pełnoekranowe ani pojedyncze
odświeżenia między próbkami nie są odtwarzane piksel w piksel. Agent może pracować
dowolnie długo. Bramkę można odłączyć lub wysłać klawisz bez czekania na odpowiedź
modelu. Połączenia HTTP z Matrixem mają własne limity transportowe.

## Obrazy i wiadomości głosowe

- Obrazy PNG, JPEG, GIF i WebP z Matrixa trafiają do `UPLOADS_DIR/images-in`.
  Terminal otrzymuje lokalną ścieżkę i opis załącznika.
- Agent może odesłać np. render Blendera, mapę z QGIS lub zrzut ekranu.
  Zapisuje gotową kopię pod unikalną nazwą w `UPLOADS_DIR/images-out`.
  Widoczne pliki są wychwytywane od razu, dlatego zapisuj pod tymczasową nazwą
  z kropką na początku i zmień ją po zakończeniu zapisu. Po potwierdzeniu
  wysyłki mostek usuwa kopię z outboxa. `/status` pokazuje pełną ścieżkę.
- Opcjonalne `VOICE_INPUT_MODE=transcribe` wysyła odszyfrowane nagranie do
  Mistral STT po HTTPS, poza ochroną E2EE Matrixa. Ustaw `MISTRAL_API_KEY`
  w konfiguracji bramki, jeśli chcesz korzystać z tego trybu.
- Domyślne `VOICE_INPUT_MODE=files` przekazuje terminalowi plik z `VOICE_DIR/in`.
  Program obsługujący audio może opublikować odpowiedź z tą samą nazwą bazową
  w `VOICE_DIR/out`, z rozszerzeniem `.opus`, `.ogg`, `.wav` lub `.mp3`.
  Gotowy plik należy opublikować przez zmianę nazwy. Bramce odpowiada dokładnie
  jeden plik na żądanie. Po udanej wysyłce kopia jest usuwana. Ten tryb czeka
  w tle bez przełączania się na STT.
- Połączenia głosowe na żywo, w których telefon dzwoni do bramki, nie są jeszcze
  obsługiwane. Starszy prototyp w `../vibe-matrix/voice_call.py` otwiera osobny
  proces i nie współużytkuje żywej sesji terminala.

## Uruchomienie E2EE

1. Załóż dedykowane konto bota. Utwórz jego tożsamość cross-signing i klucz
   odzyskiwania w zweryfikowanym kliencie Matrix. Zaloguj mostek jako osobne
   urządzenie. Użyj tokena przypisanego do tego urządzenia i podaj jego
   identyfikator jako `MATRIX_BOT_DEVICE_ID`. Mostek sprawdza oba dane przez
   `/whoami`, a połączenie z homeserverem wymaga HTTPS.
2. Skopiuj [.env.example](.env.example) do prywatnego pliku konfiguracyjnego.
   Zapisz token w `MATRIX_ACCESS_TOKEN_FILE`. Wygeneruj osobne, losowe hasło i
   zapisz je w `MATRIX_STORE_PASSPHRASE_FILE`. Pliki sekretów wymagają trybu
   `0600`. Ustaw `MATRIX_STORE_DIR` na katalog stanu o trybie `0700`, osobny
   dla każdego urządzenia i instancji. Katalog zawiera klucze urządzenia oraz
   stan SDK w SQLite chroniony hasłem. Zachowaj katalog i hasło przy restartach.
3. Zatrzymaj usługę mostka na czas konfiguracji i uruchom z tą samą konfiguracją:
   `bridge-headless recover-device /prywatna/sciezka/do/klucza-odzyskiwania`.
   Plik klucza odzyskiwania wymaga trybu `0600`. Przechowuj kopię poza hostem
   i usuń kopię tymczasową, gdy weryfikacja urządzenia się powiedzie.
4. Porównaj publiczny klucz główny cross-signing operatora z wartością z jego
   zaufanego klienta, poza mostkiem. Zatwierdź operatora poleceniem
   `bridge-headless verify-user @operator:example.org <publiczny-klucz-base64>`.
   Operator również powinien zweryfikować konto mostka w swoim kliencie.
   Dopiero wtedy uruchom usługę i użyj `/attach` z urządzenia operatora.

Mostek przetwarza tylko zaszyfrowane zdarzenia z urządzeń zweryfikowanych przez
cross-signing. Klucze pokoju wysyła do zaufanych urządzeń. Wyłączone jest
przekazywanie historii E2EE przy zaproszeniach. Załączniki są szyfrowane przed
wysłaniem, a pobrane załączniki sprawdzane kryptograficznie przed zapisaniem.
W pokoju bez E2EE i przy błędzie weryfikacji mostek wstrzymuje działanie.
Reakcje zostały wyłączone, ponieważ ujawniałyby metadane poza szyfrowaniem.

E2EE kończy się na komputerze mostka. Terminal i pliki są tam odszyfrowane.
Katalogi `UPLOADS_DIR`, `VOICE_DIR` i ich podkatalogi używane przez mostek
muszą mieć tryb `0700`. Używaj wyłącznie konta hosta i natywnych narzędzi,
którym powierzysz zawartość projektu. Szyfrowanie Matrixa nie chroni przed
procesem działającym jako ten sam użytkownik systemu.

## Konfiguracja i migracja

- Zapisz dopuszczone **MXID** w `MATRIX_ALLOWED_USERS` jako listę oddzieloną
  przecinkami. Pusta lista przerywa start. Wspólny dostęp można jawnie
  włączyć przez `MATRIX_ALLOW_ALL_ROOM_MEMBERS=1` przy pustej liście.
  Bramka sprawdza listę obecnych członków przed publikacją tekstu, obrazu
  lub audio. Publikacja jest wstrzymywana, gdy do pokoju dołączy obca osoba
  lub serwer nie zwróci listy członków. Dostęp do historii już zapisanej w Matrix
  zależy od konfiguracji pokoju, więc ogranicz również widoczność historii.
- Domyślny prefiks sesji to `vibe-matrix-rs`. `TMUX_SESSION` może wskazywać
  wcześniej utworzoną sesję domyślną w tym samym serwerze. `/new` tworzy
  oddzielne sesje pod prefiksem. Natywne CLI można wskazać przez `VIBE_CMD`,
  `CODEX_CMD`, `CLAUDE_CMD` i `AGY_CMD`.
- W konfiguracjach ze starej wersji `AGENT_MODE`, `VIBE_TIMEOUT_SECS` i
  `SESSION_FILE` nie sterują już pracą terminala. Sesje zachowuje tmux,
  a bramka zapisuje nazwę wybranego terminala w `ACTIVE_TERMINAL_FILE`.
  Na PC ustaw ten plik w katalogu stanu użytkownika zgodnie z przykładem.
- Przy aktualizacji istniejącej instalacji sprawdź `TMUX_TMPDIR` przed
  restartem. Użyj starego socketu, jeżeli chcesz zachować działające sesje.
  Przy uruchamianiu poza instalatorem możesz wystartować serwer tmux poza
  procesem bramki poleceniem `tmux -D`, pod tym samym kontem i z tym samym
  `TMUX_TMPDIR`. Usługa systemowa pod dedykowanym kontem ma dostęp tylko
  do plików i narzędzi dostępnych dla tego konta.
- Bramce wystarczy wychodzące połączenie HTTPS z homeserverem. Nie udostępniaj
  socketu tmux przez sieć. Lokalna bramka nie przekazuje tokena Matrix do
  nowo uruchamianego tmuxa.

```bash
cargo build --locked --release --workspace
cargo test --locked --workspace
# Opcjonalny test E2EE na jednorazowym lokalnym Synapse z włączoną rejestracją:
MATRIX_TEST_URL=http://127.0.0.1:8008 \
  cargo test --locked --workspace --test synapse_e2ee -- --ignored
```
