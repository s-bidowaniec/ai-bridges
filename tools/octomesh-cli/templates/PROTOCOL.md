# OctoMesh Agent Collaboration Protocol

You are connected to **OctoMesh** (`octo`), a local peer-to-peer agent mesh with automatic rate-limit failover.

## Available MCP Tools
1. `mesh_status` — Check which peer agents and role chains (`coder`, `reviewer`, etc.) are `ready` or on `rate_limited` cooldown.
2. `mesh_ask` — Delegate a subtask, second opinion, or code review to another peer or role chain.
3. `mesh_handoff` — Record a structured checkpoint summary in the local SQLite blackboard before ending your session or hitting context limits.

## Mandatory Delegation Rules
1. **Never Call Yourself:** Always pass your own agent identifier in `caller` (e.g. `"claude"`, `"codex"`, `"vibe"`). Never set `target` equal to `caller`.
2. **Increment Delegation Depth:** If you were invoked with depth $d$, pass `depth: d + 1` when calling `mesh_ask`. Requests exceeding `max_depth` (default `2`) are automatically rejected to prevent infinite loops.
3. **Prefer Roles Over Hardcoded Peers:** Target roles like `"reviewer"` or `"coder"` so `octo` can fail over transparently if the primary peer hits a rate limit or timeout.
4. **Respect Readonly Reviews:** When requesting a code review or audit, set `readonly: true` and `include_diff: true`.
5. **Leave a Handoff Note:** Before terminating a multi-step task, call `mesh_handoff` with a concise summary of completed work, modified files, and remaining steps so another terminal session can resume cleanly.
