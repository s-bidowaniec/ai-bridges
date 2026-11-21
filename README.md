# ai-bridges

Monorepo mostków (bridges) między platformami AI a komunikatorami / CLI.

> 🥇 **Pierwszy bridge:** [`vibe-matrix`](bridges/vibe-matrix/) — Matrix ↔ Vibe CLI (działający, Python)

## Bridge'i

| # | Bridge | Status | Język | Opis |
|---|---|---|---|---|
| 1 | [`bridges/vibe-matrix`](bridges/vibe-matrix/) | ✅ działa | Python 3.12 | Mostek Matrix room ↔ Vibe CLI (`vibe -p` headless + TUI w tmux). Reakcje 👀✅❌, typing indicator, media (image/video/audio/file) w obie strony, allowlist, kolejka promptów. |
| — | `bridges/vibe-matrix-rs` | 📋 roadmapa | Rust | Przepisanie na Rust z hardeningiem — patrz [docs/ROADMAP-RUST.md](docs/ROADMAP-RUST.md) |

## Struktura

```
ai-bridges/
├── bridges/
│   └── <nazwa-bridga>/     # każdy bridge jako osobny katalog
│       ├── src / kod
│       ├── *.service       # systemd units
│       ├── *.sh            # skrypty pomocnicze
│       └── *.md            # dokumentacja / fix notes
└── docs/
    └── ROADMAP-RUST.md     # roadmapa przepisania na Rust + hardening
```

## Zasady

- **Sekrety nigdy w repo** — tokeny tylko w `env` lokalnie (`.gitignore` pilnuje)
- Każdy bridge ma własny systemd service z hardeningiem (`ProtectSystem=strict`, `NoNewPrivileges`, etc.)
- Dokumentacja fixów (`FIX_NOTES*.md`) zostaje w repo jako historia
- Nowe bridge'i dodawaj jako kolejny katalog w `bridges/`
