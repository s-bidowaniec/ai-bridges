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

command -v tmux >/dev/null || { echo "Zainstaluj tmux przed uruchomieniem mostka." >&2; exit 1; }

echo "== 2/5 build"
cd "$BRIDGE"
if command -v cargo >/dev/null; then
  cargo build --locked --release -p bridge-headless
else
  echo "BRAK cargo — zainstaluj rust (https://rustup.rs) i odpal ponownie" >&2; exit 1
fi

echo "== 3/5 config (env z tokenami)"
install -d -m 0700 "$CONF_DIR" "$STATE_DIR" "$STATE_DIR/uploads" "$STATE_DIR/voice" "$STATE_DIR/tmux"
mkdir -p "$STATE_DIR/vibe-work"
if [ ! -f "$CONF_DIR/env" ]; then
  sed "s|%h|$HOME|g" pc-install/env.example > "$CONF_DIR/env"
  chmod 600 "$CONF_DIR/env"
  echo "Wypełnij $CONF_DIR/env (token urządzenia, MATRIX_ROOM_ID i magazyn E2EE) i odpal ponownie." >&2
  exit 0
fi
chmod 600 "$CONF_DIR/env"

echo "== 4/5 systemd user unit"
mkdir -p "$HOME/.config/systemd/user"
sed "s|%h|$HOME|g" pc-install/vibe-matrix-rs.service > "$HOME/.config/systemd/user/vibe-matrix-rs.service"
cp pc-install/vibe-matrix-terminal.service "$HOME/.config/systemd/user/vibe-matrix-terminal.service"
systemctl --user daemon-reload

echo "== 5/5 enable + start"
systemctl --user enable --now vibe-matrix-rs
sleep 2
systemctl --user status vibe-matrix-rs --no-pager | head -8
echo
echo "W pokoju Matrix użyj /new projekt codex albo /sessions i /open <nazwa>."
echo "Lokalny dostęp: TMUX_TMPDIR=$STATE_DIR/tmux tmux list-sessions"
