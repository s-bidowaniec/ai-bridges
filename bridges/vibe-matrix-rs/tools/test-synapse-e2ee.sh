#!/usr/bin/env bash
# Run the real E2EE integration test with a disposable loopback homeserver.
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "Usage: $0 <python-with-matrix-synapse>" >&2
  exit 2
fi

synapse_python="$(command -v "$1")"
workspace="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
scratch="$(mktemp -d)"
synapse_pid=""
cleanup() {
  if [[ -n "$synapse_pid" ]]; then
    kill "$synapse_pid" 2>/dev/null || true
    wait "$synapse_pid" 2>/dev/null || true
  fi
  rm -rf -- "$scratch"
}
trap cleanup EXIT

cd -- "$scratch"
"$synapse_python" -m synapse.app.homeserver --server-name bridge-test.invalid \
  --config-path homeserver.yaml --generate-config --report-stats=no
port="$("$synapse_python" - <<'PY'
import socket
from pathlib import Path

import yaml

with socket.socket() as listener:
    listener.bind(("127.0.0.1", 0))
    port = listener.getsockname()[1]
path = Path("homeserver.yaml")
config = yaml.safe_load(path.read_text())
config.update(
    enable_registration=True,
    enable_registration_without_verification=True,
    suppress_key_server_warning=True,
    rc_login={
        "address": {"per_second": 100, "burst_count": 100},
        "account": {"per_second": 100, "burst_count": 100},
    },
    listeners=[
        {
            "port": port,
            "bind_addresses": ["127.0.0.1"],
            "type": "http",
            "tls": False,
            "resources": [{"names": ["client"], "compress": False}],
        }
    ],
)
path.write_text(yaml.safe_dump(config))
print(port)
PY
)"
"$synapse_python" -m synapse.app.homeserver --config-path homeserver.yaml >synapse.log 2>&1 &
synapse_pid=$!
export MATRIX_TEST_URL="http://127.0.0.1:$port"
ready=""
for attempt in {1..40}; do
  if curl -fsS --max-time 1 "$MATRIX_TEST_URL/_matrix/client/versions" >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.25
done
if [[ -z "$ready" ]]; then
  tail -50 synapse.log >&2
  exit 1
fi

cd -- "$workspace"
cargo test --locked --workspace --test synapse_e2ee -- --ignored
