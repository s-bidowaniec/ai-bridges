#!/usr/bin/env bash
# Bootstrap E2EE dla instancji mostka (per bot/agent).
#
# Użycie:
#   ./pc-install/e2ee-bootstrap.sh <agent> <plik-z-kluczem-odzyskiwania> <operator-mxid> <master-key-operatora-base64>
#
# Skąd wziąć dane:
#   - klucz odzyskiwania bota: zaloguj się w Element jako @<agent>:matrix.owl-post.eu
#     → Ustawienia → Bezpieczeństwo i prywatność → „Skonfiguruj” Secure Backup → skopiuj klucz odzyskiwania
#     do pliku na dysku (chmod 600), NIE wklejaj go do czatu
#   - operator-mxid + master-key: w Element jako operator → Ustawienia → Bezpieczeństwo
#     → „Klucz publiczny cross-signing” (master key base64)
#   - hasła botów: na VPS /root/matrix-credentials.txt (ssh vps 'sudo cat ...')
set -euo pipefail

AGENT="${1:?agent (vibe|agy|claude|codex)}"
KEYFILE="${2:?plik z kluczem odzyskiwania bota (0600)}"
OPERATOR="${3:?MXID operatora, np. @stanislaw:matrix.owl-post.eu}"
MASTER="${4:?master key cross-signing operatora (base64)}"

CONF="$HOME/.config/vibe-matrix-rs"
UNIT="vibe-matrix-rs@${AGENT}"

[ -f "$CONF/$AGENT.env" ] || { echo "brak $CONF/$AGENT.env" >&2; exit 1; }
stat -c '%a' "$KEYFILE" | grep -q '^600$' || { echo "klucz odzyskiwania musi mieć chmod 600" >&2; exit 1; }

set -a; source "$CONF/$AGENT.env"; set +a

echo "== zatrzymuję $UNIT"
systemctl --user stop "$UNIT" || true

echo "== recover-device (import cross-signing z klucza odzyskiwania)"
"$HOME/.local/bin/bridge-headless" recover-device "$KEYFILE"

echo "== verify-user $OPERATOR"
"$HOME/.local/bin/bridge-headless" verify-user "$OPERATOR" "$MASTER"

echo "== start $UNIT"
systemctl --user start "$UNIT"
sleep 3
systemctl --user status "$UNIT" --no-pager | head -6
echo
echo "GOTOWE dla $AGENT. Pamiętaj też zweryfikować urządzenie bota w Element jako operator"
echo "(Ustawienia → Bezpieczeństwo → urządzenia → zweryfikuj @${AGENT})."
shred -u "$KEYFILE" 2>/dev/null || rm -f "$KEYFILE"
