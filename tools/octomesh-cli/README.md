# octomesh-cli (`octo`)

**Hybrid P2P & Failover Mesh Bridge for Native Terminal AI Agents**

`octomesh-cli` connects native terminal AI agents (Claude CLI, Codex CLI, Mistral Vibe CLI, Lumo, Antigravity) into a resilient, peer-to-peer local mesh over the **Model Context Protocol (MCP)**. If an agent hits a rate limit, exhausts tokens, or times out, `octo` automatically places that peer on cooldown in a local SQLite blackboard and fails over to the next healthy peer in the role chain—preserving task context, `git diff`, and anti-loop guarantees.

---

## Architecture

```
+-------------------------------------------------------------------+
|                     Terminal Agent Session(s)                     |
|   (Claude CLI / Codex CLI / Mistral Vibe / Lumo / Antigravity)    |
+---------------------------------+---------------------------------+
                                  | MCP over stdio (JSON-RPC 2.0)
                                  v
+-------------------------------------------------------------------+
|                   octo mcp-serve / octo ask (Rust)                |
|                                                                   |
|  +-----------------------+    +--------------------------------+  |
|  |   Anti-Loop Guard     |    |     SQLite Blackboard          |  |
|  | - max_depth check     |    |     (.octomesh/state.db)       |  |
|  | - caller != target    |    | - Peer health & cooldowns      |  |
|  +-----------+-----------+    | - Task audit log               |  |
|              |                | - Cross-terminal handoff notes |  |
|              v                +---------------+----------------+  |
|  +--------------------------------------------+----------------+  |
|  |                      Failover Router                        |  |
|  |  Resolves target peer OR role chain -> iterates candidates  |  |
|  +------------------------------+------------------------------+  |
+---------------------------------|---------------------------------+
                                  | Subprocess (JSON on stdin/stdout)
                                  v
+-------------------------------------------------------------------+
|                  Python Adapter Layer (adapters/)                 |
|  +-------------+   +-----------+   +-----------+   +-----------+  |
|  |  runner.py  |-->|  mock.py  |   | claude.py |   |  vibe.py  |  |
|  | context.py  |   | (offline) |   | (headless)|   |  codex.py |  |
|  +-------------+   +-----------+   +-----------+   +-----------+  |
+-------------------------------------------------------------------+
```

---

## Quickstart

### 1. Build the Rust CLI & Sync Python Adapters

```bash
# Build `octo` binary
cargo build --release

# Install Python adapter environment (requires uv)
cd adapters && uv sync && cd ..
```

### 2. Initialize Your Workspace

```bash
./target/release/octo init --sync-agents
```

This creates:
- `octomesh.toml` (peer definitions, role failover chains, cooldowns)
- `.octomesh/PROTOCOL.md` (agent delegation rules)
- Appends mesh instructions to `AGENTS.md` and `CLAUDE.md` (with `--sync-agents`)

### 3. Test Failover Offline Using `mock` Peers

The default `octomesh.toml.example` configures `mock-fast` (which simulates `RATE_LIMITED` when `"rate_limit"` is in the prompt) followed by `mock-safe`:

```bash
# Inspect peer health
./target/release/octo status

# Trigger automatic failover from mock-fast -> mock-safe
./target/release/octo ask coder "Implement rate_limit retry logic" --include-diff

# Verify mock-fast is now marked rate_limited on cooldown
./target/release/octo status
```

### 4. Register as an MCP Server

Add `octo mcp-serve` to your agent's MCP configuration (see `examples/mcp.json`):

```json
{
  "mcpServers": {
    "octomesh": {
      "command": "octo",
      "args": ["mcp-serve"]
    }
  }
}
```

---

## Configuration (`octomesh.toml`)

```toml
[mesh]
max_depth = 2
default_cooldown_secs = 300
state_db = ".octomesh/state.db"
python_runner = "uv"
python_runner_args = ["run", "--project", "adapters", "python", "-m", "octomesh_adapters.runner"]

[roles]
coder = ["claude", "codex", "vibe"]
reviewer = ["vibe", "claude"]
mock_chain = ["mock-fast", "mock-safe"]

[peers.claude]
adapter = "claude"
command = "claude"
mode = "tmux"            # "tmux" = trwała sesja agenta, "oneshot" = subprocess `-p` per zadanie
session_prefix = "octo"  # sesja tmux: octo-claude
timeout_secs = 180
cooldown_secs = 300

[peers.vibe]
adapter = "vibe"
command = "vibe"
mode = "tmux"
launch_args = ["--yolo"]
timeout_secs = 180
cooldown_secs = 180
```

### Peer modes: `tmux` (persistent) vs `oneshot`

- **`mode = "tmux"`** (zalecane dla vibe/codex/claude/agy): agent żyje w stałej sesji
  tmux `<session_prefix>-<peer>` (`tmux ls` pokazuje np. `octo-vibe`). Adapter robi tylko
  `send-keys -l <prompt>` + `Enter`, potem poll `capture-pane -p -S -300` aż ekran się
  ustabilizuje (3 identyczne próbki), i zwraca **samą nową część ekranu** (suffix po
  poprzednim capture). Kontekst rozmowy zostaje w sesji między kolejnymi `octo ask`.
  Wymaga tmux na hoście. `launch_args` leci jako argumenty startowe CLI w sesji.
- **`mode = "oneshot"`** (domyślny, back-compat): adapter odpala CLI jako subprocess
  (`vibe --prompt ...`, `agy -p ...`) — bez pamięci między zadaniami.

Reguła: `mock` zawsze jedzie jako oneshot; prawdziwe CLI mogą jechać w obu trybach.

---

## Adding a New CLI Adapter

## Adding a New CLI Adapter

1. Create `adapters/octomesh_adapters/my_agent.py` subclassing `BaseAdapter`:

```python
from .base import AdapterRequest, AdapterResponse, BaseAdapter, ErrorCode


class MyAgentAdapter(BaseAdapter):
    name = "my_agent"

    def execute(self, req: AdapterRequest) -> AdapterResponse:
        # Invoke your CLI headless binary and map rate-limit/timeout/auth errors
        return AdapterResponse(
            status=ErrorCode.OK,
            output="Response from my_agent",
            peer=req.peer,
        )
```

2. Register `"my_agent": MyAgentAdapter` in `adapters/octomesh_adapters/runner.py`
   (`ADAPTER_REGISTRY` dla oneshot; dopisz też do `TMUX_CAPABLE`, jeśli CLI potrafi
   żyć w sesji tmux — wtedy dostępny jako `mode = "tmux"` przez `TmuxCliAdapter`,
   z domyślnymi `launch_args` w `tmux_cli.DEFAULT_LAUNCH_ARGS`).
3. Add `[peers.my_agent]` to `octomesh.toml` and include `"my_agent"` in `[roles]`.
