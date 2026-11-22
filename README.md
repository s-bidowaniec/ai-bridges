# ai-bridges

Monorepo mostków (bridges) między platformami AI a komunikatorami / CLI oraz narzędzi (tools) wspierających pracę agentów.

> 🥇 **Pierwszy bridge:** [`vibe-matrix`](bridges/vibe-matrix/) — Matrix ↔ Vibe CLI (działający, Python)

## Bridge'i

| # | Bridge | Status | Język | Opis |
|---|---|---|---|---|
| 1 | [`bridges/vibe-matrix`](bridges/vibe-matrix/) | ✅ działa | Python 3.12 | Mostek Matrix room ↔ Vibe CLI (`vibe -p` headless + TUI w tmux). Reakcje 👀✅❌, typing indicator, media (image/video/audio/file) w obie strony, allowlist, kolejka promptów. |
| — | `bridges/vibe-matrix-rs` | 📋 prototyp (Faza 5) | Rust | Przepisanie na Rust z hardeningiem — [docs/ROADMAP-RUST.md](docs/ROADMAP-RUST.md). Code review prototypu przez octomesh: [P1+P2](docs/REVIEW-vibe-matrix-rs-octomesh-P1-P2.md), [P3 + podsumowanie](docs/REVIEW-vibe-matrix-rs-octomesh-P3-summary.md) |

## Tools

| Tool | Status | Opis |
|---|---|---|
| [`tools/octomesh-cli`](tools/octomesh-cli/) | ✅ zweryfikowane na VPS | **OctoMesh (`octo`)** — lokalny mesh P2P między terminalowymi agentami AI (Claude, Codex, Vibe, Antigravity, Lumo) przez MCP over stdio + SQLite blackboard (health/cooldown per peer) z automatycznym failoverem w łańcuchu ról (`reviewer`, `coder`). Anti-loop guard (max_depth, caller≠target), audit trail tasków, handoff notes. Użyty do niezależnego code review `vibe-matrix-rs` (raporty w `docs/`). |

## Struktura

```
ai-bridges/
├── bridges/
│   └── <nazwa-bridga>/     # każdy bridge jako osobny katalog
│       ├── src / kod
│       ├── *.service       # systemd units
│       ├── *.sh            # skrypty pomocnicze
│       └── *.md            # dokumentacja / fix notes
├── tools/
│   └── <nazwa-narzedzia>/  # np. octomesh-cli (mesh agentów)
└── docs/
    ├── ROADMAP-RUST.md     # roadmapa przepisania na Rust + hardening
    └── REVIEW-*.md         # raporty code review (via octomesh)
```

## Zasady

- **Sekrety nigdy w repo** — tokeny tylko w `env` lokalnie (`.gitignore` pilnuje)
- Każdy bridge ma własny systemd service z hardeningiem (`ProtectSystem=strict`, `NoNewPrivileges`, etc.)
- Dokumentacja fixów (`FIX_NOTES*.md`) zostaje w repo jako historia
- Nowe bridge'i dodawaj jako kolejny katalog w `bridges/`, narzędzia agentowe do `tools/`
