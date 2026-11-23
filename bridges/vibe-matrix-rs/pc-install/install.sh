#!/usr/bin/env bash
# Instalacja vibe-matrix-rs na PC (Linux, user-level systemd) — tryb voice do auta.
# Użycie: ./pc-install/install.sh
set -euo pipefail

REPO="https://github.com/s-bidowaniec/ai-bridges.git"
DEST="$HOME/ai-bridges"
BRIDGE="$DEST/bridges/vibe-matrix-rs"
CONF_DIR="$HOME/.config/vibe-matrix-rs"
STATE_DIR="$HOME/.local/share/vibe-matrix-rs"

echo "== 1/5 klon repo"
if [ -d "$DEST/.git" ]; then (cd "$DEST" && git pull -q); else git clone -q "$REPO" "$DEST"; fi

echo "== 2/5 build"
cd "$BRIDGE"
if command -v cargo >/dev/null; then
  cargo build --release -p bridge-headless
else
  echo "BRAK cargo — zainstaluj rust (https://rustup.rs) i odpal ponownie" >&2; exit 1
fi

echo "== 3/5 config (env z tokenami)"
mkdir -p "$CONF_DIR" "$STATE_DIR" "$STATE_DIR/uploads" "$STATE_DIR/vibe-work" "$STATE_DIR/tmux"
if [ ! -f "$CONF_DIR/env" ]; then
  sed "s|%h|$HOME|g" pc-install/env.example > "$CONF_DIR/env"
  chmod 600 "$CONF_DIR/env"
  echo "Wypełnij $CONF_DIR/env (MATRIX_ACCESS_TOKEN, MATRIX_ROOM_ID, MISTRAL_API_KEY...) i odpal ponownie." >&2
  exit 0
fi
chmod 600 "$CONF_DIR/env"

echo "== 4/5 systemd user unit"
mkdir -p "$HOME/.config/systemd/user"
sed "s|%h|$HOME|g" pc-install/vibe-matrix-rs.service > "$HOME/.config/systemd/user/vibe-matrix-rs.service"
systemctl --user daemon-reload

echo "== 5/5 enable + start"
systemctl --user enable --now vibe-matrix-rs
sleep 2
systemctl --user status vibe-matrix-rs --no-pager | head -8
echo
echo "== dopisuję TMUX_TMPDIR do rc shella (lokalny 'tmux attach' musi widzieć ten sam socket co mostek)"
TMUX_LINE='export TMUX_TMPDIR=$HOME/.local/share/vibe-matrix-rs/tmux'
for rc in "$HOME/.bashrc" "$HOME/.zshrc"; do
  if [ -f "$rc" ] && ! grep -qF 'TMUX_TMPDIR=$HOME/.local/share/vibe-matrix-rs/tmux' "$rc"; then
    printf '\n# vibe-matrix-rs: socket tmux współdzielony z mostkiem (tmux attach widzi sesję agenta)\n%s\n' "$TMUX_LINE" >> "$rc"
    echo "  dopisano do $rc"
  fi
done

echo "GOTOWE. Jedna sesja agenta w dwóch miejscach naraz:"
echo "  - lokalnie na PC:  tmux attach -t vibe-matrix-rs-vibe   (to samo okno, co czyta mostek; /window w pokoju otwiera je ponownie)"
echo "  - zdalnie przez Matrix/Element: piszesz w pokoju, mostek robi send-keys do tej samej sesji"
echo "W pokoju Matrix: /voice on  → voice messages → odpowiedzi głosem (ciągła konwersacja, kontekst w sesji)."
echo "Logi: journalctl --user -u vibe-matrix-rs -f"
